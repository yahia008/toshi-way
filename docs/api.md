# toshi-way for businesses: API reference

Take payments through toshi-way from your own website or system. Your customer pays into an escrow: the money is **locked on the Lightning network, not sent**. You are paid when they hand over a secret (for example, a QR code shown to your courier at delivery), and they are refunded automatically if that never happens before the deadline. If you connect your own Lightning node, the money goes straight to you and nobody else ever holds it.

This page is the reference. For the JavaScript library, see [`sdk/js/README.md`](../sdk/js/README.md). Everything here runs today on the regtest test network; no real money is involved yet.

## How a payment works

```
 your server                       toshi-way                         your customer
     |                                 |                                    |
     |  1. create an escrow  --------> |                                    |
     |  <-- escrow + checkout_url      |                                    |
     |                                 |                                    |
     |  2. send the customer to checkout_url ---------------------------->  |
     |                                 |  3. opens the page, signs up if    |
     |                                 |     new, accepts and pays          |
     |  <-- webhook: escrow.accepted   |                                    |
     |  <-- webhook: escrow.locked     |  (money is locked, not yet yours)  |
     |                                 |                                    |
     |  4. you deliver the goods or service                                 |
     |                                 |  5. customer hands over the        |
     |                                 |     release QR; you submit it      |
     |  <-- webhook: escrow.settled    |     (or they release it themselves)|
     |        you are paid, on your own node                                |
```

If the customer never pays, or never releases, the escrow ends by itself at the deadline: `escrow.expired` (never paid) or `escrow.refunded` (paid, then refunded in full).

**The customer always pays on a toshi-way page, never inside your site.** The customer's release secret is made in their own browser and has to stay there. That is what keeps the money safe, and it is why this is a redirect and not an embedded widget.

## Quick start

1. **Make an API key.** Sign in to the web app, open **API** in the sidebar, and under **API keys** make a key with both scopes. Copy it: it is shown once.
2. **Make an offer** from your server:

```js
import { Toshi } from "@toshi-way/client";

const toshi = new Toshi({ apiKey: process.env.TOSHI_KEY, baseUrl: "https://pay.example.com" });

const escrow = await toshi.escrows.create({
  amount_sats: 25_000,
  description: "Parcel to Ikeja, deliver by Friday noon",
  hold_secs: 4 * 3600,                    // 60 seconds to 6 hours
  reference: "ORDER-1001",                // your own order number
  return_url: "https://shop.example.com/orders/1001/thanks",
  idempotencyKey: "order-1001",           // a repeat of this call makes no second escrow
});

// 3. Send your customer to escrow.checkout_url
```

