"use client";

import Link from "next/link";
import { useCallback, useEffect, useState, type FormEvent } from "react";
import { api, describeError } from "@/lib/api";
import { formatLocalTime, formatSats } from "@/lib/clock";
import {
  PinInput,
  type Balance,
  type NodeInfo,
  type PinStatus,
  type RampInfo,
  type RampOrder,
  type Withdrawal,
} from "@/lib/schemas";
import { useSession } from "@/lib/session";
import { SignIn } from "./Account";
import { demoInvoice, hasDemoWallet } from "./DemoWallet";
import { WalletIcon } from "./landing/icons";
import {
  BankAccountFields,
  DemoBankNotice,
  money,
  QuoteLines,
  RampChip,
  rampFinal,
  useRampQuote,
  type BankAccount,
} from "./naira";
import { PageHeader } from "./PageParts";
import {
  Button,
  Card,
  ConfirmButton,
  Field,
  inputClass,
  Notice,
  Spinner,
} from "./ui";

type Payable = Extract<Balance, { payouts: true }>;
type Ramp = Extract<RampInfo, { enabled: true }>;

/** The routing-fee allowance held back while a withdrawal of `sats` is paid (mirrors the server). */
function feeAllowance(sats: number, b: Payable): number {
  return Math.max(
    b.fee_allowance.min_sats,
    Math.floor((sats * b.fee_allowance.percent) / 100),
  );
}

/** The most that can be withdrawn in one go: room for the fee allowance, and within the daily limit. */
function maxWithdrawal(b: Payable): number {
  let a = Math.floor(
    (b.available_sats * 100) / (100 + b.fee_allowance.percent),
  );
  while (a > 0 && a + feeAllowance(a, b) > b.available_sats) a--;
  while (a + 1 + feeAllowance(a + 1, b) <= b.available_sats) a++;
  return Math.max(0, Math.min(a, b.limits?.left_in_window_sats ?? a));
}

/** The withdrawal PIN, asked for on every withdrawal. Typed into a password field, never stored. */
function PinField({
  value,
  onChange,
  label = "Withdrawal PIN",
}: {
  value: string;
  onChange: (v: string) => void;
  label?: string;
}) {
  return (
    <Field label={label}>
      <input
        type="password"
        value={value}
        onChange={(e) =>
          onChange(e.target.value.replace(/\D/g, "").slice(0, 6))
        }
        inputMode="numeric"
        autoComplete="off"
        placeholder="••••••"
        className={`${inputClass} w-40 font-mono tracking-[0.4em]`}
      />
    </Field>
  );
}

/**
 * What the service owes a seller whose escrows settled on its shared node, and the ways to take it
 * out: to any Lightning wallet, or to a Nigerian bank account in naira.
 */
export function BalancePage() {
  const session = useSession();
  const [balance, setBalance] = useState<Balance | null>(null);
  const [withdrawals, setWithdrawals] = useState<Withdrawal[]>([]);
  const [orders, setOrders] = useState<RampOrder[]>([]);
  const [ramp, setRamp] = useState<RampInfo | null>(null);
  const [node, setNode] = useState<NodeInfo | null>(null);
  const [pin, setPin] = useState<PinStatus | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const b = await api.balance();
      setBalance(b);
      if (b.payouts) setWithdrawals((await api.withdrawals()).withdrawals);
      setError(null);
    } catch (err) {
      setError(describeError(err));
    }
    // Bank payouts are optional: without them the page still shows the balance.
    api
      .rampInfo()
      .then(setRamp)
      .catch(() => setRamp({ enabled: false }));
    api
      .rampOrders()
      .then((o) => setOrders(o.orders.filter((x) => x.direction === "offramp")))
      .catch(() => {});
  }, []);

  const signedIn = session?.kind === "user";
  useEffect(() => {
    if (!signedIn) return;
    // eslint-disable-next-line react-hooks/set-state-in-effect -- loads from the API on mount
    void load();
    api
      .node()
      .then(setNode)
      .catch(() => {});
    api
      .withdrawalPin()
      .then(setPin)
      .catch(() => {});
  }, [signedIn, load]);

  // Follow withdrawals until they finish: each one ends only when the node says so.
  const moving =
    withdrawals.some((w) => w.state === "pending") ||
    orders.some((o) => !rampFinal(o));
  useEffect(() => {
    if (!moving) return;
    const t = setInterval(load, 3000);
    return () => clearInterval(t);
  }, [moving, load]);

  if (session === undefined) return <Spinner />;
  if (!session) return <SignIn note="Sign in to see your balance." />;
  if (session.kind === "operator")
    return <Notice tone="info">The operator account has no balance.</Notice>;

  return (
    <div className="space-y-6">
      <PageHeader icon={<WalletIcon className="size-5" />} title="Balance">
        Sats from escrows released on the shared node are kept here for you
        until you withdraw them.
      </PageHeader>
      {error && <Notice tone="error">{error}</Notice>}
      {!balance && !error && <Spinner />}
      {balance && !balance.payouts && (
        <Notice tone="info">
          This server doesn&apos;t hold balances, so released payments go
          straight to the seller&apos;s own node.
        </Notice>
      )}
      {balance?.payouts && (
        <>
          {node?.mode === "own" && (
            <Notice tone="info">
              Your own node is connected, so new escrows pay you there directly
              and don&apos;t add to this balance. Anything below is from escrows
              settled before you connected it.{" "}
              <Link href="/node" className="font-semibold underline">
                My node
              </Link>
            </Notice>
          )}
          <Totals balance={balance} />
          {pin && <PinCard pin={pin} onChange={setPin} />}
          {pin?.set && (
            <WithdrawCard balance={balance} ramp={ramp} onDone={load} />
          )}
          <History withdrawals={withdrawals} orders={orders} />
        </>
      )}
    </div>
  );
}

