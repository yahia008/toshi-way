import assert from "node:assert/strict";
import { test } from "node:test";
import { Toshi, ToshiError } from "../src/index.js";

/** A fetch that answers from a script and records what it was asked. */
function fakeFetch(script) {
  const calls = [];
  const f = async (url, init) => {
    calls.push({ url, ...init, body: init.body ? JSON.parse(init.body) : undefined });
    const next = script.shift();
    if (next instanceof Error) throw next;
    const { status = 200, json, headers = {}, text } = next;
    return new Response(text ?? JSON.stringify(json), { status, headers });
  };
  f.calls = calls;
  return f;
}

const make = (script, extra = {}) => {
  const fetch = fakeFetch(script);
  const sleeps = [];
  const toshi = new Toshi({ apiKey: "tk_abc", baseUrl: "https://pay.example.com/", fetch, sleep: async (ms) => void sleeps.push(ms), ...extra });
  return { toshi, fetch, sleeps };
};

test("a call goes to the right address with the key, and the answer is returned as is", async () => {
  const { toshi, fetch } = make([{ json: { id: "e1", state: "CREATED" } }]);
  const e = await toshi.escrows.get("e/1");
  assert.deepEqual(e, { id: "e1", state: "CREATED" });
  assert.equal(fetch.calls[0].url, "https://pay.example.com/api/v1/escrows/e%2F1", "trailing slash dropped, id escaped");
  assert.equal(fetch.calls[0].method, "GET");
  assert.equal(fetch.calls[0].headers.Authorization, "Bearer tk_abc");
});

test("create sends the fields as given and an idempotency key that is the same on a retry", async () => {
  const { toshi, fetch, sleeps } = make([{ status: 503, json: { error: { code: "node_unavailable", message: "down" } } }, { status: 201, json: { id: "e1" } }]);
  const e = await toshi.escrows.create({ amount_sats: 5000, description: "Parcel", hold_secs: 3600, reference: "ORD-1", return_url: "https://shop.example.com/thanks" });
  assert.equal(e.id, "e1");
  assert.equal(fetch.calls.length, 2, "retried once after a 503");
  assert.deepEqual(fetch.calls[0].body, { amount_sats: 5000, description: "Parcel", hold_secs: 3600, reference: "ORD-1", return_url: "https://shop.example.com/thanks" });
  const key = fetch.calls[0].headers["Idempotency-Key"];
  assert.match(key, /^[0-9a-f-]{36}$/);
  assert.equal(fetch.calls[1].headers["Idempotency-Key"], key, "the same key on the retry, so no second escrow");
  assert.equal(sleeps.length, 1);
});

test("your own idempotency key is used, and is not sent in the body", async () => {
  const { toshi, fetch } = make([{ status: 201, json: { id: "e1" } }]);
  await toshi.escrows.create({ amount_sats: 1, description: "", hold_secs: 60, idempotencyKey: "order-1001" });
  assert.equal(fetch.calls[0].headers["Idempotency-Key"], "order-1001");
  assert.equal("idempotencyKey" in fetch.calls[0].body, false);
});

test("create refuses obviously wrong input before calling the server", async () => {
  const { toshi, fetch } = make([]);
  await assert.rejects(() => toshi.escrows.create({ amount_sats: 0, description: "x", hold_secs: 60 }), TypeError);
  await assert.rejects(() => toshi.escrows.create({ amount_sats: 1.5, description: "x", hold_secs: 60 }), TypeError);
  await assert.rejects(() => toshi.escrows.create({ amount_sats: 1, hold_secs: 60 }), TypeError);
  await assert.rejects(() => toshi.escrows.create({ amount_sats: 1, description: "x" }), TypeError);
  assert.equal(fetch.calls.length, 0);
});

test("an error answer becomes a ToshiError with the code, status and request id", async () => {
  const { toshi } = make([{ status: 409, json: { error: { code: "invalid_state", message: "cannot cancel an escrow in state SETTLED" } }, headers: { "x-request-id": "req-9" } }]);
  await assert.rejects(
    () => toshi.escrows.cancel("e1"),
    (e) => e instanceof ToshiError && e.status === 409 && e.code === "invalid_state" && e.requestId === "req-9" && /SETTLED/.test(e.message),
  );
});

