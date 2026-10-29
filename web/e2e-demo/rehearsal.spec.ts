// The demo, played exactly as a presenter would, with the seeded cast from `scripts/demo-env.sh up`.
// The script it follows is docs/demo.md. Screenshots land in rehearsal-screenshots/ (gitignored).
//
//   ./scripts/demo-env.sh up
//   (cd web && npm run rehearse)

import { execFile } from "node:child_process";
import { mkdirSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { promisify } from "node:util";
import { expect, test, type Browser, type BrowserContext, type Page } from "@playwright/test";

const REPO = resolve(__dirname, "../..");
const SHOTS = resolve(__dirname, "../rehearsal-screenshots");
mkdirSync(SHOTS, { recursive: true });

type Cast = Record<"courier" | "customer" | "arbiter", { id: string; token: string; fingerprint?: string }>;
const cast: Cast = JSON.parse(readFileSync(resolve(REPO, ".demo/accounts.json"), "utf8"));
const arbiterBackup = readFileSync(resolve(REPO, ".demo/arbiter-key.json"), "utf8");

const run = promisify(execFile);
const COMPOSE = ["compose", "-f", resolve(REPO, "regtest/docker-compose.yml")];
async function channelBalance(node: string): Promise<number> {
  const { stdout } = await run("docker", [...COMPOSE, "exec", "-T", node, "lncli", "--network=regtest", "channelbalance"]);
  return Number(JSON.parse(stdout).local_balance.sat);
}

const badge = (page: Page, label: string) => page.locator("span[title]", { hasText: label });
const PHONE = { width: 390, height: 844 };

type Persona = { page: Page; context: BrowserContext };

/** Sign in the way a person would: paste the account token on the home page. */
async function signedIn(browser: Browser, who: keyof Cast, viewport?: { width: number; height: number }): Promise<Persona> {
  const context = await browser.newContext(viewport ? { viewport } : {});
  const page = await context.newPage();
  await page.goto("/");
  await page.getByLabel("Token", { exact: true }).fill(cast[who].token);
  const password = "rehearsal password 123";
  await page.getByLabel("New password for this device", { exact: true }).fill(password);
  await page.getByLabel("Confirm the new password").fill(password);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("heading", { name: "My escrows" })).toBeVisible();
  return { page, context };
}

async function shot(p: Persona, name: string) {
  await p.page.screenshot({ path: resolve(SHOTS, `${name}.png`), fullPage: true });
}

/** The courier makes an offer. Leave `arbiter` out for a plain one; the buyer is left open (a link). */
async function makeOffer(
  courier: Persona,
  what: string,
  sats: string,
  hold: [string, "minutes"],
  arbiter?: string,
  currency?: string,
) {
  const { page } = courier;
  await page.goto("/escrows/new");
  await page.getByLabel("Description").fill(what);
  await page.getByRole("button", { name: "Continue" }).click();
  if (arbiter) await page.getByLabel("Arbiter's user id").fill(arbiter);
  if (currency) await page.getByLabel("Also show the value in").selectOption(currency);
  await page.getByLabel("Amount (sats)").fill(sats);
  await page.getByLabel("Hold time amount").fill(hold[0]);
  await page.getByLabel("Hold time unit").selectOption(hold[1]);
  await page.getByRole("button", { name: "Create escrow" }).click();
  await page.waitForURL(/\/escrows\/[0-9a-f-]{36}$/);
  return page.url().split("/").pop()!;
}

/** The customer opens the courier's link, accepts, and pays with one click. */
async function acceptAndPay(customer: Persona, id: string) {
  const { page } = customer;
  await page.goto(`/escrows/${id}`);
  await page.getByRole("button", { name: /Accept offer/ }).click();
  await expect(page.getByRole("button", { name: /with my wallet/ })).toBeVisible();
  await page.getByRole("button", { name: /with my wallet/ }).click();
  await expect(badge(page, "Funds locked")).toBeVisible();
}

const t0 = Date.now();
const stamp = (label: string) => console.log(`  [${((Date.now() - t0) / 1000).toFixed(0).padStart(4)}s] ${label}`);

test.afterEach(async ({ browser }) => {
  await Promise.all(browser.contexts().map((c) => c.close().catch(() => {})));
});

test.describe("scene 1: a delivery, paid on handover", () => {
  test("courier makes an offer, customer pays, handover by QR, courier is paid on their own node", async ({ browser }) => {
    const before = await channelBalance("seller2");
    const courier = await signedIn(browser, "courier");
    const customer = await signedIn(browser, "customer", PHONE);
    stamp("both signed in");

    // The courier names no buyer: the escrow page is a link to send to the customer.
    // The parcel goes to Lagos, so the courier also shows what 5,000 sats is worth in naira.
    const id = await makeOffer(courier, "Parcel #4821 to Lekki, Lagos", "5000", ["30", "minutes"], undefined, "NGN");
    await expect(courier.page.getByText("≈ ₦4,875.00 when offered")).toBeVisible();
    await expect(courier.page.getByText("Funds go to").locator("..").getByText("The seller's own node")).toBeVisible();
    await shot(courier, "1a-courier-offer");
    stamp("offer created");

    await acceptAndPay(customer, id);
    // The customer sees the same locked value in naira, marked as display only.
    await expect(customer.page.getByText("≈ ₦4,875.00 when offered")).toBeVisible();
    await expect(badge(courier.page, "Funds locked")).toBeVisible();
    await expect(courier.page.getByText("Funds are locked: you can start the work.")).toBeVisible();
    // The stand-in wallet says what it is on screen, so nobody in the room is misled.
    await expect(customer.page.getByText("Demo wallet · regtest")).toBeVisible();
    await shot(customer, "1b-customer-locked-phone");
    await shot(courier, "1c-courier-locked");
    stamp("funds locked on both screens");

    // Handover: the customer, holding the parcel, reveals the QR; the courier's phone opens the link.
    await customer.page.getByRole("button", { name: "Reveal release QR" }).click();
    await customer.page.getByRole("button", { name: "Yes, show the QR" }).click();
    await expect(customer.page.getByRole("img", { name: "Release QR" })).toBeVisible();
    await shot(customer, "1d-customer-release-qr-phone");
    const secret = (await customer.page.evaluate((id) => localStorage.getItem(`toshi.secret.${id}`), id))!;
    await courier.page.goto(`/escrows/${id}#release=${secret}`);
    await shot(courier, "1e-courier-scanned");
    await courier.page.getByRole("button", { name: /^Release 5,000 sats/ }).click();
    await expect(badge(courier.page, "Settled")).toBeVisible();
    await expect(badge(customer.page, "Settled")).toBeVisible();
    await shot(courier, "1f-courier-paid");
    stamp("settled");

    // The money is on the courier's own node, not the service's.
    await expect.poll(() => channelBalance("seller2"), { timeout: 20_000 }).toBe(before + 5000);
    stamp("courier's node balance rose by 5,000 sats");
  });
});

