// Both demo paths through the browser, modelled on scripts/demo.sh.
//
//   ./regtest/setup.sh
//   OPERATOR_TOKEN=... cargo run -p escrow-api --bin toshi-server
//   (cd web && npm run dev -- -p 3001)
//   (cd web && npm run e2e)
//
// The buyer's invoice is paid from the buyer node with lncli, as a developer would by hand.

import { execFile, spawn } from "node:child_process";
import { createHmac } from "node:crypto";
import { createServer } from "node:http";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { promisify } from "node:util";
import { expect, test, type Browser, type BrowserContext, type Page } from "@playwright/test";

const REPO = resolve(__dirname, "../..");
const COMPOSE = ["compose", "-f", resolve(REPO, "regtest/docker-compose.yml")];

/** Regtest only mines on demand; LND reports "not synced" after two hours without a block. */
async function freshChainTip() {
  const run = promisify(execFile);
  const cli = [...COMPOSE, "exec", "-T", "bitcoind", "bitcoin-cli", "-regtest", "-rpcuser=toshi", "-rpcpassword=toshi"];
  await run("docker", [...cli, "loadwallet", "miner"]).catch(() => {});
  await run("docker", [...cli, "-rpcwallet=miner", "-generate=1"]);
}

/** Pay from the buyer node. lncli blocks while the payment is held, so don't wait for it. */
function payInvoice(invoice: string) {
  const child = spawn("docker", [...COMPOSE, "exec", "-T", "buyer", "lncli", "--network=regtest", "payinvoice", "--force", invoice], {
    stdio: "ignore",
    detached: true,
  });
  child.unref();
}

const stateBadge = (page: Page, label: string) => page.locator("span[title]", { hasText: label });

const PASSWORD = "correct horse battery staple";

/** Fill the two password boxes of the create-account form. */
async function choosePassword(page: Page, password = PASSWORD) {
  await page.getByLabel("Create a password").first().fill(password);
  await page.getByLabel("Confirm the password").first().fill(password);
}

/** Sign in on a fresh page with a token and a new password for that device. */
async function importToken(page: Page, token: string, password = PASSWORD) {
  await page.getByLabel("Token", { exact: true }).fill(token);
  await page.getByLabel("New password for this device", { exact: true }).fill(password);
  await page.getByLabel("Confirm the new password").fill(password);
  await page.getByRole("button", { name: "Sign in" }).click();
}

type Party = { context: BrowserContext; page: Page; id: string; token: string; requests: { url: string; body: string | null }[] };

/** A fresh browser profile with a new account, recording every request it sends. */
async function newParty(browser: Browser): Promise<Party> {
  const context = await browser.newContext();
  const page = await context.newPage();
  const requests: Party["requests"] = [];
  page.on("request", (r) => requests.push({ url: r.url(), body: r.postData() }));

  await page.goto("/");
  await choosePassword(page);
  await page.getByRole("button", { name: "Create account" }).click();
  await expect(page.getByText("Save your token now")).toBeVisible();
  const token = (await page.locator("code").nth(0).textContent())!.trim();
  const id = (await page.locator("code").nth(1).textContent())!.trim();
  expect(token).toMatch(/^tw_/);
  expect(id).toMatch(/^u_/);
  await page.getByLabel("I have saved my token somewhere safe").check();
  await page.getByRole("button", { name: "Continue" }).click();
  await expect(page.getByRole("heading", { name: "My escrows" })).toBeVisible();
  return { context, page, id, token, requests };
}

async function createEscrow(
  seller: Party,
  buyerId: string,
  description: string,
  hold: [string, "minutes" | "hours"],
  arbiterId?: string,
) {
  const { page } = seller;
  await page.goto("/escrows/new");
  await page.getByLabel("Description").fill(description);
  await page.getByRole("button", { name: "Continue" }).click();
  await page.getByLabel("Buyer's user id").fill(buyerId);
  if (arbiterId) await page.getByLabel("Arbiter's user id").fill(arbiterId);
  await page.getByLabel("Amount (sats)").fill("1000");
  await page.getByLabel("Hold time amount").fill(hold[0]);
  await page.getByLabel("Hold time unit").selectOption(hold[1]);
  await page.getByRole("button", { name: "Create escrow" }).click();
  await page.waitForURL(/\/escrows\/[0-9a-f-]{36}$/);
  return page.url().split("/").pop()!;
}

/** Buyer accepts and pays; returns once both sides show the funds locked. */
async function acceptAndPay(buyer: Party, seller: Party, id: string) {
  await buyer.page.goto(`/escrows/${id}`);
  await buyer.page.getByRole("button", { name: /Accept offer/ }).click();
  await expect(buyer.page.getByRole("button", { name: "Copy invoice" })).toBeVisible();
  const invoice = (await buyer.page.locator("code", { hasText: /^lnbcrt/ }).first().textContent())!.trim();
  await expect(buyer.page.getByText("Pay within")).toBeVisible();
  payInvoice(invoice);
  for (const p of [buyer.page, seller.page]) await expect(stateBadge(p, "Funds locked")).toBeVisible();
}

test.beforeAll(freshChainTip);

// Each test opens several browser profiles. Close them when it ends, or they pile up across the
// suite until the browser runs out of memory and a tab crashes.
test.afterEach(async ({ browser }) => {
  await Promise.all(browser.contexts().map((c) => c.close().catch(() => {})));
});