test("client errors are not retried, but rate limits are, honouring Retry-After", async () => {
  const bad = make([{ status: 400, json: { error: { code: "invalid_amount", message: "no" } } }]);
  await assert.rejects(() => bad.toshi.escrows.get("e1"), (e) => e.code === "invalid_amount");
  assert.equal(bad.fetch.calls.length, 1);

  const limited = make([{ status: 429, json: { error: { code: "rate_limited", message: "slow" } }, headers: { "retry-after": "7" } }, { json: { id: "e1" } }]);
  assert.equal((await limited.toshi.escrows.get("e1")).id, "e1");
  assert.deepEqual(limited.sleeps, [7000]);
});

test("a network failure is retried for reads, and gives up after maxRetries", async () => {
  const boom = () => new TypeError("fetch failed");
  const ok = make([boom(), { json: { id: "e1" } }]);
  assert.equal((await ok.toshi.escrows.get("e1")).id, "e1");

  const dead = make([boom(), boom(), boom()]);
  await assert.rejects(() => dead.toshi.escrows.get("e1"), (e) => e.code === "network" && e.status === 0);
  assert.equal(dead.fetch.calls.length, 3, "one try and two retries");
});

test("cancel is never repeated on its own", async () => {
  const { toshi, fetch } = make([{ status: 503, json: { error: { code: "node_unavailable", message: "down" } } }]);
  await assert.rejects(() => toshi.escrows.cancel("e1"), (e) => e.status === 503);
  assert.equal(fetch.calls.length, 1);
});

test("an answer that is not JSON is reported, not crashed on", async () => {
  const { toshi } = make([{ text: "<html>oops</html>" }]);
  await assert.rejects(() => toshi.escrows.get("e1"), (e) => e.code === "bad_response");
  const html = make([{ status: 502, text: "bad gateway" }, { status: 502, text: "bad gateway" }, { status: 502, text: "bad gateway" }]);
  await assert.rejects(() => html.toshi.escrows.get("e1"), (e) => e.status === 502 && e.code === "http_502");
});

test("a request that gets no answer in time is a timeout", async () => {
  const hang = (url, init) => new Promise((_, reject) => init.signal.addEventListener("abort", () => reject(Object.assign(new Error("aborted"), { name: "AbortError" }))));
  const toshi = new Toshi({ apiKey: "tk_a", baseUrl: "https://x.example.com", fetch: hang, timeoutMs: 20, maxRetries: 0 });
  await assert.rejects(() => toshi.me(), (e) => e.code === "timeout");
});

test("waitFor polls until the escrow reaches a state, and times out otherwise", async () => {
  const states = ["INVOICE_ISSUED", "INVOICE_ISSUED", "LOCKED"].map((state) => ({ json: { id: "e1", state } }));
  const { toshi, fetch } = make(states);
  const e = await toshi.escrows.waitFor("e1", ["LOCKED", "SETTLED"], { intervalMs: 1, timeoutMs: 5000 });
  assert.equal(e.state, "LOCKED");
  assert.equal(fetch.calls.length, 3);

  const always = async () => new Response(JSON.stringify({ id: "e1", state: "INVOICE_ISSUED" }), { status: 200 });
  const stuck = new Toshi({ apiKey: "tk_a", baseUrl: "https://x.example.com", fetch: always });
  await assert.rejects(() => stuck.escrows.waitFor("e1", "LOCKED", { intervalMs: 20, timeoutMs: 60 }), (e) => e.code === "timeout" && /INVOICE_ISSUED/.test(e.message));
});

test("the constructor checks what it needs", () => {
  assert.throws(() => new Toshi({ baseUrl: "https://x" }), TypeError);
  assert.throws(() => new Toshi({ apiKey: "tk_a" }), TypeError);
  assert.throws(() => new Toshi({ apiKey: "tk_a", baseUrl: "https://x", fetch: null }), TypeError);
});

test("note posts the kind and text and is never repeated on its own", async () => {
  const { toshi, fetch } = make([{ status: 503, json: { error: { code: "node_unavailable", message: "down" } } }]);
  await assert.rejects(() => toshi.escrows.note("e1", "delivered", "tracking NG123"), (e) => e.status === 503);
  assert.equal(fetch.calls.length, 1);
  assert.equal(fetch.calls[0].url, "https://pay.example.com/api/v1/escrows/e1/notes");
  assert.equal(fetch.calls[0].method, "POST");
  assert.deepEqual(fetch.calls[0].body, { kind: "delivered", text: "tracking NG123" });
});
