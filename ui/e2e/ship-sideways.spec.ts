/**
 * Nothing on Ship scrolls the page sideways, however long what a row says (W21-AC01; Ada,
 * 2026-10-30: "I can horizontal scroll the UI off the screen", on a phone in production). The
 * mock's rows say little; production's say a failed job's whole note ("gates: differential + host
 * + landing over the disk, landing bench · pod kiln-52d46c91d9-0: …"), which on a phone stood its
 * row's line 2,072 px wide. So the failing row here says as much, in its name, its line and a tag:
 * at the project's width and at 320 the page doesn't move sideways and nothing reaches past the
 * screen's right edge unclipped, with the row closed and open (its Output and Jobs), and its line
 * ends in an ellipsis inside its row.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");

const LONG_LINE =
  "gates: differential + host + landing over the disk, landing bench · pod kiln-52d46c91d9-0: step test-build exited 101 after cargo test --locked --workspace --lib reported 3 failures in crates/kiln/src/ship.rs and crates/kiln/src/ci/notify.rs";
const LONG_NAME = "sync: a move carries its homes across every runner, its leases, its journals and the replicas each of them keeps in step with the others";
const LONG_TAG = "release-candidate-for-the-runners-upgrade-of-2026-10-30";

/** The fixed failing row (3333333) saying as much as production's do. */
const longRows = (page: Page) =>
  page.route(
    (url) => url.pathname === "/api/ship/commits",
    async (route) => {
      const res = await route.fetch();
      const body = (await res.json()) as { commits: { short: string; message?: string; line: string; tags?: string[] }[] };
      for (const c of body.commits) if (c.short === "3333333") Object.assign(c, { message: LONG_NAME, line: LONG_LINE, tags: [LONG_TAG] });
      await route.fulfill({ response: res, json: body });
    },
  );

/** How far the page scrolls sideways, and what reaches past the screen's right edge with nothing clipping it. */
const sideways = (page: Page) =>
  page.evaluate(() => {
    const vw = document.documentElement.clientWidth;
    const over: string[] = [];
    const clipped = (el: Element) => {
      for (let p = el.parentElement; p; p = p.parentElement) if (/(hidden|clip|auto|scroll)/.test(getComputedStyle(p).overflowX) && p.getBoundingClientRect().right <= vw + 1) return true;
      return false;
    };
    for (const el of document.querySelectorAll("body *")) {
      const r = el.getBoundingClientRect();
      if (r.width === 0 || r.right <= vw + 1 || clipped(el)) continue;
      const cls = typeof el.className === "string" && el.className.trim() ? `.${el.className.trim().split(/\s+/).join(".")}` : "";
      over.push(`${el.tagName.toLowerCase()}${cls} right ${Math.round(r.right)}`);
    }
    const page = Math.max(document.documentElement.scrollWidth, document.body.scrollWidth) - vw;
    return { page: page > 1 ? page : 0, over: over.slice(0, 8) };
  });

for (const at of ["its own width", "320"]) {
  test(`at ${at}, a row saying a whole job's note scrolls nothing sideways, closed and open`, async ({ page, isMobile }) => {
    if (at === "320") test.skip(!isMobile, "a phone's narrowest; the desktop projects check their own width");
    if (at === "320") await page.setViewportSize({ width: 320, height: 720 });
    await longRows(page);
    await page.goto("/ship");
    const row = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").filter({ hasText: "3333333" }).first();
    await expect(row).toContainText(LONG_LINE.slice(0, 40), { timeout: 15_000 });
    expect(await sideways(page)).toEqual({ page: 0, over: [] });

    // Its line ends in an ellipsis inside its row.
    const line = row.locator(".ship-main .ship-text");
    const fit = await line.evaluate((el) => {
      const r = el.getBoundingClientRect();
      const box = el.closest(".ship-r")?.getBoundingClientRect();
      return { cut: el.scrollWidth > el.clientWidth, inside: !!box && r.right <= box.right + 1, ellipsis: getComputedStyle(el).textOverflow };
    });
    expect(fit).toEqual({ cut: true, inside: true, ellipsis: "ellipsis" });

    await row.getByRole("button", { name: /^Open / }).click();
    await expect(row.getByRole("region", { name: "Output" })).toBeVisible();
    await expect(row.getByRole("region", { name: "Jobs" })).toBeVisible();
    await expect(row.getByRole("list", { name: "Stages" })).toBeVisible();
    await expect.poll(() => sideways(page), { timeout: 10_000 }).toEqual({ page: 0, over: [] });
  });
}
