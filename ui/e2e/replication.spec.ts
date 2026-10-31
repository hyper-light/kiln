import { expect, test } from "@playwright/test";

test("U15 missing feeds stay unknown while product observations stay explicit", async ({ page }) => {
  const rates = { writes: null, batches: null, wire: null, busy: null };
  await page.route("**/api/conversations/status", (route) =>
    route.fulfill({
      json: {
        at: 1800000000000,
        on: true,
        mirrors_wanted: 0,
        restore: { state: "idle" },
        notes: [],
        stores: ["server", "runner"].map((role) => ({
          key: role,
          role,
          machine: role,
          id: role,
          version: "test",
          connected: true,
          state: "live",
          heard: 1800000000000,
          report: null,
          rates,
          spark: [],
        })),
      },
    }),
  );
  await page.route("**/healthz/server", (route) =>
    route.fulfill({ json: { store: { engine: "amphora", group: true, replication: { node_id: 2, leader: 1, applied: 4, durable_commit: 5, fenced: null } } } }),
  );
  await page.goto("/system");
  const figure = page.locator(".rp-figure");
  await expect(figure).toContainText("Amphora · node 2 · leader 1 · applied 4 · durable commit 5");
  await expect(figure.locator(".rp-metrics")).toContainText("Unknown");
  await expect(figure).not.toContainText("All stores current");
  await expect(figure).not.toContainText("Idle · in sync");
  await expect(figure.locator(".rp-cable")).toHaveCount(0);
  expect(await figure.evaluate((el) => el.getBoundingClientRect().right > document.documentElement.clientWidth)).toBe(false);
});
