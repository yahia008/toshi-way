"use client";

import { QRCodeSVG } from "qrcode.react";
import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { api, ApiError, describeError } from "@/lib/api";
import { formatSats } from "@/lib/clock";
import { DisputeForm, SecretInput, type Escrow, type EscrowEvents, type Invoice } from "@/lib/schemas";
import { loadArbiterKey, openSealed, sealSecret } from "@/lib/arbiter";
import { scannerSupported, secretFromScan } from "@/lib/scan";
import { buildReleaseLink, generateSecret, hashSecret, loadSecret, saveSecret } from "@/lib/secret";
import { getWebLN, payWithWebLN } from "@/lib/webln";
import { Button, Card, ConfirmButton, CopyButton, CopyField, Countdown, Field, inputClass, Notice, Spinner } from "../ui";
import { useDefaultArbiter, useStoredSecret } from "./hooks";
import { EvidenceList, NotesBox, ResponseWindow, useResponseWindow } from "./Notes";
import { NairaPay, PayChoice } from "./NairaPay";
import { Scanner } from "./Scanner";

type PanelProps = {
  escrow: Escrow;
  setEscrow: (e: Escrow) => void;
  refresh: () => Promise<void>;
  /** A release secret taken from a `#release=` link, if this page was opened with one. */
  linkSecret: string | null;
  /** The escrow's history, so an arbiter can read what was reported right where they decide. */
  events?: EscrowEvents | null;
};

type Run = (name: string, fn: () => Promise<Escrow>) => Promise<boolean>;
type Actions = { run: Run; busy: string | null };

/** Base URL for links shown to the other party; set NEXT_PUBLIC_APP_URL when phones can't reach localhost. */
function appUrl(): string {
  return (process.env.NEXT_PUBLIC_APP_URL || window.location.origin).replace(/\/+$/, "");
}

export function EscrowPanel({ escrow, setEscrow, refresh, linkSecret, events }: PanelProps) {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // Every action returns the escrow as the server now reports it; that, not the click, drives the UI.
  const run: Run = async (name, fn) => {
    setBusy(name);
    setError(null);
    setNotice(null);
    try {
      setEscrow(await fn());
      return true;
    } catch (err) {
      if (err instanceof ApiError && (err.code === "invalid_state" || err.code === "too_late")) {
        await refresh();
        setNotice(
          err.code === "too_late"
            ? describeError(err)
            : "The escrow changed in the meantime. This is its latest state.",
        );
      } else {
        setError(describeError(err));
      }
      return false;
    } finally {
      setBusy(null);
    }
  };
  const actions: Actions = { run, busy };
  const role = escrow.your_role;

  let body: ReactNode;
  if (role === "operator") body = <OperatorPanel escrow={escrow} actions={actions} />;
  else if (role === "arbiter") body = <ArbiterPanel escrow={escrow} actions={actions} events={events ?? null} />;
  else if (role === "buyer") body = <BuyerPanel escrow={escrow} actions={actions} linkSecret={linkSecret} events={events ?? null} />;
  else body = <SellerPanel escrow={escrow} actions={actions} linkSecret={linkSecret} events={events ?? null} />;

  const releasable = escrow.state === "LOCKED" || escrow.state === "DISPUTED";

  return (
    <Card>
      <div className="space-y-5">
        <NextSteps escrow={escrow} />
        {linkSecret && !releasable && role !== "operator" && role !== "arbiter" && (
          <Notice tone="info">
            You opened a release link, but there is nothing to release while the escrow is in this state.
          </Notice>
        )}
        {body}
        {notice && <Notice tone="info">{notice}</Notice>}
        {error && <Notice tone="error">{error}</Notice>}
      </div>
    </Card>
  );
}

// What happens next, per state and role.