function Totals({ balance }: { balance: Payable }) {
  const rows = [
    { label: "Earned", value: balance.earned_sats },
    ...(balance.maturing_sats > 0
      ? [{ label: "Maturing", value: balance.maturing_sats }]
      : []),
    { label: "On the way", value: balance.held_sats },
    { label: "Withdrawn", value: balance.withdrawn_sats },
  ];
  const limits = balance.limits;
  return (
    <section className="rounded-2xl bg-neutral-950 p-6 text-white shadow-sm dark:bg-neutral-900">
      <div className="text-xs font-semibold uppercase tracking-[0.12em] text-amber-400">
        Available to withdraw
      </div>
      <div className="mt-2 text-4xl font-bold tabular-nums">
        {formatSats(balance.available_sats)}
      </div>
      <dl
        className={`mt-5 grid gap-4 border-t border-white/10 pt-4 ${rows.length > 3 ? "grid-cols-2 sm:grid-cols-4" : "grid-cols-3"}`}
      >
        {rows.map((r) => (
          <div key={r.label}>
            <dt className="text-xs text-neutral-400">{r.label}</dt>
            <dd className="mt-0.5 text-sm font-semibold tabular-nums">
              {formatSats(r.value)}
            </dd>
          </div>
        ))}
      </dl>
      {limits && (
        <div className="mt-4 space-y-1 text-xs text-neutral-400">
          {balance.maturing_sats > 0 && (
            <p>
              Maturing: paid in naira, so it can be withdrawn{" "}
              {limits.naira_hold_hours} hours after release, once the bank
              transfer can no longer be reversed.
            </p>
          )}
          <p>
            You can withdraw up to {formatSats(limits.window_cap_sats)} in any{" "}
            {limits.window_hours} hours;{" "}
            {formatSats(limits.left_in_window_sats)} left now.
          </p>
        </div>
      )}
    </section>
  );
}