4. **Add a webhook** (**API** in the sidebar, then **Webhooks**) so your server hears about every change. See [Webhooks](#webhooks).

## Authentication

Send your API key as a bearer token on every request:

```
Authorization: Bearer tk_...
```

A key is a narrow, revocable credential for your software. Make and revoke keys on the API page; the key is shown once and only its hash is stored. Each key has scopes:

| Scope | Lets the key |
|---|---|
| `escrows:read` | list escrows, read one, read its history |
| `escrows:write` | create escrows as the seller, and cancel them |

A key acts as the account that made it, so an escrow it creates has that account as the seller. **Keys cannot** change your node, your arbiter key, your API keys or webhooks, or anything a buyer or arbiter does (accepting, paying, releasing, disputing). A key is refused on those endpoints with `403 forbidden`. Revoking a key stops it on the very next call (`401`).

Treat a key like a password. Keep it in your server's settings, never in a web page, a mobile app or a repository. Note that **cancelling an escrow the buyer has already paid refunds the buyer**, so a leaked write key can undo your sales (it cannot take money for itself). Make a read-only key for anything that only needs to look.

## Conventions

- **Base URL:** the address of the server, for example `https://pay.example.com`. All paths start with `/api/v1`.
- **Format:** JSON in and out. Send `Content-Type: application/json` with a body. Bodies are limited to 16 KB.
- **Times** are unix seconds (UTC). **Amounts** are whole satoshis.
- **Errors** are `{"error": {"code": "...", "message": "..."}}` with a matching HTTP status. Branch on `code`, not on `message`. Every response carries an `X-Request-Id`; quote it when you ask for help.
- **Rate limits:** 120 requests a minute per key by default. Past it you get `429` with a `Retry-After` header (seconds). The JavaScript library waits and retries for you.
- **Idempotency:** send an `Idempotency-Key` header with `POST /escrows` (the library does). A repeat with the same key and the same body returns the first result instead of making a second escrow; the same key with a different body is `422 idempotency_key_reuse`. Use your order id so a retry of your whole job is safe. The server remembers keys in memory, so a server restart forgets them: after a restart, check for an existing escrow with your `reference` (list, then match) before creating again.

## The escrow

```json
{
  "id": "5b1f0c0e-9d1a-4c0e-8d52-0c3f4b9f2f10",
  "buyer_id": null,
  "seller_id": "u_3a9f1c2d4e5b6a70",
  "amount_sats": 25000,
  "description": "Parcel to Ikeja, deliver by Friday noon",
  "reference": "ORDER-1001",
  "return_url": "https://shop.example.com/orders/1001/thanks",
  "checkout_url": "https://pay.example.com/escrows/5b1f0c0e-9d1a-4c0e-8d52-0c3f4b9f2f10",
  "payment_hash": null,
  "state": "CREATED",
  "invoice_expires_at": null,
  "deadline_at": 1793100000,
  "created_at": 1793085600,
  "updated_at": 1793085600,
  "your_role": "seller",
  "seller_node": "own",
  "arbiter_id": null,
  "delivered_at": null,
  "fiat": null
}
```

| Field | Meaning |
|---|---|
| `buyer_id` | `null` for an open offer: the first person to open the link and accept becomes the buyer. Set it if you know which toshi-way user is paying. |
| `reference` | Your own order number (up to 100 characters), returned everywhere. |
| `return_url` | The link back to your site the customer is offered after accepting. |
| `checkout_url` | Where to send your customer. Present when the server knows its public web address. If it is missing, use `<web address>/escrows/<id>`. |
| `payment_hash` | Set once the buyer accepts. |
| `invoice_expires_at` | After accepting, the buyer has 10 minutes to pay the invoice. |
| `deadline_at` | The settle-by time: the escrow can be released until then; at it, a locked escrow is refunded. |
| `seller_node` | `own`: paid straight to your node. `shared`: the service's node, which then holds the money and owes it to you (custodial). |
| `delivered_at` | When the seller marked the escrow delivered (see [notes](#post-apiv1escrowsidnotes-scope-escrowswrite)), or `null`. A claim, not a payment: only the buyer's secret releases the money. |
| `fiat` | What the sum was worth in a currency when you made the offer. Display only. |

### States

| State | Meaning | You do |
|---|---|---|
| `CREATED` | The offer exists; nobody has accepted. | wait; cancel to withdraw it |
| `INVOICE_ISSUED` | The buyer accepted; waiting for payment. | wait |
| `LOCKED` | The buyer paid. The money is locked. | **deliver**, then collect the release |
| `SETTLED` | Released to you. You are paid. | fulfil the order |
| `REFUNDED` | Returned to the buyer. | cancel the order |
| `EXPIRED_UNPAID` | Never paid, or withdrawn. | cancel the order |
| `DISPUTED` | The buyer raised a dispute. If an arbiter was named they decide; otherwise the operator can cancel (refund). | wait |
| `FAILED` | Something went wrong in a way that needs the operator. | contact the operator |

Only `SETTLED` means you have been paid. Never treat `LOCKED` as paid: locked money can still be refunded.

## Endpoints

### `GET /api/v1/me`
Who the key belongs to and what it may do. A good first call to check a key works. Any valid key.

```json
{ "id": "u_3a9f1c2d4e5b6a70", "role": "user", "api_key": { "id": "key_...", "scopes": ["escrows:read", "escrows:write"] } }
```

### `POST /api/v1/escrows` (scope `escrows:write`)
Make an offer. Returns `201` and the escrow.

| Field | Required | Notes |
|---|---|---|
| `amount_sats` | yes | 1 up to the server's limit (1,000,000 by default) |
| `description` | yes | up to 500 characters. The first line is the title shown on the page; the rest is shown as terms. |
| `hold_secs` | yes | seconds until the deadline: 60 to 21,600 (6 hours) |
| `buyer_id` | no | a toshi-way user id; leave out for an open offer |
| `reference` | no | your order number, up to 100 characters, no control characters |
| `return_url` | no | `https://` only, a plain host, no `user@`, up to 500 characters (`http://localhost` is allowed for testing) |
| `arbiter_id` | no | a user both sides trust to decide disputes; they must have set up a key |
| `fiat_currency` | no | for example `USD`; shows an approximate value beside the sats |

Errors: `invalid_amount`, `invalid_description`, `invalid_deadline`, `invalid_reference`, `invalid_return_url`, `unknown_buyer`, `unknown_arbiter`, `arbiter_has_no_key`, `unsupported_currency`, `rate_unavailable`, `no_node` (you have no node connected and the server has no shared one), `idempotency_key_reuse`.

### `GET /api/v1/escrows/{id}` (scope `escrows:read`)
One escrow. Returns `404 not_found` for an escrow you have no part in, so ids cannot be probed.

### `GET /api/v1/escrows`  (scope `escrows:read`)
Your escrows, newest first: **every one still in play plus the newest 100 that have ended**, and `totals` over your whole history.

```json
{ "escrows": [ ... ], "totals": { "escrows": 412, "settled": { "count": 300, "received": 9100000, "paid": 0 }, "refunded": { "count": 41, "sats": 800000 } } }
```

To find an escrow by your order number, list and match on `reference`.

### `GET /api/v1/escrows/{id}/events` (scope `escrows:read`)
The escrow's history: `audit` (each change: `actor`, `action`, `from_state`, `to_state`, `reason`, `at`) and `node_events` (what the Lightning node reported, with its time).

### `POST /api/v1/escrows/{id}/cancel` (scope `escrows:write`)
Withdraw the offer. If the buyer has already paid, **this refunds them**. Not repeatable on its own: a second call returns `409 invalid_state` once it is over. Returns the escrow.

### `POST /api/v1/escrows/{id}/notes` (scope `escrows:write`)
Say what happened while the money is locked. It moves no money and does not extend the deadline; it goes on the escrow's history (`GET .../events`, action `delivered` or `evidence`) and shows on the escrow page, where the buyer and any arbiter read it.

```json
{ "kind": "delivered", "text": "Signed for by J. Okafor, tracking NG123" }
```

| Field | Notes |
|---|---|
| `kind` | `delivered`: the seller says the goods or work were delivered. Only the seller. `evidence`: either side, for whoever decides a dispute |
| `text` | 1 to 500 characters: a tracking number, a link, a short description |

Only while the escrow is `LOCKED` or `DISPUTED` and before its deadline (`409 invalid_state` or `409 too_late` otherwise). Each person can add 10 notes to one escrow. A buyer's note needs their own account, not an API key; a key acts as the seller. A note produces an `escrow.updated` webhook with the state unchanged. Returns the escrow.

**Marking delivered does not pay you.** Only the buyer's release secret does. If the buyer stays silent they are refunded at the deadline, so mark delivery early, and if there is an arbiter, open a dispute with your proof before the deadline so they can decide.

## Sending the customer to checkout

Redirect the customer to `checkout_url`. They land on the escrow page. A customer with no account is signed up on the spot (they choose a password and save a recovery token); one with an account signs in. They accept the offer, pay the Lightning invoice, and the page updates by itself when the payment is locked.

If you gave a `return_url`, the page shows a **"Return to your-shop.example.com"** button once the customer has accepted. Things to design around:

- It is a link the customer chooses to click, with your address spelled out. It is never followed automatically, and the page says you chose it.
- Nothing is added to it: no status, no id. **Do not treat someone arriving on that page as proof of payment.** Learn about payment from [webhooks](#webhooks) or by reading the escrow with your key.
- The customer may never come back (closed the tab, paid from their phone). Your order should wait for the webhook, not for the redirect.

## Webhooks

Webhooks tell your server the moment an escrow changes, so you do not have to poll. Add an address on the API page under **Webhooks** (or `POST /api/v1/me/webhooks`, with your account token, not an API key). The address must be `https://` and reachable from the internet. You get a **signing secret** (`whsec_...`) shown once. Use **Send test** to check the connection and your signature code before real events flow.

Only changes from the moment you add an address are sent, only for escrows you sell.

### The message

A `POST` with a JSON body:

```json
{
  "id": "evt_8841",
  "type": "escrow.locked",
  "created_at": 1793086100,
  "data": {
    "state": "LOCKED",
    "escrow": { "id": "5b1f0c0e-...", "reference": "ORDER-1001", "state": "LOCKED", "amount_sats": 25000 }
  }
}
```

| `type` | When |
|---|---|
| `escrow.accepted` | the buyer accepted; the invoice was issued |
| `escrow.locked` | the buyer paid; the money is locked |
| `escrow.settled` | released to you: **you have been paid** |
| `escrow.refunded` | refunded to the buyer |
| `escrow.expired` | ended without a payment |
| `escrow.disputed` | the buyer raised a dispute |
| `escrow.failed` | needs the operator |
| `escrow.updated` | something changed within a state (for example an arbiter confirmed) |
| `webhook.test` | from **Send test** |

`data.state` is what the escrow **became in this event**. `data.escrow` is the escrow **as it is now**, which can be newer if your server was slow or down. The message contains no invoice and no secret. Use it as a signed nudge: read the escrow with your key if you need to be certain before you act.

Headers: `Toshi-Signature`, `Toshi-Event-Id` (the same as `id`), `Toshi-Event-Type`, `Content-Type: application/json`.

### Check the signature

Anyone can post to your address, so check every message. `Toshi-Signature` is `t=<unix seconds>,v1=<hex>`, where `v1` is the HMAC-SHA256, keyed with your signing secret, of the text `<t>.<raw body>`.

1. Take the body **exactly as it arrived** (the raw bytes). Do not parse and re-serialise it first: the signature covers the original text.
2. Compute the HMAC and compare it with `v1` in constant time.
3. Reject the message if `t` is more than five minutes from your clock, so an old message cannot be replayed.
4. Only then trust and parse the JSON.

Node, with the library:

```js
import { verifyWebhook } from "@toshi-way/client";

// Express: use express.raw({ type: "application/json" }) on this route so req.body is the raw bytes.
app.post("/toshi-webhook", express.raw({ type: "application/json" }), (req, res) => {
  let event;
  try {
    event = verifyWebhook({ payload: req.body, signature: req.get("Toshi-Signature"), secret: process.env.TOSHI_WHSEC });
  } catch {
    return res.sendStatus(400);
  }
  // ... handle event (see below), then answer quickly with a 2xx
  res.sendStatus(200);
});
```

Python:

```python
import hmac, hashlib, time

def verify(raw_body: bytes, header: str, secret: str, tolerance: int = 300) -> None:
    parts = dict(p.split("=", 1) for p in header.split(",") if "=" in p)
    t, sig = parts.get("t"), parts.get("v1")
    if not t or not sig:
        raise ValueError("bad header")
    expected = hmac.new(secret.encode(), t.encode() + b"." + raw_body, hashlib.sha256).hexdigest()
    if not hmac.compare_digest(expected, sig):
        raise ValueError("bad signature")
    if abs(time.time() - int(t)) > tolerance:
        raise ValueError("too old")
```

PHP:

```php
function verify(string $raw, string $header, string $secret, int $tolerance = 300): void {
    parse_str(str_replace(',', '&', $header), $p);
    if (empty($p['t']) || empty($p['v1'])) throw new Exception('bad header');
    $expected = hash_hmac('sha256', $p['t'] . '.' . $raw, $secret);
    if (!hash_equals($expected, $p['v1'])) throw new Exception('bad signature');
    if (abs(time() - (int)$p['t']) > $tolerance) throw new Exception('too old');
}
```

To test your code, the signature of the body `{"id":"evt_1"}` at `t=1700000000` with the secret `whsec_test` is `c89214b5b5da833daed6f0b8c5bb6bd58cea9022bd80ccc78230f3942d632925`.

### Delivery, retries and ordering

- Any `2xx` answer means delivered. Answer quickly (within 10 seconds) and do the real work afterwards.
- Anything else (another status, no answer in 10 seconds, a refused connection) counts as a failure. We retry the **same event** after 10 seconds, 30 seconds, 2 minutes, 10 minutes, 30 minutes, then every hour. Later events wait behind it, so events always arrive in order and none are skipped.
- After **3 days** of failures the address is switched off and the API page says so. Delete it and add it again once your server is back.
- Redirects are not followed, and we only call public `https://` addresses.
- Delivery is **at least once**. The same event can arrive twice (for example if your `200` was lost). Use `Toshi-Event-Id` to ignore repeats.

A sound handler: verify the signature, check you have not handled `id` before, store it, answer `200`, then update the order. Move an order to paid only on `escrow.settled`.

## Error codes

| `code` | Status | Meaning |
|---|---|---|
| `unauthorized` | 401 | missing, wrong or revoked key |
| `forbidden` | 403 | the key lacks the scope, or the endpoint is closed to keys, or you may not do that |
| `not_found` | 404 | no such escrow (or one you have no part in) |
| `invalid_body`, `invalid_request` | 400 | the request could not be read or is not valid |
| `invalid_amount`, `invalid_description`, `invalid_deadline`, `invalid_reference`, `invalid_return_url` | 400 or 422 | a field is wrong; the message says which |
| `unknown_buyer`, `unknown_arbiter`, `arbiter_has_no_key`, `invalid_arbiter` | 400 | the named user is not usable |
| `unsupported_currency`, `rate_unavailable` | 400 or 503 | fiat display not available |
| `no_node` | 409 | you have no Lightning node connected and the server has no shared one |
| `invalid_state` | 409 | the escrow's state does not allow that, for example cancelling one already settled |
| `conflict` | 409 | the escrow changed at the same moment; repeat the call |
| `idempotency_key_reuse` | 422 | that `Idempotency-Key` was used with a different body |
| `payload_too_large` | 413 | the body is over 16 KB |
| `rate_limited` | 429 | slow down; see `Retry-After` |
| `node_unavailable`, `node_rejected` | 503, 502 | the Lightning node is down or refused; try again shortly |
| `internal` | 500 | our fault; quote the `X-Request-Id` |

## Before you go live

- Use a **read-only key** wherever you can, and keep keys out of anything a customer can see.
- **Connect your own node** (My node in the web app) so you are paid directly. With the shared node the service holds your money until you withdraw it.
- Fulfil an order on `escrow.settled`, not on `escrow.locked` or on the customer returning to your site.
- Verify every webhook signature, ignore repeated event ids, and answer fast.
- Make your create call idempotent with your order id as the key, and look for an existing escrow by `reference` after a server restart.
- Set a `hold_secs` long enough for your delivery, but remember it is capped at 6 hours today, and the customer has 10 minutes to pay after accepting.
- Everything described here is on the **regtest test network**. It has not been used with real money.

## What is not here yet

Buyer-side or arbiter webhooks, API keys that can be limited to one customer, listing escrows by your `reference` directly, and pages of more than 100 finished escrows. Tell us what you need next.
