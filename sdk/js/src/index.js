import { randomUUID } from "node:crypto";

export { verifyWebhook, WebhookSignatureError } from "./webhooks.js";

/** An error answer from the API, or a failure to get one. */
export class ToshiError extends Error {
  /**
   * @param {object} d
   * @param {number} d.status the HTTP status, or 0 when no answer was received
   * @param {string} d.code the API's error code (`invalid_state`, `not_found`, ...), or `network` / `timeout`
   * @param {string} d.message
   * @param {string | null} [d.requestId] quote this when asking for help
   * @param {number | null} [d.retryAfter] seconds the server asked you to wait
   */
  constructor({ status, code, message, requestId = null, retryAfter = null }) {
    super(message);
    this.name = "ToshiError";
    this.status = status;
    this.code = code;
    this.requestId = requestId;
    this.retryAfter = retryAfter;
  }
}

const RETRY_STATUSES = new Set([429, 502, 503, 504]);
const MAX_WAIT_MS = 30_000;

const defaultSleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * A client for the toshi-way escrow API. Use an API key (`tk_...`) made on your Account page.
 *
 * Field names are the API's own (snake_case), so the reference in docs/api.md applies word for word.
 */
export class Toshi {
  #apiKey;
  #baseUrl;
  #fetch;
  #timeoutMs;
  #maxRetries;
  #sleep;

  /**
   * @param {object} options
   * @param {string} options.apiKey your API key
   * @param {string} options.baseUrl the server's address, for example `https://pay.example.com`
   * @param {typeof fetch} [options.fetch] a fetch to use instead of the global one
   * @param {number} [options.timeoutMs] per request, default 15000
   * @param {number} [options.maxRetries] extra tries for requests that are safe to repeat, default 2
   */
  constructor({ apiKey, baseUrl, fetch: fetchImpl = globalThis.fetch, timeoutMs = 15_000, maxRetries = 2, sleep = defaultSleep }) {
    if (!apiKey || typeof apiKey !== "string") throw new TypeError("apiKey is required");
    if (!baseUrl || typeof baseUrl !== "string") throw new TypeError("baseUrl is required");
    if (typeof fetchImpl !== "function") throw new TypeError("no fetch available: use Node 18 or newer, or pass one in");
    this.#apiKey = apiKey;
    this.#baseUrl = baseUrl.replace(/\/+$/, "");
    this.#fetch = fetchImpl;
    this.#timeoutMs = timeoutMs;
    this.#maxRetries = maxRetries;
    this.#sleep = sleep;

    this.escrows = {
      create: (params) => this.#createEscrow(params),
      get: (id) => this.#request("GET", `/api/v1/escrows/${encodeURIComponent(id)}`),
      list: () => this.#request("GET", "/api/v1/escrows"),
      events: (id) => this.#request("GET", `/api/v1/escrows/${encodeURIComponent(id)}/events`),
      note: (id, kind, text) =>
        this.#request("POST", `/api/v1/escrows/${encodeURIComponent(id)}/notes`, { body: { kind, text }, retry: false }),
      cancel: (id) => this.#request("POST", `/api/v1/escrows/${encodeURIComponent(id)}/cancel`, { retry: false }),
      waitFor: (id, until, options) => this.#waitFor(id, until, options),
    };
  }

  /** Who this key belongs to, and what it may do. A good first call to check a key works. */
  me() {
    return this.#request("GET", "/api/v1/me");
  }

  async #createEscrow(params) {
    if (!params || typeof params !== "object") throw new TypeError("create() needs an object");
    const { idempotencyKey, ...body } = params;
    if (!Number.isInteger(body.amount_sats) || body.amount_sats <= 0) throw new TypeError("amount_sats must be a whole number above zero");
    if (typeof body.description !== "string") throw new TypeError("description must be a string");
    if (!Number.isInteger(body.hold_secs)) throw new TypeError("hold_secs must be a whole number of seconds");
    // One key for every try of this call, so a retry after a lost answer cannot make a second escrow.
    // Pass your own (for example your order id) to make a repeat of the whole call safe too.
    return this.#request("POST", "/api/v1/escrows", { body, idempotencyKey: idempotencyKey ?? randomUUID(), retry: true });
  }

  async #waitFor(id, until, { timeoutMs = 60_000, intervalMs = 2_000 } = {}) {
    const done = typeof until === "function" ? until : (e) => [].concat(until).includes(e.state);
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const escrow = await this.escrows.get(id);
      if (done(escrow)) return escrow;
      if (Date.now() + intervalMs > deadline) {
        throw new ToshiError({ status: 0, code: "timeout", message: `the escrow did not reach the state in ${timeoutMs} ms (now ${escrow.state})` });
      }
      await this.#sleep(intervalMs);
    }
  }

  async #request(method, path, { body, idempotencyKey, retry = method === "GET" } = {}) {
    const headers = { Authorization: `Bearer ${this.#apiKey}`, Accept: "application/json" };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (idempotencyKey) headers["Idempotency-Key"] = idempotencyKey;

    for (let attempt = 0; ; attempt++) {
      let error;
      try {
        const res = await this.#send(method, path, headers, body);
        const text = await res.text();
        let json = null;
        try {
          json = text ? JSON.parse(text) : null;
        } catch {
          // handled below as an unexpected answer
        }
        if (res.ok) {
          if (json === null) throw new ToshiError({ status: res.status, code: "bad_response", message: "the server sent an answer that is not JSON" });
          return json;
        }
        const retryAfter = Number(res.headers.get("retry-after"));
        error = new ToshiError({
          status: res.status,
          code: json?.error?.code ?? `http_${res.status}`,
          message: json?.error?.message ?? `the server answered ${res.status}`,
          requestId: res.headers.get("x-request-id"),
          retryAfter: Number.isFinite(retryAfter) && retryAfter > 0 ? retryAfter : null,
        });
        if (!RETRY_STATUSES.has(res.status)) throw error;
      } catch (e) {
        if (e instanceof ToshiError && !RETRY_STATUSES.has(e.status) && e.code !== "network" && e.code !== "timeout") throw e;
        error = e instanceof ToshiError ? e : new ToshiError({ status: 0, code: "network", message: "could not reach the server" });
      }
      if (!retry || attempt >= this.#maxRetries) throw error;
      const wait = error.retryAfter ? error.retryAfter * 1000 : 500 * 2 ** attempt * (0.75 + Math.random() * 0.5);
      await this.#sleep(Math.min(wait, MAX_WAIT_MS));
    }
  }

  async #send(method, path, headers, body) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.#timeoutMs);
    try {
      return await this.#fetch(`${this.#baseUrl}${path}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: controller.signal,
      });
    } catch (e) {
      throw new ToshiError({
        status: 0,
        code: e?.name === "AbortError" ? "timeout" : "network",
        message: e?.name === "AbortError" ? `no answer within ${this.#timeoutMs} ms` : "could not reach the server",
      });
    } finally {
      clearTimeout(timer);
    }
  }
}
