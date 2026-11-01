/**
 * An agent's conversation never scrolls sideways (docs/ci.md W21-R13, as W21-AC01 checks a page):
 * whatever the agent puts in it stays in its column, words too long for a phone's line (an id, a
 * path, an address) included. demo-long's session brings them as the server would: notices,
 * approvals and a question, a picture, a page, tool steps, a table and code. Nothing runs past the
 * column but inside what scrolls or clips its own (a code block, a wide table), and what is too wide
 * all the same is cut at the column's edge, the conversation held still. In the production build
 * against the mock, on a desktop and a phone.
 */
import { gunzipSync, gzipSync } from "node:zlib";
import { expect, test, type Locator, type Page } from "@playwright/test";

/** Against a real kiln server (playwright.config.ts): this drives the mock's demo-long. */
const REAL = !!process.env.E2E_BASE_URL;

/** Words no phone's line holds whole: an agent's id, a path, an address, a line of output. */
const ID = "01a148187ac3f9e2d4b5a6c3f9e2d4b5a6c3f9e2d4b5a6c3f9e2d4b5a6c3f9e2d4b5a6";
const FILE = "/home/ada/Projects/kiln/deploy/helm/kiln/templates/peering-configuration-for-the-mesh-gateway-and-its-sidecars.yaml";
const ADDRESS = `https://kiln.example.internal:7070/agents/${ID}/changes?file=deploy%2Fhelm%2Fkiln%2Ftemplates%2Fpeering.yaml&line=120`;
const LINE = "0123456789abcdef".repeat(14);
const MOCKUP = ["┌" + "─".repeat(94) + "┐", `│ IMAGE BUILD ▸ main ▸ ${LINE.slice(0, 71)} │`, "└" + "─".repeat(94) + "┘"].join("\n");

const T = Date.UTC(2026, 9, 17, 23, 0, 0);
let seq = 100_000;
/** After everything of demo-long's, in its order. */
const ev = (e: Record<string, unknown>) => ({ seq: ++seq, at: T + seq, ...e });
const tool = (id: string, name: string, kind: string, title: string, more: Record<string, unknown> = {}) =>
  ev({ id, kind: "tool", tool_id: id, name, tool: kind, title, input: {}, status: "done", started: T, ended: T + 1000, ...more });

const EVENTS = [
  ev({ id: "w-u1", kind: "user", text: `Look at ${ID}, ${ADDRESS} and ${FILE}` }),
  ev({ id: "w-th", kind: "thinking", text: `Reading ${FILE}, for ${ID}.` }),
  tool("w-sh", "Bash", "shell", `kubectl --context=${ID} get pods -o jsonpath='{.items[*].status.containerStatuses[*].imageID}'`, { exit_code: 0, output: `${LINE}\n${FILE}` }),
  tool("w-rd", "Read", "read", FILE),
  tool("w-ed", "Edit", "edit", FILE, { diffs: [{ path: FILE, hunks: [{ old_start: 1, old_lines: 1, new_start: 1, new_lines: 1, lines: [`-${LINE}`, `+${LINE}${LINE}`] }] }] }),
  tool("w-wf", "WebFetch", "web", ADDRESS, { output: ADDRESS }),
  tool("w-mcp", "mcp__claude_ai_Claude_Docs__export_document_as_markdown", "mcp", ID, { server: "claude_ai_Claude_Docs", tool_short: "export_document_as_markdown", output: LINE }),
  // The page demo-long shows: a chart drawn as it plays.
  tool("w-page", "mcp__kiln__show_page", "mcp", `Image build ${ID}`, {
    output: JSON.stringify({
      kiln_page: {
        id: "3f1c0de5a2b94e7a8c6d0b1e2f3a4b5c",
        title: `Image build ${ID}`,
        path: "/v/3f1c0de5a2b94e7a8c6d0b1e2f3a4b5c/",
        files: 0,
        heights: [
          [320, 310],
          [1144, 310],
        ],
      },
    }),
  }),
  ev({ id: "w-n1", kind: "notice", level: "info", text: `Agent "Kiln UI" (${ID}) finished: ${FILE}` }),
  ev({ id: "w-n2", kind: "notice", level: "error", text: `Couldn't reach ${ADDRESS}` }),
  ev({
    id: "w-a1",
    kind: "assistant",
    text: [
      `### ${ID}`,
      `See ${ADDRESS}, \`${FILE}\` and [${ID}](${ADDRESS}).`,
      `> ${ID}`,
      `- ${LINE}`,
      "",
      "| Image | Digest | Platform | Size | Pushed to |",
      "|---|---|---|---|---|",
      `| kiln-runner | sha256:${LINE.slice(0, 64)} | linux/amd64 | 182 MB | ${ADDRESS} |`,
      "",
      "```",
      LINE,
      "```",
      "",
      "![the whole dashboard](/tmp/kiln-mock/wide.png)",
    ].join("\n"),
  }),
];

