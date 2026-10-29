# toshi-way

Lightning HODL-invoice escrow. Two parties who don't trust each other transact over Bitcoin Lightning, with the payment held in escrow until a condition is met. Funds are released to the seller when the buyer (or recipient) reveals a secret, and refunded automatically if that doesn't happen before a deadline.

**Principle:** prove one real lock → condition → settle-or-refund cycle on Lightning before building anything else.

## How it works

A HODL invoice lets the receiving node accept a locked payment (HTLC) without revealing the preimage `P`. While it waits, the funds are committed but not spent. The node can then:

- **Settle** by revealing `P`, which pays the seller, or
- **Cancel**, which fails the HTLCs and refunds the buyer.

If the node does nothing before HTLC expiry, the lock times out and the buyer is refunded. That window is the escrow.

### Mode A: preimage release (MVP, non-custodial)

1. Buyer's client generates a random 32-byte secret `P` and `H = sha256(P)`. Only `H` is shared.
2. Seller's node creates a hold invoice for `H` and the agreed amount.
3. Buyer pays. The payment locks and shows as accepted-but-unsettled.
4. Seller sees funds are committed and does the job.
5. Buyer or recipient hands `P` to the seller (QR or long token).
6. Seller's node settles with `P`. If `P` never arrives, the invoice is cancelled or times out and the buyer is refunded.

`P` must be a full 256-bit value, never a short PIN, or the seller could brute-force it from `H`.

### Mode B: coordinator-held release (stretch)

The service runs the node and holds `P`, settling or cancelling based on a verified condition. Easier for users without a node, but custodial, so it must be labelled as such.

## Goals (MVP)

- Create an escrow with amount (sats), deadline and description.
- Lock the buyer's payment with a real hold invoice on regtest.
- Settle on correct `P`; refund automatically after the deadline.
- Drive escrow state from node events only, never from client claims.
- Web UI showing lock, countdown, release action and outcome.
- Demo both the happy path and the timeout-refund path.

## Non-goals (MVP)

Custody beyond what the node holds in flight and the shared node's balances (see below), a real naira payment provider (the ramp is built against an interface with a labelled demo provider, see below), custom routing, holds longer than a day or two, custodial wallets as the seller side, tokens, native mobile apps, and proof that the real-world work happened.

## Escrow state machine

States: `CREATED`, `INVOICE_ISSUED`, `LOCKED`, `SETTLED`, `REFUNDED`, `EXPIRED_UNPAID`, `DISPUTED`, `FAILED`.

| From | To |
|------|----|
| CREATED | INVOICE_ISSUED, EXPIRED_UNPAID (offer withdrawn, declined or lapsed) |
| INVOICE_ISSUED | LOCKED, EXPIRED_UNPAID |
| LOCKED | SETTLED, REFUNDED, DISPUTED |
| DISPUTED | REFUNDED, SETTLED (operator, within safe window) |
| any | FAILED (unrecoverable error) |

`SETTLED` and `REFUNDED` are terminal. If escrow state and node state disagree, the node is authoritative.

## Architecture

- **Backend:** Rust, Axum, Tokio.
- **Node:** LND over gRPC (invoices and router services), behind a `LightningAdapter` trait so core logic is testable with a mock. A `NodeDirectory` picks the node for each seller: their own, or (if the operator allows it) the service's shared node.
- **Database:** SQLx behind the `Store` trait (escrows, node events, audit log, users): SQLite by default (`escrow-sqlite`) or PostgreSQL (`escrow-postgres`, selected by `DATABASE_URL`). Both have versioned migrations and pass the same store contract tests.
- **Frontend:** Next.js, TypeScript, Tailwind, Zod.
- **Modules:** web client, API layer, escrow core (state machine, idempotency), Lightning adapter, persistence, deadline scheduler.

Constraints: all node calls have timeouts; invoice subscriptions reconnect and reconcile on startup; state changes are transactional; restricted macaroons kept out of source control; `P` is never stored or logged.

