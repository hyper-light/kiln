/**
 * How long a message takes from the box's send button to the agent's socket with a large set this
 * browser kept (2026-11-13: every send saved first, reading and rewriting the whole set). Seeds
 * W20's database with `E2E_HELD` messages of `E2E_HELD_KB` each, waits for the page to settle,
 * sends ten, and prints the times. Run on its own: E2E_PERF=1.
 */
import { expect, test, type Page } from "@playwright/test";

test.skip(!process.env.E2E_PERF, "a measurement, run with E2E_PERF=1");

const HELD = Number(process.env.E2E_HELD ?? 200);
const KB = Number(process.env.E2E_HELD_KB ?? 200);

async function seed(page: Page) {
  await page.addInitScript(
    ({ held, kb }) => {
      if (sessionStorage.getItem("seeded")) return;
      sessionStorage.setItem("seeded", "1");
      const open = indexedDB.open("kiln.agents.held", 1);
      open.onupgradeneeded = () => {
        open.result.createObjectStore("held");
        open.result.createObjectStore("legacy", { autoIncrement: true });
      };
      open.onsuccess = () => {
        const bytes = new Uint8Array(kb * 1024).fill(97).buffer;
        const set: Record<string, unknown[]> = {};
        for (let i = 0; i < held; i += 1) {
          (set[`other-${i % 10}`] ??= []).push({ id: `held-${i}`, text: `kept ${i}`, at: i, owner: "someone-else", originals: [{ name: `f${i}.bin`, mime: "application/octet-stream", bytes }] });
        }
        const tx = open.result.transaction("held", "readwrite");
        tx.objectStore("held").put(set, "originals");
        tx.oncomplete = () => open.result.close();
      };
    },
    { held: HELD, kb: KB },
  );
}

test("composer_send latency: send button to socket, with a large kept set", async ({ page }) => {
  await seed(page);
  const at = new Map<string, number>();
  page.on("websocket", (ws) =>
    ws.on("framesent", (f) => {
      try {
        const frame = JSON.parse(String(f.payload)) as { t?: string; c?: { type?: string; text?: string } };
        if (frame.t === "cmd" && frame.c?.type === "send" && frame.c.text) at.set(frame.c.text, performance.now());
      } catch {
        /* not ours */
      }
    }),
  );
  await page.goto("/agents/demo-long");
  await expect(page.locator(".hx-agent.ag-ready")).toBeVisible({ timeout: 30_000 });
  await page.waitForTimeout(3_000);
  const field = page.getByRole("textbox", { name: "Message to the agent" });
  const button = page.locator(".hx-dock").getByRole("button", { name: /^(Send|Queue|Steer|Send now)$/ });
  const times: number[] = [];
  for (let i = 0; i < 10; i += 1) {
    const text = `latency ${i}`;
    await field.fill(text);
    const start = performance.now();
    await button.first().click();
    await expect.poll(() => at.has(text), { timeout: 30_000 }).toBe(true);
    times.push((at.get(text) ?? start) - start);
    await expect(field).toHaveValue("");
  }
  const sorted = [...times].sort((a, b) => a - b);
  const median = sorted[Math.floor(sorted.length / 2)] ?? 0;
  console.log(`LATENCY held=${HELD}x${KB}KB ms=${times.map((t) => t.toFixed(0)).join(",")} median=${median.toFixed(0)} max=${(sorted.at(-1) ?? 0).toFixed(0)}`);
});
