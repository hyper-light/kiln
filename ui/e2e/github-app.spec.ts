/**
 * Settings → CI → GitHub (docs/ci.md W14-R10), in the production build against the mock: with no
 * App yet, "Create kiln's GitHub App" posts kiln's manifest to GitHub's new-App page, in the
 * organization named, as GitHub's manifest flow asks.
 */
import { expect, test } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): this one would leave for GitHub. */
const REAL = !!process.env.E2E_BASE_URL;

test.skip(REAL, "posts to github.com");

test("creating kiln's GitHub App posts its manifest to GitHub", async ({ page }) => {
  let posted: { url: string; manifest: string | null } | null = null;
  await page.route("https://github.com/**", async (route) => {
    const form = new URLSearchParams(route.request().postData() ?? "");
    posted = { url: route.request().url(), manifest: form.get("manifest") };
    await route.fulfill({ status: 200, contentType: "text/html", body: "<title>GitHub</title>" });
  });
  await page.goto("/settings#ci.github");
  const group = page.locator("#ci\\.github");
  await expect(group).toBeVisible();
  await group.getByLabel("Organization").fill("hyper-light");
  await group.getByRole("button", { name: "Create kiln's GitHub App" }).click();
  await expect.poll(() => posted?.url).toBe("https://github.com/organizations/hyper-light/settings/apps/new?state=mock");
  const manifest = JSON.parse(posted!.manifest ?? "{}") as { redirect_url?: string; public?: boolean };
  expect(manifest.redirect_url).toMatch(/\/api\/ci\/github\/app\/callback$/);
  expect(manifest.public).toBe(false);
});