## Timeouts and safety

Timeouts are the hardest part. The service enforces its own settle-by time well before real HTLC expiry (and before LND's hold-expiry margin), unpaid invoices expire in minutes, final CLTV delta is set deliberately, and creation rejects deadlines beyond a configured maximum hold. Exact values must be tested on regtest and recorded here.

Measured on regtest: LND auto-cancels a held HTLC 12 blocks before it expires (`invoices.holdexpirydelta`), and the payer adds about 3 blocks to the invoice's final CLTV delta. Usable hold window is therefore about `final_cltv_delta + 3 - 12` blocks, roughly 71 blocks (about 12 hours) with the default delta of 80. The service's settle-by must be earlier than `expiry_height - 12`.

## Whose node, and who holds the money

Each escrow records which node holds its invoice (`seller_node` in the API):

- **`own`** (non-custodial). The seller connected their own LND node. The hold invoice is created on it, so a release pays the seller directly and the service never holds the sats. Sellers supply the node's gRPC address, its TLS certificate and an invoice-only macaroon (`invoices:read invoices:write info:read`). Saving runs a check (connect, synced, create and cancel a probe invoice) and stores nothing if it fails. The macaroon is encrypted at rest (AES-256-GCM under `NODE_CREDENTIAL_KEY`), the connection is pinned to the supplied certificate, and the address must be public (no loopback, private, link-local or metadata addresses) unless `ALLOW_PRIVATE_NODE_ADDRESSES=1` for regtest.
- **`shared`** (custodial after release). The service's own node, used for sellers who have none when `SHARED_NODE` is on (the default). The operator receives the sats and owes them to the seller. The UI says so. Set `SHARED_NODE=off` so every seller must bring their own and the service can never receive anyone's money.

The kind is fixed when the escrow is created, so changing node settings cannot strand an invoice. A seller cannot change or remove their node while an escrow with an invoice is in flight on it.

## Balances and withdrawals

A seller paid on the **shared** node has a balance: the operator received those sats and owes them. It is never stored, only derived: settled shared-node escrows where the person is the seller, minus their withdrawals. A seller with their own node was paid straight to it and has nothing held here (the Balance page says so, and still shows anything from before they connected it).

- **Withdraw any time, to any Lightning wallet.** The seller pastes an invoice from their wallet (`POST /api/v1/me/withdrawals {invoice}`); the invoice sets the amount. The shared node pays it with a routing-fee limit of 1% (at least 10 sats). While a payment is in flight its amount plus that limit is held; once it succeeds only the fee it actually cost is kept, and a failed one gives everything back.
- **No double spending.** The balance check and the withdrawal record are one atomic step (a single conditional insert on SQLite, an advisory lock per user on Postgres), and an invoice can be used once, ever: even after a failed withdrawal it is not tried again, since a payment the node lost track of may still complete (the seller makes a new invoice). Tested with ten withdrawals racing at once on all three stores.
- **Only the node decides how a payment ended.** A background loop asks the node about every pending withdrawal; one the node has no record of after a minute is treated as failed. Paying needs a second macaroon on the shared node (`PAYOUT_MACAROON`: `offchain:read offchain:write invoices:read invoices:write info:read`); without it there are no withdrawals and the balance is read-only.
- **Slowing down theft and cash-outs.** Every withdrawal (Lightning or bank) needs a 6-digit **withdrawal PIN**, set on the Balance page and kept only as an Argon2 hash; five wrong guesses lock it for 15 minutes, and changing it needs the current one. A stolen account token is therefore not enough to empty a balance. Sats from an escrow **paid in naira** stay "maturing" for `NAIRA_HOLD_HOURS` (default 24) after release, the time a bank transfer can still be reversed, so money paid from a stolen account can't be cashed out over Lightning at once. And a seller can withdraw at most `WITHDRAW_LIMIT_SATS` (default 1,000,000) in any 24 hours. All three are checked in the same atomic step as the balance.
- `GET /api/v1/me/balance` gives `available_sats`, `earned_sats`, `held_sats` and `withdrawn_sats`; `GET /api/v1/me/withdrawals[/{id}]` lists them.

## Paying and getting paid in naira

For people without a Lightning wallet, a payment provider sits between Nigerian bank transfers and Lightning. The escrow itself is unchanged: it is still a hold invoice in sats, and the provider simply pays or receives it.

- **On-ramp (buyer pays in naira).** After accepting an offer, the buyer picks their bank and account number (the holder's name is looked up so they can check it) and gets transfer details: an account, the exact amount and a reference, valid until shortly before the invoice expires. When the transfer arrives the provider pays the escrow's invoice, and the escrow locks as usual. If the seller isn't paid in the end (refund, expiry, cancellation), the full naira amount goes back to the account the transfer actually came from, as reported by the provider, never to an account the buyer merely typed (otherwise money sent from a stolen account could be refunded clean into another). If no transfer arrives in time, nothing happens.
- **Naira or sats, both ways.** A seller can price an escrow in naira: the app converts it to sats at the current rate (shown before creating) and records the naira as the escrow's fiat value; the escrow itself is in sats, fixed from then on. A buyer whose escrow is waiting for payment picks "Pay in sats" (the Lightning invoice) or "Pay in naira" (bank transfer), each showing its amount, whatever the escrow was priced in.
- **Off-ramp (seller withdraws to a bank).** From the Balance page: a bank, an account number and an amount in sats. The shared node pays the provider's invoice out of the seller's balance (a normal withdrawal, with the same rules), then the provider transfers naira to the account.
- **Price.** The server's rate source (`FIAT_PROVIDER`) prices NGN; the provider fee (`RAMP_FEE_BPS`, default 1.5%) is added to what a buyer pays and taken from what a seller receives, and every order stores its rate, amount and fee. Smallest conversion 1,000 sats.
- **Provider.** `RampProvider` is a trait: banks, account-name lookup, deposit details, paying and receiving Lightning, and sending naira. `RAMP_PROVIDER=demo` is the only one built, and the server refuses to start it unless its node is on regtest: its Lightning side is real (a regtest node), its bank side is pretend ("Demo Bank (no real money)"; the buyer presses "I've sent the transfer"), and every screen says so. A real provider is a new implementation of the trait plus a webhook for incoming transfers.
- Endpoints: `GET /api/v1/ramp` (terms and banks), `POST /api/v1/ramp/resolve`, `POST /api/v1/ramp/quote`, `POST|GET /api/v1/escrows/{id}/onramp`, `POST /api/v1/ramp/offramp`, `GET /api/v1/ramp/orders[/{id}]`, and for the demo only `POST /api/v1/ramp/orders/{id}/demo-deposit` (the buyer) and `POST /api/v1/operator/escrows/{id}/demo-onramp` (the operator, on the buyer's behalf). `scripts/pay-naira.sh <invoice>` uses the latter to fund an escrow in naira from its invoice, as the demo wallet does in sats.

## Fiat display

An optional, **display-only** fiat value (PRD FR-16) so people can see roughly what a sum is worth in their own currency. There is no conversion: the escrow is always in sats, and nothing in the escrow logic reads the fiat value.

- **Locked at creation.** When the seller picks a currency, the server takes a rate once and stores the currency, the rate, the computed amount, when it was taken and where it came from. It never changes afterwards, whatever bitcoin does (tested: a halved price and a dead feed leave an existing escrow's value untouched, even through settlement). The UI shows it as "≈ ₦4,875.00 when offered, at ₦97,500,000.00 per bitcoin, <source>, display only".
- **Integer money.** The rate is stored as minor units per bitcoin (cents, kobo, ...) and amounts in the currency's minor unit, rounded half up in 128-bit integers; the web preview mirrors the same arithmetic. Supported: USD, EUR, GBP, CAD, AUD, CHF, JPY, NGN, KES, GHS, ZAR, INR, BRL.
- **The server fetches rates, the browser never does**, so the page still makes no third-party calls. `FIAT_PROVIDER` selects the source: `off` (the default), `coingecko` (all currencies above, including naira), `mempool` (mempool.space, 7 currencies, no African currencies), or `fixed:USD=65000,NGN=97500000` (demo rates, labelled "demo rate", for regtest where there is no market). Live rates are cached for a minute, a stale one is used for up to ten minutes if the feed is down, and then it is unavailable.
- **Failure is explicit.** If a seller asks for a value and the feed is down, creation fails with `rate_unavailable` rather than quietly dropping it; leaving the value out always works. A zero or absurd rate is treated as a broken feed.
- **Honest caveat.** This is a snapshot at the moment of the offer. Over a multi-hour hold the price will move, and the page says so.

## Arbitration

A seller can name an **arbiter** when creating an escrow: a user both sides trust to decide a dispute. Without one, behaviour is unchanged (a dispute is flagged and the operator can cancel).

- **Why it needs cryptography.** A refund only needs a cancel, but a release needs the buyer's secret `P`, which only the buyer's browser has. So when the buyer accepts, their browser encrypts `P` to the arbiter's public key (ECDH P-256 + HKDF-SHA-256 + AES-256-GCM, bound to that key) and uploads only the ciphertext. The server stores it but cannot read it; only the arbiter's browser can.
- **Setting up.** An arbiter opens the Arbiter page, which makes a key pair in their browser, registers the public half, and keeps the private half on their device (with backup and restore). The key's fingerprint is shown on every escrow that names them so both sides can check it with the arbiter out of band. The key is recorded on the escrow at creation, so it cannot be swapped afterwards.
- **Confirmation.** The server cannot check the sealed blob, and a buyer could upload junk. So before relying on arbitration the arbiter opens the blob, checks `sha256(P)` equals the payment hash, and confirms. The seller sees "not confirmed yet" until then.
- **Deciding.** Only the named arbiter, only while the escrow is `DISPUTED`, and only before the deadline (a hold cannot be extended). *Release* sends the opened `P`, checked against the hash like any release; *refund* cancels the hold. The decision and the arbiter's note go on the audit log before the node is touched, and a rejected attempt leaves no trace. Once an arbitrated escrow is disputed the operator can no longer cancel it. **The other side's time to answer:** the side that did not open the dispute gets a quarter of the time left until the deadline (at most an hour) before the arbiter may decide, ended early by any note they add; deciding sooner is `409 response_window`, and `GET .../events` carries `decide_after` while the wait lasts.
- **Limits, stated plainly.** The arbiter is trusted by both sides, which is what an arbiter is: they hold the means to release (they can open `P`) and could leak it. The server can still relay a swapped key if the parties never compare fingerprints, though the arbiter's confirmation would then fail. If nobody decides before the deadline the buyer is refunded. Arbiters are not paid or ranked by this service. The server does not validate that a registered key is a real curve point; a bad one only stops the arbiter's own escrows.

## Trust model

In Mode A nobody holds the funds, but the seller's node sees `P` at settlement and the coordinator is trusted to relay state correctly. Revealing `P` shows the buyer chose to release, not that the work was good. The buyer holds the power to withhold `P`, so the strongest use case is delivery, where the recipient must be present at handoff.

## Planned API

| Method | Endpoint | Purpose |
|--------|----------|---------|
| POST | `/api/v1/escrows` | Create an escrow offer (`buyer_id`, `arbiter_id` and `fiat_currency` are optional; omit the buyer for an open offer) |
| POST | `/api/v1/escrows/:id/accept` | Buyer accepts and submits `H` (and `sealed_secret` when there is an arbiter) |
| GET | `/api/v1/escrows/:id` | Details and state |
| GET | `/api/v1/escrows/:id/invoice` | Hold invoice to pay |
| POST | `/api/v1/escrows/:id/release` | Submit `P` to settle |
| POST | `/api/v1/escrows/:id/cancel` | Request cancellation |
| POST | `/api/v1/escrows/:id/dispute` | Flag a dispute |
| GET | `/api/v1/escrows/:id/events` | Audit and event history |
| GET | `/api/v1/me` | The caller's `{id, role}` |
| GET | `/api/v1/rates` | Whether fiat values are on, the price source, and the offered currencies |
| GET | `/api/v1/rates/:currency` | The current price of one bitcoin in a currency, to preview a value |
| GET / PUT | `/api/v1/me/arbiter-key` | Read or register the public key buyers encrypt release secrets to (arbiters) |
| GET | `/api/v1/escrows/:id/arbitration` | The arbiter fetches the sealed secret and hash to judge |
| POST | `/api/v1/escrows/:id/arbitration/confirm` | The arbiter confirms they can open it |
| POST | `/api/v1/escrows/:id/arbitration/decide` | The arbiter releases (with the opened secret) or refunds a disputed escrow |
| GET / PUT / DELETE | `/api/v1/me/node` | Read, connect (address, TLS cert, invoice macaroon) or remove the seller's own Lightning node |
| GET | `/health` | Health, including node connectivity and `server_time` |

## Milestones

1. **Prove the primitive.** From a script or minimal Rust program: create a hold invoice, pay it, observe accepted, settle; repeat with cancel. No UI until this works.
2. **Escrow core.** State machine, schema, idempotency, event-driven transitions, deadline scheduler.
3. **API.** Endpoints, auth, audit log, restart reconciliation.
4. **Frontend.** Buyer and seller views, QR release, outcome display.
5. **Hardening and demo.** Failure tests, timeout tuning, docs, rehearsed demo.

## Demo script

The full script (three scenes, talk track, limits and troubleshooting) is in [`demo.md`](demo.md), and `./scripts/demo-env.sh up` builds the environment it needs. The lead use case is a **courier delivery**. In outline:

- **Happy path:** seller creates an offer, buyer accepts and pays, UI shows funds locked, buyer shows the release QR, seller scans it, payment settles.
- **Refund path:** a second escrow with a short deadline expires and refunds automatically with no human action. This proves it is a real escrow.

## Local regtest network

Defined in `regtest/`, run with Docker Compose (no Polar needed).

```bash
./regtest/setup.sh
```

This starts `bitcoind` plus four LND v0.18.5 nodes, funds them, and opens channels `buyer → router → seller` and `router → seller2` (`seller2` plays a second seller with their own node). It is safe to re-run. Credentials are exported to `regtest/creds/<node>/` (`tls.cert`, `admin.macaroon`), which is gitignored.

| Node | gRPC | REST |
|------|------|------|
| buyer | `localhost:10001` | `localhost:8081` |
| router | `localhost:10002` | `localhost:8082` |
| seller | `localhost:10003` | `localhost:8083` |
| seller2 | `localhost:10004` | `localhost:8084` |

Useful commands:

```bash
docker compose -f regtest/docker-compose.yml exec seller lncli --network=regtest getinfo
docker compose -f regtest/docker-compose.yml down        # stop, keep state
docker compose -f regtest/docker-compose.yml down -v     # stop and wipe the network
```

`admin.macaroon` is for local Phase 1 work only. `setup.sh` also bakes `toshi.macaroon` for the seller node, limited to `invoices:read`, `invoices:write` and `info:read`; `toshi-server` and the seller adapter in the tests use it.

## Status

- [x] PRD drafted
- [x] Regtest network: 3 LND nodes, channels open, routed payment verified, `addholdinvoice` verified
- [x] Phase 1 Rust program (`phase1/`): hold invoice → pay → accepted → settle / cancel, both verified
- [x] Timeout path: with no action LND auto-cancels 12 blocks before HTLC expiry and the buyer is refunded in full (see `flow.md` section 7)
- [x] Escrow core (`escrow-core/`): state machine, timing, `LightningAdapter` + mock, `Store` + in-memory store, service logic; 27 tests passing
- [x] Phase 2 part 2: SQLite store (`escrow-sqlite/`), LND adapter plus event and scheduler loops (`escrow-lnd/`); verified on regtest including the automatic deadline refund
- [x] Phase 3 API (`escrow-api/`): auth, per-escrow authorization, idempotency, rate limiting, `/health`; 20 tests; both demo paths verified over HTTP on regtest (`scripts/demo.sh`)
- [x] CORS, `GET /api/v1/escrows` (own escrows), Next.js scaffold in `web/`
- [x] Phase 4 frontend (`web/`): account, new escrow, escrow page by role and state with release QR hand-over, operator queue; 44 unit tests; both demo paths pass end to end in the browser (Playwright, `npm run e2e`)
- [x] `server_time` on `/health` for client clock-skew correction
- [x] Hardening 1: `CREATED` offers can be withdrawn (seller, operator) or declined (buyer) and lapse at their deadline; `GET /api/v1/me`
- [x] Hardening 2: restricted invoice-only macaroon (`regtest/creds/seller/toshi.macaroon`, now the server default), restart test on the real node
- [x] Hardening 3: SQLite schema migrations (`PRAGMA user_version`), open-offer links (no buyer id needed; the first user to accept becomes the buyer)
- [x] Extras: WebLN one-click payment, camera scan of the release QR, PostgreSQL store (`DATABASE_URL`)
- [x] Sellers bring their own node (non-goal lifted): per-seller node directory, encrypted credentials, `/api/v1/me/node`, "My node" page, second regtest seller node
- [x] Arbitration (non-goal lifted): optional arbiter per escrow, buyer's secret sealed to them in the browser, confirmation, release-or-refund decision on the record
- [x] Demo preparation: lead use case chosen (courier delivery), `scripts/demo-env.sh`, seeded cast, demo wallet, `docs/demo.md`, scripted rehearsal (`npm run rehearse`)
- [x] Fiat display (PRD FR-16): optional currency per escrow, rate taken and locked at creation, display only (`FIAT_PROVIDER`)
- [ ] Trying real wallets on a public network (a signet attempt was abandoned: see `docs/flow.md`, section 21)

## Open questions

- ~~Which use case leads the demo?~~ Answered: courier delivery (the recipient is physically present at the handover).
- ~~Who owns the seller's node?~~ Answered: the seller can connect their own (non-custodial); the shared hosted node is optional and labelled custodial.
- Regtest only, or signet/testnet with real wallets?
- Maximum supported hold time on the target network and node settings.

## Running the tests

```bash
cargo test --workspace                                    # fast, no network needed
./regtest/setup.sh                                        # once, to start the regtest network
cargo test -p escrow-lnd -- --ignored --test-threads=1    # real-node tests (4), about a minute

docker compose -f regtest/docker-compose.yml --profile postgres up -d postgres
cargo test -p escrow-postgres -- --ignored --test-threads=1   # store contract + race on a real PostgreSQL (wipes it)
DATABASE_URL=postgres://toshi:toshi@127.0.0.1:55432/toshi cargo run -p escrow-api --bin toshi-server   # run on PostgreSQL
cargo run -p phase1 -- both                               # the original hold-invoice demo

export OPERATOR_TOKEN=$(openssl rand -hex 16)             # 24+ characters
export NODE_CREDENTIAL_KEY=$(openssl rand -hex 32)        # keep it: it decrypts sellers' stored macaroons
export ALLOW_PRIVATE_NODE_ADDRESSES=1                     # regtest only: lets sellers point at localhost nodes
cargo run -p escrow-api --bin toshi-server &              # API on 127.0.0.1:3000, SQLite in ./toshi.db
./scripts/demo.sh                                         # both demo paths over HTTP
```

Building needs `protoc` on the PATH (the LND client generates code from protobuf files).
