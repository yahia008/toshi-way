export type EscrowState =
  | "CREATED"
  | "INVOICE_ISSUED"
  | "LOCKED"
  | "SETTLED"
  | "REFUNDED"
  | "EXPIRED_UNPAID"
  | "DISPUTED"
  | "FAILED";

/** An escrow as the API shows it. Times are unix seconds. */
export interface Escrow {
  id: string;
  /** `null` for an open offer nobody has accepted yet. */
  buyer_id: string | null;
  seller_id: string;
  amount_sats: number;
  description: string;
  /** Your own order number, if you set one. */
  reference: string | null;
  /** The link back you offered the buyer, if any. */
  return_url: string | null;
  /** Where to send your customer. Present when the server knows its web address. */
  checkout_url?: string;
  payment_hash: string | null;
  state: EscrowState;
  invoice_expires_at: number | null;
  deadline_at: number;
  created_at: number;
  updated_at: number;
  your_role: "buyer" | "seller" | "arbiter" | "operator";
  /** `own`: paid straight to the seller's node. `shared`: the service's node (custodial). */
  seller_node: "own" | "shared";
  arbiter_id: string | null;
  arbiter_key: string | null;
  arbiter_fingerprint: string | null;
  arbiter_sealed: boolean;
  arbiter_confirmed_at: number | null;
  /** When the seller said it was delivered (a claim: only the buyer's secret releases the money), or `null`. */
  delivered_at: number | null;
  /** What the sum was worth when the offer was made. Display only. */
  fiat: {
    currency: string;
    exponent: number;
    amount_minor: number;
    rate_minor_per_btc: number;
    quoted_at: number;
    source: string;
  } | null;
}

export interface Totals {
  escrows: number;
  settled: { count: number; received: number; paid: number };
  refunded: { count: number; sats: number };
}

export interface EscrowList {
  escrows: Escrow[];
  totals: Totals;
}

export interface EscrowHistory {
  audit: { actor: string; action: string; from_state: EscrowState; to_state: EscrowState; reason: string; at: number }[];
  node_events: { node_invoice_state: string; at: number }[];
}

export interface CreateEscrowParams {
  amount_sats: number;
  description: string;
  /** Seconds from now until the settle-by deadline. */
  hold_secs: number;
  /** Omit for an open offer: the first person to accept becomes the buyer. */
  buyer_id?: string;
  /** Your own order number (up to 100 characters). */
  reference?: string;
  /** A link back to your site, shown to the buyer after they accept. `https://` only. */
  return_url?: string;
  arbiter_id?: string;
  /** Show the amount in this currency too (display only), for example `USD`. */
  fiat_currency?: string;
  /** Makes a repeat of this call safe. Defaults to a random value, reused across retries. */
  idempotencyKey?: string;
}

export interface ToshiOptions {
  apiKey: string;
  baseUrl: string;
  fetch?: typeof fetch;
  /** Per request, in milliseconds. Default 15000. */
  timeoutMs?: number;
  /** Extra tries for requests that are safe to repeat. Default 2. */
  maxRetries?: number;
  sleep?: (ms: number) => Promise<void>;
}

export class ToshiError extends Error {
  constructor(d: { status: number; code: string; message: string; requestId?: string | null; retryAfter?: number | null });
  /** The HTTP status, or 0 when no answer was received. */
  status: number;
  /** The API's error code (`invalid_state`, `not_found`, ...), or `network` / `timeout`. */
  code: string;
  requestId: string | null;
  retryAfter: number | null;
}

export class Toshi {
  constructor(options: ToshiOptions);
  me(): Promise<{ id: string; role: string; api_key?: { id: string; scopes: ("escrows:read" | "escrows:write")[] } }>;
  escrows: {
    create(params: CreateEscrowParams): Promise<Escrow>;
    get(id: string): Promise<Escrow>;
    list(): Promise<EscrowList>;
    events(id: string): Promise<EscrowHistory>;
    /** Say it was delivered (`delivered`, seller only) or add evidence (`evidence`) on a locked or disputed escrow. Moves no money. */
    note(id: string, kind: "delivered" | "evidence", text: string): Promise<Escrow>;
    /** Cancel the offer. If the buyer has already paid, this refunds them. Only the seller (or a key with `escrows:write`) can. */
    cancel(id: string): Promise<Escrow>;
    /** Poll until the escrow is in one of the states (or `until(escrow)` is true). Webhooks are better in production. */
    waitFor(
      id: string,
      until: EscrowState | EscrowState[] | ((escrow: Escrow) => boolean),
      options?: { timeoutMs?: number; intervalMs?: number },
    ): Promise<Escrow>;
  };
}

export type WebhookEventType =
  | "escrow.accepted"
  | "escrow.locked"
  | "escrow.settled"
  | "escrow.refunded"
  | "escrow.expired"
  | "escrow.disputed"
  | "escrow.failed"
  | "escrow.updated"
  | "webhook.test";

export interface WebhookEvent {
  /** Use this to ignore an event you have already handled. */
  id: string;
  type: WebhookEventType;
  created_at: number;
  data: {
    /** What the escrow became in this event. */
    state?: EscrowState;
    /** The escrow as it is now, which may be newer than `state`. */
    escrow?: Escrow;
    message?: string;
  };
}

export class WebhookSignatureError extends Error {
  constructor(reason: string);
  reason: string;
}

export function verifyWebhook(args: {
  /** The request body exactly as it arrived. */
  payload: string | Uint8Array;
  /** The `Toshi-Signature` header. */
  signature: string | null | undefined;
  /** Your endpoint's signing secret (`whsec_...`). */
  secret: string;
  /** How old a message may be, in seconds. Default 300. */
  toleranceSecs?: number;
  now?: number;
}): WebhookEvent;