test("happy path: lock, hand over the release QR link, settle", async ({ browser }) => {
  const seller = await newParty(browser);
  const buyer = await newParty(browser);
  const id = await createEscrow(seller, buyer.id, "e2e parcel", ["30", "minutes"]);
  await expect(seller.page.getByText("Link for the buyer")).toBeVisible();

  await acceptAndPay(buyer, seller, id);
  await expect(seller.page.getByText("Funds are locked: you can start the work.")).toBeVisible();

  // Once paid, the buyer has no cancel button (the API would answer 403 anyway).
  await expect(buyer.page.getByRole("button", { name: /^Cancel/ })).toHaveCount(0);

  // A wrong secret is refused and changes nothing.
  await seller.page.getByLabel("Release secret").fill("ab".repeat(32));
  await seller.page.getByRole("button", { name: /^Release 1,000 sats/ }).click();
  await expect(seller.page.getByText("That secret doesn't match this escrow.")).toBeVisible();
  // A short code is refused before it is sent.
  await seller.page.getByLabel("Release secret").fill("123456");
  await seller.page.getByRole("button", { name: /^Release 1,000 sats/ }).click();
  await expect(seller.page.getByText("exactly 64 hex characters")).toBeVisible();
  await expect(stateBadge(seller.page, "Funds locked")).toBeVisible();

  // The buyer reveals the release QR; the link it encodes is what the seller's camera would open.
  await buyer.page.getByRole("button", { name: "Reveal release QR" }).click();
  await buyer.page.getByRole("button", { name: "Yes, show the QR" }).click();
  await expect(buyer.page.getByRole("img", { name: "Release QR" })).toBeVisible();
  const secret = await buyer.page.evaluate((id) => localStorage.getItem(`toshi.secret.${id}`), id);
  expect(secret).toMatch(/^[0-9a-f]{64}$/);

  await seller.page.goto(`/escrows/${id}#release=${secret}`);
  await expect(seller.page.getByText("The secret was filled in from the release link.")).toBeVisible();
  expect(new URL(seller.page.url()).hash).toBe(""); // removed from the address bar at once
  await expect(seller.page.getByLabel("Release secret")).toHaveValue(secret!);
  await seller.page.getByRole("button", { name: /^Release 1,000 sats/ }).click();

  for (const p of [seller.page, buyer.page]) await expect(stateBadge(p, "Settled")).toBeVisible();
  await expect(seller.page.getByText("Done. You were paid.")).toBeVisible();

  // The secret left the browser exactly once: in the seller's release call.
  const carrying = [...seller.requests, ...buyer.requests].filter(
    (r) => r.url.includes(secret!) || r.body?.includes(secret!),
  );
  expect(carrying).toHaveLength(1);
  expect(carrying[0].url).toMatch(new RegExp(`/api/v1/escrows/${id}/release$`));

  // Someone who isn't a party gets "not found", not the escrow.
  const stranger = await newParty(browser);
  await stranger.page.goto(`/escrows/${id}`);
  await expect(stranger.page.getByText("Escrow not found")).toBeVisible();
});

test("refund path: a short hold is refunded with nobody acting", async ({ browser }) => {
  const seller = await newParty(browser);
  const buyer = await newParty(browser);
  const id = await createEscrow(seller, buyer.id, "e2e refund", ["1.25", "minutes"]); // 75 s: the shortest allowed is 60 s, and the buyer needs a few seconds to accept
  await acceptAndPay(buyer, seller, id);
  for (const p of [seller.page, buyer.page]) {
    await expect(stateBadge(p, "Refunded")).toBeVisible({ timeout: 120_000 });
  }
  await expect(buyer.page.getByText("Your payment was cancelled and the funds are back in your wallet.")).toBeVisible();
});

test("an offer nobody has accepted can be withdrawn by the seller or declined by the buyer", async ({ browser }) => {
  const seller = await newParty(browser);
  const buyer = await newParty(browser);

  const withdrawn = await createEscrow(seller, buyer.id, "e2e withdrawn", ["30", "minutes"]);
  await seller.page.getByRole("button", { name: "Withdraw offer" }).click();
  await seller.page.getByRole("button", { name: "Withdraw offer" }).last().click();
  await expect(stateBadge(seller.page, "Ended unpaid")).toBeVisible();
  await buyer.page.goto(`/escrows/${withdrawn}`);
  await expect(stateBadge(buyer.page, "Ended unpaid")).toBeVisible();
  await expect(buyer.page.getByRole("button", { name: /Accept offer/ })).toHaveCount(0);

  const declined = await createEscrow(seller, buyer.id, "e2e declined", ["30", "minutes"]);
  await buyer.page.goto(`/escrows/${declined}`);
  await buyer.page.getByRole("button", { name: "Decline" }).click();
  await buyer.page.getByRole("button", { name: "Yes, decline" }).click();
  await expect(stateBadge(buyer.page, "Ended unpaid")).toBeVisible();
  await expect(stateBadge(seller.page, "Ended unpaid")).toBeVisible();
});

test("signing in with a token finds the account's id and role", async ({ browser }) => {
  const user = await newParty(browser);
  const fresh = await browser.newContext();
  const page = await fresh.newPage();
  await page.goto("/");
  await importToken(page, user.token);
  await expect(page.getByRole("heading", { name: "My escrows" })).toBeVisible();
  await expect(page.getByText(user.id)).toBeVisible();
});

test("open offer: a link anyone can open; a brand-new visitor becomes the buyer", async ({ browser }) => {
  const seller = await newParty(browser);
  const { page } = seller;

  await page.goto("/escrows/new");
  await page.getByLabel("Description").fill("e2e open offer");
  await page.getByRole("button", { name: "Continue" }).click();
  await page.getByLabel("Amount (sats)").fill("1000");
  await page.getByLabel("Hold time amount").fill("30");
  await page.getByLabel("Hold time unit").selectOption("minutes");
  await page.getByRole("button", { name: "Create escrow" }).click(); // no buyer id
  await page.waitForURL(/\/escrows\/[0-9a-f-]{36}$/);
  const id = page.url().split("/").pop()!;
  await expect(page.getByText("the first person to open the link and accept becomes the buyer")).toBeVisible();

  // A visitor with no account follows the link, creates one on the spot, and lands on the offer.
  const context = await browser.newContext();
  const visitor = await context.newPage();
  await visitor.goto(`/escrows/${id}`);
  await choosePassword(visitor);
  await visitor.getByRole("button", { name: "Create account" }).click();
  await visitor.getByLabel("I have saved my token somewhere safe").check();
  await visitor.getByRole("button", { name: "Continue" }).click();
  await expect(visitor.getByText("e2e open offer")).toBeVisible();
  await expect(visitor.getByRole("button", { name: "Decline" })).toHaveCount(0);

  await visitor.getByRole("button", { name: /Accept offer/ }).click();
  await expect(visitor.getByRole("button", { name: "Copy invoice" })).toBeVisible();
  await expect(stateBadge(page, "Awaiting payment")).toBeVisible();

  // Now it belongs to the visitor: a third person gets "not found".
  const stranger = await newParty(browser);
  await stranger.page.goto(`/escrows/${id}`);
  await expect(stranger.page.getByText("Escrow not found")).toBeVisible();
});

