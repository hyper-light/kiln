/**
 * Ship's rows (plan 01a175b7f20b7277b15bddd2ad001bcc, step 5), at 390 against the mock's fixed rows:
 * every row keeps one height whatever its state, and keeps it as a row opens, a change unfolds and
 * the row closes again. And each row as f-ship draws it (plan 01a1856fe81375b1bd1d1d86f270a45b,
 * step 6): its stand word in mono capitals in the stand's colour, who with a person's or an agent's
 * mark, the environment it runs in as a flag, and the strip's stages named under its pips (not on a
 * phone), at 390 and 1280.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's commits");
test.use({ viewport: { width: 390, height: 844 } });

/** Each row's own line (not what opens or unfolds under it), in CSS pixels. */
const heights = (page: Page) => page.locator(".ship-row > .ship-r").evaluateAll((els) => els.map((e) => Math.round(e.getBoundingClientRect().height)));

test("a row keeps one height whatever its state", async ({ page }) => {
  await page.goto("/ship");
  const rows = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li");
  await expect(rows.first()).toBeVisible();
  const before = await heights(page);
  expect(before.length).toBeGreaterThan(3);
  expect(new Set(before).size).toBe(1);

  await rows
    .first()
    .getByRole("button", { name: /^Open / })
    .click();
  await page.getByRole("button", { name: /Show 1 more commit of CR12/ }).click();
  const during = await heights(page);
  expect(new Set(during).size).toBe(1);
  expect(during[0]).toBe(before[0]);

  await rows
    .first()
    .getByRole("button", { name: /^Close / })
    .click();
  expect(await heights(page)).toEqual(before);
});

for (const width of [390, 1280])
  test(`at ${width}, each row as f-ship draws it: the stand in capitals, who with a mark, the environment as a flag`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    await page.goto("/ship");
    const rows = page.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li.ship-row");
    await expect(rows.first()).toBeVisible();
    const drawn = await rows.evaluateAll((els) =>
      els.map((li) => {
        const word = li.querySelector<HTMLElement>(":scope > .ship-r .ship-stand");
        const st = word ? getComputedStyle(word) : null;
        const who = li.querySelector(":scope > .ship-r .ship-who");
        const flag = li.querySelector<HTMLElement>(":scope > .ship-r .ship-env");
        return {
          stand: li.dataset.stand ?? "",
          caps: st?.textTransform ?? "",
          mono: /mono/i.test(st?.fontFamily ?? ""),
          size: st?.fontSize ?? "",
          color: st?.color ?? "",
          mark: who?.querySelector("svg.sg")?.getAttribute("data-state") ?? "",
          name: who?.textContent ?? "",
          flag: flag ? { env: flag.dataset.env ?? "", mark: flag.querySelector("svg.sg")?.getAttribute("data-state") ?? "", height: Math.round(flag.getBoundingClientRect().height) } : null,
          m2: Math.round(li.querySelector(":scope > .ship-r .ship-m2")?.getBoundingClientRect().height ?? 0),
        };
      }),
    );
    expect(drawn.length).toBeGreaterThan(3);
    for (const r of drawn) {
      expect({ caps: r.caps, mono: r.mono, size: r.size }, r.stand).toEqual({ caps: "uppercase", mono: true, size: "10px" });
      // A person's or an agent's mark, an agent's for an agent's principal, and a name beside it.
      expect(["agent", "person"], r.stand).toContain(r.mark);
      expect(r.name.trim().length, r.stand).toBeGreaterThan(0);
      if (r.name.startsWith("agent ")) expect(r.mark).toBe("agent");
      // Its second line holds a flag's height, flag or none.
      expect(r.m2, r.stand).toBe(20);
    }
    // The stands' colours, f-ship's: needs you sand, shipped green.
    expect(drawn.find((r) => r.stand === "needs_you")?.color).toBe("rgb(226, 207, 174)");
    expect(drawn.find((r) => r.stand === "shipped")?.color).toBe("rgb(191, 220, 207)");
    // Where a row runs: a flag with its mark, production's tab.
    const flags = drawn.flatMap((r) => (r.flag ? [r.flag] : []));
    expect(flags.length).toBeGreaterThan(0);
    for (const f of flags) expect(f).toEqual({ env: f.env, mark: "flag", height: 20 });
    expect(flags.map((f) => f.env)).toContain("production");
    // The strip names its stages under its pips; on a phone, the pips alone.
    const named = await rows
      .first()
      .locator(".ship-pips .js-l")
      .evaluateAll((els) => els.filter((e) => getComputedStyle(e).display !== "none").map((e) => e.textContent));
    expect(named).toEqual(width > 640 ? ["Review", "Land", "Checks", "Build", "Staging", "Prod", "Replant"] : []);
    // And every row's line keeps one height.
    expect(new Set(await rows.locator(":scope > .ship-r").evaluateAll((els) => els.map((e) => Math.round(e.getBoundingClientRect().height)))).size).toBe(1);
  });
