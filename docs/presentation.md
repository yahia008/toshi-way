---
marp: true
title: toshi-way
description: Escrow on Bitcoin Lightning, without a middleman
paginate: true
theme: default
---

<!--
How to use this file
  Marp (VS Code extension or `npx @marp-team/marp-cli docs/presentation.md --pdf`): renders as slides.
  Plain Markdown: reads as an outline. Slides are split by lines containing only ---.
  Speaker notes are the HTML comments under each slide.
Everything shown runs on a local Bitcoin regtest network. No real money.
-->

# toshi-way

## Escrow on Bitcoin Lightning, without a middleman

Your payment is **locked**, not sent.
It is released when you say so, and refunded if nothing happens.

<!--
One sentence to open: "This is escrow where nobody holds your money."
-->

---

# The problem

Two people who don't trust each other, one transaction.

- **Pay first:** the seller can take the money and vanish.
- **Deliver first:** the buyer can take the goods and not pay.
- **Escrow services** fix it by holding the money, which means you now trust *them*: their fees, their custody, their power to freeze it.

<!--
Everyone in the room has been on one side of this. Land it before moving on.
-->

---

# The idea

Lightning has a feature called a **hold invoice**: a payment can be *accepted but not settled*.

The money is committed. The seller can see it is there. Nobody can spend it.

Then one of two things happens:

| | |
|---|---|
| **Release** | the buyer hands over a secret, the seller settles, the seller is paid |
| **Refund** | the deadline passes, the hold is cancelled, the buyer gets everything back |

**The lock itself is the escrow.**

---

# How it works

1. **The seller makes an offer:** amount, description, deadline.
2. **The buyer accepts.** Their browser makes a random secret and sends only its *hash*.
3. **The buyer pays.** The payment locks. Both sides see **Funds locked**, because the Lightning node says so, not because anyone clicked "I paid".
4. **Release:** the buyer shows the secret (a QR code) when they have what they paid for. The seller submits it and is paid.
5. **Refund:** no secret before the deadline, and the buyer is refunded automatically.

<!--
Point at step 3. "The website never decides that money is locked."
-->

---

# Who holds the money? Nobody.

| Secret | Where it lives |
|---|---|
| **The release secret** | The buyer's browser. The server sees it once, at release, and never stores it. |
| **The account token** | Encrypted on your device under your password. The server keeps only a hash. |
| **The seller's node key** | Invoice-only. It can create invoices, not spend. |
| **An arbiter's private key** | The arbiter's browser. Never the server. |

The server is a **coordinator, not a wallet.** It cannot take or release your money on its own.

---

# Product 1: the escrow app

A web app for buyers and sellers. No install, works on a phone.

- **Make an offer** in two steps: pick the kind of deal, then set amount and terms. Title and description are filled in for you.
- **One link to send.** An open offer is a link anyone can open; the first person to accept becomes the buyer.
- **Pay from any Lightning wallet,** or one tap with a browser wallet.
- **Everyone sees the same truth:** a live countdown, a progress timeline, and a history of who did what.
- **A dashboard** that says what needs *you* right now.

---

# The handover moment

Built for **deliveries**, where the recipient is physically present.

1. Customer pays up front. The courier starts work knowing the money is committed.
2. The courier arrives. The customer shows a **release QR**.
3. The courier scans it. The sats go straight to the courier's node.

The secret travels after the `#` in the link. Browsers never send that part to a server, so it is in no log.

<!--
This answers the obvious objection: "the buyer could just never release". At a handover they are standing in front of you.
-->

---

# If nothing happens

**Nobody shows up?** The customer is refunded automatically at the deadline. No one lifts a finger, and no one can stop it.

**Nobody pays?** The offer simply lapses.

A scheduler checks every second, and the node's own timeout sits safely behind it as a backstop.

The refund path is not a feature someone has to remember to use. It is what happens by default.

---

# When people disagree

An **arbiter** is someone both sides trust to decide a dispute. They never hold the money.

- The buyer's secret is also **sealed to the arbiter's key** in the buyer's browser. The server cannot read it.
- The arbiter checks it is genuine and **confirms**, so the seller knows arbitration is real before starting work.
- After a dispute: **release to the seller** or **refund the buyer**. Their note goes on the record.

No arbiter named? If the operator has set one up, the **platform's arbiter** is used by default, and the seller can opt out.

---

# Fair disputes

- **Delivery claims and evidence.** The seller can mark an escrow delivered with proof (a tracking number, a link). Either side can add evidence. Both sides and the arbiter read it in one place.
- **Time to answer.** When a dispute opens, the other side gets a quarter of the time left (at most an hour) before the arbiter can decide, or until they add their side.
- **Deal-type guidance.** Each kind of deal lists what an arbiter will look for: a courier's signed receipt, a registrar record, a handover form.
- **No extensions.** The deadline holds. If nobody decides in time, the buyer is refunded.