test("one-click WebLN payment by the buyer and camera scan of the release QR by the seller", async ({ browser }) => {
  const seller = await newParty(browser);
  const buyer = await newParty(browser);

  // The buyer's browser has a WebLN wallet; here it pays through the buyer node like a developer would.
  await buyer.page.exposeFunction("__payFromWallet", (invoice: string) => payInvoice(invoice));
  await buyer.page.addInitScript(() => {
    (window as unknown as { webln: unknown }).webln = {
      enable: async () => {},
      sendPayment: (invoice: string) => {
        (window as unknown as { __payFromWallet: (i: string) => void }).__payFromWallet(invoice);
        return new Promise(() => {}); // a hold invoice keeps the payment pending while locked
      },
    };
  });

  // The seller's browser has a camera that "sees" whatever the test puts in __scanText.
  await seller.page.addInitScript(() => {
    const w = window as unknown as Record<string, unknown>;
    w.__scanText = "";
    w.BarcodeDetector = class {
      async detect() {
        return w.__scanText ? [{ rawValue: w.__scanText }] : [];
      }
    };
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 64;
    navigator.mediaDevices.getUserMedia = async () => canvas.captureStream(5);
  });

  const id = await createEscrow(seller, buyer.id, "e2e webln and scan", ["30", "minutes"]);
  await buyer.page.goto(`/escrows/${id}`);
  await buyer.page.getByRole("button", { name: /Accept offer/ }).click();
  await buyer.page.getByRole("button", { name: /with my wallet/ }).click();
  // The page makes no claim of its own: it flips to LOCKED only when the API reports it.
  for (const p of [buyer.page, seller.page]) await expect(stateBadge(p, "Funds locked")).toBeVisible();

  const secret = await buyer.page.evaluate((id) => localStorage.getItem(`toshi.secret.${id}`), id);
  await seller.page.getByRole("button", { name: "Scan the buyer's QR" }).click();
  // Scanning another escrow's code is refused; this escrow's link fills the field but does not release.
  await seller.page.evaluate(
    (t) => ((window as unknown as { __scanText: string }).__scanText = t),
    `http://localhost:3001/escrows/not-this-one#release=${secret}`,
  );
  await expect(seller.page.getByText("That release code is for a different escrow.")).toBeVisible();
  await seller.page.evaluate(
    (t) => ((window as unknown as { __scanText: string }).__scanText = t),
    `http://localhost:3001/escrows/${id}#release=${secret}`,
  );
  await expect(seller.page.getByLabel("Release secret")).toHaveValue(secret!);
  await expect(stateBadge(seller.page, "Funds locked")).toBeVisible();

  await seller.page.getByRole("button", { name: /^Release 1,000 sats/ }).click();
  for (const p of [seller.page, buyer.page]) await expect(stateBadge(p, "Settled")).toBeVisible();
});


/** Spendable sats on a regtest node's channels (what the seller would see in their own wallet). */
async function channelBalance(node: string): Promise<number> {
  const run = promisify(execFile);
  const { stdout } = await run("docker", [...COMPOSE, "exec", "-T", node, "lncli", "--network=regtest", "channelbalance"]);
  return Number(JSON.parse(stdout).local_balance.sat);
}

const creds = (node: string, file: string) => resolve(REPO, "regtest/creds", node, file);

test("a seller connects their own node and is paid on it", async ({ browser }) => {
  const seller = await newParty(browser);
  const buyer = await newParty(browser);
  const { page } = seller;

  // Before connecting a node the seller is told the service's shared node would hold the money.
  await page.goto("/escrows/new");
  await expect(page.getByText("this escrow will use the service's shared node")).toBeVisible();

  // Wrong credentials are refused and nothing is saved: seller's macaroon against seller2's address.
  await page.getByRole("link", { name: "My node" }).click();
  await expect(page.getByText("No node connected.").first()).toBeVisible();
  await page.getByLabel("Node address").fill("https://localhost:10004");
  await page.getByLabel("TLS certificate").fill(readFileSync(creds("seller2", "tls.cert"), "utf8"));
  await page.getByLabel("Invoice macaroon (hex)").fill(readFileSync(creds("seller", "toshi.macaroon")).toString("hex"));
  await page.getByRole("button", { name: "Check and save" }).click();
  await expect(page.getByText(/could not|refused|did not pass/i).first()).toBeVisible();
  await expect(page.getByText("Your node is connected.")).toHaveCount(0);

  // Right credentials: connected.
  await page.getByLabel("Invoice macaroon (hex)").fill(readFileSync(creds("seller2", "toshi.macaroon")).toString("hex"));
  await page.getByRole("button", { name: "Check and save" }).click();
  await expect(page.getByText("Your node is connected.")).toBeVisible();
  await expect(page.getByText("https://localhost:10004")).toBeVisible();
  // The credentials are not kept in the form, and the page never shows the macaroon back.
  await expect(page.getByLabel("Invoice macaroon (hex)")).toHaveValue("");

  // An escrow now lives on the seller's own node.
  const before = await channelBalance("seller2");
  const id = await createEscrow(seller, buyer.id, "e2e own node", ["30", "minutes"]);
  await expect(page.getByText("The seller's own node")).toBeVisible();
  await acceptAndPay(buyer, seller, id);
  await buyer.page.getByRole("button", { name: "Reveal release QR" }).click();
  await buyer.page.getByRole("button", { name: "Yes, show the QR" }).click();
  const secret = await buyer.page.evaluate((id) => localStorage.getItem(`toshi.secret.${id}`), id);
  await page.goto(`/escrows/${id}#release=${secret}`);
  await page.getByRole("button", { name: /^Release 1,000 sats/ }).click();
  for (const p of [page, buyer.page]) await expect(stateBadge(p, "Settled")).toBeVisible();

  // The sats arrived on the seller's own node, not the service's.
  await expect.poll(() => channelBalance("seller2"), { timeout: 20_000 }).toBe(before + 1000);

  // While an escrow is in flight the node is locked in; once it is over it can be changed.
  await page.goto("/node");
  await expect(page.getByRole("button", { name: "Disconnect" })).toBeVisible();
});


/** A party who has made an arbiter key in their browser. */
async function newArbiter(browser: Browser): Promise<Party & { fingerprint: string }> {
  const arbiter = await newParty(browser);
  await arbiter.page.goto("/arbiter");
  await arbiter.page.getByRole("button", { name: "Create key" }).click();
  await expect(arbiter.page.getByText("Your arbiter key is ready.")).toBeVisible();
  const fingerprint = (await arbiter.page.getByText(/^([0-9a-f]{4} ){15}[0-9a-f]{4}$/).first().textContent())!.trim();
  return Object.assign(arbiter, { fingerprint });
}