test.describe("scene 2: nobody shows up, the customer is refunded automatically", () => {
  test("a short hold expires and the money goes back with no human action", async ({ browser }) => {
    const courier = await signedIn(browser, "courier");
    const customer = await signedIn(browser, "customer", PHONE);
    const id = await makeOffer(courier, "Parcel #4822 (the courier never arrives)", "3000", ["1.25", "minutes"]); // 75 s: the shortest allowed is 60 s, and the customer needs a few seconds to accept
    await acceptAndPay(customer, id);
    await shot(customer, "2a-customer-countdown-phone");
    stamp("locked; waiting out the deadline with nobody acting");
    await expect(badge(customer.page, "Refunded")).toBeVisible({ timeout: 120_000 });
    await expect(badge(courier.page, "Refunded")).toBeVisible();
    await shot(customer, "2b-customer-refunded-phone");
    stamp("refunded");
  });
});

test.describe("scene 3: a dispute, decided by a trusted arbiter", () => {
  test("the customer withholds the secret, the arbiter looks at the evidence and pays the courier", async ({ browser }) => {
    const arbiter = await signedIn(browser, "arbiter");
    // The arbiter's key lives in their browser. Restore the prepared backup, as they would on a new device.
    await arbiter.page.goto("/arbiter");
    await arbiter.page.getByLabel("Restore from a backup").fill(arbiterBackup);
    await arbiter.page.getByRole("button", { name: "Restore", exact: true }).click();
    await expect(arbiter.page.getByText("Restored the key")).toBeVisible();
    await expect(arbiter.page.getByText("The private key is on this device.")).toBeVisible();
    await shot(arbiter, "3a-arbiter-key");

    const courier = await signedIn(browser, "courier");
    const customer = await signedIn(browser, "customer", PHONE);
    const id = await makeOffer(courier, "Parcel #4823, signature on delivery", "8000", ["30", "minutes"], cast.arbiter.id);
    await expect(courier.page.getByText("This escrow has an arbiter")).toBeVisible();
    await expect(courier.page.getByText(cast.arbiter.fingerprint!).first()).toBeVisible();
    await shot(courier, "3b-courier-names-arbiter");

    await acceptAndPay(customer, id);
    await expect(courier.page.getByText("The arbiter hasn't confirmed yet.")).toBeVisible();
    await shot(courier, "3c-courier-waiting-for-arbiter");

    await arbiter.page.goto(`/escrows/${id}`);
    await arbiter.page.getByRole("button", { name: "Check the sealed secret" }).click();
    await expect(arbiter.page.getByText("The sealed secret opens and matches the payment hash.")).toBeVisible();
    await arbiter.page.getByRole("button", { name: "Confirm to both parties" }).click();
    await expect(courier.page.getByText("The arbiter hasn't confirmed yet.")).toHaveCount(0);
    stamp("arbiter verified and confirmed");

    // The courier delivered; the customer will not release. The courier opens a dispute.
    await courier.page.getByRole("button", { name: "Open a dispute…" }).click();
    await courier.page.getByLabel("What went wrong?").fill("Delivered at 14:05, signed for by the customer's wife. Customer will not release.");
    await courier.page.getByRole("button", { name: "Open dispute", exact: true }).click();
    for (const p of [courier, customer, arbiter]) await expect(badge(p.page, "Disputed")).toBeVisible();
    await shot(arbiter, "3d-arbiter-dispute");
    stamp("dispute opened");

    // The customer has a short time to answer; saying their side ends the arbiter's wait at once.
    await customer.page.getByLabel("Your note").fill("The parcel was left with a neighbour, I was not home.");
    await customer.page.getByRole("button", { name: "Add evidence" }).click();
    await expect(arbiter.page.getByRole("button", { name: "Release to the seller" })).toBeEnabled();
    stamp("customer answered");

    await arbiter.page.getByLabel("Your note").fill("Signed delivery note matches. Releasing to the courier.");
    await arbiter.page.getByRole("button", { name: "Release to the seller" }).click();
    await arbiter.page.getByRole("button", { name: "Yes, pay the seller" }).click();
    for (const p of [courier, customer, arbiter]) await expect(badge(p.page, "Settled")).toBeVisible();
    // The note is in the history twice when the arbiter's request records the settlement itself.
    await expect(customer.page.getByText("Signed delivery note matches.").first()).toBeVisible();
    await shot(customer, "3e-customer-after-decision-phone");
    await shot(courier, "3f-courier-paid");
    stamp("arbiter released the payment to the courier");
  });
});