/** Set the withdrawal PIN (needed before any withdrawal), or change it with the current one. */
function PinCard({
  pin,
  onChange,
}: {
  pin: PinStatus;
  onChange: (p: PinStatus) => void;
}) {
  const [open, setOpen] = useState(!pin.set);
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [again, setAgain] = useState("");
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{
    tone: "success" | "error";
    text: string;
  } | null>(null);

  async function save(e: FormEvent) {
    e.preventDefault();
    const parsed = PinInput.safeParse(next);
    if (!parsed.success)
      return setMsg({ tone: "error", text: parsed.error.issues[0].message });
    if (next !== again)
      return setMsg({ tone: "error", text: "The two PINs don't match." });
    setBusy(true);
    setMsg(null);
    try {
      onChange(await api.setWithdrawalPin(next, pin.set ? current : undefined));
      setCurrent("");
      setNext("");
      setAgain("");
      setOpen(false);
      setMsg({
        tone: "success",
        text: pin.set ? "PIN changed." : "PIN set. You can withdraw now.",
      });
    } catch (err) {
      setMsg({ tone: "error", text: describeError(err) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card title={pin.set ? undefined : "Set a withdrawal PIN"}>
      {pin.set && !open ? (
        <div className="flex flex-wrap items-center justify-between gap-3 text-sm">
          <span className="flex items-center gap-2">
            <span className="size-2 rounded-full bg-emerald-500" />
            Withdrawals are protected by your PIN.
          </span>
          <Button variant="ghost" onClick={() => setOpen(true)}>
            Change PIN
          </Button>
        </div>
      ) : (
        <form onSubmit={save} className="space-y-4" noValidate>
          {!pin.set && (
            <p className="text-sm text-neutral-600 dark:text-neutral-400">
              Every withdrawal asks for this 6-digit PIN, so someone who gets
              hold of your sign-in still can&apos;t take your money. Choose one
              you don&apos;t use elsewhere, and avoid runs like 123456. It
              can&apos;t be recovered if you forget it.
            </p>
          )}
          <div className="flex flex-wrap gap-4">
            {pin.set && (
              <PinField
                label="Current PIN"
                value={current}
                onChange={setCurrent}
              />
            )}
            <PinField
              label={pin.set ? "New PIN" : "PIN"}
              value={next}
              onChange={setNext}
            />
            <PinField label="Repeat it" value={again} onChange={setAgain} />
          </div>
          {msg && <Notice tone={msg.tone}>{msg.text}</Notice>}
          <div className="flex gap-2">
            <Button type="submit" busy={busy}>
              {pin.set ? "Change PIN" : "Set PIN"}
            </Button>
            {pin.set && (
              <Button variant="ghost" onClick={() => setOpen(false)}>
                Cancel
              </Button>
            )}
          </div>
        </form>
      )}
      {pin.set && !open && msg?.tone === "success" && (
        <div className="mt-3">
          <Notice tone="success">{msg.text}</Notice>
        </div>
      )}
    </Card>
  );
}

/** One place to take money out: pick Lightning or bank, and only that form is shown. */
function WithdrawCard({
  balance,
  ramp,
  onDone,
}: {
  balance: Payable;
  ramp: RampInfo | null;
  onDone: () => Promise<void>;
}) {
  const bank = ramp?.enabled && ramp.offramp ? ramp : null;
  const [method, setMethod] = useState<"lightning" | "bank">("lightning");
  const active = bank ? method : "lightning";
  const tabs = [
    { id: "lightning" as const, label: "Lightning wallet" },
    ...(bank ? [{ id: "bank" as const, label: "Bank account (₦)" }] : []),
  ];
  return (
    <Card title="Withdraw">
      <div className="space-y-5">
        {tabs.length > 1 && (
          <div
            role="tablist"
            aria-label="Withdraw to"
            className="grid grid-cols-2 gap-1 rounded-xl bg-neutral-100 p-1 dark:bg-neutral-800"
          >
            {tabs.map((t) => (
              <button
                key={t.id}
                type="button"
                role="tab"
                aria-selected={active === t.id}
                onClick={() => setMethod(t.id)}
                className={`rounded-lg px-3 py-2 text-sm font-semibold transition-colors ${
                  active === t.id
                    ? "bg-white shadow-sm dark:bg-neutral-950"
                    : "text-neutral-500 hover:text-neutral-800 dark:hover:text-neutral-200"
                }`}
              >
                {t.label}
              </button>
            ))}
          </div>
        )}
        {active === "bank" && bank ? (
          <BankWithdraw balance={balance} ramp={bank} onDone={onDone} />
        ) : (
          <LightningWithdraw balance={balance} onDone={onDone} />
        )}
        {ramp && !bank && (
          <p className="text-xs text-neutral-500">
            Bank payouts in naira aren&apos;t offered on this server.
          </p>
        )}
      </div>
    </Card>
  );
}

function LightningWithdraw({
  balance,
  onDone,
}: {
  balance: Payable;
  onDone: () => Promise<void>;
}) {
  const [invoice, setInvoice] = useState("");
  const [pin, setPin] = useState("");
  const [demoAmount, setDemoAmount] = useState("");
  const [busy, setBusy] = useState<"pay" | "demo" | null>(null);
  const [msg, setMsg] = useState<{
    tone: "success" | "error";
    text: string;
  } | null>(null);
  const max = maxWithdrawal(balance);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (!invoice.trim())
      return setMsg({
        tone: "error",
        text: "Paste a Lightning invoice from your wallet.",
      });
    if (pin.length !== 6)
      return setMsg({
        tone: "error",
        text: "Enter your 6-digit withdrawal PIN.",
      });
    setBusy("pay");
    setMsg(null);
    try {
      const w = await api.withdraw(invoice, pin);
      setInvoice("");
      setPin("");
      setMsg({
        tone: "success",
        text:
          w.state === "succeeded"
            ? `${formatSats(w.amount_sats)} sent to your wallet.`
            : `Sending ${formatSats(w.amount_sats)}. It shows below as it goes through.`,
      });
      await onDone();
    } catch (err) {
      setMsg({ tone: "error", text: describeError(err) });
    } finally {
      setBusy(null);
    }
  }

  async function makeDemoInvoice() {
    const amount = Number(demoAmount || max);
    if (!Number.isInteger(amount) || amount < 1)
      return setMsg({ tone: "error", text: "Enter a whole number of sats." });
    setBusy("demo");
    setMsg(null);
    try {
      setInvoice(await demoInvoice(amount));
    } catch (err) {
      setMsg({
        tone: "error",
        text:
          err instanceof Error
            ? err.message
            : "The demo wallet couldn't make an invoice.",
      });
    } finally {
      setBusy(null);
    }
  }

  return (
    <form onSubmit={submit} className="space-y-4" noValidate>
      <Field
        label="Lightning invoice"
        hint={`Make an invoice in your wallet (Receive) and paste it here. Up to ${formatSats(max)} now.`}
      >
        <textarea
          value={invoice}
          onChange={(e) => setInvoice(e.target.value)}
          rows={3}
          spellCheck={false}
          autoComplete="off"
          placeholder="lnbc…"
          className={`${inputClass} font-mono text-xs`}
        />
      </Field>
      {hasDemoWallet && (
        <details className="rounded-lg border border-dashed border-neutral-300 p-3 dark:border-neutral-700">
          <summary className="cursor-pointer text-sm font-medium text-neutral-600 dark:text-neutral-400">
            Use the demo wallet
          </summary>
          <div className="mt-3 flex flex-wrap items-end gap-2">
            <Field label="Demo wallet: make an invoice for">
              <input
                value={demoAmount}
                onChange={(e) =>
                  setDemoAmount(e.target.value.replace(/\D/g, ""))
                }
                inputMode="numeric"
                placeholder={String(max)}
                className={`${inputClass} w-36`}
              />
            </Field>
            <Button
              variant="secondary"
              onClick={makeDemoInvoice}
              busy={busy === "demo"}
              disabled={max < 1}
            >
              Make invoice
            </Button>
          </div>
        </details>
      )}
      <PinField value={pin} onChange={setPin} />
      {msg && <Notice tone={msg.tone}>{msg.text}</Notice>}
      <Button
        type="submit"
        busy={busy === "pay"}
        disabled={balance.available_sats <= 0}
      >
        Withdraw
      </Button>
      <p className="text-xs text-neutral-500">
        While it is paid, {balance.fee_allowance.percent}% (at least{" "}
        {balance.fee_allowance.min_sats} sats) is held for routing fees. Only
        what the payment actually cost is kept.
      </p>
    </form>
  );
}

function BankWithdraw({
  balance,
  ramp,
  onDone,
}: {
  balance: Payable;
  ramp: Ramp;
  onDone: () => Promise<void>;
}) {
  const [account, setAccount] = useState<BankAccount>({
    bankCode: "",
    accountNumber: "",
    resolved: null,
  });
  const [amount, setAmount] = useState("");
  const [pin, setPin] = useState("");
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{
    tone: "success" | "error";
    text: string;
  } | null>(null);
  const max = maxWithdrawal(balance);
  const sats = Number(amount) || null;
  const { quote, error: quoteError } = useRampQuote("offramp", sats);
  const tooSmall = sats !== null && sats < ramp.min_sats;
  const tooBig = sats !== null && sats > max;

  async function send() {
    if (!sats || !account.resolved) return;
    setBusy(true);
    setMsg(null);
    try {
      const o = await api.startOfframp(
        sats,
        account.resolved.bank_code,
        account.resolved.account_number,
        pin,
      );
      setAmount("");
      setPin("");
      setMsg({
        tone: "success",
        text: `${money(o.fiat_minor, o)} is on its way to ${o.bank.account_name}. Follow it below.`,
      });
      await onDone();
    } catch (err) {
      setMsg({ tone: "error", text: describeError(err) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-4">
      {ramp.demo && <DemoBankNotice />}
      <BankAccountFields
        banks={ramp.banks}
        value={account}
        onChange={setAccount}
      />
      <Field
        label="Amount in sats"
        hint={`From ${formatSats(ramp.min_sats)} up to ${formatSats(max)}.`}
        error={
          tooSmall
            ? `The smallest bank payout is ${formatSats(ramp.min_sats)}.`
            : tooBig
              ? "That is more than you can withdraw now."
              : undefined
        }
      >
        <div className="flex gap-2">
          <input
            value={amount}
            onChange={(e) => setAmount(e.target.value.replace(/\D/g, ""))}
            inputMode="numeric"
            placeholder={String(max)}
            className={inputClass}
          />
          <Button
            variant="secondary"
            onClick={() => setAmount(String(max))}
            disabled={max < 1}
          >
            Max
          </Button>
        </div>
      </Field>
      {quote && !tooSmall && !tooBig && (
        <QuoteLines quote={quote} total="You receive" />
      )}
      {quoteError && !tooSmall && <Notice tone="error">{quoteError}</Notice>}
      <PinField value={pin} onChange={setPin} />
      {msg && <Notice tone={msg.tone}>{msg.text}</Notice>}
      <ConfirmButton
        variant="primary"
        busy={busy}
        disabled={
          !account.resolved || !quote || tooSmall || tooBig || pin.length !== 6
        }
        confirmLabel="Yes, send it"
        confirmText={
          quote && account.resolved ? (
            <>
              Send <strong>{money(quote.fiat_minor, quote)}</strong> (
              {formatSats(quote.amount_sats)}) to{" "}
              <strong>{account.resolved.account_name}</strong> at{" "}
              {account.resolved.bank_name}? A bank transfer can&apos;t be taken
              back.
            </>
          ) : (
            ""
          )
        }
        onConfirm={send}
      >
        Withdraw to bank
      </ConfirmButton>
    </div>
  );
}

const withdrawalChip: Record<
  Withdrawal["state"],
  { label: string; cls: string }
> = {
  pending: {
    label: "Sending",
    cls: "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-200",
  },
  succeeded: {
    label: "Sent",
    cls: "bg-emerald-100 text-emerald-900 dark:bg-emerald-950 dark:text-emerald-200",
  },
  failed: {
    label: "Failed, returned",
    cls: "bg-red-100 text-red-900 dark:bg-red-950 dark:text-red-200",
  },
};

/** Lightning withdrawals and bank payouts together, newest first. */
function History({
  withdrawals,
  orders,
}: {
  withdrawals: Withdrawal[];
  orders: RampOrder[];
}) {
  const items = [
    ...withdrawals
      .filter((w) => w.kind === "lightning")
      .map((w) => ({ at: w.created_at, key: w.id, w })),
    ...orders.map((o) => ({ at: o.created_at, key: o.id, o })),
  ].sort((a, b) => b.at - a.at);

  return (
    <Card title="History">
      {items.length === 0 ? (
        <p className="text-sm text-neutral-500">No withdrawals yet.</p>
      ) : (
        <ul className="divide-y divide-neutral-200 dark:divide-neutral-800">
          {items.map((i) => (
            <li
              key={i.key}
              className="flex flex-wrap items-start justify-between gap-3 py-3 text-sm"
            >
              {"w" in i && i.w ? (
                <>
                  <div className="min-w-0">
                    <div className="font-medium">To a Lightning wallet</div>
                    <div className="text-xs text-neutral-500">
                      {formatLocalTime(i.w.created_at)}
                      {i.w.fee_sats !== null &&
                        i.w.state === "succeeded" &&
                        ` · routing fee ${formatSats(i.w.fee_sats)}`}
                    </div>
                    {i.w.failure && (
                      <div className="text-xs text-red-700 first-letter:uppercase dark:text-red-400">
                        {i.w.failure}. The sats are back in your balance; make a
                        new invoice to try again.
                      </div>
                    )}
                  </div>
                  <div className="flex items-center gap-3">
                    <span className="font-semibold tabular-nums">
                      {formatSats(i.w.amount_sats)}
                    </span>
                    <span
                      className={`rounded-full px-2 py-0.5 text-xs font-semibold ${withdrawalChip[i.w.state].cls}`}
                    >
                      {withdrawalChip[i.w.state].label}
                    </span>
                  </div>
                </>
              ) : (
                "o" in i &&
                i.o && (
                  <>
                    <div className="min-w-0">
                      <div className="font-medium">
                        To {i.o.bank.account_name} · {i.o.bank.bank_name}{" "}
                        {i.o.bank.account_number}
                      </div>
                      <div className="text-xs text-neutral-500">
                        {formatLocalTime(i.o.created_at)} ·{" "}
                        {formatSats(i.o.amount_sats)}
                        {i.o.fiat_reference && ` · ref ${i.o.fiat_reference}`}
                      </div>
                      {i.o.state === "failed" && i.o.note && (
                        <div className="text-xs text-red-700 first-letter:uppercase dark:text-red-400">
                          {i.o.note}
                        </div>
                      )}
                    </div>
                    <div className="flex items-center gap-3">
                      <span className="font-semibold tabular-nums">
                        {money(i.o.fiat_minor, i.o)}
                      </span>
                      <RampChip state={i.o.state} />
                    </div>
                  </>
                )
              )}
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}