/** Everyone's view of an arbitrated escrow that has been paid, locked and confirmed by the arbiter. */
async function arbitratedLocked(browser: Browser, description: string) {
  const arbiter = await newArbiter(browser);
  const seller = await newParty(browser);
  const buyer = await newParty(browser);
  const id = await createEscrow(seller, buyer.id, description, ["30", "minutes"], arbiter.id);
  // Both sides can see who the arbiter is and the key to check with them.
  await expect(seller.page.getByText("This escrow has an arbiter")).toBeVisible();
  await expect(seller.page.getByText(arbiter.fingerprint).first()).toBeVisible();

  await acceptAndPay(buyer, seller, id);
  await expect(seller.page.getByText("The arbiter hasn't confirmed yet.")).toBeVisible();

  // The arbiter opens the buyer's sealed secret in their own browser and confirms.
  await arbiter.page.goto(`/escrows/${id}`);
  await arbiter.page.getByRole("button", { name: "Check the sealed secret" }).click();
  await expect(arbiter.page.getByText("The sealed secret opens and matches the payment hash.")).toBeVisible();
  await arbiter.page.getByRole("button", { name: "Confirm to both parties" }).click();
  await expect(arbiter.page.getByText("You confirmed this.")).toBeVisible();
  await expect(seller.page.getByText("The arbiter hasn't confirmed yet.")).toHaveCount(0);
  await expect(seller.page.getByText("confirmed", { exact: true })).toBeVisible();
  return { arbiter, seller, buyer, id };
}

async function openDispute(party: Party, reason: string) {
  await party.page.getByRole("button", { name: "Open a dispute…" }).click();
  await party.page.getByLabel("What went wrong?").fill(reason);
  await party.page.getByRole("button", { name: "Open dispute", exact: true }).click();
}

test("arbitration: the arbiter releases a disputed escrow to the seller, and the secret reaches only them", async ({ browser }) => {
  const { arbiter, seller, buyer, id } = await arbitratedLocked(browser, "e2e arbiter release");
  const secret = (await buyer.page.evaluate((id) => localStorage.getItem(`toshi.secret.${id}`), id))!;
  expect(secret).toMatch(/^[0-9a-f]{64}$/);

  // The buyer has the goods but will not release; the seller disputes.
  await openDispute(seller, "The buyer has the parcel and will not release.");
  for (const p of [seller.page, buyer.page, arbiter.page]) await expect(stateBadge(p, "Disputed")).toBeVisible();

  // The operator can no longer cancel it over the arbiter's head; the buyer and seller cannot decide.
  await expect(arbiter.page.getByText("A dispute is waiting for your decision.")).toBeVisible();
  await expect(seller.page.getByRole("button", { name: "Release to the seller" })).toHaveCount(0);

  await arbiter.page.getByLabel("Your note").fill("Delivery photo is genuine.");
  await arbiter.page.getByRole("button", { name: "Release to the seller" }).click();
  await arbiter.page.getByRole("button", { name: "Yes, pay the seller" }).click();
  for (const p of [seller.page, buyer.page, arbiter.page]) await expect(stateBadge(p, "Settled")).toBeVisible();
  // The note appears in the history more than once (the decision, and the settlement it caused).
  await expect(seller.page.getByText("Delivery photo is genuine.").first()).toBeVisible();

  // The buyer never sent the secret in the clear: only a sealed blob. The seller never saw it. The
  // arbiter sent it once, in the decision.
  const carrying = (p: Party) => p.requests.filter((r) => r.url.includes(secret) || r.body?.includes(secret));
  expect(carrying(buyer)).toHaveLength(0);
  expect(carrying(seller)).toHaveLength(0);
  expect(carrying(arbiter)).toHaveLength(1);
  expect(carrying(arbiter)[0].url).toMatch(/\/arbitration\/decide$/);
  const accept = buyer.requests.find((r) => r.url.endsWith(`/escrows/${id}/accept`))!;
  expect(JSON.parse(accept.body!).sealed_secret).toMatch(/^[A-Za-z0-9+/=]{160,}$/);
});

test("evidence: the seller marks delivery, both sides add evidence, and the arbiter reads it all when deciding", async ({ browser }) => {
  const { arbiter, seller, buyer } = await arbitratedLocked(browser, "e2e evidence");

  // Before anything is said, the page explains what the note is for.
  await expect(seller.page.getByText("Delivered? Say so, with proof")).toBeVisible();

  // The seller says it was delivered, with proof. Nothing is released: the money stays locked.
  await seller.page.getByLabel("Your note").fill("Signed for by J. Okafor, tracking NG123");
  await seller.page.getByRole("button", { name: "Mark as delivered" }).click();
  await expect(seller.page.getByText("You marked this delivered.")).toBeVisible();
  await expect(stateBadge(seller.page, "Funds locked")).toBeVisible();

  // The buyer is told, sees the proof, and answers with their side.
  await expect(buyer.page.getByText("The seller says this was delivered.")).toBeVisible();
  await expect(buyer.page.getByText("Signed for by J. Okafor, tracking NG123").first()).toBeVisible();
  await buyer.page.getByLabel("Your note").fill("The box was empty when I opened it");
  await buyer.page.getByRole("button", { name: "Add evidence" }).click();
  await expect(buyer.page.getByText("The box was empty when I opened it").first()).toBeVisible();
  await expect(seller.page.getByText("The box was empty when I opened it").first()).toBeVisible();

  // The buyer's dashboard asks for their move, not just the escrow page.
  await buyer.page.goto("/");
  await expect(buyer.page.getByText("The seller says it was delivered")).toBeVisible();
  await buyer.page.goBack();

  // An empty note is refused before it is sent.
  await buyer.page.getByRole("button", { name: "Add evidence" }).click();
  await expect(buyer.page.getByText("Write something first")).toBeVisible();

  // When the seller disputes, the arbiter finds both sides' words right where they decide.
  await openDispute(seller, "The buyer has the parcel and will not release.");
  await expect(arbiter.page.getByText("Delivery and evidence from both sides")).toBeVisible();
  await expect(arbiter.page.getByText("Signed for by J. Okafor, tracking NG123").first()).toBeVisible();
  await expect(arbiter.page.getByText("The box was empty when I opened it").first()).toBeVisible();
  // They are on the history too, in the order they were said.
  await expect(arbiter.page.getByText("Seller says it was delivered", { exact: true })).toBeVisible();
  await expect(arbiter.page.getByText("Evidence added", { exact: true }).first()).toBeVisible();
});

