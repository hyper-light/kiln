/**
 * W9 (docs/agentic.md): a runner's engine offers `container` beside `microvm` and `host`, in the production build
 * against the mock. On metal-ryzen-02, whose agents can run in containers (rootless podman, an image declared), it's
 * chosen, confirmed in containers' words, and held; on metal-ryzen-01, which declares no image, it can't be chosen,
 * says why, and the server refuses it too. Nothing scrolls sideways. On a desktop and a phone, in Chromium and WebKit.
 */
import { expect, test, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): these set the mock's runners' engines. */
const REAL = !!process.env.E2E_BASE_URL;

test.skip(REAL, "sets the mock's runners' engines");

/** Runner `name`'s details, from the Runners table: its engine control. */
async function engineOf(page: Page, name: string) {
  await page.goto("/runners");
  await page.locator("tr[data-href]").filter({ hasText: name }).first().click();
  const control = page.getByRole("group", { name: `Engine of ${name}` });
  await expect(control).toBeVisible();
  return control;
}

/** Nothing on the page wider than it. */
async function nothingSideways(page: Page) {
  const over = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(over, "the page scrolls sideways").toBeLessThanOrEqual(0);
}

test("container is offered where a runner's agents can run in containers, and says why where they can't", async ({ page }) => {
  // As the mock starts it: metal-ryzen-02's owner runs its agents on its host.
  expect((await page.request.put("/api/runners/metal-ryzen-02/engine", { data: { engine: "host" } })).ok()).toBe(true);

  const able = await engineOf(page, "metal-ryzen-02");
  const container = able.getByRole("button", { name: "container" });
  await expect(container).toBeEnabled();
  await container.click();
  const confirm = page.getByRole("group", { name: "Run its new sessions in containers" });
  await expect(confirm).toBeVisible();
  await nothingSideways(page);
  await confirm.getByRole("button", { name: /^Switch/ }).click();
  await expect(container).toHaveAttribute("aria-pressed", "true");
  const held = await (await page.request.get("/api/runners/metal-ryzen-02/engine")).json();
  expect(held.engine).toBe("container");
  expect(held.container).toEqual({ usable: true, why: null });

  const unable = await engineOf(page, "metal-ryzen-01");
  const refused = unable.getByRole("button", { name: "container" });
  await expect(refused).toBeDisabled();
  await expect(refused).toHaveAttribute("title", /container_image/);
  await nothingSideways(page);
  const put = await page.request.put("/api/runners/metal-ryzen-01/engine", { data: { engine: "container" } });
  expect(put.status()).toBe(409);

  // As it started, for the next project.
  expect((await page.request.put("/api/runners/metal-ryzen-02/engine", { data: { engine: "host" } })).ok()).toBe(true);
});