function NextSteps({ escrow }: { escrow: Escrow }) {
  const buyer = escrow.your_role === "buyer";
  const seller = escrow.your_role === "seller";
  const text: Record<Escrow["state"], string> = {
    CREATED: buyer
      ? "Accepting creates a secret on this device and a Lightning invoice for you to pay. Nothing is paid yet."
      : seller
        ? "The buyer has to accept the offer. Send them the link below."
        : "Waiting for the buyer to accept.",
    INVOICE_ISSUED: buyer
      ? "Pay the invoice before it expires. Your payment is locked, not sent: the seller only gets it when you hand over your secret. If you don't, you are refunded automatically at the deadline."
      : "Waiting for the buyer to pay. If the invoice expires unpaid, the escrow ends and nothing is owed.",
    LOCKED: buyer
      ? "Your payment is locked. When you have what you paid for, show the seller your release QR. If you never do, you are refunded automatically at the deadline."
      : seller
        ? "The buyer's payment is locked, so you can start the work. On delivery, get the buyer's release QR (or 64-character secret) and submit it before the deadline. After the deadline the buyer is refunded automatically and the payment can no longer be claimed."
        : "Funds are locked. They settle if the secret is submitted before the deadline, otherwise they are refunded.",
    DISPUTED: escrow.arbiter_id
      ? "A dispute is open and the arbiter will decide it: either the seller is paid or the buyer is refunded. The deadline still applies: if no decision is made before it, the buyer is refunded automatically. The parties can still settle it themselves with the secret."
      : "A dispute is open and the operator will review it. The deadline still applies: if the secret isn't submitted before it, the buyer is refunded automatically. The parties can still settle it themselves (release with the secret) or the seller can give a refund.",
    SETTLED: buyer ? "Done. The seller has been paid." : seller ? "Done. You were paid." : "Settled: the seller was paid.",
    REFUNDED: buyer
      ? "Done. Your payment was cancelled and the funds are back in your wallet."
      : seller
        ? "Cancelled. The buyer was refunded."
        : "Refunded to the buyer.",
    EXPIRED_UNPAID: "This offer ended without a payment: it was withdrawn, declined or its time ran out. Nothing was paid and nothing is owed.",
    FAILED: "Something went wrong between the escrow and the Lightning node. An operator needs to look at it.",
  };
  return <p className="text-sm leading-relaxed text-neutral-700 dark:text-neutral-300">{text[escrow.state]}</p>;
}

function Deadline({ escrow }: { escrow: Escrow }) {
  return (
    <Countdown
      until={escrow.deadline_at}
      label="Release deadline"
      endedText="Passed: refunding the buyer"
    />
  );
}

function FinalSummary({ escrow }: { escrow: Escrow }) {
  switch (escrow.state) {
    case "SETTLED":
      return <Notice tone="success">{formatSats(escrow.amount_sats)} settled to the seller.</Notice>;
    case "REFUNDED":
      return <Notice tone="success">{formatSats(escrow.amount_sats)} returned to the buyer.</Notice>;
    case "EXPIRED_UNPAID":
      return <Notice tone="info">Ended without a payment.</Notice>;
    case "FAILED":
      return <Notice tone="error">Needs operator attention. Your funds are governed by the Lightning node.</Notice>;
    default:
      return null;
  }
}

// Buyer

