// Typed API client: base URL, bearer token, Zod-checked responses and the API's error format.
// A 401 on an authenticated call signs the user out here, in one place.

import { z } from "zod";
import {
  ApiErrorBody,
  Escrow,
  EscrowEvents,
  EscrowList,
  Health,
  Invoice,
  ApiKeyList,
  NewWebhook,
  WebhookList,
  WebhookTestResult,
  ArbiterKey,
  DefaultArbiter,
  Arbitration,
  Me,
  RateQuote,
  RatesInfo,
  NodeInfo,
  NewApiKey,
  NewUser,
  Balance,
  Withdrawal,
  WithdrawalList,
  PinStatus,
  RampInfo,
  ResolvedAccount,
  RampQuote,
  RampOrder,
  RampOrderList,
  EscrowRampOrder,
  type NewEscrowRequest,
} from "./schemas";
import { getSession, removeVault } from "./session";

export const API_URL = (process.env.NEXT_PUBLIC_API_URL ?? "http://127.0.0.1:3000").replace(/\/+$/, "");

export class ApiError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly status: number,
    /** Seconds to wait, from `Retry-After` on 429 responses. */
    readonly retryAfter: number | null = null,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

type Options = {
  body?: unknown;
  headers?: Record<string, string>;
  /** Overrides the session token, e.g. to check a token before signing in with it. */
  token?: string | null;
  /** Leave the session alone on 401 (used while checking a token someone typed). */
  keepSessionOn401?: boolean;
};

async function request<T>(method: string, path: string, schema: z.ZodType<T>, opts: Options = {}): Promise<T> {
  const token = opts.token !== undefined ? opts.token : (getSession()?.token ?? null);
  const headers: Record<string, string> = { Accept: "application/json", ...opts.headers };
  if (token) headers.Authorization = `Bearer ${token}`;
  if (opts.body !== undefined) headers["Content-Type"] = "application/json";

  let res: Response;
  try {
    res = await fetch(`${API_URL}${path}`, {
      method,
      headers,
      body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
      cache: "no-store",
    });
  } catch {
    throw new ApiError("network", "Can't reach the escrow server. Check your connection; nothing has changed.", 0);
  }

  const text = await res.text();
  let json: unknown = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    // Fall through: handled as an unexpected response below.
  }

  if (!res.ok) {
    const parsed = ApiErrorBody.safeParse(json);
    const code = parsed.success ? parsed.data.error.code : `http_${res.status}`;
    const message = parsed.success ? parsed.data.error.message : `Unexpected response (HTTP ${res.status})`;
    const retry = Number(res.headers.get("Retry-After"));
    if (res.status === 401 && token && !opts.keepSessionOn401 && token === getSession()?.token) removeVault();
    throw new ApiError(code, message, res.status, Number.isFinite(retry) && retry > 0 ? retry : null);
  }

  const parsed = schema.safeParse(json);
  if (!parsed.success) {
    throw new ApiError("bad_response", "The server sent a response this app doesn't understand.", res.status);
  }
  return parsed.data;
}

const esc = (id: string) => `/api/v1/escrows/${encodeURIComponent(id)}`;

