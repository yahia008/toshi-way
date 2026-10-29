"use client";

import { useRouter } from "next/navigation";
import { useState, type FormEvent, type ReactNode } from "react";
import {
  ArrowIcon,
  CheckIcon,
  LockIcon,
  QrIcon,
  ShieldIcon,
} from "../landing/icons";
import { Button, CopyField, inputClass } from "../ui";

export type Side = "seller" | "buyer";

const choices: { side: Side; title: string; body: string; icon: ReactNode }[] =
  [
    {
      side: "seller",
      title: "I'm getting paid",
      body: "Set the amount and a deadline, then send your buyer the link. They accept and pay; the money stays locked until released.",
      icon: <QrIcon className="size-5" />,
    },
    {
      side: "buyer",
      title: "I'm paying",
      body: "Open the escrow link your seller sent you, or give them your user id so they can make you an offer.",
      icon: <LockIcon className="size-5" />,
    },
  ];

/** The first question of a new escrow: which side of the deal are you on? */
export function SidePicker({
  side,
  onChange,
}: {
  side: Side;
  onChange: (s: Side) => void;
}) {
  return (
    <section aria-labelledby="side-heading" className="space-y-3">
      <h2 id="side-heading" className="text-2xl font-bold tracking-tight">
        Which side of this deal are you on?
      </h2>
      <div
        role="radiogroup"
        aria-labelledby="side-heading"
        className="grid gap-4 sm:grid-cols-2"
      >
        {choices.map((c) => {
          const on = c.side === side;
          return (
            <button
              key={c.side}
              type="button"
              role="radio"
              aria-checked={on}
              onClick={() => onChange(c.side)}
              className={`relative rounded-2xl border p-5 text-left transition-colors ${
                on
                  ? "border-neutral-950 bg-neutral-950 text-white dark:border-amber-500 dark:bg-neutral-900"
                  : "border-neutral-200 bg-white hover:border-neutral-400 dark:border-neutral-800 dark:bg-neutral-900/40 dark:hover:border-neutral-600"
              }`}
            >
              {on && (
                <span className="absolute right-4 top-4 grid size-6 place-items-center rounded-full bg-emerald-500 text-white">
                  <CheckIcon className="size-4" />
                </span>
              )}
              <span
                className={`grid size-10 place-items-center rounded-lg ${on ? "bg-white/10" : "bg-neutral-100 dark:bg-neutral-800"}`}
              >
                {c.icon}
              </span>
              <div className="mt-4 text-lg font-semibold">{c.title}</div>
              <p
                className={`mt-1 text-sm ${on ? "text-neutral-300" : "text-neutral-600 dark:text-neutral-400"}`}
              >
                {c.body}
              </p>
            </button>
          );
        })}
      </div>
    </section>
  );
}

/** What a buyer does: escrows are made by sellers, so a buyer opens the seller's link. */
export function BuyerPanel({ userId }: { userId: string | null }) {
  const router = useRouter();
  const [link, setLink] = useState("");
  const [error, setError] = useState<string | null>(null);

  function open(e: FormEvent) {
    e.preventDefault();
    try {
      const url = new URL(link.trim(), window.location.origin);
      if (
        url.origin !== window.location.origin ||
        !url.pathname.startsWith("/escrows/")
      )
        throw new Error();
      router.push(url.pathname + url.hash);
    } catch {
      setError(
        "That isn't an escrow link from this site. Paste the full link your seller sent you.",
      );
    }
  }

  return (
    <div className="space-y-6">
      <form
        onSubmit={open}
        className="rounded-2xl border border-neutral-200 bg-white p-6 dark:border-neutral-800 dark:bg-neutral-900/60"
        noValidate
      >
        <h2 className="text-lg font-semibold">Have a link from your seller?</h2>
        <p className="mt-1 text-sm text-neutral-600 dark:text-neutral-400">
          Paste it here to review the offer, accept it and pay. Your payment
          stays locked until you release it.
        </p>
        <div className="mt-4 flex flex-col gap-2 sm:flex-row">
          <input
            aria-label="Escrow link"
            value={link}
            onChange={(e) => {
              setLink(e.target.value);
              setError(null);
            }}
            className={`${inputClass} font-mono`}
            placeholder="https://…/escrows/…"
            autoComplete="off"
            spellCheck={false}
          />
          <Button type="submit" disabled={!link.trim()}>
            Open escrow <ArrowIcon className="ml-1 inline size-4" />
          </Button>
        </div>
        {error && (
          <p className="mt-2 text-xs text-red-700 dark:text-red-400">{error}</p>
        )}
      </form>
      <div className="rounded-2xl border border-neutral-200 bg-white p-6 dark:border-neutral-800 dark:bg-neutral-900/60">
        <h2 className="text-lg font-semibold">
          Or give the seller your user id
        </h2>
        {userId ? (
          <div className="mt-3 space-y-2">
            <CopyField value={userId} />
            <p className="text-xs text-neutral-500">
              Not secret. They enter it as the buyer when they make you an
              offer.
            </p>
          </div>
        ) : (
          <p className="mt-1 text-sm text-neutral-600 dark:text-neutral-400">
            Your user id isn&apos;t stored on this device yet. It appears on
            your dashboard once you have an escrow.
          </p>
        )}
      </div>
      <p className="flex items-start gap-2 text-sm text-neutral-600 dark:text-neutral-400">
        <ShieldIcon className="mt-0.5 size-4 shrink-0" />
        The platform never holds your money. It stays locked by the Lightning
        network until you release it or the hold time runs out.
      </p>
    </div>
  );
}

