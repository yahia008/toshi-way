# Deploying toshi-way on Render

A step-by-step guide to putting the API server, the web app and the database on [Render](https://render.com), and connecting them to a Lightning node you run or rent elsewhere. Written for the person doing the deploy. For how the system works, see [`architecture.md`](architecture.md).

**Do the whole thing on a public test network first** (signet). It costs nothing, and it is the only way to find the mistakes that would otherwise cost money.

## What goes where

```
Render                                           Somewhere else
  toshi-web   (Next.js)  ── browser calls ──►    your Lightning node (LND)  ◄── the Bitcoin network
  toshi-api   (Rust)     ── gRPC + TLS ─────►      with channels and liquidity
  toshi-db    (Postgres) ◄── private network
```

- **On Render:** the API server, the web app and the Postgres database. None of them keeps files, which suits Render.
- **Not on Render:** the Lightning node. A node's channel state must never be rolled back (restoring an old copy can make it broadcast outdated state and lose funds), and Render snapshots disks once a day and restores whole snapshots. It also needs other nodes to be able to connect to it. Use a managed node, or a small server you control.

## Before you start

You need:

1. **A Render account**, and this repository connected to it.
2. **A Lightning node (LND)** reachable from the internet over gRPC, with **channels and liquidity**. On a test network, get coins from a faucet and open channels first. Two checks with your provider:
   - It must allow **hold invoices** (LND's invoices sub-server: `AddHoldInvoice`, `SettleInvoice`, `CancelInvoice`). Some managed providers restrict it, and the escrow cannot work without it.
   - It must let you make **custom macaroons** (below).
3. **Two domains or subdomains**, for example `app.example.com` (web) and `api.example.com` (API). Render gives each service an `onrender.com` address, which is enough for a rehearsal.

## Step 1: make the node's keys

On the node, make a macaroon that can only handle invoices, and one that can also pay (needed so sellers can withdraw):

```bash
lncli bakemacaroon invoices:read invoices:write info:read --save_to=toshi.macaroon
lncli bakemacaroon offchain:read offchain:write invoices:read invoices:write info:read --save_to=payout.macaroon
```

Never give the server the node's admin macaroon. Then encode the three files as one line each:

```bash
base64 -w0 ~/.lnd/tls.cert        # LND_TLS_CERT_B64
base64 -w0 toshi.macaroon         # LND_MACAROON_B64
base64 -w0 payout.macaroon        # PAYOUT_MACAROON_B64
```

Make the credential key that seals sellers' stored node details and webhook secrets, and **keep a copy somewhere safe and separate from Render**. If it is lost, those cannot be opened again:

```bash
openssl rand -hex 32              # NODE_CREDENTIAL_KEY
```

## Step 2: create the Blueprint

1. In Render, choose **New, Blueprint**, pick this repository, and let it read [`render.yaml`](../render.yaml).
2. Render creates the database and both services and asks for every setting marked `sync: false`. Fill in what you have now:

| Setting | Value |
|---|---|
| `NODE_CREDENTIAL_KEY` | from step 1 |
| `LND_ADDR` | your node's `https://host:10009` |
| `LND_TLS_CERT_B64`, `LND_MACAROON_B64`, `PAYOUT_MACAROON_B64` | from step 1 |
| `CORS_ORIGINS`, `PUBLIC_WEB_URL`, `NEXT_PUBLIC_API_URL`, `NEXT_PUBLIC_APP_URL` | **leave empty for now** (you do not know the addresses yet) |
| `PLATFORM_ARBITER_ID` | leave empty unless you have set one up |

3. The services will fail their first build or start without the addresses, or start with the wrong ones. That is expected.

## Step 3: connect the addresses (the second deploy)

1. Note each service's address (or attach your custom domains): `toshi-api` becomes the **API address**, `toshi-web` the **web address**.
2. On **toshi-api**, set `CORS_ORIGINS` and `PUBLIC_WEB_URL` to the **web address** (`https://…`, no trailing slash). Deploy it.
3. On **toshi-web**, set `NEXT_PUBLIC_API_URL` to the **API address** and `NEXT_PUBLIC_APP_URL` to the **web address**. **Deploy it again**: these are written into the pages when they are built, so changing them needs a new build.
4. Deploys are manual on purpose (`autoDeployTrigger: "off"`). Look at what changed before every deploy of a service that handles money.

## Step 4: check it works

1. `https://<api address>/livez` says `ok`, and `https://<api address>/health` says the node is `up`. If `/health` says `down`, the API cannot reach your node: check `LND_ADDR`, the certificate and the macaroon.
2. Open the web address, create an account, and **save the token**.
3. Open **My node** and check it shows the shared node; make a **tiny** escrow between two accounts you control, pay it from a second wallet, release it, and watch the seller's **Balance** go up.
4. Make a withdrawal to your own wallet. Remember a new PIN waits 24 hours (`PIN_COOLOFF_HOURS`) before it can authorise one: set the PIN on day one.
5. Sign in on the operator page with `OPERATOR_TOKEN` (read it in the API service's settings).

## Settings that matter

| Setting | Default here | Why |
|---|---|---|
| `MAX_ESCROW_SATS` | 50,000 | Start small and raise it. The form still shows the code's own maximum; an amount above this is refused by the server. |
| `WITHDRAW_LIMIT_SATS` | 100,000 per 24 hours | A ceiling on what one seller can take out in a day. |
| `PIN_COOLOFF_HOURS` | 24 | A new or changed PIN waits before it works, so a stolen sign-in cannot empty a balance at once. |
| `REGISTER_LIMIT_PER_MIN` | 60 | Sign-ups per minute, **for everyone together** (see below). |
| `RAMP_PROVIDER`, `FIAT_PROVIDER` | `off` | No naira or fiat until a real provider is connected. The demo provider refuses to run on mainnet. |
| `TRUSTED_PROXIES` | empty | See below. |
| `ALLOW_PRIVATE_NODE_ADDRESSES` | not set | Never set it on a public server: it lets sellers point the server at private networks. |

### Rate limits behind Render

The server limits sign-ups and anonymous requests by the visitor's address, but Render's proxy appends its own address to `X-Forwarded-For` and does not remove one a visitor sends, so the real address cannot be trusted. With `TRUSTED_PROXIES` empty, every visitor looks like one address: **the sign-up limit is shared by everyone**, and so are anonymous page loads. Signed-in requests are limited per account, not per address. That is fine for a small launch. If you need per-visitor limits later, put Cloudflare in front and set `TRUSTED_PROXIES` to its ranges.

## Keeping it safe

- **Back up the credential key** (step 1) and the **node's static channel backup** (`lncli exportchanbackup --all`), somewhere that is not Render and not the node. Without the channel backup, a node that loses its disk loses its funds.
- **Database backups.** Use a paid Postgres plan with backups for real money. The database holds accounts, escrows and balances.
- **Never restore the node from an old snapshot.** Restoring outdated channel state can cost you the channel's funds.
- **The shared node is a hot wallet.** Everything sellers have not withdrawn sits on it. Keep its balance and channel size modest, and watch it.
- **The server cannot read people's release secrets** and cannot release or refund on its own. It tells the node what to do and records what the node says. Escrows that the platform arbitrates are decided by a person holding the arbiter key in a browser.
- **Deploy by hand,** and check `/health` afterwards.

## If something goes wrong

| What you see | Likely cause |
|---|---|
| `/health` says node `down` | The node is offline, the address, certificate or macaroon is wrong, or its provider changed the address. The API keeps running; escrows on the node pause. |
| Web app loads but every action fails with a network error | `NEXT_PUBLIC_API_URL` is wrong or empty (deploy the web app again after fixing it), or `CORS_ORIGINS` on the API does not match the web address exactly. |
| "Too many requests" for everyone at once | The shared sign-up or request limit was reached. Raise `REGISTER_LIMIT_PER_MIN` or `RATE_LIMIT_PER_MIN`, or wait a minute. |
| Server exits at start with `NODE_CREDENTIAL_KEY` or `LND_…` in the message | A setting is missing or malformed. The credential key must be 64 hex characters; the encoded values must be valid base64. |
| Deploy fails while building the API | The Rust build is large. Retry, and if it keeps failing, check Render's build log for memory limits. |

## What this setup does not do yet

- **Naira and bank withdrawals.** Off until a real provider is integrated.
- **Per-visitor rate limits** on Render (above).
- **A hosted Lightning node.** You provide it.
- **Automatic deploys, staging and monitoring.** Add them once the basics work.
