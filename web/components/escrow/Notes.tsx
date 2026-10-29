"use client";

import { useState, type FormEvent } from "react";
import { api } from "@/lib/api";
import { formatLocalTime } from "@/lib/clock";
import {
  NoteForm,
  type AuditEntry,
  type Escrow,
  type EscrowEvents,
} from "@/lib/schemas";
import { Button, Field, inputClass, Notice } from "../ui";

type Run = (name: string, fn: () => Promise<Escrow>) => Promise<boolean>;

const isNote = (a: AuditEntry) =>
  a.action === "delivered" || a.action === "evidence";

/** What each side said while the money was locked, oldest first. */
export function notesOf(events: EscrowEvents | null | undefined): AuditEntry[] {
  return (events?.audit ?? []).filter(isNote).sort((a, b) => a.at - b.at);
}

const who = (escrow: Escrow, actor: string) =>
  actor === escrow.seller_id
    ? "The seller"
    : actor === escrow.buyer_id
      ? "The buyer"
      : "A party";

/** The delivery claim and evidence so far, for whoever has to decide. Read-only. */
export function EvidenceList({
  escrow,
  events,
  title = "Delivery and evidence",
}: {
  escrow: Escrow;
  events: EscrowEvents | null | undefined;
  title?: string;
}) {
  const notes = notesOf(events);
  if (notes.length === 0) return null;
  return (
    <div className="space-y-2 rounded-lg border border-neutral-200 p-3 text-sm dark:border-neutral-800">
      <div className="text-xs font-semibold uppercase tracking-wide text-neutral-500">
        {title}
      </div>
      <ul className="space-y-2">
        {notes.map((n, i) => (
          <li key={i}>
            <span className="font-medium">
              {who(escrow, n.actor)}
              {n.action === "delivered" ? " says it was delivered" : ""}:
            </span>{" "}
            <span className="whitespace-pre-wrap break-words">{n.reason}</span>
            <span className="ml-2 text-xs text-neutral-500">
              {formatLocalTime(n.at)}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * Where the two sides say what happened while the money is locked. The seller can say it was
 * delivered, and either side can add evidence. None of it moves money: it is the record whoever
 * decides a dispute reads, and it nudges a quiet buyer to release or object before the deadline.
 */
export function NotesBox({
  escrow,
  events,
  run,
  busy,
}: {
  escrow: Escrow;
  events: EscrowEvents | null | undefined;
  run: Run;
  busy: string | null;
}) {
  const seller = escrow.your_role === "seller";
  const notes = notesOf(events);
  const delivered = notes.find((n) => n.action === "delivered");
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);

  async function send(kind: "delivered" | "evidence", e?: FormEvent) {
    e?.preventDefault();
    const parsed = NoteForm.safeParse(text);
    if (!parsed.success)
      return setError(parsed.error.issues[0]?.message ?? "Invalid note");
    if (await run(kind, () => api.addNote(escrow.id, kind, parsed.data)))
      setText("");
  }

  return (
    <div className="space-y-4">
      {delivered && !seller && (
        <Notice tone="warn">
          <strong>The seller says this was delivered.</strong> If you have what
          you paid for, release the payment. If not, open a dispute or add
          evidence before the deadline: if you do nothing, you are refunded when
          it passes, and the seller may object.
        </Notice>
      )}
      {delivered && seller && (
        <Notice tone="success">
          You marked this delivered. The buyer has been told on this page.
        </Notice>
      )}
      <EvidenceList escrow={escrow} events={events} />
      <form
        onSubmit={(e) => send("evidence", e)}
        className="space-y-3 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800"
      >
        <div className="font-semibold">
          {seller && !delivered
            ? "Delivered? Say so, with proof"
            : "Add evidence"}
        </div>
        <p className="text-sm text-neutral-600 dark:text-neutral-400">
          {seller && !delivered
            ? "A tracking number, a signed receipt or a link. It doesn't release the payment: only the buyer's secret does. It tells the buyer, and it is what an arbiter reads if there is a dispute."
            : "A link, a reference number or a short description. Both sides and the arbiter can read it."}
        </p>
        <Field
          label="Your note"
          error={error ?? undefined}
          hint={`${text.length}/500`}
        >
          <textarea
            value={text}
            onChange={(e) => {
              setText(e.target.value);
              setError(null);
            }}
            rows={3}
            maxLength={500}
            className={inputClass}
          />
        </Field>
        <div className="flex flex-wrap gap-2">
          {seller && !delivered && (
            <Button
              type="button"
              busy={busy === "delivered"}
              onClick={() => send("delivered")}
            >
              Mark as delivered
            </Button>
          )}
          <Button
            type="submit"
            variant={seller && !delivered ? "secondary" : "primary"}
            busy={busy === "evidence"}
          >
            Add evidence
          </Button>
        </div>
      </form>
    </div>
  );
}
