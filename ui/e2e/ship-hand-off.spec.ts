/**
 * Ship's Hand off (docs/ci.md W26-R18, R02), as review-ship.html draws it, against the mock: a
 * repository whose newest commit is a person's failure (hyper-raft's, W26-R29) offers Hand off
 * first, then Retry; handed off, its failure is on the agent
 * the repository has attached (ci-doctor) and its row says so, with Retry alone. An agent's change
 * hands its open thread to its author's agent; its row then reads that agent at work, with no Hand
 * off and Review pair. An agent's stacked diff and change request alike hand both their threads
 * over, read fixing, and once the agent's fix is in, v2 is the repository's newest and the handed
 * revision's page says "Fixed in v2". The row holds its height throughout. A browser that can sign
 * (WebCrypto, on a secure page: the mock's 127.0.0.1 is one) hands off with its own key; one that
 * can't has kiln sign it for the person, and says why (docs/ci.md W27-R08). What a browser hands
 * off, and what it sent with it, is its own (the mock keeps them by user agent), reset before and
 * after.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

const rowOf = (page: Page, text: string) => page.getByRole("list", { name: "Repositories", exact: true }).locator(":scope > li", { hasText: text }).first();

/** What this browser sent with its hand-offs, as the mock kept it: its own hand-on, or why it had none. */
const sentOf = async (page: Page) =>
  (
    (await (await page.request.get("/api/mock/ship/hand-offs")).json()) as {
      hand_offs: { hand_on?: { dsseEnvelope?: { payloadType?: string; signatures?: { sig?: string }[] }; verificationMaterial?: { certificate?: { rawBytes?: string } } }; unsigned?: string }[];
    }
  ).hand_offs;

test("a person's failure goes to the agent attached to it, and its row says who has it", async ({ page }) => {
  await page.request.post("/api/mock/ship/hand-off?reset=1");
  try {
    await page.goto("/ship");
    // Ship's rows are repositories (W26-R29): hyper-raft's newest is a person's failure.
    const row = rowOf(page, "hyper-light/hyper-raft");
    await expect(row).toContainText("rust lint + test · assertion failed in raft::lease");
    const acts = row.locator(".ship-acts > button");
    await expect(acts.nth(0)).toHaveText("Hand off");
    await expect(acts.nth(1)).toHaveText("Retry");
    const before = (await row.boundingBox())?.height ?? 0;

    await row.getByRole("button", { name: "Hand off" }).click();
    await expect(row).toContainText("ci-doctor fixing · assertion failed in raft::lease", { timeout: 15_000 });
    // Handed off from this browser, signed with its own key (W27-R08): the person's hand-on went with
    // it, and nothing says kiln signed it for them.
    const [sent, ...more] = await sentOf(page);
    expect(more).toEqual([]);
    expect(sent?.unsigned).toBeUndefined();
    expect(sent?.hand_on?.dsseEnvelope?.payloadType).toBe("application/vnd.kiln.custody+json");
    expect(sent?.hand_on?.dsseEnvelope?.signatures?.[0]?.sig?.length ?? 0).toBeGreaterThan(0);
    expect(sent?.hand_on?.verificationMaterial?.certificate?.rawBytes?.length ?? 0).toBeGreaterThan(0);
    await expect(page.getByText("kiln signed the hand-off for you")).toHaveCount(0);
    await expect(row.getByRole("button", { name: "Hand off" })).toHaveCount(0);
    await expect(row.getByRole("button", { name: "Retry" })).toBeVisible();
    expect((await row.boundingBox())?.height ?? 0).toBe(before);
  } finally {
    await page.request.post("/api/mock/ship/hand-off?reset=1");
  }
});

test("a browser that can't sign has kiln sign the hand-off for the person, and says why", async ({ page }) => {
  // A page that isn't secure has no WebCrypto: as here.
  await page.addInitScript(() => Object.defineProperty(globalThis.crypto, "subtle", { value: undefined }));
  await page.request.post("/api/mock/ship/hand-off?reset=1");
  try {
    await page.goto("/ship");
    const row = rowOf(page, "sync: a move carries its homes");
    await row.getByRole("button", { name: "Hand off" }).click();
    await expect(row).toContainText("ci-doctor fixing · assertion failed in sync::reconnect");
    await expect(page.getByText("kiln signed the hand-off for you: this page isn't secure (https), so this browser has no key. Over https, this browser signs its own.")).toBeVisible();
    expect(await sentOf(page)).toEqual([{ unsigned: "this page isn't secure (https), so this browser has no key" }]);
  } finally {
    await page.request.post("/api/mock/ship/hand-off?reset=1");
  }
});

