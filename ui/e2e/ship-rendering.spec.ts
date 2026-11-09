/** Ship stays usable while a large diff's syntax work is pending (W26-R15, AC06).
 * Hold real worker replies: text, line identity, scrolling and controls must already work;
 * offscreen/closed files ask for no work, and a late reply cannot revive an inactive owner.
 */
import { expect, test, type Page } from "@playwright/test";
import { pickCommit } from "./ship-pick";

test.skip(!!process.env.E2E_BASE_URL, "drives the mock's commits");
const SHA = "3".repeat(40);
const count = 121;
const pathOf = (n: number) => `src/scroll/file-${String(n).padStart(3, "0")}.${n % 3 === 1 ? "tsx" : "rs"}`;
const marker = (n: number) => `stress_file_${String(n).padStart(3, "0")}`;
const fixture = () => {
  const files = Array.from({ length: count }, (_, n) => ({ path: pathOf(n), old_path: null, added: 1, deleted: 1, binary: false, status: "M", viewed: false, generated: n === 1 }));
  const patch = files
    .map((f, n) => {
      const signature = n % 3 === 1 ? `export function ${marker(n)}` : `pub fn ${marker(n)}`;
      const suffix = n % 3 === 1 ? "(): number {" : "() -> u32 {";
      const body = Array.from({ length: 22 }, (_, i) => ` ${n % 3 === 1 ? "const" : "let"} value_${i} = ${i};`);
      return [
        `diff --git a/${f.path} b/${f.path}`,
        `--- a/${f.path}`,
        `+++ b/${f.path}`,
        "@@ -97,25 +97,25 @@",
        `-${signature}_old${suffix}`,
        `+${signature}${suffix}`,
        ...body,
        "     1",
        " }",
        "",
      ].join("\n");
    })
    .join("");
  return { repo: "hyper-light/kiln", commit: SHA, parent: "p".repeat(40), files, patch, truncated: false };
};

type Request = { type: string; id: number; language?: string; texts?: string[] };
type Probe = { requests: Request[]; pending: number; pendingIds: number[]; hold: boolean; release: () => void };

async function observeWorker(page: Page) {
  await page.addInitScript(() => {
    const Native = window.Worker;
    const pending: { worker: Worker; data: unknown }[] = [];
    let replaying = false;
    const probe: Probe = {
      requests: [],
      pending: 0,
      pendingIds: [],
      hold: true,
      release() {
        probe.hold = false;
        replaying = true;
        for (const { worker, data } of pending.splice(0)) worker.dispatchEvent(new MessageEvent("message", { data }));
        replaying = false;
        probe.pending = 0;
        probe.pendingIds = [];
      },
    };
    class ObservedWorker extends Native {
      constructor(url: string | URL, options?: WorkerOptions) {
        super(url, options);
        this.addEventListener(
          "message",
          (event: MessageEvent) => {
            if (!replaying && probe.hold && ["highlight", "error"].includes(event.data?.type)) {
              event.stopImmediatePropagation();
              pending.push({ worker: this, data: event.data });
              probe.pending = pending.length;
              probe.pendingIds.push(event.data.id);
            }
          },
          true,
        );
      }
      override postMessage(message: unknown, transfer?: Transferable[] | StructuredSerializeOptions) {
        if (message && typeof message === "object" && "type" in message && ["highlight", "cancel"].includes(String(message.type))) probe.requests.push(structuredClone(message) as Request);
        if (Array.isArray(transfer)) super.postMessage(message, transfer);
        else super.postMessage(message, transfer);
      }
    }
    window.Worker = ObservedWorker;
    (window as unknown as { shipSyntaxProbe: Probe }).shipSyntaxProbe = probe;
  });
}

const read = (page: Page) =>
  page.evaluate(() => {
    const probe = (window as unknown as { shipSyntaxProbe: Probe }).shipSyntaxProbe;
    return { requests: probe.requests, pending: probe.pending, pendingIds: probe.pendingIds };
  });
const requested = async (page: Page, n: number) => (await read(page)).requests.some((r) => r.type === "highlight" && r.texts?.some((text) => text.includes(marker(n))));
const release = (page: Page) => page.evaluate(() => (window as unknown as { shipSyntaxProbe: Probe }).shipSyntaxProbe.release());

async function openLarge(page: Page) {
  await observeWorker(page);
  await page.route("**/api/diffs/D41/versions/*/files", (route) => route.fulfill({ json: { ...fixture(), diff: "D41", version: Number(new URL(route.request().url()).pathname.split("/").at(-2)) } }));
  await page.route("**/api/diffs/D41/versions/*/blob?*", (route) => {
    const path = new URL(route.request().url()).searchParams.get("path");
    const n = Number(path?.match(/file-(\d+)/)?.[1]);
    const signature = n % 3 === 1 ? `export function ${marker(n)}(): number {` : `pub fn ${marker(n)}() -> u32 {`;
    const body = Array.from({ length: 22 }, (_, i) => `${n % 3 === 1 ? "const" : "let"} value_${i} = ${i};`);
    return route.fulfill({ contentType: "text/plain", body: [...Array<string>(96).fill("// unchanged context"), signature, ...body, "    1", "}"].join("\n") });
  });
  await page.goto("/ship");
  const row = await pickCommit(page, "pipeline: dynamic matrices");
  const files = row.locator('section[aria-label="Files"]');
  await expect(files.locator("details.rv-f")).toHaveCount(count);
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  return { row, files, first: files.locator("details.rv-f").nth(0), closed: files.locator("details.rv-f").nth(1), last: files.locator("details.rv-f").nth(count - 1) };
}

