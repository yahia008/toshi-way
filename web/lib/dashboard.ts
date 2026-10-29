// What the dashboard shows, worked out from the escrows the API lists. All of it is derived: nothing
// here decides what is true about an escrow (the server does that), it only decides what to put in
// front of the person and in what order.

import type { Escrow, EscrowState, Totals } from "./schemas";

export type Role = "buyer" | "seller" | "arbiter";
export type Tab = "active" | "completed" | "all";
export type RoleFilter = "all" | Role;

const ACTIVE: ReadonlySet<EscrowState> = new Set(["CREATED", "INVOICE_ISSUED", "LOCKED", "DISPUTED"]);

/** Still in play: not yet settled, refunded, expired or failed. */
export function isActive(e: Pick<Escrow, "state">): boolean {
  return ACTIVE.has(e.state);
}

/** Escrows where the person is one of the parties (operators have none). */
function asParty(e: Escrow): Role | null {
  return e.your_role === "buyer" || e.your_role === "seller" || e.your_role === "arbiter" ? e.your_role : null;
}

export type TodoKind = "accept" | "pay" | "handover" | "deliver" | "confirm" | "decide";

export type Todo = {
  escrow: Escrow;
  kind: TodoKind;
  /** The thing to do, as a short imperative. */
  title: string;
  /** Why it matters, or what to know first. */
  detail: string;
  /** When it has to be done by (unix seconds), for ordering and a countdown. */
  by: number | null;
};

/**
 * The next move, if it is *yours*. Returns null when you are only waiting on someone else, when
 * there is nothing to do, or when the deadline has already passed (the server is about to refund).
 */
export function todoFor(e: Escrow, now: number): Todo | null {
  if (!isActive(e) || now >= e.deadline_at) return null;
  const mk = (kind: TodoKind, title: string, detail: string, by: number | null): Todo => ({ escrow: e, kind, title, detail, by });

  switch (asParty(e)) {
    case "buyer":
      if (e.state === "CREATED") {
        return mk("accept", "Accept this offer", "Accepting makes a Lightning invoice for you to pay. Nothing is paid yet.", e.deadline_at);
      }
      if (e.state === "INVOICE_ISSUED") {
        return mk("pay", "Pay the invoice", "Your payment is locked, not sent, once you pay.", e.invoice_expires_at ?? e.deadline_at);
      }
      if (e.state === "LOCKED") {
        if (e.delivered_at) {
          return mk(
            "handover",
            "The seller says it was delivered",
            "Release if you have it. If not, open a dispute and add your evidence before the deadline, or you are refunded.",
            e.deadline_at,
          );
        }
        return mk("handover", "Release when you have the goods", "Show your release QR at handover, or open a dispute.", e.deadline_at);
      }
      return null;
    case "seller":
      if (e.state === "LOCKED") {
        const waitForArbiter = e.arbiter_id && !e.arbiter_confirmed_at;
        return mk(
          "deliver",
          e.delivered_at ? "Delivery claimed: get the buyer's release" : "Deliver, then release with the buyer's secret",
          e.delivered_at
            ? "Ask the buyer for their release QR. If they stay quiet, open a dispute with your proof before the deadline."
            : waitForArbiter
              ? "The arbiter hasn't confirmed yet: consider waiting before you start."
              : "The funds are locked, so it is safe to start. Release before the deadline.",
          e.deadline_at,
        );
      }
      return null;
    case "arbiter":
      if (e.state === "DISPUTED") {
        return mk("decide", "A dispute needs your decision", "Pay the seller or refund the buyer before the deadline.", e.deadline_at);
      }
      if ((e.state === "INVOICE_ISSUED" || e.state === "LOCKED") && e.arbiter_sealed && !e.arbiter_confirmed_at) {
        return mk("confirm", "Check the sealed secret and confirm", "The seller is waiting to know you can release this.", e.deadline_at);
      }
      return null;
    default:
      return null;
  }
}

/** Every move that is yours, most urgent first (no deadline last). */
export function todos(escrows: Escrow[], now: number): Todo[] {
  return escrows
    .map((e) => todoFor(e, now))
    .filter((t): t is Todo => t !== null)
    .sort((a, b) => (a.by ?? Infinity) - (b.by ?? Infinity) || b.escrow.updated_at - a.escrow.updated_at);
}