export const api = {
  /** `/health` answers 503 with a body when the node is down; both are useful here. */
  async health(): Promise<Health> {
    const res = await fetch(`${API_URL}/health`, { cache: "no-store" });
    const parsed = Health.safeParse(await res.json());
    if (!parsed.success) throw new ApiError("bad_response", "Unexpected /health response", res.status);
    return parsed.data;
  },

  register: () => request("POST", "/api/v1/users", NewUser, { token: null }),

  me: (opts?: Pick<Options, "token" | "keepSessionOn401">) => request("GET", "/api/v1/me", Me, opts),

  apiKeys: () => request("GET", "/api/v1/me/api-keys", ApiKeyList),

  createApiKey: (body: { name: string; scopes: string[] }) => request("POST", "/api/v1/me/api-keys", NewApiKey, { body }),

  revokeApiKey: (id: string) =>
    request("DELETE", `/api/v1/me/api-keys/${encodeURIComponent(id)}`, z.object({ revoked: z.boolean() })),

  webhooks: () => request("GET", "/api/v1/me/webhooks", WebhookList),

  createWebhook: (url: string) => request("POST", "/api/v1/me/webhooks", NewWebhook, { body: { url } }),

  deleteWebhook: (id: string) =>
    request("DELETE", `/api/v1/me/webhooks/${encodeURIComponent(id)}`, z.object({ deleted: z.boolean() })),

  testWebhook: (id: string) =>
    request("POST", `/api/v1/me/webhooks/${encodeURIComponent(id)}/test`, WebhookTestResult),

  node: (opts?: Pick<Options, "token" | "keepSessionOn401">) => request("GET", "/api/v1/me/node", NodeInfo, opts),

  connectNode: (body: { address: string; tls_cert: string; macaroon: string }) =>
    request("PUT", "/api/v1/me/node", NodeInfo, { body }),

  disconnectNode: () => request("DELETE", "/api/v1/me/node", NodeInfo),

  listMine: (opts?: Pick<Options, "token" | "keepSessionOn401">) =>
    request("GET", "/api/v1/escrows", EscrowList, opts),

  create: (body: NewEscrowRequest, idempotencyKey: string) =>
    request("POST", "/api/v1/escrows", Escrow, { body, headers: { "Idempotency-Key": idempotencyKey } }),

  get: (id: string) => request("GET", esc(id), Escrow),

  /** `sealedSecret`: the release secret encrypted to the arbiter, required when the escrow has one. */
  accept: (id: string, paymentHash: string, sealedSecret?: string) =>
    request("POST", `${esc(id)}/accept`, Escrow, {
      body: sealedSecret ? { payment_hash: paymentHash, sealed_secret: sealedSecret } : { payment_hash: paymentHash },
    }),

  /** Whether fiat values are on, and which currencies. */
  rates: () => request("GET", "/api/v1/rates", RatesInfo),

  /** The current price of one bitcoin in `currency`, to preview a value before creating. */
  rate: (currency: string) => request("GET", `/api/v1/rates/${encodeURIComponent(currency)}`, RateQuote),

  defaultArbiter: () => request("GET", "/api/v1/arbiter/default", DefaultArbiter),

  arbiterKey: () => request("GET", "/api/v1/me/arbiter-key", ArbiterKey),

  setArbiterKey: (publicKey: string) =>
    request("PUT", "/api/v1/me/arbiter-key", ArbiterKey, { body: { public_key: publicKey } }),

  arbitration: (id: string) => request("GET", `${esc(id)}/arbitration`, Arbitration),

  confirmArbiter: (id: string) => request("POST", `${esc(id)}/arbitration/confirm`, Escrow),

  /** `preimage` is needed to release; a refund needs none. */
  decide: (id: string, decision: "release" | "refund", note: string, preimage?: string) =>
    request("POST", `${esc(id)}/arbitration/decide`, Escrow, {
      body: { decision, note, ...(preimage ? { preimage } : {}) },
    }),

  invoice: (id: string) => request("GET", `${esc(id)}/invoice`, Invoice),

  release: (id: string, preimage: string) =>
    request("POST", `${esc(id)}/release`, Escrow, { body: { preimage } }),

  cancel: (id: string) => request("POST", `${esc(id)}/cancel`, Escrow),

  dispute: (id: string, reason: string) => request("POST", `${esc(id)}/dispute`, Escrow, { body: { reason } }),

  /** A delivery claim (the seller) or a piece of evidence (either side) on a locked or disputed escrow. */
  addNote: (id: string, kind: "delivered" | "evidence", text: string) =>
    request("POST", `${esc(id)}/notes`, Escrow, { body: { kind, text } }),

  events: (id: string) => request("GET", `${esc(id)}/events`, EscrowEvents),

  /** What the service owes you from escrows settled on its shared node. */
  balance: () => request("GET", "/api/v1/me/balance", Balance),

  withdrawals: () => request("GET", "/api/v1/me/withdrawals", WithdrawalList),

  /** Pay a Lightning invoice out of your balance; the invoice sets the amount. */
  withdraw: (invoice: string, pin: string) =>
    request("POST", "/api/v1/me/withdrawals", Withdrawal, { body: { invoice, pin } }),

  withdrawalPin: () => request("GET", "/api/v1/me/withdrawal-pin", PinStatus),

  /** Set the withdrawal PIN; changing one needs the current PIN. */
  setWithdrawalPin: (pin: string, currentPin?: string) =>
    request("PUT", "/api/v1/me/withdrawal-pin", PinStatus, {
      body: currentPin ? { pin, current_pin: currentPin } : { pin },
    }),

  /** Whether paying and getting paid in naira is offered, and to which banks. */
  rampInfo: () => request("GET", "/api/v1/ramp", RampInfo),

  /** The account holder's name, so people can check it before any money moves. */
  resolveAccount: (bank_code: string, account_number: string) =>
    request("POST", "/api/v1/ramp/resolve", ResolvedAccount, { body: { bank_code, account_number } }),

  rampQuote: (direction: "onramp" | "offramp", amount_sats: number) =>
    request("POST", "/api/v1/ramp/quote", RampQuote, { body: { direction, amount_sats } }),

  /** Pay an escrow by bank transfer. The account given is where a refund goes. */
  startOnramp: (id: string, bank_code: string, account_number: string) =>
    request("POST", `${esc(id)}/onramp`, RampOrder, { body: { bank_code, account_number } }),

  onramp: (id: string) => request("GET", `${esc(id)}/onramp`, EscrowRampOrder),

  /** Send part of your balance to your bank account, in naira. */
  startOfframp: (amount_sats: number, bank_code: string, account_number: string, pin: string) =>
    request("POST", "/api/v1/ramp/offramp", RampOrder, { body: { amount_sats, bank_code, account_number, pin } }),

  rampOrders: () => request("GET", "/api/v1/ramp/orders", RampOrderList),

  /** Demo provider only: pretend the bank transfer arrived. */
  demoDeposit: (orderId: string) =>
    request("POST", `/api/v1/ramp/orders/${encodeURIComponent(orderId)}/demo-deposit`, RampOrder),

  operatorQueue: (opts?: Pick<Options, "token" | "keepSessionOn401">) =>
    request("GET", "/api/v1/operator/escrows", EscrowList, opts),
};