test("a change's open thread goes to its author's agent, and its row reads that agent at work", async ({ page }) => {
  await page.request.post("/api/mock/ship/hand-off?reset=1");
  try {
    await page.goto("/ship");
    const row = rowOf(page, "pipeline: dynamic matrices");
    await expect(row).toContainText("kim: empty matrix meant? · 1 thread");
    const before = (await row.boundingBox())?.height ?? 0;

    await row.getByRole("button", { name: "Hand off" }).click();
    await expect(row).toContainText("matrix fixing · kim's thread");
    await expect(row.getByRole("button", { name: "Hand off" })).toHaveCount(0);
    await expect(row.getByRole("button", { name: "Review" })).toHaveCount(0);
    expect((await row.boundingBox())?.height ?? 0).toBe(before);
  } finally {
    await page.request.post("/api/mock/ship/hand-off?reset=1");
  }
});

/** A row's own line (its head), whose height holds while what it says changes. */
const headHeight = async (row: ReturnType<typeof rowOf>) => (await row.locator(":scope > .ship-r").boundingBox())?.height ?? 0;

for (const [kind, name, title, sha] of [
  ["diff", "D45", "matrix: cap the expansion", "d5".repeat(20)],
  ["request", "CR15", "sched: cap a share", "c5".repeat(20)],
] as const) {
  test(`w26_r18 an agent's ${kind === "diff" ? "stacked diff" : "change request"} hands both its threads to its agent, reads fixing, then fixed in v2`, async ({ page }) => {
    await page.request.post("/api/mock/ship/hand-off?reset=1");
    await page.request.post(`/api/mock/ship/arrive?agent=${kind}`);
    try {
      await page.goto("/ship");
      const row = rowOf(page, title);
      await expect(row).toContainText(name);
      await expect(row).toContainText("kim: what caps it? · 2 threads");
      const before = await headHeight(row);

      await row.getByRole("button", { name: "Hand off" }).click();
      await expect(row).toContainText("matrix fixing · 2 threads");
      await expect(row.getByRole("button", { name: "Hand off" })).toHaveCount(0);
      expect(await headHeight(row)).toBe(before);

      // The agent's fix is in: v2 is the repository's newest, its checks running, and the handed
      // revision's own page says which version fixed it, once (rows fold nothing since W26-R29).
      expect((await page.request.post(`/api/mock/ship/fixed?sha=${sha}`)).status()).toBe(200);
      await page.reload();
      const tip = rowOf(page, title);
      await expect(tip).toContainText("Checks running");
      expect(await headHeight(tip)).toBe(before);
      await page.goto(`/ship/hyper-light/kiln/${sha}`);
      const handed = page.getByRole("region", { name: "Commit", exact: true });
      await expect(handed.locator(".ship-stand")).toHaveText("Superseded");
      await expect(handed.locator(".ship-commit__line")).toHaveText("Fixed in v2");
    } finally {
      await page.request.post("/api/mock/ship/hand-off?reset=1");
      await page.request.post("/api/mock/ship/arrive?clear=1");
    }
  });
}

test("asked again, Hand off says there's nothing open to hand", async ({ page }) => {
  await page.request.post("/api/mock/ship/hand-off?reset=1");
  try {
    const sha = "3".repeat(40);
    const first = await page.request.post("/api/ship/hand-off", { data: { repo: "hyper-light/kiln", sha } });
    expect(first.status()).toBe(200);
    expect(await first.json()).toMatchObject({ handed: { to: "ci-doctor", what: ["checks"] } });
    const again = await page.request.post("/api/ship/hand-off", { data: { repo: "hyper-light/kiln", sha } });
    expect(again.status()).toBe(409);
    expect(await again.json()).toMatchObject({ error: "3333333 has nothing open to hand off" });
  } finally {
    await page.request.post("/api/mock/ship/hand-off?reset=1");
  }
});