function BuyerPanel({ escrow, actions, linkSecret, events }: { escrow: Escrow; actions: Actions; linkSecret: string | null; events: EscrowEvents | null }) {
  // Bumped after the secret is (re)generated so the stored-secret check runs again.
  const [secretVersion, setSecretVersion] = useState(0);
  const stored = useStoredSecret(escrow.id, escrow.payment_hash, secretVersion);

  async function accept() {
    const ok = await actions.run("accept", async () => {
      // Store P before sending H: if the response is lost, the secret is still here.
      let p = loadSecret(escrow.id);
      if (!p) {
        p = generateSecret();
        saveSecret(escrow.id, p);
      }
      // With an arbiter, the secret is also encrypted to their key so only they can open it. The
      // server stores that blob but cannot read it.
      const send = async (secret: string) =>
        api.accept(
          escrow.id,
          await hashSecret(secret),
          escrow.arbiter_key ? await sealSecret(secret, escrow.arbiter_key) : undefined,
        );
      try {
        return await send(p);
      } catch (err) {
        if (!(err instanceof ApiError) || err.code !== "hash_in_use") throw err;
        p = generateSecret();
        saveSecret(escrow.id, p);
        return await send(p);
      }
    });
    if (ok) setSecretVersion((v) => v + 1);
  }

  switch (escrow.state) {
    case "CREATED":
      return (
        <div className="space-y-3">
          <Notice tone="warn">
            Your release secret will be stored <strong>only in this browser</strong>. If this browser&apos;s data
            is cleared, the secret is lost and the escrow can only end in a refund. Use the browser you will have
            with you at delivery.
          </Notice>
          {escrow.arbiter_id && <ArbiterNotice escrow={escrow} />}
          <Button onClick={accept} busy={actions.busy === "accept"}>
            Accept offer of {formatSats(escrow.amount_sats)}
          </Button>
          {escrow.buyer_id ? (
            <ConfirmButton
              confirmText="Decline this offer? Nothing has been paid."
              confirmLabel="Yes, decline"
              variant="danger"
              busy={actions.busy === "cancel"}
              onConfirm={() => actions.run("cancel", () => api.cancel(escrow.id))}
            >
              Decline
            </ConfirmButton>
          ) : (
            <p className="text-xs text-neutral-500">
              Accepting makes you the buyer of this offer. If you are not the person it was meant for, close this page.
            </p>
          )}
        </div>
      );
    case "INVOICE_ISSUED":
      return (
        <div className="space-y-4">
          <SecretHealth stored={stored} />
          <PayChoice escrow={escrow} sats={<InvoiceBox escrow={escrow} />} />
          <p className="flex items-center gap-2 text-sm text-neutral-500">
            <span className="size-2 animate-pulse rounded-full bg-sky-500" />
            After you pay, this page updates by itself once the Lightning node confirms the funds are locked.
          </p>
          <ConfirmButton
            confirmText="Cancel this escrow? Nothing has been paid yet."
            confirmLabel="Yes, cancel"
            variant="danger"
            busy={actions.busy === "cancel"}
            onConfirm={() => actions.run("cancel", () => api.cancel(escrow.id))}
          >
            Cancel
          </ConfirmButton>
        </div>
      );
    case "LOCKED":
    case "DISPUTED":
      return (
        <div className="space-y-4">
          <Deadline escrow={escrow} />
          <NairaPay escrow={escrow} canStart={false} />
          {linkSecret ? (
            <ReleaseForm escrow={escrow} actions={actions} initial={linkSecret} fromLink />
          ) : stored.status === "ok" ? (
            <ReleaseHandover escrow={escrow} secret={stored.secret} actions={actions} />
          ) : stored.status === "checking" ? (
            <Spinner label="Checking your secret…" />
          ) : (
            <>
              <SecretHealth stored={stored} />
              <ReleaseForm escrow={escrow} actions={actions} />
            </>
          )}
          <NotesBox escrow={escrow} events={events} run={actions.run} busy={actions.busy} />
          {escrow.state === "LOCKED" && <DisputeBox escrow={escrow} actions={actions} />}
          <p className="text-xs text-neutral-500">
            Once the funds are locked only the seller can cancel. If nothing happens, you are refunded at the
            deadline.
          </p>
        </div>
      );
    default:
      return (
        <div className="space-y-4">
          <FinalSummary escrow={escrow} />
          <NairaPay escrow={escrow} canStart={false} />
        </div>
      );
  }
}

function SecretHealth({ stored }: { stored: ReturnType<typeof useStoredSecret> }) {
  if (stored.status === "missing") {
    return (
      <Notice tone="warn">
        This browser doesn&apos;t hold the release secret for this escrow (it was accepted on another device, or
        the browser data was cleared). Without it the payment can&apos;t be released and will be refunded at the
        deadline. If you have the 64-character secret elsewhere, paste it to release.
      </Notice>
    );
  }
  if (stored.status === "mismatch") {
    return (
      <Notice tone="error">
        The secret stored in this browser doesn&apos;t match this escrow&apos;s payment hash, so it can&apos;t
        release it.
      </Notice>
    );
  }
  return null;
}

