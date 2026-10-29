# @toshi-way/client

A small JavaScript client for the toshi-way escrow API. It makes and reads escrows, and it checks webhook signatures. No dependencies; it uses the built-in `fetch` and `crypto`, so it needs **Node 18 or newer**.

The full API reference, including webhooks and error codes, is in [`docs/api.md`](../../docs/api.md). Field names here are the API's own (`amount_sats`, `hold_secs`, ...), so the reference applies word for word.

> Not published to npm yet. To use it in a project, pack it and install the file:
>
> ```bash
> cd sdk/js && npm pack                       # makes toshi-way-client-0.1.0.tgz
> cd your-project && npm install ../path/to/toshi-way-client-0.1.0.tgz
> ```
>
> or import it by path (`import { Toshi } from "./sdk/js/src/index.js"`), or copy `src/`. A complete Next.js shop
> is in [`examples/nextjs-shop`](examples/nextjs-shop).

## Make an offer

```js
import { Toshi } from "@toshi-way/client";

const toshi = new Toshi({ apiKey: process.env.TOSHI_KEY, baseUrl: "https://pay.example.com" });

const escrow = await toshi.escrows.create({
  amount_sats: 25_000,
  description: "Parcel to Ikeja, deliver by Friday noon",
  hold_secs: 4 * 3600,
  reference: "ORDER-1001",
  return_url: "https://shop.example.com/orders/1001/thanks",
  idempotencyKey: "order-1001",
});

console.log(escrow.checkout_url); // send your customer here
```

Make the API key on the web app's API page. Keep it on your server.

## The calls

| Call | Does | Scope |
|---|---|---|
| `toshi.me()` | who the key belongs to, and its scopes | any |
| `toshi.escrows.create(params)` | make an offer | `escrows:write` |
| `toshi.escrows.get(id)` | read one escrow | `escrows:read` |
| `toshi.escrows.list()` | everything in play plus the newest 100 finished, with `totals` | `escrows:read` |
| `toshi.escrows.events(id)` | the escrow's history | `escrows:read` |
| `toshi.escrows.note(id, kind, text)` | say it was delivered (`"delivered"`) or add evidence (`"evidence"`) while the money is locked. Moves no money | `escrows:write` |
| `toshi.escrows.cancel(id)` | withdraw the offer (refunds the buyer if they have paid) | `escrows:write` |
| `toshi.escrows.waitFor(id, states)` | poll until the escrow reaches a state. Fine for scripts and tests; use webhooks in production | `escrows:read` |

## Retries

Reads are retried on network errors, timeouts, `429` (waiting as long as `Retry-After` says) and `502/503/504`, up to `maxRetries` times (default 2). `create` is retried the same way, but only because it carries an idempotency key that stays the same on every try, so a lost answer can never make a second escrow. `cancel` is never repeated on its own. Anything else (a `400`, `403`, `404`, `409`...) is raised straight away.

## Errors

Every failure is a `ToshiError` with `status` (0 if no answer came), `code` (the API's error code, or `network` / `timeout`), `message`, `requestId` and `retryAfter`.

```js
import { ToshiError } from "@toshi-way/client";

try {
  await toshi.escrows.cancel(id);
} catch (e) {
  if (e instanceof ToshiError && e.code === "invalid_state") {
    // already settled or over: nothing to cancel
  } else throw e;
}
```

## Webhooks

```js
import { verifyWebhook } from "@toshi-way/client";

const event = verifyWebhook({
  payload: rawBody,                          // the body EXACTLY as received (a Buffer or string)
  signature: req.headers["toshi-signature"],
  secret: process.env.TOSHI_WHSEC,           // shown once when you added the address
});
```

`verifyWebhook` checks the signature in constant time and rejects messages more than 5 minutes old (`toleranceSecs` to change it). It throws a `WebhookSignatureError` for anything it cannot trust, and returns the parsed event otherwise. **Pass the raw body**: parsing and re-serialising it changes the text and breaks the signature. In Express use `express.raw({ type: "application/json" })` for that route.

[`examples/webhook-server.mjs`](examples/webhook-server.mjs) is a complete receiver: it verifies, ignores repeats, answers quickly, and reads the escrow from the API before marking an order paid. [`examples/checkout.mjs`](examples/checkout.mjs) makes an offer and prints the checkout link.

## Tests

```bash
cd sdk/js && npm test     # 18 tests, no server needed
```