/** One line on what happens next, for any escrow, from this person's point of view. */
export function nextLine(e: Escrow): string {
  const role = asParty(e);
  switch (e.state) {
    case "CREATED":
      return role === "buyer" ? "Waiting for you to accept" : "Waiting for the buyer to accept";
    case "INVOICE_ISSUED":
      return role === "buyer" ? "Waiting for your payment" : "Waiting for the buyer's payment";
    case "LOCKED":
      if (role === "arbiter") return e.arbiter_confirmed_at ? "Nothing to do unless a dispute opens" : "Waiting for you to confirm";
      if (e.delivered_at) return role === "buyer" ? "The seller says it's delivered: release or dispute" : "Delivery claimed: waiting for the buyer";
      return role === "buyer" ? "Funds locked: release at handover" : "Funds locked: deliver, then release";
    case "DISPUTED":
      return role === "arbiter" ? "Waiting for your decision" : e.arbiter_id ? "Waiting for the arbiter" : "Waiting for the operator";
    case "SETTLED":
      return role === "seller" ? "You were paid" : role === "buyer" ? "The seller was paid" : "Paid to the seller";
    case "REFUNDED":
      return role === "buyer" ? "You were refunded" : "Refunded to the buyer";
    case "EXPIRED_UNPAID":
      return "Ended without a payment";
    case "FAILED":
      return "Needs the operator's attention";
  }
}

export type Summary = {
  /** Moves that are yours right now. */
  attention: number;
  /** Escrows still in play, in any role. */
  active: number;
  /** Sats you have locked as a buyer, and sats locked for you as a seller. */
  lockedOut: number;
  lockedIn: number;
  lockedCount: number;
  settled: { count: number; received: number; paid: number };
  refunded: { count: number; sats: number };
};

/** The headline numbers. Only money you are a party to counts: an arbiter's escrows are not theirs. */
export function summarize(escrows: Escrow[], now: number, totals?: Totals): Summary {
  const s: Summary = {
    attention: todos(escrows, now).length,
    active: escrows.filter(isActive).length,
    lockedOut: 0,
    lockedIn: 0,
    lockedCount: 0,
    settled: { count: 0, received: 0, paid: 0 },
    refunded: { count: 0, sats: 0 },
  };
  for (const e of escrows) {
    const role = asParty(e);
    if (e.state === "LOCKED" || e.state === "DISPUTED") {
      if (role === "buyer") s.lockedOut += e.amount_sats;
      if (role === "seller") s.lockedIn += e.amount_sats;
      if (role === "buyer" || role === "seller") s.lockedCount += 1;
    } else if (e.state === "SETTLED") {
      s.settled.count += 1;
      if (role === "seller") s.settled.received += e.amount_sats;
      if (role === "buyer") s.settled.paid += e.amount_sats;
    } else if (e.state === "REFUNDED") {
      s.refunded.count += 1;
      if (role === "buyer" || role === "seller") s.refunded.sats += e.amount_sats;
    }
  }
  // The list holds every escrow in play but only the newest that have ended; the server's totals cover them all.
  if (totals) {
    s.settled = { ...totals.settled };
    s.refunded = { ...totals.refunded };
  }
  return s;
}

/** The escrows for a tab and role filter, newest activity first. */
export function filterEscrows(escrows: Escrow[], tab: Tab, role: RoleFilter): Escrow[] {
  return escrows
    .filter((e) => (tab === "all" ? true : tab === "active" ? isActive(e) : !isActive(e)))
    .filter((e) => role === "all" || e.your_role === role)
    .sort((a, b) => b.updated_at - a.updated_at || (a.id < b.id ? -1 : 1));
}

/** How many escrows each role has, for the filter chips. */
export function roleCounts(escrows: Escrow[]): Record<RoleFilter, number> {
  const c: Record<RoleFilter, number> = { all: escrows.length, buyer: 0, seller: 0, arbiter: 0 };
  for (const e of escrows) {
    const r = asParty(e);
    if (r) c[r] += 1;
  }
  return c;
}

/** The most recently changed escrows. */
export function recentActivity(escrows: Escrow[], n = 5): Escrow[] {
  return [...escrows].sort((a, b) => b.updated_at - a.updated_at || (a.id < b.id ? -1 : 1)).slice(0, n);
}

/** "just now", "5m ago", "3h ago", "2d ago". */
export function timeAgo(now: number, then: number): string {
  const secs = Math.max(0, Math.floor(now - then));
  if (secs < 45) return "just now";
  if (secs < 3600) return `${Math.max(1, Math.round(secs / 60))}m ago`;
  if (secs < 86_400) return `${Math.round(secs / 3600)}h ago`;
  return `${Math.round(secs / 86_400)}d ago`;
}

/** "29m", "2h 5m", "1d 3h": a short time left, for tight spaces. */
export function shortDuration(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86_400) {
    const m = Math.floor((s % 3600) / 60);
    return m ? `${Math.floor(s / 3600)}h ${m}m` : `${Math.floor(s / 3600)}h`;
  }
  const h = Math.floor((s % 86_400) / 3600);
  return h ? `${Math.floor(s / 86_400)}d ${h}h` : `${Math.floor(s / 86_400)}d`;
}

/** A short form of an id, for showing next to a title. */
export function shortId(id: string): string {
  return id.slice(0, 8);
}

/** A greeting that depends on the hour (local time), for the page header. */
export function greeting(hour: number): string {
  if (hour < 5) return "Working late";
  if (hour < 12) return "Good morning";
  if (hour < 18) return "Good afternoon";
  return "Good evening";
}
