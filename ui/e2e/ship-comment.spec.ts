/**
 * Comment, reply and resolve from an item's Files (Changes plan 01a17bd748ff73d98f09996ecdc6873d,
 * step 3, in the one place Ada chose: Review's Files, first in the dropdown), against the mock's
 * D41: a line's number opens a comment and shift-click on another takes the lines between; posted,
 * the thread shows under its last line, here and on kiln's review page; a reply joins it; resolved,
 * it folds on both. Chromium and WebKit at 1280 and 390.
 */
import { expect, test, type Page } from "@playwright/test";

const REAL = !!process.env.E2E_BASE_URL;
test.skip(REAL, "drives the mock's review");

interface KeptThread {
  id: string;
  file: string | null;
  side: string;
  start_line: number | null;
  end_line: number | null;
  state: string;
  comments: { id: string; body: string }[];
}

/** D41's threads, as kiln keeps them. */
const threadsOf = async (page: Page) => ((await (await page.request.get("/api/diffs/D41/comments")).json()) as { threads: KeptThread[] }).threads;

for (const width of [1280, 390])
  test(`at ${width}, a comment on a range of lines, a reply and resolve, from Files and on kiln's review page`, async ({ page, isMobile, browserName }) => {
    test.skip(isMobile, "the width is set here; the desktop projects run it");
    test.setTimeout(120_000);
    const said = `On these lines (${browserName}, ${width} px, ${Date.now()})`;
    const answer = `And a reply (${browserName}, ${width} px)`;
    try {
      await page.setViewportSize({ width, height: 900 });
      await page.goto("/ship");
      const row = page.locator(".ship-row").filter({ hasText: "D41" }).first();
      await expect(row).toBeVisible({ timeout: 15_000 });
      await row.locator(".ship-go").click();
      const files = row.locator('section[aria-label="Files"]');
      await expect(files.locator(".rv-sum")).toBeVisible({ timeout: 15_000 });
      // Two lines next to each other on the new side of one file, with no thread under either.
      const pick = await files.evaluate((sec) => {
        const free = [...sec.querySelectorAll<HTMLElement>(".rv-l[data-rvl*='|new:']")].filter(
          (r) => !(r.querySelector<HTMLButtonElement>(".rv-ln")?.disabled ?? true) && !r.nextElementSibling?.classList.contains("rv-thread"),
        );
        for (let i = 0; i + 1 < free.length; i++) {
          const [fa, la] = (free[i]?.dataset.rvl ?? "").split("|new:");
          const [fb, lb] = (free[i + 1]?.dataset.rvl ?? "").split("|new:");
          if (fa && fa === fb && Number(lb) === Number(la) + 1) return { file: fa, a: Number(la), b: Number(lb) };
        }
        return null;
      });
      if (!pick) throw new Error("D41 has no two free lines next to each other");
      const line = (n: number) => files.locator(`.rv-l[data-rvl="${pick.file}|new:${n}"]`);
      // A line's number, then shift-click the next: the lines between are picked, the composer under the last.
      await line(pick.a).locator(".rv-ln").click();
      await line(pick.b)
        .locator(".rv-ln")
        .click({ modifiers: ["Shift"] });
      await expect(files.locator(".rv-l[data-picked]")).toHaveCount(2);
      const writing = files.locator(".rv-thread").filter({ has: page.getByPlaceholder(`Comment on lines ${pick.a}–${pick.b}`) });
      const field = writing.getByPlaceholder(`Comment on lines ${pick.a}–${pick.b}`);
      await expect(field).toBeVisible();
      await field.fill(said);
      await writing.getByRole("button", { name: "Comment", exact: true }).click({ timeout: 15_000 });
      // Posted: the thread under the last line, kept by kiln on both lines. (A posted thread has its
      // kind, data-k; the composer, which stays open with the same words until kiln answers, has none.)
      const thread = files.locator(".rv-thread[data-k]").filter({ hasText: said });
      await expect(thread).toBeVisible({ timeout: 15_000 });
      await expect.poll(async () => (await threadsOf(page)).find((t) => t.comments[0]?.body === said)?.id ?? "").not.toBe("");
      // Once kiln has it (the thread shown at once is swapped for kiln's), it sits right under the last line.
      await expect.poll(() => thread.evaluate((el) => (el.previousElementSibling as HTMLElement | null)?.dataset.rvl)).toBe(`${pick.file}|new:${pick.b}`);
      const kept = (await threadsOf(page)).find((t) => t.comments[0]?.body === said);
      expect({ file: kept?.file, side: kept?.side, start: kept?.start_line, end: kept?.end_line }).toEqual({ file: pick.file, side: "new", start: pick.a, end: pick.b });
      // A reply joins it.
      await thread.getByRole("button", { name: "Reply", exact: true }).first().click({ timeout: 15_000 });
      const replying = thread.getByPlaceholder(/^Reply to /);
      await replying.fill(answer);
      await thread.getByRole("button", { name: "Reply", exact: true }).last().click({ timeout: 15_000 });
      await expect(thread).toContainText(answer, { timeout: 15_000 });
      // Resolved, it folds to one line.
      await thread.getByRole("button", { name: "Resolve" }).click({ timeout: 15_000 });
      const folded = line(pick.b).locator("xpath=following-sibling::*[1]");
      await expect(folded).toHaveAttribute("data-done", "1", { timeout: 15_000 });
      await expect(folded).toContainText("2 · resolved");
      await expect.poll(async () => (await threadsOf(page)).find((t) => t.id === kept?.id)?.state).toBe("resolved");
      // On kiln's review page: the same thread under the same line, folded as resolved, the reply in it.
      await page.goto("/review/D41");
      const there = page.locator(`#rv-t-${kept?.id ?? ""}`);
      await expect(there).toBeVisible({ timeout: 15_000 });
      await expect(there).toHaveAttribute("data-resolved", "true");
      await expect(there.locator(".rv-thread-folded")).toContainText("2 comments");
      const under = await there.evaluate((el) => [...(el.closest(".rv-under")?.previousElementSibling?.querySelectorAll('.rv-num[data-side="new"]') ?? [])].map((b) => b.textContent));
      expect(under).toContain(String(pick.b));
      // Opened, it holds the comment and the reply. (A click as the page is still settling can miss
      // the folded line, in WebKit at 390 px: it's clicked again until the thread opens.)
      await expect(async () => {
        const fold = there.locator(".rv-thread-folded");
        if (await fold.count()) await fold.click({ timeout: 2_000 });
        await expect(there).toContainText(said, { timeout: 1_000 });
      }).toPass({ timeout: 15_000 });
      await expect(there).toContainText(answer);
    } finally {
      for (const t of (await threadsOf(page)).filter((x) => x.comments[0]?.body === said)) for (const c of [...t.comments].reverse()) await page.request.delete(`/api/comments/${c.id}`);
    }
  });
