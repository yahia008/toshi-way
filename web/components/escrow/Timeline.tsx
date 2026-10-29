"use client";

import { formatLocalTime } from "@/lib/clock";
import type { Escrow, EscrowEvents } from "@/lib/schemas";
import { Card } from "../ui";

// Audit entries record transitions; describe each by the state it led to.
const toStateText: Record<string, string> = {
  INVOICE_ISSUED: "Buyer accepted; hold invoice issued",
  LOCKED: "Funds locked",
  SETTLED: "Seller paid",
  REFUNDED: "Buyer refunded",
  EXPIRED_UNPAID: "Ended without payment",
  DISPUTED: "Dispute opened",
  FAILED: "Marked for operator attention",
};

const nodeText: Record<string, string> = {
  OPEN: "Invoice open, waiting for payment",
  ACCEPTED: "Payment locked by the node",
  SETTLED: "Node settled the payment",
  CANCELED: "Node cancelled the invoice",
};

type Row = { at: number; title: string; detail?: string; source: "app" | "node" };

export function Timeline({ events, escrow }: { events: EscrowEvents | null; escrow: Escrow }) {
  if (!events) return null;
  const who = (actor: string) =>
    actor === escrow.buyer_id
      ? "the buyer"
      : actor === escrow.seller_id
        ? "the seller"
        : actor === escrow.arbiter_id
          ? "the arbiter"
          : actor;
  const rows: Row[] = [
    { at: escrow.created_at, title: "Offer created by the seller", source: "app" as const },
    ...events.audit.map((a) => ({
      at: a.at,
      title:
        a.action === "arbiter_confirm"
          ? "Arbiter confirmed they can release this escrow"
          : a.action === "arbiter_decision"
            ? "Arbiter decided"
            : a.action === "delivered"
              ? "Seller says it was delivered"
              : a.action === "evidence"
                ? "Evidence added"
                : (a.to_state && toStateText[a.to_state]) || `${a.from_state ?? "?"} → ${a.to_state ?? "?"}`,
      detail: [a.actor && `by ${who(a.actor)}`, a.reason].filter(Boolean).join(" · "),
      source: "app" as const,
    })),
    ...events.node_events.map((n) => ({
      at: n.at,
      title: nodeText[n.node_invoice_state] ?? `Node reported ${n.node_invoice_state}`,
      source: "node" as const,
    })),
  ].sort((a, b) => a.at - b.at);

  return (
    <Card title="History">
      {rows.length === 0 ? (
        <p className="text-sm text-neutral-500">Nothing yet.</p>
      ) : (
        <ol className="space-y-3 border-l border-neutral-200 pl-4 dark:border-neutral-800">
          {rows.map((r, i) => (
            <li key={i} className="relative">
              <span
                className={`absolute -left-[1.3rem] top-1.5 size-2.5 rounded-full ${r.source === "node" ? "bg-amber-500" : "bg-neutral-400"}`}
              />
              <div className="text-sm font-medium">
                {r.title}
                {r.source === "node" && (
                  <span className="ml-2 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-semibold uppercase text-amber-900 dark:bg-amber-950 dark:text-amber-200">
                    node
                  </span>
                )}
              </div>
              <div className="text-xs text-neutral-500">
                {formatLocalTime(r.at)}
                {r.detail ? ` · ${r.detail}` : ""}
              </div>
            </li>
          ))}
        </ol>
      )}
    </Card>
  );
}