export const escrowTypes = [
  {
    id: "p2p",
    dealTitle: "P2P asset sale",
    template:
      "Buyer receives:\n\u2022 [item being sold]\n\u2022 Condition: [new / used]\n\u2022 Transfer method: [details]\n\u2022 Buyer confirms receipt before funds release",
    evidence: [
      "A payment receipt or on-chain transaction id that matches the agreed amount and recipient",
      "Proof the item was handed over, such as a courier receipt",
      "Note: a bank transfer can be reversed later, so it counts as weaker proof",
    ],
    title: "P2P asset sale",
    body: "Personal goods or a crypto exchange",
    placeholder: "e.g. Vintage watch, sold as described, shipped insured",
  },
  {
    id: "service",
    dealTitle: "Freelance / services",
    template:
      "Seller delivers:\n\u2022 [the work, e.g. 5-page website]\n\u2022 Revisions included: [number]\n\u2022 Deadline: [date]\n\u2022 Buyer releases the funds once the work is accepted",
    evidence: [
      "The delivered files or a link to the finished work",
      "The buyer's messages accepting it or asking for revisions",
      "How many revisions were agreed and used",
    ],
    title: "Freelance / services",
    body: "Work delivered for payment",
    placeholder:
      "e.g. 5-page portfolio site with CMS and 3 rounds of revisions",
  },
  {
    id: "goods",
    dealTitle: "Cargo / goods shipment",
    template:
      "Seller ships:\n\u2022 [goods and quantity]\n\u2022 Shipping method and tracking: [details]\n\u2022 Delivery address and date: [details]\n\u2022 Buyer releases the funds once the goods arrive as described",
    evidence: [
      "The courier's signed delivery record or tracking showing delivered",
      "Photos of the goods at dispatch and on arrival",
      "Proof it went to the agreed address",
    ],
    title: "Cargo / goods shipment",
    body: "Physical goods, shipped and tracked",
    placeholder: "e.g. 20 boxes of coffee beans, tracked delivery to Lagos",
  },
  {
    id: "digital",
    dealTitle: "Digital asset transfer",
    template:
      "Seller transfers:\n\u2022 [domain, website, NFT or account]\n\u2022 Transfer method: [registrar / marketplace / credentials]\n\u2022 Everything needed to take full control\n\u2022 Buyer releases the funds once control is confirmed",
    evidence: [
      "A registrar or marketplace record showing the new owner",
      "Proof the seller kept no way back in, such as changed credentials",
      "Screenshots of the transfer being completed",
    ],
    title: "Digital asset transfer",
    body: "Domains, websites, NFTs, accounts",
    placeholder:
      "e.g. The domain example.com, transferred to my registrar account",
  },
  {
    id: "vehicle",
    dealTitle: "Vehicle sale",
    template:
      "Seller hands over:\n\u2022 [make, model, year, mileage]\n\u2022 Condition and known faults: [details]\n\u2022 Papers and keys included\n\u2022 Buyer releases the funds after inspection and handover",
    evidence: [
      "A signed handover form and the transfer of title",
      "Photos or video of the vehicle at handover",
      "Papers and keys listed as handed over",
    ],
    title: "Vehicle sale",
    body: "Cars, bikes, boats — private sale",
    placeholder:
      "e.g. 2018 Toyota Corolla, 62,000 km, with papers and one set of keys",
  },
  {
    id: "property",
    dealTitle: "Property deal",
    template:
      "Deal covers:\n\u2022 [property or deposit and address]\n\u2022 Amount and what it secures: [details]\n\u2022 Conditions for refund or release: [details]\n\u2022 Buyer releases the funds once the terms are met",
    evidence: [
      "A signed agreement setting out the conditions",
      "A record that the conditions were met, such as a handover receipt",
      "Note: the arbiter cannot settle legal ownership, so keep this to deposits and handovers",
    ],
    title: "Property / real estate",
    body: "Deposits or private property deals",
    placeholder: "e.g. Rental deposit for a 2-bed flat, refundable on handover",
  },
] as const;

