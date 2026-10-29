"use client";

import { useRouter } from "next/navigation";
import Link from "next/link";
import { useEffect, useRef, useState, type FormEvent } from "react";
import { api, ApiError, describeError } from "@/lib/api";
import { formatFiat, formatRate, satsForValue, valueOfSats } from "@/lib/fiat";
import {
  MAX_AMOUNT_SATS,
  MAX_DESCRIPTION,
  NewEscrowForm,
  type NodeInfo,
  type RateQuote,
  type DefaultArbiter,
  type RatesInfo,
} from "@/lib/schemas";
import { useSession } from "@/lib/session";
import { SignIn } from "./Account";
import {
  BuyerPanel,
  EvidenceGuide,
  escrowTypes,
  SidePicker,
  TypePicker,
  type EscrowType,
  type Side,
} from "./escrow/SidePicker";
import { Button, Card, Field, inputClass, Notice, Spinner } from "./ui";

type Values = {
  title: string;
  buyer_id: string;
  arbiter_id: string;
  /** Who decides a dispute: the platform's arbiter, one the seller names, or nobody. */
  arbiter_mode: "platform" | "own" | "none";
  fiat_currency: string;
  amount_sats: string;
  /** The price in naira, when the seller prices the deal in naira. */
  amount_naira: string;
  description: string;
  hold_amount: string;
  hold_unit: "minutes" | "hours";
};

const initial: Values = {
  title: escrowTypes[0].dealTitle,
  buyer_id: "",
  arbiter_id: "",
  arbiter_mode: "platform",
  fiat_currency: "",
  amount_sats: "",
  amount_naira: "",
  description: escrowTypes[0].template,
  hold_amount: "1",
  hold_unit: "hours",
};

// Server error codes that belong to a particular field.
const fieldOfCode: Record<string, keyof Values> = {
  unknown_buyer: "buyer_id",
  unknown_arbiter: "arbiter_id",
  unsupported_currency: "fiat_currency",
  invalid_arbiter: "arbiter_id",
  arbiter_has_no_key: "arbiter_id",
  invalid_amount: "amount_sats",
  invalid_description: "description",
  invalid_deadline: "hold_amount",
};

/** The title leads the description, so it is the first thing on the dashboard and the escrow page. */
function composeDescription(v: Pick<Values, "title" | "description">): string {
  const title = v.title.trim();
  const body = v.description.trim();
  return title && body ? `${title}\n${body}` : title || body;
}

function StepBar({ step }: { step: 1 | 2 }) {
  const steps = ["Concept", "Terms"];
  return (
    <ol
      aria-label="Progress"
      className="flex flex-wrap items-center gap-3 text-sm"
    >
      {steps.map((label, i) => {
        const n = i + 1;
        const on = n === step;
        return (
          <li
            key={label}
            aria-current={on ? "step" : undefined}
            className="flex items-center gap-2"
          >
            <span
              className={`grid size-6 place-items-center rounded-full text-xs font-bold ${
                on
                  ? "bg-neutral-950 text-white dark:bg-amber-500 dark:text-black"
                  : n < step
                    ? "bg-emerald-500 text-white"
                    : "bg-neutral-200 text-neutral-600 dark:bg-neutral-800 dark:text-neutral-400"
              }`}
            >
              {n}
            </span>
            <span className={on ? "font-semibold" : "text-neutral-500"}>
              {label}
            </span>
            {n < steps.length && (
              <span className="mx-1 h-px w-8 bg-neutral-300 dark:bg-neutral-700" />
            )}
          </li>
        );
      })}
    </ol>
  );
}

