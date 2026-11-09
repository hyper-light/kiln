/** W21-R02: a live subagent card renders one complete summary as its metadata changes.
 * DOM and geometry evidence is separate from the screenshot's unconfirmed native paint cause. */
import { gunzipSync } from "node:zlib";
import { expect, test, type Locator } from "@playwright/test";

const summary = "I’ll check the failing fixture against the actual agent DOM and saved browser evidence, then tighten its entry detection while keeping the first-frame geometry assertions.";

async function geometry(card: Locator) {
  return card.evaluate((el) => {
    const rect = (node: Pick<Element, "getBoundingClientRect">) => {
      const { x, y, width, height, bottom } = node.getBoundingClientRect();
      return { x, y, width, height, bottom };
    };
    const result = el.querySelector(".hx-sub-result")!;
    const paragraph = result.querySelector("p")!;
    const range = document.createRange();
    range.selectNodeContents(paragraph);
    const style = getComputedStyle(result);
    return {
      card: rect(el),
      head: rect(el.querySelector(".hx-sub-head")!),
      meta: rect(el.querySelector(".hx-sub-meta")!),
      result: rect(result),
      paragraph: rect(paragraph),
      text: rect(range),
      resultStyle: { display: style.display, fontSize: style.fontSize, lineHeight: style.lineHeight, overflow: style.overflow, lineClamp: style.webkitLineClamp },
      html: el.outerHTML,
    };
  });
}

test("W21-R02 a live subagent has one unclipped summary through successive activity updates", async ({ page }, testInfo) => {
  test.skip(!!process.env.E2E_BASE_URL, "a controlled mock subagent snapshot");
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(String(error)));
  const started = Date.now() - (3 * 60 + 37) * 60_000;
  let task = {
    id: "screenshot-child",
    tool_id: "call-card",
    description: "/root/ui_causality",
    kind: "subagent",
    class: "agent",
    status: "running",
    background: true,
    depth: 1,
    tokens: 0,
    tool_uses: 319,
    duration_ms: 0,
    started,
    stoppable: false,
    last_tool: "Delegating: /root",
    summary,
  };
  const control: { state: Record<string, unknown> | null; send: ((frame: object) => void) | null } = { state: null, send: null };
  await page.routeWebSocket(
    (url) => url.pathname === "/api/harness/sessions/tree-lead/ws",
    (ws) => {
      const server = ws.connectToServer();
      let complete = false;
      control.send = (frame) => ws.send(JSON.stringify(frame));
      server.onMessage((message) => {
        if (complete) return;
        const text = typeof message === "string" ? message : gunzipSync(message).toString("utf8");
        for (const line of text.split("\n").filter(Boolean)) {
          const frame = JSON.parse(line) as { t: string; state?: Record<string, unknown>; events?: object[] };
          if (frame.t === "state") {
            control.state = { ...frame.state, tasks: [task] };
            frame.state = control.state;
          }
          if (frame.t === "events")
            frame.events = [
              { id: "card-prompt", kind: "user", seq: 1, at: started - 1000, text: "Check this live subagent card." },
              {
                id: "call-card",
                kind: "tool",
                seq: 2,
                at: started,
                tool_id: "call-card",
                tool: "agent",
                name: "subAgentActivity",
                task: task.id,
                title: task.description,
                input: {},
                status: "running",
                started,
                output: summary,
              },
            ];
          if (frame.t === "synced") complete = true;
          ws.send(JSON.stringify(frame));
        }
      });
      ws.onMessage((message) => server.send(message));
    },
  );
  await page.goto("/agents/tree-lead");
  const card = page.locator('.hx-sub[data-ev="call-card"]');
  await expect(card).toBeVisible({ timeout: 30_000 });
  expect(await page.evaluate(() => matchMedia("(prefers-reduced-motion: reduce)").matches)).toBe(false);
  const kept = await card.elementHandle();
  const evidence: Awaited<ReturnType<typeof geometry>>[] = [];
  const check = async (text: string) => {
    await expect(card.locator(".hx-sub-result")).toHaveCount(1);
    await expect(card.locator(".hx-sub-result p")).toHaveCount(1);
    await expect(card.locator(".hx-sub-result")).toHaveText(text);
    expect((await card.innerText()).split(text).length - 1, "the complete summary occurs once in the card").toBe(1);
    const box = await geometry(card);
    expect(box.meta.y, "metadata follows the heading").toBeGreaterThanOrEqual(box.head.bottom - 1);
    expect(box.result.y, "summary follows all metadata rows").toBeGreaterThanOrEqual(box.meta.bottom - 1);
    expect(box.text.y, "summary text starts within its paragraph").toBeGreaterThanOrEqual(box.paragraph.y - 1);
    expect(box.text.bottom, "the last summary line is inside the result").toBeLessThanOrEqual(box.result.bottom + 1);
    expect(box.result.bottom, "the result fits inside its card").toBeLessThanOrEqual(box.card.bottom + 1);
    expect(box.resultStyle.lineClamp).toMatch(/^(none|0)$/);
    evidence.push(box);
  };
  try {
    await check(summary);
    await testInfo.attach("subagent-card-screenshot-words", { body: await card.screenshot({ animations: "allow" }), contentType: "image/png" });
    for (const [index, activity] of [
      "Running rg --files /home/adalundhe/.cache/kiln-ui-speedup-2026-11-08/worktree/ui/src",
      "Delegating: /root",
      "Running sed -n 1,160p ui/src/views/harness/timeline.tsx",
    ].entries()) {
      task = { ...task, tool_uses: 320 + index, last_tool: activity };
      if (!control.state || !control.send) throw new Error("The controlled session did not publish its state");
      control.send({ t: "state", state: { ...control.state, tasks: [task] } });
      await expect(card.locator(".hx-sub-meta")).toContainText(activity);
      await check(summary);
    }
    const changed = "The card keeps one full summary after its activity and commentary both change. Every final line remains visible.";
    task = { ...task, summary: changed };
    if (!control.state || !control.send) throw new Error("The controlled session did not publish its state");
    control.send({ t: "state", state: { ...control.state, tasks: [task] } });
    await check(changed);
    await expect(card).not.toContainText(summary);
    expect(await kept?.evaluate((el) => el.isConnected), "activity updates retain the same card").toBe(true);
    expect(errors).toEqual([]);
    await testInfo.attach("subagent-card-normal-motion", { body: await card.screenshot({ animations: "allow" }), contentType: "image/png" });
  } finally {
    await testInfo.attach("subagent-card-geometry", { body: JSON.stringify({ evidence, errors }, null, 2), contentType: "application/json" });
  }
});
