/**
 * A commit's own page (Commit page plan 01a175b7f26e724d9159b4120a514fee), against the mock's fixed
 * rows: an opened row's Commit link leads to /ship/{owner}/{name}/{sha}, which loads; every commit
 * Ship lists has its page; one kiln doesn't know says so instead of breaking (step 1). A failing
 * commit shows what failed before anything else, then its six stage cards and its trace, failures
 * first; one nothing failed in starts at its stages (step 2). It says what ships with it, where each
 * environment stands from it with the one image they run, and its runs link to their pages (step 3).
 * Its Build names which platforms built and shows a build a newer push superseded as superseded
 * (Build and replant plan 01a175b7f27570ee9f5b248d7bb40c0f, step 1).
 */
import { expect, test } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

test("Commit on an opened row leads to the commit's own page", async ({ page }) => {
  await page.goto("/ship");
  const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator(".ship-go").click();
  const link = row.getByRole("link", { name: /^Commit [0-9a-f]{7}$/ });
  await expect(link).toBeVisible({ timeout: 15_000 });
  const short = ((await link.textContent()) ?? "").replace("Commit ", "");
  await link.click();
  await expect(page).toHaveURL(new RegExp(`/ship/hyper-light/kiln/${short}[0-9a-f]*$`));
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("pipeline: dynamic matrices");
  await expect(page.getByRole("region", { name: "Commit" })).toContainText(short);
});

test("every commit Ship lists has its page, and one kiln doesn't know says so", async ({ page, isMobile }) => {
  test.skip(isMobile, "the desktop projects visit them all");
  test.setTimeout(120_000);
  const list = (await (await page.request.get("/api/ship/commits")).json()) as { commits: { repo: string; sha: string; short: string; message: string }[] };
  expect(list.commits.length).toBeGreaterThan(0);
  for (const c of list.commits) {
    await page.goto(`/ship/${c.repo}/${c.sha}`);
    const commit = page.getByRole("region", { name: "Commit" });
    await expect(commit, `${c.short}'s page`).toContainText(c.short, { timeout: 15_000 });
    if (c.message) await expect(page.getByRole("heading", { level: 1 })).toHaveText(c.message);
  }
  await page.goto("/ship/hyper-light/kiln/0000000000000000000000000000000000000000");
  await expect(page.getByRole("alert")).toContainText("Couldn't load the commit");
});

/** The page of the listed commit with this message (the first, newest, when two share it). */
const pageOf = async (page: import("@playwright/test").Page, message: string) => {
  const list = (await (await page.request.get("/api/ship/commits")).json()) as { commits: { repo: string; sha: string; message: string }[] };
  const c = list.commits.find((x) => x.message === message);
  if (!c) throw new Error(`no commit "${message}"`);
  await page.goto(`/ship/${c.repo}/${c.sha}`);
  const commit = page.getByRole("region", { name: "Commit" });
  return { commit, sections: commit.locator(":scope > section") };
};

test("a failing commit shows what failed before anything else, then its stages and its trace, failures first", async ({ page }) => {
  const { commit, sections } = await pageOf(page, "sync: a move carries its homes");
  await expect(sections.first()).toHaveAttribute("aria-label", "Errors", { timeout: 15_000 });
  const errors = commit.getByRole("region", { name: "Errors" });
  await expect(errors).toContainText("rust lint + test");
  await expect(errors).toContainText("assertion failed in sync::reconnect");
  await expect(errors.getByRole("link", { name: "Its log" })).toHaveAttribute("href", /\/runs\/[^/]+\/jobs\/[^/]+$/);
  await expect(sections.nth(1)).toHaveAttribute("aria-label", "Stages");
  await expect(commit.getByRole("region", { name: "Stages" }).locator(".ship-job")).toHaveCount(6);
  await expect(sections.nth(2)).toHaveAttribute("aria-label", "Build");
  await expect(sections.nth(3)).toHaveAttribute("aria-label", "Trace");
  await expect(commit.getByRole("region", { name: "Trace" }).locator(".ship-lane__name").first()).toContainText("rust lint + test");
});

test("a commit nothing failed in starts at its stages", async ({ page }) => {
  const { commit, sections } = await pageOf(page, "deploy: a rollback holds");
  await expect(sections.first()).toHaveAttribute("aria-label", "Stages", { timeout: 15_000 });
  await expect(commit.getByRole("region", { name: "Errors" })).toHaveCount(0);
});