const typeIcons: Record<string, ReactNode> = {
  p2p: <path d="M7 8h11l-3-3m3 11H7l3 3" />,
  service: (
    <>
      <rect x="3.5" y="7.5" width="17" height="12" rx="2" />
      <path d="M9 7.5V6a1.5 1.5 0 0 1 1.5-1.5h3A1.5 1.5 0 0 1 15 6v1.5M3.5 13h17" />
    </>
  ),
  goods: (
    <path d="m12 3 8 4.5v9L12 21l-8-4.5v-9L12 3Zm0 9 8-4.5M12 12v9M12 12 4 7.5" />
  ),
  digital: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M3.5 12h17M12 3.5c2.5 2.5 3.5 5.5 3.5 8.5s-1 6-3.5 8.5c-2.5-2.5-3.5-5.5-3.5-8.5s1-6 3.5-8.5Z" />
    </>
  ),
  vehicle: (
    <>
      <path d="M4 15v-3l2-4.5h12L20 12v3M4 15h16M4 15v2.5M20 15v2.5" />
      <circle cx="8" cy="15" r="0.6" />
      <circle cx="16" cy="15" r="0.6" />
    </>
  ),
  property: <path d="M4 11 12 4l8 7M6 9.5V20h12V9.5M10 20v-6h4v6" />,
};

/** If it comes to a dispute, what the arbiter will look for. Shown while the seller sets the deal up. */
export function EvidenceGuide({ type }: { type: EscrowType }) {
  const t = escrowTypes.find((x) => x.id === type)!;
  return (
    <div className="rounded-xl border border-neutral-200 p-4 text-sm dark:border-neutral-800">
      <div className="font-semibold">
        If there&apos;s a dispute, an arbiter will look for
      </div>
      <ul className="mt-2 list-disc space-y-1 pl-5 text-neutral-600 dark:text-neutral-400">
        {t.evidence.map((e) => (
          <li key={e}>{e}</li>
        ))}
      </ul>
      <p className="mt-2 text-xs text-neutral-500">
        Either side can add this evidence on the escrow page once the money is
        locked.
      </p>
    </div>
  );
}

export type EscrowType = (typeof escrowTypes)[number]["id"];

/** "What is this escrow for?" Picking one tailors the description prompt. */
export function TypePicker({
  value,
  onChange,
}: {
  value: EscrowType;
  onChange: (t: EscrowType) => void;
}) {
  return (
    <div
      role="radiogroup"
      aria-label="What is this escrow for?"
      className="grid gap-3 sm:grid-cols-3"
    >
      {escrowTypes.map((t) => {
        const on = t.id === value;
        return (
          <button
            key={t.id}
            type="button"
            role="radio"
            aria-checked={on}
            onClick={() => onChange(t.id)}
            className={`rounded-xl border-2 p-4 text-left transition-colors ${
              on
                ? "border-amber-500 bg-amber-50 dark:bg-amber-500/10"
                : "border-neutral-200 hover:border-neutral-400 dark:border-neutral-800 dark:hover:border-neutral-600"
            }`}
          >
            <span className="mb-3 grid size-9 place-items-center rounded-lg bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-300">
              <svg
                viewBox="0 0 24 24"
                className="size-5"
                fill="none"
                stroke="currentColor"
                strokeWidth={1.7}
                strokeLinecap="round"
                strokeLinejoin="round"
                aria-hidden
              >
                {typeIcons[t.id]}
              </svg>
            </span>
            <div className="text-sm font-semibold">{t.title}</div>
            <div className="mt-0.5 text-xs text-neutral-600 dark:text-neutral-400">
              {t.body}
            </div>
          </button>
        );
      })}
    </div>
  );
}