/** The picture, kept at its own size (wider than any screen). */
const FILES = [{ path: "/tmp/kiln-mock/wide.png", seq, meta: { size: 1_234_567, mtime: T, mime: "image/png", width: 2880, height: 900 }, sha: "a".repeat(64) }];
const PNG = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==", "base64");

const APPROVALS = [
  {
    id: "w-ask",
    tool_id: "w-ask",
    tool_name: "AskUserQuestion",
    tool: "question",
    title: "Image build",
    input: {},
    cwd: "/tmp",
    at: T,
    questions: [
      {
        question: `How should the image build for ${ID} go?`,
        header: "Image build",
        multi: false,
        other: true,
        secret: false,
        options: [
          { label: `Wait for main's image run ${ID}`, description: `Leave the disk alone, and deploy what it's built: ${FILE}`, preview: MOCKUP },
          { label: "Free the disk now", description: `You or Kiln Distro handle the rest: ${ADDRESS}` },
        ],
      },
    ],
  },
  { id: "w-web", tool_id: "w-web", tool_name: "WebFetch", tool: "web", title: ADDRESS, detail: ID, input: {}, cwd: "/tmp", at: T },
  { id: "w-use", tool_id: "w-use", tool_name: "mcp__claude_ai_Claude_Docs__export_document_as_markdown", tool: "other", title: ID, agent: ID, input: {}, cwd: "/tmp", at: T },
  { id: "w-cmd", tool_id: "w-cmd", tool_name: "Bash", tool: "shell", title: "kubectl", command: `kubectl get pods --context=${ID}`, reason: `For ${ID}`, input: {}, cwd: FILE, at: T },
];

type Frame = { t?: string; state?: Record<string, unknown>; files?: unknown[] };