test("W26-AC06: a large cold diff keeps plain rows and controls usable while worker syntax is delayed", async ({ page }) => {
  const { files, first, closed, last } = await openLarge(page);
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(String(error)));
  await first.scrollIntoViewIfNeeded();
  await expect.poll(() => requested(page, 0)).toBe(true);
  await expect.poll(async () => (await read(page)).pending).toBeGreaterThan(0);
  expect(await requested(page, 1)).toBe(false);
  expect(await requested(page, count - 1)).toBe(false);
  await expect(closed).not.toHaveAttribute("open");
  await expect(first.locator(".rv-t span[class]")).toHaveCount(0);
  const identity = await files.locator("[data-rvl]").evaluateAll((rows) => rows.map((row) => [row.getAttribute("data-rvl"), row.textContent]));
  expect(identity.length).toBeGreaterThan(3_000);
  // Input and scrolling are independent of an unfinished real worker request.
  const search = files.getByRole("textbox", { name: "Search the change" });
  await search.fill("stress");
  await expect(search).toHaveValue("stress");
  await search.fill("");
  await last.scrollIntoViewIfNeeded();
  await expect(last).toBeInViewport();
  await expect.poll(() => requested(page, count - 1)).toBe(true);
  expect(await requested(page, 60)).toBe(false);
  expect(await requested(page, 1)).toBe(false);
  const geometry = () => last.locator(".rv-l").evaluateAll((rows) => rows.map((row) => ({ at: row.getAttribute("data-rvl"), text: row.textContent, height: row.getBoundingClientRect().height })));
  const plain = await geometry();
  await release(page);
  await expect(last.locator(".rv-t .hl-kw").first()).toBeVisible();
  expect(await geometry()).toEqual(plain);
  expect(await files.locator("[data-rvl]").evaluateAll((rows) => rows.map((row) => [row.getAttribute("data-rvl"), row.textContent]))).toEqual(identity);
  const completedRequests = (await read(page)).requests.filter((r) => r.type === "highlight" && r.texts?.some((text) => text.includes(marker(count - 1)))).length;
  await first.scrollIntoViewIfNeeded();
  await last.scrollIntoViewIfNeeded();
  await expect(last.locator(".rv-t .hl-kw").first()).toBeVisible();
  expect(await geometry()).toEqual(plain);
  expect((await read(page)).requests.filter((r) => r.type === "highlight" && r.texts?.some((text) => text.includes(marker(count - 1)))).length).toBe(completedRequests);
  // Worker fetches have their own resource timeline; the document must not compile/fetch syntax WASM.
  expect(
    await page.evaluate(() =>
      performance
        .getEntriesByType("resource")
        .filter((r) => /\.wasm(?:\.gz)?(?:$|\?)/.test(r.name))
        .map((r) => r.name),
    ),
  ).toEqual([]);
  expect(errors).toEqual([]);
});

test("W26-AC06: collapse and unmount cancel syntax ownership before a delayed reply is delivered", async ({ page }) => {
  const { row, first, closed } = await openLarge(page);
  await first.scrollIntoViewIfNeeded();
  await expect.poll(async () => (await read(page)).pending).toBeGreaterThan(0);
  await expect.poll(() => requested(page, 0)).toBe(true);
  const firstId = (await read(page)).requests.filter((r) => r.type === "highlight" && r.texts?.some((text) => text.includes(marker(0)))).at(-1)?.id;
  expect(firstId).toBeDefined();
  await expect.poll(async () => (await read(page)).pendingIds.includes(firstId ?? -1)).toBe(true);
  await first.locator(":scope > summary").click();
  await expect(first).not.toHaveAttribute("open");
  await expect.poll(async () => (await read(page)).requests.some((r) => r.type === "cancel" && r.id === firstId)).toBe(true);
  await release(page);
  await expect(first.locator(".rv-t span[class]")).toHaveCount(0);
  await first.locator(":scope > summary").click();
  await expect(first.locator(".rv-t .hl-kw").first()).toBeVisible();
  await page.evaluate(() => {
    (window as unknown as { shipSyntaxProbe: Probe }).shipSyntaxProbe.hold = true;
  });
  await closed.locator(":scope > summary").click();
  await expect.poll(() => requested(page, 1)).toBe(true);
  await expect.poll(async () => (await read(page)).pending).toBeGreaterThan(0);
  const secondId = (await read(page)).requests.filter((r) => r.type === "highlight" && r.texts?.some((text) => text.includes(marker(1)))).at(-1)?.id;
  expect(secondId).toBeDefined();
  await expect.poll(async () => (await read(page)).pendingIds.includes(secondId ?? -1)).toBe(true);
  // Invoke the real close control without scrolling it into view first: unmount owns this cancellation.
  await row.locator(".ship-go").evaluate((button) => (button as HTMLButtonElement).click());
  await expect(row.locator(".ship-open")).toHaveCount(0);
  await expect.poll(async () => (await read(page)).requests.some((r) => r.type === "cancel" && r.id === secondId)).toBe(true);
  await release(page);
  await expect(row.locator(".ship-open")).toHaveCount(0);
  await expect(row.locator(".ship-go")).toHaveAttribute("aria-expanded", "false");
});

test("W26-AC06: the whole file receives worker tokens at their unchanged source lines", async ({ page }) => {
  const { files, first } = await openLarge(page);
  await release(page);
  await first.getByRole("button", { name: `Open ${pathOf(0)} whole`, exact: true }).click();
  const whole = files.locator(".rv-full");
  await expect(whole).toBeVisible();
  await expect(whole.locator(".rv-fullb .rv-l")).toHaveCount(121);
  const signature = whole.locator(`[data-rvl="${pathOf(0)}|new:97"]`);
  await signature.scrollIntoViewIfNeeded();
  await expect(signature.locator(".hl-kw").first()).toHaveText("pub");
  await expect(signature).toContainText(`pub fn ${marker(0)}() -> u32 {`);
});