function InvoiceBox({ escrow }: { escrow: Escrow }) {
  const [invoice, setInvoice] = useState<Invoice | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [wallet, setWallet] = useState(false);
  const [walletMsg, setWalletMsg] = useState<{ tone: "info" | "error"; text: string } | null>(null);
  const [paying, setPaying] = useState(false);

  // Wallet extensions inject `window.webln` after load, so look once the page is up.
  useEffect(() => {
    const t = setTimeout(() => setWallet(getWebLN() !== null), 300);
    return () => clearTimeout(t);
  }, []);

  async function payWithWallet(request: string) {
    setPaying(true);
    setWalletMsg(null);
    try {
      await payWithWebLN(request);
      // We do not claim the funds are locked: this page changes only when the API says so.
      setWalletMsg({ tone: "info", text: "Payment sent to your wallet. This page updates once the node confirms the funds are locked." });
    } catch (err) {
      setWalletMsg({ tone: "error", text: err instanceof Error ? err.message : "The wallet could not pay." });
    } finally {
      setPaying(false);
    }
  }

  useEffect(() => {
    let alive = true;
    api
      .invoice(escrow.id)
      .then((inv) => alive && setInvoice(inv))
      .catch((err) => alive && setError(describeError(err)));
    return () => {
      alive = false;
    };
  }, [escrow.id, escrow.payment_hash]);

  if (error) return <Notice tone="error">{error}</Notice>;
  if (!invoice) return <Spinner label="Fetching the invoice…" />;

  const expires = invoice.expires_at ?? escrow.invoice_expires_at;
  return (
    <div className="space-y-4">
      <div className="flex flex-col items-center gap-3">
        {/* Generated locally: the invoice is never sent to a QR service. Upper case makes a smaller code. */}
        <div className="rounded-xl bg-white p-3">
          <QRCodeSVG
            value={`lightning:${invoice.payment_request.toUpperCase()}`}
            size={232}
            marginSize={2}
            className="h-auto w-full max-w-[232px]"
            title="Lightning invoice"
          />
        </div>
        <div className="text-sm font-medium">Scan with a Lightning wallet to pay {formatSats(escrow.amount_sats)}</div>
      </div>
      {wallet && (
        <div className="space-y-2">
          <Button onClick={() => payWithWallet(invoice.payment_request)} busy={paying}>
            Pay {formatSats(escrow.amount_sats)} with my wallet
          </Button>
          {walletMsg && <Notice tone={walletMsg.tone}>{walletMsg.text}</Notice>}
        </div>
      )}
      <CopyField value={invoice.payment_request} label="Copy invoice" />
      {expires && <Countdown until={expires} label="Pay within" endedText="Expired" />}
      {process.env.NODE_ENV === "development" && (
        <details className="text-xs text-neutral-500">
          <summary className="cursor-pointer">Paying on regtest</summary>
          <p className="mt-2">From the repository root:</p>
          <CopyField
            value={`docker compose -f regtest/docker-compose.yml exec -T buyer lncli --network=regtest payinvoice --force ${invoice.payment_request}`}
          />
        </details>
      )}
    </div>
  );
}

/** The buyer's release QR: a link with the secret in the fragment. Hidden until revealed. */
function ReleaseHandover({ escrow, secret, actions }: { escrow: Escrow; secret: string; actions: Actions }) {
  const [shown, setShown] = useState(false);
  const link = buildReleaseLink(appUrl(), escrow.id, secret);

  if (!shown) {
    return (
      <div className="space-y-3 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
        <div className="font-semibold">Release QR</div>
        <p className="text-sm text-neutral-600 dark:text-neutral-400">
          Showing this QR is how you pay the seller. Anyone who scans it can release the payment, so only reveal it
          when you have what you paid for.
        </p>
        <ConfirmButton
          confirmText="Have you received what you paid for? Whoever scans the QR can release the payment to the seller."
          confirmLabel="Yes, show the QR"
          onConfirm={() => setShown(true)}
          variant="primary"
        >
          Reveal release QR
        </ConfirmButton>
      </div>
    );
  }

  return (
    <div className="space-y-4 rounded-lg border border-amber-400 p-4 dark:border-amber-700">
      <div className="flex flex-col items-center gap-3">
        <div className="rounded-xl bg-white p-3">
          <QRCodeSVG value={link} size={256} marginSize={2} className="h-auto w-full max-w-[256px]" title="Release QR" />
        </div>
        <p className="text-center text-sm">
          Let the seller scan this with their phone camera. It opens this escrow with the secret filled in.
        </p>
      </div>
      <div className="flex flex-wrap gap-2">
        <CopyButton value={link} label="Copy release link" />
        <CopyButton value={secret} label="Copy secret only" />
        <Button variant="ghost" onClick={() => setShown(false)}>
          Hide
        </Button>
      </div>
      <div className="border-t border-neutral-200 pt-3 dark:border-neutral-800">
        <ConfirmButton
          confirmText={`Release ${formatSats(escrow.amount_sats)} to the seller now? This can't be undone.`}
          confirmLabel="Release now"
          busy={actions.busy === "release"}
          onConfirm={() => actions.run("release", () => api.release(escrow.id, secret))}
        >
          Or release it yourself from here
        </ConfirmButton>
      </div>
    </div>
  );
}