test("platform arbiter: a seller who names none gets the platform's, and it decides the dispute", async ({ browser }) => {
  // Needs a server started with PLATFORM_ARBITER_ID set to the seeded arbiter, which changes every
  // escrow's flow, so it is skipped on the usual server:
  //   PLATFORM_ARBITER_ID=$(jq -r .arbiter.id .demo/accounts.json) ./scripts/demo-env.sh up
  let cast: { arbiter: { id: string; token: string; fingerprint: string } } | null = null;
  let backup = "";
  try {
    cast = JSON.parse(readFileSync(resolve(REPO, ".demo/accounts.json"), "utf8"));
    backup = readFileSync(resolve(REPO, ".demo/arbiter-key.json"), "utf8");
  } catch {
    // no seeded cast
  }
  test.skip(!cast, "needs the seeded demo cast (.demo/accounts.json)");
  const base = process.env.WEB_URL ?? "http://localhost:3001";
  const def = await (await fetch(`${base}/api/v1/arbiter/default`, { headers: { authorization: `Bearer ${cast!.arbiter.token}` } })).json();
  test.skip(def.id !== cast!.arbiter.id, "the server has no platform arbiter set to the seeded arbiter");

  // The platform's arbiter holds its key in a browser, restored from the prepared backup.
  const context = await browser.newContext();
  const arbiter = await context.newPage();
  await arbiter.goto("/");
  await importToken(arbiter, cast!.arbiter.token);
  await arbiter.goto("/arbiter");
  await arbiter.getByLabel("Restore from a backup").fill(backup);
  await arbiter.getByRole("button", { name: "Restore", exact: true }).click();
  await expect(arbiter.getByText("The private key is on this device.")).toBeVisible();

  const seller = await newParty(browser);
  const buyer = await newParty(browser);

  // The form offers the platform's arbiter, selected, with its key fingerprint, and nothing was typed.
  await seller.page.goto("/escrows/new");
  await seller.page.getByLabel("Description").fill("e2e platform arbiter");
  await seller.page.getByRole("button", { name: "Continue" }).click();
  await expect(seller.page.getByRole("radio", { name: /The platform's arbiter/ })).toHaveAttribute("aria-checked", "true");
  await expect(seller.page.getByText(cast!.arbiter.fingerprint).first()).toBeVisible();
  await seller.page.getByLabel("Buyer's user id").fill(buyer.id);
  await seller.page.getByLabel("Amount (sats)").fill("1000");
  await seller.page.getByLabel("Hold time amount").fill("30");
  await seller.page.getByLabel("Hold time unit").selectOption("minutes");
  await seller.page.getByRole("button", { name: "Create escrow" }).click();
  await seller.page.waitForURL(/\/escrows\/[0-9a-f-]{36}$/);
  const id = seller.page.url().split("/").pop()!;
  await expect(seller.page.getByText("The platform's arbiter decides a dispute")).toBeVisible();

  // The buyer's browser seals the secret to the platform's key, as for any arbiter.
  await acceptAndPay(buyer, seller, id);
  await arbiter.goto(`/escrows/${id}`);
  await arbiter.getByRole("button", { name: "Check the sealed secret" }).click();
  await arbiter.getByRole("button", { name: "Confirm to both parties" }).click();
  await expect(arbiter.getByText("You confirmed this.")).toBeVisible();

  // The buyer has the goods but will not release; the seller disputes and the platform decides.
  await openDispute(seller, "The buyer has the parcel and will not release.");
  await expect(arbiter.getByText("A dispute is waiting for your decision.")).toBeVisible();
  await arbiter.getByLabel("Your note").fill("Proof of delivery accepted.");
  await arbiter.getByRole("button", { name: "Release to the seller" }).click();
  await arbiter.getByRole("button", { name: "Yes, pay the seller" }).click();
  for (const p of [seller.page, buyer.page, arbiter]) await expect(stateBadge(p, "Settled")).toBeVisible();
  await context.close();
});

test("arbitration: the arbiter refunds a disputed escrow to the buyer", async ({ browser }) => {
  const { arbiter, seller, buyer } = await arbitratedLocked(browser, "e2e arbiter refund");
  await openDispute(buyer, "Nothing arrived.");
  for (const p of [seller.page, buyer.page, arbiter.page]) await expect(stateBadge(p, "Disputed")).toBeVisible();

  await arbiter.page.getByLabel("Your note").fill("No proof of delivery.");
  await arbiter.page.getByRole("button", { name: "Refund the buyer" }).click();
  await arbiter.page.getByRole("button", { name: "Yes, refund the buyer" }).click();
  for (const p of [seller.page, buyer.page, arbiter.page]) await expect(stateBadge(p, "Refunded")).toBeVisible();
  // The note appears in the history more than once (the decision, and the refund it caused).
  await expect(buyer.page.getByText("No proof of delivery.").first()).toBeVisible();
});

test("arbitration: an arbiter without their private key on this device cannot decide, and a stranger is turned away", async ({ browser }) => {
  const { arbiter, id } = await arbitratedLocked(browser, "e2e arbiter wrong device");

  // The same account on a browser that does not hold the private key.
  const token = arbiter.token;
  const elsewhere = await browser.newContext();
  const page = await elsewhere.newPage();
  await page.goto("/");
  await importToken(page, token);
  await expect(page.getByRole("heading", { name: "My escrows" })).toBeVisible();
  await page.goto(`/escrows/${id}`);
  // Already confirmed, so there is nothing to check; the point is that no private key is needed to look.
  await expect(page.getByText("You confirmed this.")).toBeVisible();
  await page.goto("/arbiter");
  await expect(page.getByText("This browser does not have the private key for this fingerprint")).toBeVisible();

  const stranger = await newParty(browser);
  await stranger.page.goto(`/escrows/${id}`);
  await expect(stranger.page.getByText("Escrow not found")).toBeVisible();
});


test("a fiat value is shown beside the sats, worked out once at creation, and matches the preview", async ({ browser }) => {
  const seller = await newParty(browser);
  const buyer = await newParty(browser);
  const { page } = seller;
  await page.goto("/escrows/new");
  await page.getByLabel("Description").fill("e2e fiat value");
  await page.getByRole("button", { name: "Continue" }).click();
  // Needs a server with FIAT_PROVIDER set (the demo environment has it); skip where fiat is off.
  await page.getByLabel("Amount (sats)").waitFor();
  test.skip((await page.getByLabel("Also show the value in").count()) === 0, "fiat values are not enabled on this server");

  await page.getByLabel("Buyer's user id").fill(buyer.id);
  await page.getByLabel("Amount (sats)").fill("5000");
  await page.getByLabel("Hold time amount").fill("30");
  await page.getByLabel("Hold time unit").selectOption("minutes");
  await page.getByLabel("Also show the value in").selectOption("NGN");

  // The preview is computed in the browser from the server's rate: 5,000 sats at 97,500,000 NGN per bitcoin.
  await expect(page.getByText("≈ ₦4,875.00")).toBeVisible();
  await expect(page.getByText(/₦97,500,000\.00 per bitcoin \(demo rate\)/)).toBeVisible();

  await page.getByRole("button", { name: "Create escrow" }).click();
  await page.waitForURL(/\/escrows\/[0-9a-f-]{36}$/);
  const id = page.url().split("/").pop()!;

  // The escrow is still in sats, and shows the locked value, its rate, source, and that it is display only.
  await expect(page.getByText("5,000 sats").first()).toBeVisible();
  await expect(page.getByText("≈ ₦4,875.00 when offered")).toBeVisible();
  await expect(page.getByText(/demo rate .* display only/)).toBeVisible();

  // The buyer sees the same locked value, and it appears in their list.
  await buyer.page.goto(`/escrows/${id}`);
  await expect(buyer.page.getByText("≈ ₦4,875.00 when offered")).toBeVisible();
  await buyer.page.goto("/");
  await expect(buyer.page.getByText("≈ ₦4,875.00")).toBeVisible();
});

test("landing page: a visitor sees the pitch, makes an account from it without losing the token screen, and signing out returns to it", async ({ browser }) => {
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto("/");

  // The pitch and every section are there for someone who is not signed in.
  await expect(page.getByRole("heading", { level: 1, name: /nobody holds the money/i })).toBeVisible();
  for (const heading of [
    "Four steps, and nobody in the middle.",
    "Built around what happens next.",
    "Built so you don't have to trust us.",
    "What it doesn't do, said plainly.",
    "Start your first protected deal.",
  ]) {
    await expect(page.getByRole("heading", { name: heading })).toBeAttached();
  }
  // No app chrome on the landing page: it has its own navigation.
  await expect(page.getByRole("link", { name: "Dashboard" })).toHaveCount(0);

  // The calls to action lead to the forms.
  await page.getByRole("link", { name: "Get started" }).first().click();
  await expect(page).toHaveURL(/#start$/);

  // Making an account here signs you in at once, which swaps the page layout. The one-time screen that
  // shows the token must survive that (it once didn't: the swap rebuilt the page and lost it).
  await choosePassword(page);
  await page.getByRole("button", { name: "Create account" }).click();
  await expect(page.getByText("Save your token now")).toBeVisible();
  await page.getByLabel("I have saved my token somewhere safe").check();
  await page.getByRole("button", { name: "Continue" }).click();
  await expect(page.getByRole("heading", { name: "My escrows" })).toBeVisible();
  await expect(page.getByRole("heading", { level: 1, name: /nobody holds the money/i })).toHaveCount(0);

  // Signing out goes back to the landing page.
  await page.getByRole("button", { name: "Lock" }).click();
  await expect(page.getByRole("heading", { level: 1, name: /nobody holds the money/i })).toBeVisible();
});

test("dashboard: shows whose move it is, what is locked, and lets you filter your escrows", async ({ browser }) => {
  const seller = await newParty(browser);
  const buyer = await newParty(browser);

  // A new account has nothing yet, and is told how to start.
  await seller.page.goto("/");
  await expect(seller.page.getByRole("heading", { level: 1, name: /^(Good (morning|afternoon|evening)|Working late)$/ })).toBeVisible();
  await expect(seller.page.getByText("You're all caught up.")).toBeVisible();
  await expect(seller.page.getByText("No escrows yet.", { exact: true })).toBeVisible();

  // The seller makes an offer for the buyer. It is the buyer's move, not the seller's.
  const id = await createEscrow(seller, buyer.id, "e2e dashboard parcel", ["30", "minutes"]);
  await buyer.page.goto("/");
  const attention = buyer.page.getByRole("region", { name: "Needs your attention" });
  await expect(attention.getByText("Accept this offer")).toBeVisible();
  await expect(attention.getByText("e2e dashboard parcel")).toBeVisible();
  await seller.page.goto("/");
  await expect(seller.page.getByText("You're all caught up.")).toBeVisible();
  await expect(seller.page.getByText("Waiting for the buyer to accept")).toBeVisible();

  // The buyer accepts and pays: now the funds are locked, and both have a move to make.
  await seller.page.goto(`/escrows/${id}`); // the helper watches the seller's escrow page flip to locked
  await acceptAndPay(buyer, seller, id);
  await buyer.page.goto("/");
  await expect(buyer.page.getByRole("region", { name: "Needs your attention" }).getByText("Release when you have the goods")).toBeVisible();
  await expect(buyer.page.getByText("1 escrow · 1,000 sats out")).toBeVisible();
  await seller.page.goto("/");
  await expect(seller.page.getByRole("region", { name: "Needs your attention" }).getByText("Deliver, then release with the buyer's secret")).toBeVisible();
  await expect(seller.page.getByText("1 escrow · 1,000 sats in")).toBeVisible();

  // The tabs split active from completed; nothing has finished yet.
  const table = seller.page.getByRole("region", { name: "My escrows" });
  await expect(table.getByText("e2e dashboard parcel")).toBeVisible();
  await expect(table.getByRole("tab", { name: /Active/ })).toHaveAttribute("aria-selected", "true");
  await table.getByRole("tab", { name: /Completed/ }).click();
  await expect(table.getByText("Nothing completed yet.")).toBeVisible();
  await table.getByRole("tab", { name: /All/ }).click();
  await expect(table.getByText("e2e dashboard parcel")).toBeVisible();

  // A row opens the escrow.
  await table.getByText("e2e dashboard parcel").click();
  await seller.page.waitForURL(new RegExp(`/escrows/${id}$`));
});

test("escrow page: the progress moves, the next move changes hands, and a long description splits into a title and terms", async ({ browser }) => {
  const seller = await newParty(browser);
  const buyer = await newParty(browser);

  // The title leads the description and the rest become the terms.
  await seller.page.goto("/escrows/new");
  await seller.page.getByLabel("Deal title").fill("Fabric order to Abuja");
  await seller.page.getByLabel("Description").fill("Deliver by Friday noon\nFragile: keep dry");
  await seller.page.getByRole("button", { name: "Continue" }).click();
  await seller.page.getByLabel("Buyer's user id").fill(buyer.id);
  await seller.page.getByLabel("Amount (sats)").fill("2000");
  await seller.page.getByLabel("Hold time amount").fill("30");
  await seller.page.getByLabel("Hold time unit").selectOption("minutes");
  await seller.page.getByRole("button", { name: "Create escrow" }).click();
  await seller.page.waitForURL(/\/escrows\/[0-9a-f-]{36}$/);
  const id = seller.page.url().split("/").pop()!;

  const progress = (p: Party) => p.page.getByRole("navigation", { name: "Escrow progress" });
  const current = (p: Party) => progress(p).locator('[aria-current="step"]');
  const move = (p: Party) => p.page.getByRole("region", { name: "Your move" });

  // The long text became a title and a terms block, and the amount leads the page.
  await expect(seller.page.getByRole("heading", { level: 1, name: "Fabric order to Abuja" })).toBeVisible();
  const terms = seller.page.getByRole("region", { name: "Terms" });
  await expect(terms).toContainText("Deliver by Friday noon");
  await expect(terms).toContainText("Fragile: keep dry");
  await expect(seller.page.getByText("2,000 sats").first()).toBeVisible();

  // An offer: the tracker is on the first step; the seller waits and the buyer has the move.
  await expect(current(seller)).toContainText("Offer");
  await expect(move(seller)).toContainText("Waiting for the buyer to accept");
  await buyer.page.goto(`/escrows/${id}`);
  await expect(move(buyer)).toContainText("Accept this offer");
  await expect(buyer.page.getByText("you", { exact: true })).toBeVisible(); // marked as you in "People"

  // Accepting and paying moves the tracker along and passes the move to the right person.
  await acceptAndPay(buyer, seller, id);
  await expect(current(buyer)).toContainText("Funds locked");
  await expect(move(buyer)).toContainText("Release when you have the goods");
  await expect(move(seller)).toContainText("Deliver, then release with the buyer's secret");
  await expect(progress(buyer).getByText("Offer")).toBeVisible();

  // The side details say where the money goes, and the technical details open on demand.
  await expect(seller.page.getByText("Funds go to")).toBeVisible();
  await seller.page.getByText("Technical details").click();
  await expect(seller.page.getByText("Payment hash")).toBeVisible();
});

test("password vault: lock and unlock, wrong password, new device import, remove from device", async ({ browser }) => {
  const user = await newParty(browser);
  const { page } = user;

  // The token is not sitting in plain text on the device.
  const stored = await page.evaluate(() => JSON.stringify({ ...localStorage }));
  expect(stored).not.toContain(user.token);

  // Lock, then a wrong password is refused and the right one gets back in.
  await page.getByRole("button", { name: "Lock" }).click();
  await expect(page.getByRole("button", { name: "Unlock" })).toBeVisible();
  await page.getByLabel("Unlock password").fill("not the password at all");
  await page.getByRole("button", { name: "Unlock" }).click();
  await expect(page.getByText(/password is not right/i)).toBeVisible();
  await page.getByLabel("Unlock password").fill(PASSWORD);
  await page.getByRole("button", { name: "Unlock" }).click();
  await expect(page.getByRole("heading", { name: "My escrows" })).toBeVisible();

  // The account page shows the token again only after the password.
  await page.goto("/account#security");
  await page.getByLabel("Password to show token").fill(PASSWORD);
  await page.getByRole("button", { name: "Show my token" }).click();
  await expect(page.getByText(user.token)).toBeVisible();

  // A new device: the token plus a new password.
  const other = await (await browser.newContext()).newPage();
  await other.goto("/");
  await importToken(other, user.token, "another long password here");
  await expect(other.getByRole("heading", { name: "My escrows" })).toBeVisible();

  // Remove from this device (on the Profile tab): only the token gets back in.
  await page.getByRole("tab", { name: "Profile" }).click();
  await page.getByRole("button", { name: "Remove from this device" }).click();
  await page.getByRole("button", { name: "Remove from this device" }).last().click();
  await expect(page.getByLabel("Token", { exact: true })).toBeVisible();
});

test("backup: a buyer's release secret survives a new device through the backup file", async ({ browser }) => {
  const seller = await newParty(browser);
  const buyer = await newParty(browser);
  const id = await createEscrow(seller, buyer.id, "e2e backup", ["30", "minutes"]);
  await buyer.page.goto(`/escrows/${id}`);
  await buyer.page.getByRole("button", { name: /Accept offer/ }).click();
  await expect(buyer.page.getByRole("button", { name: "Copy invoice" })).toBeVisible();

  // The secret is new and not backed up: the page says so.
  await expect(buyer.page.getByText(/aren't backed up|isn't backed up/)).toBeVisible();
  const secret = await buyer.page.evaluate((id) => localStorage.getItem(`toshi.secret.${id}`), id);
  expect(secret).toMatch(/^[0-9a-f]{64}$/);

  await buyer.page.goto("/account#backup");
  await buyer.page.getByLabel("Backup password").fill(PASSWORD);
  const [download] = await Promise.all([
    buyer.page.waitForEvent("download"),
    buyer.page.getByRole("button", { name: "Download backup" }).click(),
  ]);
  const path = await download.path();
  const text = (await import("node:fs")).readFileSync(path, "utf8");
  expect(text).not.toContain(secret!);
  expect(text).not.toContain(buyer.token);
  await expect(buyer.page.getByText(/all backed up/)).toBeVisible();

  // A new device: sign in with the token, then restore the file.
  const other = await (await browser.newContext()).newPage();
  await other.goto("/");
  await importToken(other, buyer.token);
  await expect(other.getByRole("heading", { name: "My escrows" })).toBeVisible();
  await other.goto("/account#backup");
  await other.getByLabel("Backup file", { exact: true }).setInputFiles(path);
  await other.getByLabel("Password of the backup file").fill("not the password at all");
  await other.getByRole("button", { name: "Restore" }).click();
  await expect(other.getByText(/password is not right for this backup/)).toBeVisible();
  await other.getByLabel("Password of the backup file").fill(PASSWORD);
  await other.getByRole("button", { name: "Restore" }).click();
  await expect(other.getByText("Restored 1 item.")).toBeVisible();
  expect(await other.evaluate((id) => localStorage.getItem(`toshi.secret.${id}`), id)).toBe(secret);
});

test("API keys: a business makes a key, its software uses it, and revoking stops it", async ({ browser }) => {
  const shop = await newParty(browser);
  const { page } = shop;
  await page.goto("/developers#keys");

  await page.getByLabel("Key name").fill("Online shop");
  await page.getByRole("button", { name: "Make key" }).click();
  await expect(page.getByText("Copy this key now.")).toBeVisible();
  const key = (await page.locator("code", { hasText: /^tk_/ }).first().textContent())!.trim();
  expect(key).toMatch(/^tk_[0-9a-f]{64}$/);
  await page.getByRole("button", { name: "I have copied it" }).click();

  // The list shows the key by its last characters only, and never the key itself.
  await expect(page.getByText(`tk_…${key.slice(-4)}`)).toBeVisible();
  await expect(page.getByText(key)).toHaveCount(0);

  // The business's own software calls the API with it, not with the account token.
  const call = (method: "get" | "post", path: string, data?: object) =>
    page.request[method](path, {
      headers: { Authorization: `Bearer ${key}`, "Content-Type": "application/json" },
      ...(data ? { data } : {}),
    });
  const made = await call("post", "/api/v1/escrows", { amount_sats: 1000, description: "order 1001", hold_secs: 3600 });
  expect(made.status()).toBe(201);
  expect((await made.json()).seller_id).toBe(shop.id);
  expect((await call("get", "/api/v1/escrows")).status()).toBe(200);
  // Keys are closed out of the account and the node.
  expect((await call("get", "/api/v1/me/node")).status()).toBe(403);
  expect((await call("get", "/api/v1/me/api-keys")).status()).toBe(403);

  // Revoke it: the very next call is refused.
  await page.getByRole("button", { name: "Revoke", exact: true }).click();
  await page.getByRole("button", { name: "Revoke key" }).click();
  await expect(page.getByText(/revoked/)).toBeVisible();
  expect((await call("get", "/api/v1/escrows")).status()).toBe(401);
});

test("checkout: a business's offer carries its order number, and the customer gets a link back after accepting", async ({ browser }) => {
  const shop = await newParty(browser);
  const customer = await newParty(browser);

  // The business's software makes a key, then an offer for this customer with its own order number.
  const k = await shop.page.request.post("/api/v1/me/api-keys", {
    headers: { Authorization: `Bearer ${shop.token}` },
    data: { name: "checkout", scopes: ["escrows:read", "escrows:write"] },
  });
  const key = (await k.json()).key as string;
  const offer = (data: object) =>
    shop.page.request.post("/api/v1/escrows", {
      headers: { Authorization: `Bearer ${key}`, "Idempotency-Key": crypto.randomUUID() },
      data: { amount_sats: 1000, description: "Parcel to Ikeja", hold_secs: 1800, ...data },
    });

  // A link that could mislead the customer is refused outright.
  const bad = await offer({ buyer_id: customer.id, return_url: "https://shop.example.com@evil.example/" });
  expect(bad.status()).toBe(400);
  expect((await bad.json()).error.code).toBe("invalid_return_url");

  const made = await offer({ buyer_id: customer.id, reference: "ORD-1001", return_url: "https://shop.example.com/thanks?o=1001" });
  expect(made.status()).toBe(201);
  const id = (await made.json()).id as string;

  // The customer is sent to the escrow page: the order shows, and there is no way out yet.
  await customer.page.goto(`/escrows/${id}`);
  await expect(customer.page.getByText("Order ORD-1001").first()).toBeVisible();
  await expect(customer.page.getByRole("link", { name: /Return to/ })).toHaveCount(0);

  // After they accept, the link back appears with the shop's address spelled out.
  await customer.page.getByRole("button", { name: /Accept offer/ }).click();
  await expect(customer.page.getByRole("button", { name: "Copy invoice" })).toBeVisible();
  const back = customer.page.getByRole("link", { name: /Return to shop\.example\.com/ });
  await expect(back).toBeVisible();
  await expect(back).toHaveAttribute("href", "https://shop.example.com/thanks?o=1001");
  await expect(back).toHaveAttribute("rel", /noopener/);

  // The seller sees the order number too, but no return link (that is for the buyer).
  await shop.page.goto(`/escrows/${id}`);
  await expect(shop.page.getByText("Order ORD-1001").first()).toBeVisible();
  await expect(shop.page.getByRole("link", { name: /Return to/ })).toHaveCount(0);
});

test("webhooks: a business adds an address, gets a signed test, and is told when an escrow changes", async ({ browser }) => {
  const shop = await newParty(browser);
  const customer = await newParty(browser);

  // The business's own server, listening for messages.
  const received: { headers: Record<string, string | string[] | undefined>; body: string }[] = [];
  const server = createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      received.push({ headers: req.headers, body });
      res.statusCode = 200;
      res.end("ok");
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as { port: number }).port;

  try {
    const { page } = shop;
    await page.goto("/developers#webhooks");
    await page.getByLabel("Webhook address").fill(`http://127.0.0.1:${port}/toshi`);
    await page.getByRole("button", { name: "Add address" }).click();
    await expect(page.getByText("Copy the signing secret now.")).toBeVisible();
    const secret = (await page.locator("code", { hasText: /^whsec_/ }).first().textContent())!.trim();
    expect(secret).toMatch(/^whsec_[0-9a-f]{64}$/);
    await page.getByRole("button", { name: "I have copied it" }).click();
    await expect(page.getByText(secret)).toHaveCount(0);

    // The signature a receiver would check: HMAC-SHA256 of "<t>.<body>" with the secret.
    const verify = (m: (typeof received)[number]) => {
      const sig = String(m.headers["toshi-signature"]);
      const [, t, v1] = /^t=(\d+),v1=([0-9a-f]{64})$/.exec(sig)!;
      return createHmac("sha256", secret).update(`${t}.${m.body}`).digest("hex") === v1;
    };

    // "Send test" reaches the server and the page says so.
    await page.getByRole("button", { name: "Send test" }).click();
    await expect(page.getByText(/Delivered: your server answered 200/)).toBeVisible();
    expect(received).toHaveLength(1);
    expect(JSON.parse(received[0].body).type).toBe("webhook.test");
    expect(verify(received[0])).toBe(true);

    // A real change: the shop makes an offer and the customer accepts it on our page.
    const made = await page.request.post("/api/v1/escrows", {
      headers: { Authorization: `Bearer ${shop.token}` },
      data: { buyer_id: customer.id, amount_sats: 1000, description: "Parcel", hold_secs: 1800, reference: "ORD-2002" },
    });
    expect(made.status()).toBe(201);
    const id = (await made.json()).id as string;
    await customer.page.goto(`/escrows/${id}`);
    await customer.page.getByRole("button", { name: /Accept offer/ }).click();
    await expect(customer.page.getByRole("button", { name: "Copy invoice" })).toBeVisible();

    await expect.poll(() => received.length, { timeout: 20_000 }).toBeGreaterThanOrEqual(2);
    const event = JSON.parse(received[1].body);
    expect(event.type).toBe("escrow.accepted");
    expect(event.data.state).toBe("INVOICE_ISSUED");
    expect(event.data.escrow.id).toBe(id);
    expect(event.data.escrow.reference).toBe("ORD-2002");
    expect(verify(received[1])).toBe(true);
    // Nothing from before the address was added, and nothing twice.
    await page.waitForTimeout(5000);
    expect(received.filter((m) => JSON.parse(m.body).type === "escrow.accepted")).toHaveLength(1);

    // Delete it from the page: the list empties.
    await page.reload();
    await page.getByRole("button", { name: "Delete", exact: true }).click();
    await page.getByRole("button", { name: "Delete address" }).click();
    await expect(page.getByText("No addresses yet.")).toBeVisible();
  } finally {
    server.close();
  }
});