test("its runs link to their pages, and it says where each environment stands with the one image", async ({ page }) => {
  const { commit } = await pageOf(page, "deploy: a rollback holds");
  const runs = commit.getByRole("region", { name: "Runs" }).getByRole("link");
  await expect(runs.first()).toBeVisible({ timeout: 15_000 });
  const hrefs = await runs.evaluateAll((els) => els.map((el) => el.getAttribute("href") ?? ""));
  expect(hrefs.length).toBeGreaterThan(0);
  const envs = commit.getByRole("region", { name: "Environments" });
  await expect(envs).toContainText("staging");
  await expect(envs).toContainText("production");
  await expect(envs).toContainText("runs it now");
  await expect(envs).toContainText("staging and production run the same image");
  for (const href of hrefs) {
    expect(href).toMatch(/\/runs\/[^/]+$/);
    await page.goto(href);
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible({ timeout: 15_000 });
    await expect(page.getByRole("alert")).toHaveCount(0);
  }
});

test("what ships with it: a change's tip lists the commit it carries, which rides with the tip", async ({ page }) => {
  const list = (await (await page.request.get("/api/ship/commits")).json()) as { commits: { repo: string; sha: string; short: string; stand: string; message: string }[] };
  const tip = list.commits.find((x) => x.message === "pipeline: dynamic matrices" && x.stand === "needs_you");
  const old = list.commits.find((x) => x.message === "pipeline: dynamic matrices" && x.stand === "superseded");
  if (!tip || !old) throw new Error("no D41 and its superseded commit");
  await page.goto(`/ship/${tip.repo}/${tip.sha}`);
  const tipWith = page.getByRole("region", { name: "Ships with" });
  await expect(tipWith).toContainText(old.short, { timeout: 15_000 });
  await page.goto(`/ship/${old.repo}/${old.sha}`);
  const oldWith = page.getByRole("region", { name: "Ships with" });
  await expect(oldWith).toContainText("It rides with", { timeout: 15_000 });
  await expect(oldWith).toContainText(tip.short);
  await oldWith.getByRole("link").first().click();
  await expect(page).toHaveURL(new RegExp(`/ship/${tip.repo}/${tip.sha}$`));
});

for (const width of [1280, 390]) {
  test(`at ${width}, a commit's page draws every section and is never wider than its window`, async ({ page, isMobile }) => {
    test.skip(isMobile, "the width is set here; the desktop projects run it");
    await page.setViewportSize({ width, height: 900 });
    for (const [message, first] of [
      ["sync: a move carries its homes", "Errors"],
      ["deploy: a rollback holds", "Stages"],
    ] as const) {
      const { sections } = await pageOf(page, message);
      await expect(sections.first(), `${message}'s first section`).toHaveAttribute("aria-label", first, { timeout: 15_000 });
      const labels = await sections.evaluateAll((els) => els.map((el) => el.getAttribute("aria-label")));
      expect(labels.slice(labels.indexOf("Stages"))).toEqual(["Stages", "Build", "Trace", "Ships with", "Environments", "Runs"]);
      const w = await page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));
      expect(w.scroll, `${message} at ${width}`).toBe(w.client);
    }
  });
}

test("its Build names which platforms built, and a build a newer push superseded as superseded", async ({ page }) => {
  const { commit } = await pageOf(page, "deploy: a rollback holds");
  const build = commit.getByRole("region", { name: "Build" });
  await expect(build.locator('li[data-state="passed"]')).toContainText(["linux/amd64"], { timeout: 15_000 });
  await expect(build.locator('li[data-state="passed"]')).toContainText(["built"]);
  await expect(build.locator('li[data-state="superseded"]')).toContainText(["macos/arm64"]);
  await expect(build.locator('li[data-state="superseded"]')).toContainText(["superseded by a newer push"]);
  await expect(build).toContainText("The image sha256:");
  const other = await pageOf(page, "ci: a landed change");
  const built = other.commit.getByRole("region", { name: "Build" });
  await expect(built.locator('li[data-state="passed"]')).toHaveCount(2, { timeout: 15_000 });
  await expect(built.locator('li[data-state="superseded"]')).toHaveCount(0);
});