/** The server's short lower-case messages, as a sentence. */
function sentence(m: string): string {
  return m.charAt(0).toUpperCase() + m.slice(1) + (/[.!?]$/.test(m) ? "" : ".");
}

/** A message suitable for showing to the user. */
export function describeError(err: unknown): string {
  if (!(err instanceof ApiError)) return "Something went wrong. Please try again.";
  switch (err.code) {
    case "arbiter_has_no_key":
      return "That user hasn't set up a key to act as an arbiter yet. Ask them to open the Arbiter page first.";
    case "unknown_arbiter":
      return "No user has that id, so they can't be the arbiter.";
    case "invalid_arbiter":
      return "The arbiter has to be someone other than the buyer and the seller.";
    case "invalid_key":
    case "invalid_decision":
      return err.message;
    case "fiat_disabled":
      return "This server doesn't show fiat values.";
    case "unsupported_currency":
      return err.message;
    case "rate_unavailable":
      return "The price feed is unavailable right now. Try again, or leave the fiat value out.";
    case "no_node":
      return "Connect your own Lightning node before creating an escrow.";
    case "node_in_use":
      return "You have escrows in progress on this node. Finish or cancel them before changing it.";
    case "node_check_failed":
    case "invalid_node":
      return err.message;
    case "node_unavailable":
      return "The Lightning node is temporarily unavailable. Nothing has changed; try again in a moment.";
    case "rate_limited":
      return `Too many requests. Wait ${err.retryAfter ?? "a few"} seconds and try again.`;
    case "unauthorized":
      return "Your session is no longer valid. Please sign in again.";
    case "not_found":
      return "This escrow doesn't exist, or it isn't yours.";
    case "wrong_preimage":
      return "That secret doesn't match this escrow. Nothing has changed.";
    case "invalid_preimage":
    case "invalid_hex":
      return "The secret must be exactly 64 hex characters.";
    case "insufficient_balance":
      return "Your balance doesn't cover that amount plus the routing-fee allowance. Try a smaller amount.";
    case "invalid_invoice":
    case "invalid_request":
      return sentence(err.message);
    case "response_window":
      return "The other side is still within the time they have to answer the dispute. You can decide when it ends, or as soon as they add their side.";
    case "pin_not_set":
      return "Set a withdrawal PIN first (on this page), then try again.";
    case "pin_required":
      return "Enter your withdrawal PIN.";
    case "wrong_pin":
    case "pin_locked":
    case "invalid_pin":
    case "withdrawal_limit":
      return sentence(err.message);
    case "duplicate_invoice":
      return "That invoice has already been used for a withdrawal. Make a new one.";
    case "payouts_unavailable":
      return "This server doesn't hold balances, so there is nothing to withdraw here.";
    case "ramp_unavailable":
      return "Paying in naira isn't offered on this server.";
    case "provider_unavailable":
      return "The bank payment provider is unavailable right now. Nothing has changed; try again shortly.";
    case "provider_rejected":
      return "The bank payment provider turned that down: " + err.message.replace(/^.*?: /, "");
    case "too_late":
      return "The deadline has passed, so this is no longer possible. The buyer will be refunded.";
    default:
      return err.message;
  }
}
