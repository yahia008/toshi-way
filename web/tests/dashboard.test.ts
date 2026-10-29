import { describe, expect, it } from "vitest";
import {
  filterEscrows,
  greeting,
  isActive,
  nextLine,
  recentActivity,
  roleCounts,
  shortDuration,
  shortId,
  summarize,
  timeAgo,
  todoFor,
  todos,
} from "@/lib/dashboard";
import { Escrow } from "@/lib/schemas";

const NOW = 10_000;

function escrow(patch: Partial<Record<string, unknown>> = {}): Escrow {
  return Escrow.parse({
    id: "11111111-aaaa", buyer_id: "u_b", seller_id: "u_s", amount_sats: 1000, description: "parcel",
    payment_hash: null, state: "CREATED", invoice_expires_at: null, deadline_at: NOW + 3600,
    created_at: NOW - 100, updated_at: NOW - 100, your_role: "buyer", ...patch,
  });
}

describe("whose move is it", () => {
  it("is the buyer's to accept an offer, then to pay, then to release", () => {
    expect(todoFor(escrow({ state: "CREATED" }), NOW)?.kind).toBe("accept");
    const pay = todoFor(escrow({ state: "INVOICE_ISSUED", invoice_expires_at: NOW + 300 }), NOW);
    expect(pay?.kind).toBe("pay");
    expect(pay?.by).toBe(NOW + 300); // the invoice expires before the deadline: that is the urgent time
    expect(todoFor(escrow({ state: "LOCKED" }), NOW)?.kind).toBe("handover");
    // Once the seller claims delivery, the buyer is told, and the seller's line changes too.
    const claimed = escrow({ state: "LOCKED", delivered_at: NOW - 60 });
    expect(todoFor(claimed, NOW)?.title).toBe("The seller says it was delivered");
    expect(nextLine(claimed)).toMatch(/delivered: release or dispute/);
    expect(todoFor(escrow({ state: "LOCKED", your_role: "seller", delivered_at: NOW - 60 }), NOW)?.title).toMatch(/Delivery claimed/);
    expect(nextLine(escrow({ state: "LOCKED", your_role: "seller", delivered_at: NOW - 60 }))).toBe("Delivery claimed: waiting for the buyer");
  });

  it("is not the seller's while they wait for payment, only once the funds are locked", () => {
    for (const state of ["CREATED", "INVOICE_ISSUED"]) {
      expect(todoFor(escrow({ state, your_role: "seller" }), NOW)).toBeNull();
    }
    expect(todoFor(escrow({ state: "LOCKED", your_role: "seller" }), NOW)?.kind).toBe("deliver");
  });

  it("tells the seller to wait when an arbiter has not confirmed", () => {
    const t = todoFor(escrow({ state: "LOCKED", your_role: "seller", arbiter_id: "u_j" }), NOW);
    expect(t?.detail).toMatch(/hasn't confirmed/);
    const ok = todoFor(escrow({ state: "LOCKED", your_role: "seller", arbiter_id: "u_j", arbiter_confirmed_at: NOW - 5 }), NOW);
    expect(ok?.detail).not.toMatch(/hasn't confirmed/);
  });

  it("is the arbiter's to confirm a sealed secret and to decide a dispute", () => {
    const base = { your_role: "arbiter", arbiter_id: "u_j" };
    expect(todoFor(escrow({ ...base, state: "LOCKED", arbiter_sealed: true }), NOW)?.kind).toBe("confirm");
    expect(todoFor(escrow({ ...base, state: "LOCKED", arbiter_sealed: true, arbiter_confirmed_at: NOW - 1 }), NOW)).toBeNull();
    expect(todoFor(escrow({ ...base, state: "INVOICE_ISSUED", arbiter_sealed: false }), NOW)).toBeNull();
    expect(todoFor(escrow({ ...base, state: "DISPUTED" }), NOW)?.kind).toBe("decide");
  });

  it("is nobody's once an escrow has ended, been disputed without an arbiter role, or passed its deadline", () => {
    for (const state of ["SETTLED", "REFUNDED", "EXPIRED_UNPAID", "FAILED"]) {
      expect(todoFor(escrow({ state }), NOW)).toBeNull();
    }
    expect(todoFor(escrow({ state: "DISPUTED", your_role: "buyer" }), NOW)).toBeNull();
    expect(todoFor(escrow({ state: "LOCKED", deadline_at: NOW }), NOW)).toBeNull();
    expect(todoFor(escrow({ state: "LOCKED", your_role: "operator" }), NOW)).toBeNull();
  });

  it("orders the most urgent first, with no deadline last", () => {
    const list = [
      escrow({ id: "a", state: "LOCKED", deadline_at: NOW + 3000 }),
      escrow({ id: "b", state: "INVOICE_ISSUED", invoice_expires_at: NOW + 120 }),
      escrow({ id: "c", state: "LOCKED", deadline_at: NOW + 900 }),
    ];
    expect(todos(list, NOW).map((t) => t.escrow.id)).toEqual(["b", "c", "a"]);
  });
});

describe("what happens next, as a line", () => {
  it("speaks to the person's own role", () => {
    expect(nextLine(escrow({ state: "INVOICE_ISSUED" }))).toBe("Waiting for your payment");
    expect(nextLine(escrow({ state: "INVOICE_ISSUED", your_role: "seller" }))).toBe("Waiting for the buyer's payment");
    expect(nextLine(escrow({ state: "SETTLED", your_role: "seller" }))).toBe("You were paid");
    expect(nextLine(escrow({ state: "REFUNDED" }))).toBe("You were refunded");
    expect(nextLine(escrow({ state: "DISPUTED", arbiter_id: "u_j" }))).toBe("Waiting for the arbiter");
    expect(nextLine(escrow({ state: "DISPUTED" }))).toBe("Waiting for the operator");
    expect(nextLine(escrow({ state: "LOCKED", your_role: "arbiter", arbiter_confirmed_at: 1 }))).toMatch(/unless a dispute/);
  });
});

describe("the headline numbers", () => {
  const list = [
    escrow({ id: "1", state: "LOCKED", amount_sats: 5000, your_role: "buyer" }),
    escrow({ id: "2", state: "LOCKED", amount_sats: 2000, your_role: "seller" }),
    escrow({ id: "3", state: "DISPUTED", amount_sats: 700, your_role: "seller" }),
    escrow({ id: "4", state: "LOCKED", amount_sats: 9999, your_role: "arbiter" }),
    escrow({ id: "5", state: "SETTLED", amount_sats: 3000, your_role: "seller" }),
    escrow({ id: "6", state: "SETTLED", amount_sats: 1500, your_role: "buyer" }),
    escrow({ id: "7", state: "REFUNDED", amount_sats: 400, your_role: "buyer" }),
    escrow({ id: "8", state: "REFUNDED", amount_sats: 800, your_role: "arbiter" }),
    escrow({ id: "9", state: "EXPIRED_UNPAID", amount_sats: 100 }),
  ];

  it("counts only money the person is a party to, not an arbiter's escrows", () => {
    const s = summarize(list, NOW);
    expect(s.lockedOut).toBe(5000);
    expect(s.lockedIn).toBe(2700);
    expect(s.lockedCount).toBe(3);
    expect(s.settled).toEqual({ count: 2, received: 3000, paid: 1500 });
    expect(s.refunded).toEqual({ count: 2, sats: 400 });
  });

  it("counts active escrows and the moves that are yours", () => {
    const s = summarize(list, NOW);
    expect(s.active).toBe(4);
    // buyer LOCKED (handover) + seller LOCKED (deliver); the dispute and arbiter-locked one are not theirs.
    expect(s.attention).toBe(2);
  });

  it("takes settled and refunded totals from the server when it sends them", () => {
    const totals = {
      escrows: 400,
      settled: { count: 250, received: 90000, paid: 40000 },
      refunded: { count: 60, sats: 12000 },
    };
    const s = summarize(list, NOW, totals);
    expect(s.settled).toEqual(totals.settled);
    expect(s.refunded).toEqual(totals.refunded);
    // What is in play still comes from the list, which always holds all of it.
    expect(s.lockedOut).toBe(5000);
    expect(s.active).toBe(4);
  });

  it("is all zeros for nobody", () => {
    expect(summarize([], NOW)).toEqual({
      attention: 0, active: 0, lockedOut: 0, lockedIn: 0, lockedCount: 0,
      settled: { count: 0, received: 0, paid: 0 }, refunded: { count: 0, sats: 0 },
    });
  });
});

describe("filtering", () => {
  const list = [
    escrow({ id: "a", state: "LOCKED", updated_at: 5, your_role: "buyer" }),
    escrow({ id: "b", state: "SETTLED", updated_at: 9, your_role: "seller" }),
    escrow({ id: "c", state: "CREATED", updated_at: 7, your_role: "seller" }),
    escrow({ id: "d", state: "REFUNDED", updated_at: 3, your_role: "arbiter" }),
  ];
  const ids = (xs: Escrow[]) => xs.map((e) => e.id);

  it("splits active from completed and orders by latest activity", () => {
    expect(ids(filterEscrows(list, "active", "all"))).toEqual(["c", "a"]);
    expect(ids(filterEscrows(list, "completed", "all"))).toEqual(["b", "d"]);
    expect(ids(filterEscrows(list, "all", "all"))).toEqual(["b", "c", "a", "d"]);
  });

  it("filters by role and counts each role", () => {
    expect(ids(filterEscrows(list, "all", "seller"))).toEqual(["b", "c"]);
    expect(ids(filterEscrows(list, "completed", "arbiter"))).toEqual(["d"]);
    expect(roleCounts(list)).toEqual({ all: 4, buyer: 1, seller: 2, arbiter: 1 });
  });

  it("knows which states are in play", () => {
    expect(["CREATED", "INVOICE_ISSUED", "LOCKED", "DISPUTED"].every((state) => isActive({ state } as Escrow))).toBe(true);
    expect(["SETTLED", "REFUNDED", "EXPIRED_UNPAID", "FAILED"].some((state) => isActive({ state } as Escrow))).toBe(false);
  });

  it("lists recent activity, newest first, capped", () => {
    expect(ids(recentActivity(list, 2))).toEqual(["b", "c"]);
    expect(recentActivity(list).length).toBe(4);
  });
});

describe("small formatting", () => {
  it("says how long ago", () => {
    expect(timeAgo(1000, 990)).toBe("just now");
    expect(timeAgo(1000, 1000 - 300)).toBe("5m ago");
    expect(timeAgo(1000, 1000 - 7200)).toBe("2h ago");
    expect(timeAgo(1000, 1000 - 3 * 86_400)).toBe("3d ago");
    expect(timeAgo(1000, 2000)).toBe("just now"); // a clock a little behind never goes negative
  });

  it("shortens a time left", () => {
    expect(shortDuration(42)).toBe("42s");
    expect(shortDuration(29 * 60 + 50)).toBe("29m");
    expect(shortDuration(2 * 3600 + 5 * 60)).toBe("2h 5m");
    expect(shortDuration(3 * 3600)).toBe("3h");
    expect(shortDuration(86_400 + 3 * 3600)).toBe("1d 3h");
    expect(shortDuration(-5)).toBe("0s");
  });

  it("shortens ids and greets by the hour", () => {
    expect(shortId("0a42d207-4b97-4c59")).toBe("0a42d207");
    expect(greeting(3)).toBe("Working late");
    expect(greeting(9)).toBe("Good morning");
    expect(greeting(14)).toBe("Good afternoon");
    expect(greeting(20)).toBe("Good evening");
  });
});