// Seller

function SellerPanel({ escrow, actions, linkSecret, events }: { escrow: Escrow; actions: Actions; linkSecret: string | null; events: EscrowEvents | null }) {
  const refund = (
    <ConfirmButton
      confirmText={
        escrow.state === "CREATED" || escrow.state === "INVOICE_ISSUED"
          ? "Withdraw this offer? The buyer won't be able to pay."
          : `Give the buyer their ${formatSats(escrow.amount_sats)} back? You will not be paid. This can't be undone.`
      }
      confirmLabel={escrow.state === "CREATED" || escrow.state === "INVOICE_ISSUED" ? "Withdraw offer" : "Refund the buyer"}
      variant="danger"
      busy={actions.busy === "cancel"}
      onConfirm={() => actions.run("cancel", () => api.cancel(escrow.id))}
    >
      {escrow.state === "CREATED" || escrow.state === "INVOICE_ISSUED" ? "Withdraw offer" : "Give refund"}
    </ConfirmButton>
  );

  switch (escrow.state) {
    case "CREATED":
      return (
        <div className="space-y-3">
          {escrow.arbiter_id && <ArbiterNotice escrow={escrow} />}
          <div className="text-sm font-medium">Link for the buyer</div>
          <CopyField value={`${appUrl()}/escrows/${escrow.id}`} label="Copy link" />
          <p className="text-xs text-neutral-500">
            {escrow.buyer_id ? (
              <>
                The buyer must be signed in as <span className="font-mono">{escrow.buyer_id}</span> to open it.
              </>
            ) : (
              <>
                This is an open offer: <strong>the first person to open the link and accept becomes the buyer</strong>,
                so send it only to the person you are dealing with.
              </>
            )}{" "}
            The offer lapses on its own if nobody has accepted by the deadline.
          </p>
          {refund}
        </div>
      );
    case "INVOICE_ISSUED":
      return (
        <div className="space-y-4">
          <p className="flex items-center gap-2 text-sm">
            <span className="size-2 animate-pulse rounded-full bg-sky-500" />
            Waiting for the buyer&apos;s payment.
          </p>
          {escrow.invoice_expires_at && (
            <Countdown until={escrow.invoice_expires_at} label="Invoice expires in" endedText="Expired" />
          )}
          {escrow.arbiter_id && <ArbiterNotice escrow={escrow} />}
          {refund}
        </div>
      );
    case "LOCKED":
    case "DISPUTED":
      return (
        <div className="space-y-4">
          {escrow.state === "LOCKED" && (
            <Notice tone="success">
              {escrow.arbiter_id && !escrow.arbiter_confirmed_at ? (
                <strong>Funds are locked.</strong>
              ) : (
                <strong>Funds are locked: you can start the work.</strong>
              )}
            </Notice>
          )}
          {escrow.state === "LOCKED" && escrow.arbiter_id && !escrow.arbiter_confirmed_at && (
            <Notice tone="warn">
              <strong>The arbiter hasn&apos;t confirmed yet.</strong> Until they do, you can&apos;t count on them being
              able to release this if there is a dispute: the buyer&apos;s sealed secret hasn&apos;t been checked.
              Consider waiting for the confirmation before you start.
            </Notice>
          )}
          <Deadline escrow={escrow} />
          <ReleaseForm
            key={linkSecret ?? "manual"}
            escrow={escrow}
            actions={actions}
            initial={linkSecret ?? ""}
            fromLink={!!linkSecret}
          />
          <div className="flex flex-wrap gap-2">
            {refund}
          </div>
          <NotesBox escrow={escrow} events={events} run={actions.run} busy={actions.busy} />
          {escrow.state === "LOCKED" && <DisputeBox escrow={escrow} actions={actions} />}
        </div>
      );
    default:
      return <FinalSummary escrow={escrow} />;
  }
}