export function NewEscrow() {
  const session = useSession();
  const router = useRouter();
  const [values, setValues] = useState<Values>(initial);
  const [errors, setErrors] = useState<Partial<Record<keyof Values, string>>>(
    {},
  );
  const [formError, setFormError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [side, setSide] = useState<Side>("seller");
  const [kind, setKind] = useState<EscrowType>("p2p");
  const [step, setStep] = useState<1 | 2>(1);
  // One key per submission: retrying the same values reuses it, so a lost response can't create a
  // second escrow. Editing the form starts a new submission.
  const idempotencyKey = useRef<string | null>(null);
  const [node, setNode] = useState<NodeInfo | null>(null);
  const [rates, setRates] = useState<RatesInfo | null>(null);
  const [quote, setQuote] = useState<RateQuote | null>(null);
  const [quoteError, setQuoteError] = useState<string | null>(null);
  const [platform, setPlatform] = useState<DefaultArbiter | null>(null);
  const [priceIn, setPriceIn] = useState<"sats" | "naira">("sats");

  useEffect(() => {
    if (!session || session.kind !== "user") return;
    let alive = true;
    // If this fails the server still answers "no_node" when it matters, so just carry on.
    api
      .node()
      .then((n) => alive && setNode(n))
      .catch(() => {});
    // Whether fiat values are offered at all. If this fails the form simply has no currency choice.
    api
      .rates()
      .then((r) => alive && setRates(r))
      .catch(() => {});
    // Whether the platform has its own arbiter to offer. If this fails the form just asks for one by id.
    api
      .defaultArbiter()
      .then((d) => alive && setPlatform(d.id ? d : null))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [session]);

  // The price behind the preview, refreshed every minute while a currency is chosen. The server
  // takes its own rate when the escrow is created; this is only so people can see what to expect.
  const currency = values.fiat_currency;
  useEffect(() => {
    if (!currency || !session || session.kind !== "user") return;
    let alive = true;
    const load = () =>
      api
        .rate(currency)
        .then((q) => {
          if (!alive) return;
          setQuote(q);
          setQuoteError(null);
        })
        .catch((err) => alive && setQuoteError(describeError(err)));
    load();
    const t = setInterval(load, 60_000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [currency, session]);

  // Priced in naira: the escrow is still in sats, worked out from the naira at today's rate and
  // fixed when it is created. The naira value is recorded with it (as its fiat value).
  const nairaOffered =
    !!rates?.enabled && rates.currencies.some((c) => c.code === "NGN");
  const naira = Number(values.amount_naira.replace(/,/g, ""));
  const nairaSats =
    priceIn === "naira" &&
    quote?.currency === "NGN" &&
    Number.isFinite(naira) &&
    naira > 0
      ? satsForValue(
          Math.round(naira * 10 ** quote.exponent),
          quote.rate_minor_per_btc,
        )
      : null;

  if (session === undefined) return <Spinner />;
  if (!session)
    return <SignIn note="Sign in as the seller to create an escrow." />;
  if (session.kind === "operator") {
    return (
      <Notice tone="info">
        Operators can&apos;t be a party to an escrow. Sign in with a user token
        instead.
      </Notice>
    );
  }

  function set<K extends keyof Values>(key: K, value: Values[K]) {
    setValues((v) => ({ ...v, [key]: value }));
    setErrors((e) => ({ ...e, [key]: undefined }));
    idempotencyKey.current = null;
  }

  // Choosing a kind of deal writes its title and description for you. Anything you have already
  // typed yourself is kept.
  function pickKind(next: EscrowType) {
    const from = escrowTypes.find((t) => t.id === kind)!;
    const to = escrowTypes.find((t) => t.id === next)!;
    setValues((v) => ({
      ...v,
      title:
        v.title.trim() === "" || v.title === from.dealTitle
          ? to.dealTitle
          : v.title,
      description:
        v.description.trim() === "" || v.description === from.template
          ? to.template
          : v.description,
    }));
    setKind(next);
    idempotencyKey.current = null;
  }

  function next(e: FormEvent) {
    e.preventDefault();
    const found: Partial<Record<keyof Values, string>> = {};
    if (!values.title.trim()) found.title = "Give the deal a title";
    if (!values.description.trim())
      found.description = "Describe what is being sold";
    else if (composeDescription(values).length > MAX_DESCRIPTION)
      found.description = `Title and description together can be at most ${MAX_DESCRIPTION} characters`;
    setErrors(found);
    if (Object.keys(found).length === 0) setStep(2);
  }

  function pickPrice(next: "sats" | "naira") {
    setPriceIn(next);
    setQuote(null);
    setQuoteError(null);
    set("fiat_currency", next === "naira" ? "NGN" : "");
  }

  async function submit(e: FormEvent) {
    e.preventDefault();
    setFormError(null);
    if (priceIn === "naira") {
      const max = quote
        ? formatFiat(
            valueOfSats(MAX_AMOUNT_SATS, quote.rate_minor_per_btc),
            "NGN",
            quote.exponent,
          )
        : null;
      const problem = !(naira > 0)
        ? "Enter the price in naira"
        : !quote
          ? (quoteError ??
            "Still fetching the naira rate; try again in a moment")
          : nairaSats === null || nairaSats < 1
            ? "That is less than 1 sat"
            : nairaSats > MAX_AMOUNT_SATS
              ? `At most ${max} (${MAX_AMOUNT_SATS.toLocaleString("en-US")} sats)`
              : null;
      if (problem) return setErrors({ amount_naira: problem });
    }
    const parsed = NewEscrowForm.safeParse({
      ...values,
      ...(priceIn === "naira"
        ? { amount_sats: String(nairaSats), fiat_currency: "NGN" }
        : {}),
      description: composeDescription(values),
      // With a platform arbiter on offer the choice decides what is sent; without one, an id or nothing.
      ...(platform
        ? {
            arbiter_id: values.arbiter_mode === "own" ? values.arbiter_id : "",
            no_arbiter: values.arbiter_mode === "none",
          }
        : {}),
    });
    if (!parsed.success) {
      const next: Partial<Record<keyof Values, string>> = {};
      for (const issue of parsed.error.issues) {
        const key = issue.path[0] as keyof Values;
        next[key] ??= issue.message;
      }
      setErrors(next);
      if (next.description) setStep(1);
      return;
    }
    if (parsed.data.arbiter_id && parsed.data.arbiter_id === session?.userId) {
      setErrors({ arbiter_id: "You can't be your own arbiter." });
      return;
    }
    if (parsed.data.buyer_id && parsed.data.buyer_id === session?.userId) {
      setErrors({ buyer_id: "That's your own id. Enter the buyer's id." });
      return;
    }
    idempotencyKey.current ??= crypto.randomUUID();
    setBusy(true);
    try {
      const escrow = await api.create(parsed.data, idempotencyKey.current);
      router.push(`/escrows/${escrow.id}`);
    } catch (err) {
      const field = err instanceof ApiError ? fieldOfCode[err.code] : undefined;
      if (field) {
        setErrors({ [field]: describeError(err) });
        if (field === "description") setStep(1);
      } else setFormError(describeError(err));
      if (err instanceof ApiError && err.code === "idempotency_key_reuse")
        idempotencyKey.current = null;
      setBusy(false);
    }
  }

  return (
    <div className="space-y-6">
      <h1 className="text-3xl font-bold tracking-tight">New escrow</h1>
      <SidePicker side={side} onChange={setSide} />
      {side === "buyer" ? (
        <BuyerPanel userId={session.userId} />
      ) : (
        <>
          <p className="text-sm text-neutral-600 dark:text-neutral-400">
            You are the seller. The buyer accepts the offer, pays, and the
            payment stays locked until they hand you the release secret, or
            until the hold time runs out and they are refunded.
          </p>
          {node?.mode === "none" && (
            <Notice tone="warn">
              You need to{" "}
              <Link href="/node" className="font-semibold underline">
                connect your Lightning node
              </Link>{" "}
              before you can create an escrow: this service doesn&apos;t hold
              sellers&apos; money.
            </Notice>
          )}
          {node?.mode === "shared" && (
            <Notice tone="info">
              No node of yours is connected, so this escrow will use the
              service&apos;s shared node: the service receives the sats on
              release and owes them to you.{" "}
              <Link href="/node" className="font-semibold underline">
                Connect your own node
              </Link>{" "}
              to be paid directly.
            </Notice>
          )}
          <StepBar step={step} />
          {step === 1 && (
            <>
              <section className="space-y-3">
                <h2 className="text-xl font-bold tracking-tight">
                  What is this escrow for?
                </h2>
                <TypePicker value={kind} onChange={pickKind} />
                <EvidenceGuide type={kind} />
              </section>
              <Card title="Deal details">
                <form onSubmit={next} className="space-y-4" noValidate>
                  <Field
                    label="Deal title"
                    hint="A short name both of you will recognise."
                    error={errors.title}
                  >
                    <input
                      value={values.title}
                      onChange={(e) => set("title", e.target.value)}
                      className={inputClass}
                      maxLength={80}
                    />
                  </Field>
                  <Field
                    label="Description"
                    hint={`${composeDescription(values).length}/${MAX_DESCRIPTION} characters, title included. Both parties see it.`}
                    error={errors.description}
                  >
                    <textarea
                      value={values.description}
                      onChange={(e) => set("description", e.target.value)}
                      className={inputClass}
                      rows={7}
                      placeholder={
                        escrowTypes.find((t) => t.id === kind)?.placeholder
                      }
                    />
                  </Field>
                  <div className="rounded-xl bg-neutral-950 p-4 text-sm text-neutral-300">
                    <div className="font-mono text-[11px] font-semibold uppercase tracking-[0.14em] text-neutral-500">
                      A description that holds up
                    </div>
                    <ul className="mt-3 space-y-1.5">
                      <li>
                        <strong className="text-white">
                          Exactly what is being exchanged
                        </strong>{" "}
                        — model, quantity, spec.
                      </li>
                      <li>
                        <strong className="text-white">
                          Its condition or quality bar
                        </strong>{" "}
                        — faults, revisions, acceptance criteria.
                      </li>
                      <li>
                        <strong className="text-white">
                          When it must arrive
                        </strong>{" "}
                        — a date, not &ldquo;soon&rdquo;.
                      </li>
                      <li>
                        <strong className="text-white">
                          What counts as done
                        </strong>{" "}
                        — the moment the buyer should release the funds.
                      </li>
                    </ul>
                  </div>
                  <div className="flex justify-end">
                    <Button type="submit">Continue</Button>
                  </div>
                </form>
              </Card>
            </>
          )}
          {step === 2 && (
            <Card title="Amount & terms">
              <p className="mb-4 rounded-lg bg-neutral-100 px-3 py-2 text-sm dark:bg-neutral-800">
                <span className="text-neutral-500">Deal: </span>
                <strong>{values.title.trim()}</strong>{" "}
                <button
                  type="button"
                  onClick={() => setStep(1)}
                  className="ml-1 font-semibold text-amber-700 underline dark:text-amber-400"
                >
                  Edit
                </button>
              </p>
              <form onSubmit={submit} className="space-y-4" noValidate>
                {nairaOffered && (
                  <div>
                    <div className="mb-1 text-sm font-medium">Price in</div>
                    <div
                      role="radiogroup"
                      aria-label="Price in"
                      className="grid grid-cols-2 gap-2"
                    >
                      {(
                        [
                          ["sats", "Sats", "Bitcoin over Lightning"],
                          [
                            "naira",
                            "Naira (₦)",
                            "Converted to sats at today's rate",
                          ],
                        ] as const
                      ).map(([key, title, sub]) => (
                        <button
                          key={key}
                          type="button"
                          role="radio"
                          aria-checked={priceIn === key}
                          onClick={() => pickPrice(key)}
                          className={`rounded-xl border-2 px-4 py-2.5 text-left transition-colors ${
                            priceIn === key
                              ? "border-amber-500 bg-amber-50 dark:bg-amber-950/30"
                              : "border-neutral-200 hover:border-neutral-300 dark:border-neutral-800 dark:hover:border-neutral-700"
                          }`}
                        >
                          <div className="text-sm font-semibold">{title}</div>
                          <div className="text-xs text-neutral-500">{sub}</div>
                        </button>
                      ))}
                    </div>
                  </div>
                )}
                {priceIn === "naira" ? (
                  <Field
                    label="Amount (₦)"
                    hint="The buyer can pay in naira by bank transfer or in sats; either way the escrow holds sats."
                    error={errors.amount_naira ?? errors.amount_sats}
                  >
                    <input
                      value={values.amount_naira}
                      onChange={(e) =>
                        set(
                          "amount_naira",
                          e.target.value.replace(/[^\d.,]/g, ""),
                        )
                      }
                      className={inputClass}
                      inputMode="decimal"
                      placeholder="50,000"
                    />
                    {quoteError ? (
                      <p className="mt-2 text-xs text-red-700 dark:text-red-400">
                        {quoteError}
                      </p>
                    ) : !quote ? (
                      <p className="mt-2 text-xs text-neutral-500">
                        Fetching the naira rate…
                      </p>
                    ) : (
                      <p className="mt-2 text-sm">
                        {nairaSats !== null && (
                          <>
                            <strong>
                              = {nairaSats.toLocaleString("en-US")} sats
                            </strong>{" "}
                            <span className="text-neutral-500">at</span>{" "}
                          </>
                        )}
                        <span className="text-neutral-500">
                          {formatRate(
                            quote.rate_minor_per_btc,
                            quote.currency,
                            quote.exponent,
                          )}{" "}
                          per bitcoin ({quote.source}). Fixed when you create
                          the escrow.
                        </span>
                      </p>
                    )}
                  </Field>
                ) : (
                  <Field
                    label="Amount (sats)"
                    hint="Between 1 and 1,000,000 sats."
                    error={errors.amount_sats}
                  >
                    <input
                      value={values.amount_sats}
                      onChange={(e) => set("amount_sats", e.target.value)}
                      className={inputClass}
                      inputMode="numeric"
                      placeholder="1000"
                    />
                  </Field>
                )}
                {rates?.enabled && priceIn === "sats" && (
                  <Field
                    label="Also show the value in (optional)"
                    hint="Display only: the escrow is in sats. The value is worked out once, now, and never changes."
                    error={errors.fiat_currency}
                  >
                    <select
                      value={values.fiat_currency}
                      onChange={(e) => {
                        setQuote(null);
                        setQuoteError(null);
                        set("fiat_currency", e.target.value);
                      }}
                      className={inputClass}
                    >
                      <option value="">No fiat value</option>
                      {rates.currencies.map((c) => (
                        <option key={c.code} value={c.code}>
                          {c.code}
                        </option>
                      ))}
                    </select>
                    {currency && (
                      <FiatPreview
                        sats={values.amount_sats}
                        quote={quote}
                        error={quoteError}
                      />
                    )}
                  </Field>
                )}
                <Field
                  label="Hold time"
                  hint="How long the payment stays locked once paid. If the buyer hasn't released by then, they are refunded. 1 minute to 6 hours."
                  error={errors.hold_amount}
                >
                  <div className="flex gap-2">
                    <input
                      aria-label="Hold time amount"
                      value={values.hold_amount}
                      onChange={(e) => set("hold_amount", e.target.value)}
                      className={`${inputClass} max-w-32`}
                      inputMode="decimal"
                    />
                    <select
                      aria-label="Hold time unit"
                      value={values.hold_unit}
                      onChange={(e) =>
                        set("hold_unit", e.target.value as Values["hold_unit"])
                      }
                      className={`${inputClass} max-w-40`}
                    >
                      <option value="minutes">minutes</option>
                      <option value="hours">hours</option>
                    </select>
                  </div>
                </Field>
                <Field
                  label="Buyer's user id (optional)"
                  hint="Leave empty to get a link anyone can open: the first person to accept becomes the buyer. Fill it in to invite one specific person."
                  error={errors.buyer_id}
                >
                  <input
                    value={values.buyer_id}
                    onChange={(e) => set("buyer_id", e.target.value)}
                    className={`${inputClass} font-mono`}
                    placeholder="u_…"
                    autoComplete="off"
                    spellCheck={false}
                  />
                </Field>
                {platform && (
                  <fieldset className="space-y-2">
                    <legend className="text-sm font-medium">
                      Who decides if you and the buyer disagree?
                    </legend>
                    <div
                      role="radiogroup"
                      aria-label="Arbiter"
                      className="space-y-2"
                    >
                      {(
                        [
                          [
                            "platform",
                            "The platform's arbiter (recommended)",
                            "A neutral party is already in place, so a dispute can be decided in your favour if you delivered. Their key fingerprint is shown below.",
                          ],
                          [
                            "own",
                            "An arbiter I choose",
                            "Someone you and the buyer both trust, who has set up a key on the Arbiter page.",
                          ],
                          [
                            "none",
                            "No arbiter",
                            "A dispute is just flagged for the operator, and the buyer's refund is the default. You cannot be paid without the buyer's release.",
                          ],
                        ] as const
                      ).map(([id, title, body]) => (
                        <button
                          key={id}
                          type="button"
                          role="radio"
                          aria-checked={values.arbiter_mode === id}
                          onClick={() => set("arbiter_mode", id)}
                          className={`w-full rounded-xl border-2 p-3 text-left transition-colors ${
                            values.arbiter_mode === id
                              ? "border-amber-500 bg-amber-50 dark:bg-amber-500/10"
                              : "border-neutral-200 hover:border-neutral-400 dark:border-neutral-800 dark:hover:border-neutral-600"
                          }`}
                        >
                          <div className="text-sm font-semibold">{title}</div>
                          <div className="mt-0.5 text-xs text-neutral-600 dark:text-neutral-400">
                            {body}
                          </div>
                        </button>
                      ))}
                    </div>
                    {values.arbiter_mode === "platform" &&
                      platform.fingerprint && (
                        <p className="text-xs text-neutral-500">
                          Key fingerprint:{" "}
                          <span className="break-all font-mono">
                            {platform.fingerprint}
                          </span>
                        </p>
                      )}
                  </fieldset>
                )}
                {(!platform || values.arbiter_mode === "own") && (
                  <Field
                    label="Arbiter's user id (optional)"
                    hint="Someone you and the buyer both trust to decide a dispute. Without one, a dispute is just flagged for the operator and the buyer's refund is the default. They must have set up a key on the Arbiter page."
                    error={errors.arbiter_id}
                  >
                    <input
                      value={values.arbiter_id}
                      onChange={(e) => set("arbiter_id", e.target.value)}
                      className={`${inputClass} font-mono`}
                      placeholder="u_…"
                      autoComplete="off"
                      spellCheck={false}
                    />
                  </Field>
                )}
                {formError && <Notice tone="error">{formError}</Notice>}
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <Button
                    type="button"
                    variant="secondary"
                    onClick={() => setStep(1)}
                  >
                    Back
                  </Button>
                  <Button
                    type="submit"
                    busy={busy}
                    disabled={node?.mode === "none"}
                  >
                    Create escrow
                  </Button>
                </div>
              </form>
            </Card>
          )}
        </>
      )}
    </div>
  );
}

/** What the sum is worth now, so the seller knows what to expect. The real value is fixed on creation. */
function FiatPreview({
  sats,
  quote,
  error,
}: {
  sats: string;
  quote: RateQuote | null;
  error: string | null;
}) {
  if (error)
    return (
      <p className="mt-2 text-xs text-red-700 dark:text-red-400">{error}</p>
    );
  if (!quote)
    return <p className="mt-2 text-xs text-neutral-500">Fetching the price…</p>;
  const amount = Number(sats);
  const valid = Number.isInteger(amount) && amount > 0 && amount <= 1_000_000;
  return (
    <p className="mt-2 text-sm">
      {valid ? (
        <>
          <strong>
            ≈{" "}
            {formatFiat(
              valueOfSats(amount, quote.rate_minor_per_btc),
              quote.currency,
              quote.exponent,
            )}
          </strong>{" "}
          <span className="text-neutral-500">at</span>{" "}
        </>
      ) : null}
      <span className="text-neutral-500">
        {formatRate(quote.rate_minor_per_btc, quote.currency, quote.exponent)}{" "}
        per bitcoin ({quote.source})
      </span>
    </p>
  );
}