/** demo-long's session with all of the above: in its snapshot (packed or not), before it says it's synced. */
async function withWideThings(page: Page) {
  // Its question can be an experiment, as on an agent working in a repository kiln knows.
  await page.route(/\/api\/experiments\/where/, (r) => r.fulfill({ json: { repo: "hyper-light/kiln", branch: "main", workflows: ["ci.yaml"], may: true } }));
  await page.route(/\/api\/harness\/sessions\/demo-long\/files\//, (r) => r.fulfill({ body: PNG, contentType: "image/png" }));
  await page.routeWebSocket(/\/api\/harness\/sessions\/demo-long\/ws\b/, (ws) => {
    const server = ws.connectToServer();
    let added = false;
    const add = (frames: Frame[]) =>
      frames.flatMap((f): Frame[] => {
        if (f.t === "state" && f.state) f.state = { ...f.state, approvals: APPROVALS };
        if (f.t === "files") f.files = [...(f.files ?? []), ...FILES];
        if (f.t !== "synced" || added) return [f];
        added = true;
        return [{ t: "events", events: EVENTS } as Frame, f];
      });
    server.onMessage((m) => {
      if (typeof m !== "string") {
        const frames = gunzipSync(m).toString("utf8").split("\n").filter(Boolean);
        ws.send(
          gzipSync(
            add(frames.map((l) => JSON.parse(l) as Frame))
              .map((f) => JSON.stringify(f))
              .join("\n"),
          ),
        );
        return;
      }
      let f: Frame;
      try {
        f = JSON.parse(m) as Frame;
      } catch {
        ws.send(m);
        return;
      }
      for (const g of add([f])) ws.send(JSON.stringify(g));
    });
    ws.onMessage((m) => server.send(m));
  });
}

/**
 * What runs past the conversation's column: a box, or the words of a box that fits, unless what
 * holds it scrolls or clips its own and fits.
 */
const pastTheColumn = (page: Page) =>
  page.evaluate(() => {
    const flow = document.querySelector(".hx-timeline > .hx-flow");
    if (!flow) return ["no conversation"];
    const edge = flow.getBoundingClientRect().right + 0.5;
    const held = (n: Node) => {
      for (let a = n.parentElement; a && a !== flow; a = a.parentElement) if (getComputedStyle(a).overflowX !== "visible") return a.getBoundingClientRect().right <= edge;
      return false;
    };
    const name = (el: Element) => `${el.tagName.toLowerCase()}${typeof el.className === "string" && el.className.trim() ? `.${el.className.trim().split(/\s+/).join(".")}` : ""}`;
    const out: string[] = [];
    for (const el of flow.querySelectorAll("*")) {
      const r = el.getBoundingClientRect();
      if (r.right > edge && (r.width > 0 || r.height > 0) && getComputedStyle(el).position !== "fixed" && !held(el)) out.push(`${name(el)} (${Math.round(r.right - edge)}px)`);
    }
    const words = document.createTreeWalker(flow, NodeFilter.SHOW_TEXT);
    const range = document.createRange();
    for (let t = words.nextNode(); t; t = words.nextNode()) {
      if (!t.textContent?.trim() || !t.parentElement) continue;
      range.selectNodeContents(t);
      const r = range.getBoundingClientRect();
      if (r.right > edge && !held(t)) out.push(`"${t.textContent.trim().slice(0, 24)}…" in ${name(t.parentElement)} (${Math.round(r.right - edge)}px)`);
    }
    return out.slice(0, 12);
  });

/** Scrolls sideways, as W21-AC01 counts it (ci/ui/layout.mjs): it would scroll its content left and right. */
const scrollsSideways = (l: Locator) => l.evaluate((el) => /auto|scroll/.test(getComputedStyle(el).overflowX) && el.scrollWidth > el.clientWidth + 1);

test("nothing an agent puts in its conversation moves it sideways", async ({ page }) => {
  test.skip(REAL, "the mock's demo-long");
  await withWideThings(page);
  await page.goto("/agents/demo-long");
  const timeline = page.locator(".hx-timeline");
  const question = page.locator(".hx-asks .ch");
  await expect(question).toBeVisible({ timeout: 60_000 });
  await expect(page.locator(".hx-asks .hx-approval")).toHaveCount(3);
  await expect(page.locator(".hx-notice", { hasText: ID })).toHaveCount(2);
  await expect(page.locator(".mf-picture", { hasText: "wide.png" })).toBeVisible();
  await timeline.evaluate((el) => (el.scrollTop = el.scrollHeight));
  expect(await pastTheColumn(page), "as it came").toEqual([]);
  expect(await scrollsSideways(timeline)).toBe(false);

  // The question's option with a preview, picked (the preview under it on a phone, beside the
  // options on a desktop); then its experiment's setup.
  await question.getByRole("radio", { name: /^Wait for main's image run/ }).click();
  await expect(question.locator(".ch-pv-pre:visible")).toHaveCount(1);
  expect(await pastTheColumn(page), "an option and its preview").toEqual([]);
  await question.getByRole("button", { name: "Experiment…" }).click();
  await expect(question.getByRole("button", { name: "Start experiment" })).toBeVisible();
  expect(await pastTheColumn(page), "its experiment's setup").toEqual([]);

  // Too wide all the same (notices kept on one line): cut at the edge, the conversation held still.
  await page.addStyleTag({ content: ".hx-notice { white-space: nowrap; }" });
  await expect.poll(() => timeline.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeGreaterThan(0);
  expect(await scrollsSideways(timeline)).toBe(false);
});