/** Paste or confirm the 64-hex secret and release. Never releases without a button press. */
function ReleaseForm({
  escrow,
  actions,
  initial = "",
  fromLink = false,
}: {
  escrow: Escrow;
  actions: Actions;
  initial?: string;
  fromLink?: boolean;
}) {
  const [value, setValue] = useState(initial);
  const [error, setError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);
  const [canScan, setCanScan] = useState(false);

  // Feature detection needs the browser, so it runs after mount.
  useEffect(() => {
    const t = setTimeout(() => setCanScan(scannerSupported()), 0);
    return () => clearTimeout(t);
  }, []);

  // A scan only fills the field. Releasing still takes a deliberate press of the button.
  function onScan(text: string) {
    const result = secretFromScan(text, escrow.id);
    if ("error" in result) {
      setError(result.error);
      return;
    }
    setValue(result.secret);
    setError(null);
    setScanning(false);
  }

  async function submit(e: FormEvent) {
    e.preventDefault();
    const parsed = SecretInput.safeParse(value);
    if (!parsed.success) {
      setError(parsed.error.issues[0]?.message ?? "Invalid secret");
      return;
    }
    setError(null);
    await actions.run("release", () => api.release(escrow.id, parsed.data));
  }

  return (
    <form onSubmit={submit} className="space-y-3 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
      <div className="font-semibold">Release payment</div>
      {fromLink && (
        <Notice tone="info">
          The secret was filled in from the release link. Check the handover is complete, then press Release.
        </Notice>
      )}
      <Field
        label="Release secret"
        hint="Scan the buyer's release QR with your phone camera, or paste the 64-character secret."
        error={error ?? undefined}
      >
        <input
          value={value}
          onChange={(e) => {
            setValue(e.target.value);
            setError(null);
          }}
          className={`${inputClass} font-mono`}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          spellCheck={false}
          placeholder="64 hex characters"
          maxLength={80}
        />
      </Field>
      {canScan &&
        (scanning ? (
          <Scanner onText={onScan} onClose={() => setScanning(false)} />
        ) : (
          <Button variant="secondary" onClick={() => setScanning(true)}>
            Scan the buyer&apos;s QR
          </Button>
        ))}
      <Button type="submit" busy={actions.busy === "release"}>
        Release {formatSats(escrow.amount_sats)}
      </Button>
    </form>
  );
}

function DisputeBox({ escrow, actions }: { escrow: Escrow; actions: Actions }) {
  const [open, setOpen] = useState(false);
  const [reason, setReason] = useState("");
  const [error, setError] = useState<string | null>(null);

  if (!open) {
    return (
      <Button variant="ghost" onClick={() => setOpen(true)}>
        Open a dispute…
      </Button>
    );
  }

  async function submit(e: FormEvent) {
    e.preventDefault();
    const parsed = DisputeForm.safeParse(reason);
    if (!parsed.success) {
      setError(parsed.error.issues[0]?.message ?? "Invalid reason");
      return;
    }
    if (await actions.run("dispute", () => api.dispute(escrow.id, parsed.data))) setOpen(false);
  }

  return (
    <form onSubmit={submit} className="space-y-3 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
      <div className="font-semibold">Open a dispute</div>
      <p className="text-sm text-neutral-600 dark:text-neutral-400">
        Flags the escrow for the operator. It does not extend the deadline: unless the secret is submitted, the buyer
        is still refunded when it passes.
      </p>
      <Field label="What went wrong?" error={error ?? undefined}>
        <textarea
          value={reason}
          onChange={(e) => {
            setReason(e.target.value);
            setError(null);
          }}
          rows={3}
          maxLength={500}
          className={inputClass}
        />
      </Field>
      <div className="flex flex-wrap gap-2">
        <Button type="submit" variant="danger" busy={actions.busy === "dispute"}>
          Open dispute
        </Button>
        <Button variant="ghost" onClick={() => setOpen(false)}>
          Never mind
        </Button>
      </div>
    </form>
  );
}

