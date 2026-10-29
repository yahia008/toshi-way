# toshi-way

**Escrow on Bitcoin Lightning, without a middleman.** A buyer's payment is *locked* on the Lightning network rather than sent. It is released to the seller when the buyer (or the person receiving the goods) hands over a secret, and refunded automatically if that doesn't happen before a deadline. Nobody holds the money: the lock itself is the escrow.

Built around **courier deliveries**: the customer pays up front, the courier starts work knowing the payment is committed, and the customer shows a QR code at the handover to release it.

## How it works

1. The seller makes an offer (amount, description, deadline). Their own Lightning node issues a *hold invoice*.
2. The buyer's browser makes a random 256-bit secret and sends only its hash. The buyer pays; the payment is locked, not spent.
3. When the Lightning node reports the funds locked (never because a client said so), both sides see it.
4. **Release:** the buyer shows a QR holding the secret; the seller submits it and is paid straight to their own node.
5. **Refund:** if the secret never arrives, the hold is cancelled at the deadline and the buyer gets everything back, with no one acting.
6. **Dispute:** if both sides named an *arbiter*, the buyer's secret is also encrypted so only the arbiter can open it; they decide between paying the seller and refunding the buyer. They never hold the money.

## See it work

Needs Docker (with Compose), Rust, Node 20+, and `protoc`.

```bash
./scripts/demo-env.sh up      # Lightning test network, server, web app, demo wallet and a seeded cast (~25 s)
# then open http://localhost:3001 and follow docs/demo.md
(cd web && npm run rehearse)  # or watch a script play all three demo scenes (~2 min)
```

Everything runs on a local **regtest** network with a stand-in wallet; no real money is involved.

## What's here

| Path | What |
|---|---|
| `escrow-core/` | state machine, timing rules, service logic (no I/O, fully testable with a mock node) |
| `escrow-lnd/` | LND adapter, per-seller node directory, event loop and deadline scheduler |
| `escrow-sqlite/`, `escrow-postgres/` | the two storage backends, with migrations, passing one shared contract test |
| `escrow-api/` | the HTTP API and the `toshi-server` binary |
| `web/` | the Next.js app |
| `sdk/js/` | a small JavaScript client for businesses: make and read escrows, verify webhooks |
| `regtest/` | Docker Compose Lightning network (bitcoind, buyer, router, two sellers) |
| `scripts/` | `demo-env.sh` (one-command environment), `demo.sh` (curl walkthrough), `seed-demo.mjs`, `demo-wallet.mjs` |

## Docs

- [`docs/demo.md`](docs/demo.md): the demo script, with talk track, limits and troubleshooting
- [`docs/api.md`](docs/api.md): for businesses: take payments from your own site (API keys, checkout link, webhooks)
- [`sdk/js/README.md`](sdk/js/README.md): the JavaScript client
- [`docs/architecture.md`](docs/architecture.md): how the system is put together: crates, state machine, trust model, background tasks, web app
- [`docs/project.md`](docs/project.md): product spec, trust model, API and status
- [`docs/flow.md`](docs/flow.md): a log of every decision, measurement and problem, in order
- [`docs/gist.md`](docs/gist.md): the original PRD

## Tests

```bash
cargo test --workspace                                     # fast, no network needed
(cd sdk/js && npm test)                                    # the JavaScript client, no server needed
cargo test -p escrow-lnd -- --ignored --test-threads=1     # against the real regtest nodes
cargo test -p escrow-postgres -- --ignored --test-threads=1  # needs the postgres profile running
(cd web && npm test && npm run lint && npm run build)      # unit tests, lint, build
(cd web && npm run e2e)                                    # the full browser suite against the live stack
```

## Honest limits

This is a hackathon-stage project, proven on regtest only. Paying in naira and withdrawing to a Nigerian bank run on a pretend demo provider only (labelled on every screen); a real one needs a licensed payment partner. It has not been tried with real phone wallets on a public network, and whether real wallets hold a payment pending for hours is an open question. A buyer can still withhold the secret (the cost to them is only the refund), which is why delivery and an arbiter are the mitigations. Holds are capped at 6 hours by how long Lightning can safely hold a payment. See [`docs/project.md`](docs/project.md) for the full trust model.