---

# Many kinds of deal

| | |
|---|---|
| P2P sale | goods or a crypto exchange |
| Freelance and services | work delivered for payment |
| Cargo and goods | shipped and tracked |
| Digital assets | domains, websites, accounts |
| Vehicles | private sales |
| Property | deposits and handovers |

Choosing one fills in a sensible description with "what counts as done", so disputes start from a clear agreement.

---

# Product 2: for businesses

Take payment through toshi-way from your **own website or system**.

```js
const escrow = await toshi.escrows.create({
  amount_sats: 25_000,
  description: "Parcel to Ikeja, deliver by Friday noon",
  hold_secs: 4 * 3600,
  reference: "ORDER-1001",          // your order number, returned everywhere
  idempotencyKey: "order-1001",     // a retry can never make two escrows
});
// send the customer to escrow.checkout_url
```

- **API keys** you can scope and revoke, **a checkout link**, **signed webhooks**, a small **JavaScript client**.
- A complete **Next.js shop example** is included.

---

# Webhooks you can trust

Your server hears the moment an escrow changes: accepted, locked, settled, refunded, disputed.

- **Signed** with a secret only you hold. Anyone can post to your address, so every message is checked.
- **In order, at least once.** A failing event retries with growing waits, and later events wait behind it.
- **Never treat "locked" as paid.** Only `SETTLED` means you have been paid.

<!--
Mention the rule from the docs: the customer arriving back on your site proves nothing. Only the webhook, confirmed against the API, does.
-->

---

# Product 3: your node, or ours

**Your own Lightning node (the aim).**
The sats land with you directly. Nobody else ever holds them.

**The shared node (for sellers with none).**
The service receives the sats on release and owes them to you, kept as a **balance** you withdraw at any time. This is custodial, and the product says so plainly.

Either way the *escrow* is the same: locked on Lightning until released or refunded.

---

# Product 4: balances and naira

For sellers on the shared node:

- **Withdraw** to any Lightning wallet, or to a **Nigerian bank account in naira**.
- **Pay in naira:** a buyer can fund an escrow from a bank transfer.
- A live **price in naira** is shown on offers (display only; the escrow stays in sats and the value never changes).

Built to be safe:
- A **6-digit PIN** on every withdrawal, locked after five wrong tries.
- A **daily withdrawal cap**, and a **24-hour wait** on sats that came from a bank transfer, since those can be reversed.
- A withdrawal invoice can only ever be used **once**.

*The naira provider is a labelled demo today. No real transfer happens.*

---

# See it work

**Three scenes, about ten minutes:**

1. **Paid on handover.** Customer pays, gets the parcel, shows the QR, the courier is paid.
2. **Nobody shows up.** The customer is refunded with no one acting.
3. **A dispute.** The customer won't release; the arbiter reads the evidence and pays the courier.

One command starts everything: the Lightning test network, the server, the web app and a seeded cast.

```bash
./scripts/demo-env.sh up
```

---

# How it is built

- **Rust** backend (Axum, Tokio) with a **state machine** that only the node can move forward.
- **Lightning** through LND hold invoices, one node per seller.
- **SQLite or Postgres,** passing the same contract tests.
- **Next.js** web app: the browser does the cryptography.
- **A JavaScript client** with no dependencies.

Tested against a **real Lightning test network**: unit and integration tests, plus a full browser suite that plays buyers, sellers and arbiters end to end.

---

# Where we are

**Working today, on a test network (regtest):**

- The full escrow cycle: lock, release, automatic refund
- Disputes with arbiters, a platform arbiter, evidence and a response window
- Seller-owned nodes and shared-node balances, with withdrawals
- Business API, checkout links, signed webhooks, JavaScript client
- A guided demo and a scripted rehearsal

**Not yet:** real money. Everything here uses a local test network and a stand-in wallet.

---

# What's next

- **Close a known gap in withdrawals:** setting the first PIN should need more than a sign-in token.
- **Longer holds.** Six hours is too short for shipments, vehicles and property.
- **Reminders** by email or push when a delivery is claimed or a dispute opens.
- **A real naira payment provider.**
- **Catch-up for missed webhooks,** and idempotency keys that survive a restart.
- **From test network to the real one,** carefully.

---

# toshi-way

**Escrow where the lock is the escrow.**

Try it: `./scripts/demo-env.sh up`, then open the app and follow `docs/demo.md`.

- How it works in depth: `docs/architecture.md`
- For businesses: `docs/api.md` and `sdk/js/README.md`
- The demo script: `docs/demo.md`

Questions?