// Operator

function OperatorPanel({ escrow, actions }: { escrow: Escrow; actions: Actions }) {
  const cancellable = ["INVOICE_ISSUED", "LOCKED", "DISPUTED"].includes(escrow.state);
  return (
    <div className="space-y-4">
      {(escrow.state === "LOCKED" || escrow.state === "DISPUTED") && <Deadline escrow={escrow} />}
      {cancellable ? (
        <ConfirmButton
          confirmText={`Cancel and refund ${formatSats(escrow.amount_sats)} to the buyer? This can't be undone.`}
          confirmLabel="Cancel and refund"
          variant="danger"
          busy={actions.busy === "cancel"}
          onConfirm={() => actions.run("cancel", () => api.cancel(escrow.id))}
        >
          Cancel escrow
        </ConfirmButton>
      ) : (
        <FinalSummary escrow={escrow} />
      )}
    </div>
  );
}

// Arbiter

/** The arbiter's key fingerprint and confirmation status, shown to buyer and seller. */
function ArbiterNotice({ escrow }: { escrow: Escrow }) {
  const platform = useDefaultArbiter() === escrow.arbiter_id;
  return (
    <Notice tone="info">
      <div className="space-y-1">
        <div>
          <strong>{platform ? "The platform's arbiter decides a dispute" : "This escrow has an arbiter"}</strong> (
          <span className="font-mono">{escrow.arbiter_id}</span>){platform ? "." : " who decides a dispute."}
        </div>
        <div className="text-xs">
          Their key fingerprint is <span className="break-words font-mono">{escrow.arbiter_fingerprint}</span>. Check
          it with them: the buyer&apos;s release secret is encrypted to this key, so it must really be theirs.
        </div>
        <div className="text-xs">
          {escrow.arbiter_confirmed_at
            ? "The arbiter has confirmed they can open the buyer's secret."
            : "The arbiter has not confirmed yet."}
        </div>
      </div>
    </Notice>
  );
}

type Verified = { status: "idle" | "checking" | "ok" | "bad"; message?: string };

function ArbiterPanel({ escrow, actions, events }: { escrow: Escrow; actions: Actions; events: EscrowEvents | null }) {
  const [verified, setVerified] = useState<Verified>({ status: "idle" });
  // The decrypted secret lives in memory only, for as long as this panel does.
  const [secret, setSecret] = useState<string | null>(null);
  const [note, setNote] = useState("");
  // While the other side still has time to answer the dispute, an arbiter cannot decide.
  const waiting = useResponseWindow(events) !== null;

  /** Open the sealed secret with this device's private key and check it matches the payment hash. */
  async function verify(): Promise<string | null> {
    setVerified({ status: "checking" });
    try {
      const fp = escrow.arbiter_fingerprint;
      const key = fp ? loadArbiterKey(fp) : null;
      if (!key) {
        throw new Error(
          "This browser doesn't hold your private key for this escrow's key. Restore your backup on the Arbiter page.",
        );
      }
      const arbitration = await api.arbitration(escrow.id);
      const opened = await openSealed(arbitration.sealed_secret, key);
      if ((await hashSecret(opened)) !== escrow.payment_hash) {
        throw new Error(
          "The sealed secret opens, but it does not match this escrow's payment hash. It cannot release the payment, so don't confirm.",
        );
      }
      setSecret(opened);
      setVerified({ status: "ok", message: "The sealed secret opens and matches the payment hash." });
      return opened;
    } catch (err) {
      setSecret(null);
      setVerified({ status: "bad", message: err instanceof Error ? err.message : describeError(err) });
      return null;
    }
  }

  const result =
    verified.status === "ok" ? (
      <Notice tone="success">{verified.message}</Notice>
    ) : verified.status === "bad" ? (
      <Notice tone="error">{verified.message}</Notice>
    ) : null;

  switch (escrow.state) {
    case "CREATED":
      return <p className="text-sm">You are the arbiter. The buyer has not accepted yet, so there is nothing to check.</p>;
    case "INVOICE_ISSUED":
    case "LOCKED":
      return (
        <div className="space-y-4">
          <p className="text-sm">
            You are the arbiter. If either side opens a dispute you decide who is paid. First check that you can open
            the buyer&apos;s sealed secret, then confirm so the seller knows arbitration will work.
          </p>
          {escrow.arbiter_confirmed_at ? (
            <Notice tone="success">You confirmed this. Nothing more to do unless a dispute is opened.</Notice>
          ) : (
            <div className="space-y-3">
              <Button variant="secondary" onClick={verify} busy={verified.status === "checking"}>
                Check the sealed secret
              </Button>
              {result}
              {verified.status === "ok" && (
                <Button onClick={() => actions.run("confirm", () => api.confirmArbiter(escrow.id))} busy={actions.busy === "confirm"}>
                  Confirm to both parties
                </Button>
              )}
            </div>
          )}
        </div>
      );
    case "DISPUTED":
      return (
        <div className="space-y-4">
          <Notice tone="warn">
            <strong>A dispute is waiting for your decision.</strong> Read the history below, then decide. You must
            decide before the deadline: after it the buyer is refunded automatically.
          </Notice>
          <Reported escrow={escrow} events={events} />
          <EvidenceList escrow={escrow} events={events} title="Delivery and evidence from both sides" />
          <ResponseWindow escrow={escrow} events={events} />
          <Deadline escrow={escrow} />
          <Field label="Your note (shown in the history)" hint="Optional. Why you decided this way.">
            <textarea
              value={note}
              onChange={(e) => setNote(e.target.value)}
              className={inputClass}
              rows={2}
              maxLength={500}
            />
          </Field>
          {result}
          <div className="flex flex-wrap gap-2">
            <ConfirmButton
              confirmText={`Pay the seller ${formatSats(escrow.amount_sats)}? This can't be undone.`}
              confirmLabel="Yes, pay the seller"
              disabled={waiting}
              busy={actions.busy === "decide"}
              onConfirm={async () => {
                const opened = secret ?? (await verify());
                if (opened) await actions.run("decide", () => api.decide(escrow.id, "release", note, opened));
              }}
            >
              Release to the seller
            </ConfirmButton>
            <ConfirmButton
              confirmText={`Refund the buyer ${formatSats(escrow.amount_sats)}? The seller will not be paid.`}
              confirmLabel="Yes, refund the buyer"
              disabled={waiting}
              busy={actions.busy === "decide"}
              onConfirm={() => actions.run("decide", () => api.decide(escrow.id, "refund", note))}
            >
              Refund the buyer
            </ConfirmButton>
          </div>
        </div>
      );
    default:
      return <FinalSummary escrow={escrow} />;
  }
}

/** What each side reported when the dispute was opened, quoted where the arbiter decides. */
function Reported({ escrow, events }: { escrow: Escrow; events: EscrowEvents | null }) {
  const reports = (events?.audit ?? []).filter((a) => a.action === "dispute");
  if (reports.length === 0) return null;
  return (
    <div className="space-y-2 rounded-lg border border-neutral-200 p-3 text-sm dark:border-neutral-800">
      <div className="text-xs font-semibold uppercase tracking-wide text-neutral-500">What was reported</div>
      {reports.map((r, i) => (
        <p key={i}>
          <span className="font-medium">
            {r.actor === escrow.buyer_id ? "The buyer" : r.actor === escrow.seller_id ? "The seller" : "A party"}:
          </span>{" "}
          &ldquo;{r.reason}&rdquo;
        </p>
      ))}
    </div>
  );
}
