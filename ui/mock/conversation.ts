/**
 * A long, working agent conversation for the mock: /agents/demo-long.
 *
 * Forty finished turns (prompts, thoughts, reads, edits with diffs, commands with output, answers in
 * markdown), then a turn still going: a command running and a reply streaming word by word, with a
 * step added now and then. Its ids and history are the same every time the mock starts, so a place
 * kept in a conversation survives a restart of the mock, as it does a restart of the server.
 *
 *   MOCK_NO_SYNCED=1   its socket doesn't end the snapshot with {"t":"synced"} (servers before it)
 *   MOCK_SLOW_SNAPSHOT=ms   frames of the snapshot are sent this far apart (a slow link)
 *   MOCK_PAUSED=1   it is paused for a usage limit (the 5-hour window, resetting in 47 minutes) with
 *                   two messages queued, after an older build said the limit seven times in a row
 *   MOCK_LATE_SESSION=ms   for this long after the mock starts, it doesn't know the session (as a
 *                          restarted server before the session's machine reconnects): its socket
 *                          says {"t":"closed"}; then the session is announced on /api/events
 *   MOCK_DROPPED=n   its timeline let go of its first n events, which the conversation store still
 *                    has (an earlier history, before the forty turns, that a search finds)
 *   MOCK_TURNS=n     n finished turns instead of forty (a long conversation: 600 is the 4,000 events
 *                    a session keeps)
 *   MOCK_LIVE_STEPS=n   the turn still going has taken n steps already (a long turn of an agent
 *                       working on its own: a note and a command each)
 */
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import type { IncomingMessage, ServerResponse } from "node:http";
import { crc32, deflateSync, gzipSync } from "node:zlib";
import type { WebSocket } from "ws";
import type { AgentInfo, HEvent, KeptFile, Meta, SaidHit, State } from "../src/api/harness.ts";

const ID = "demo-long";
/** A fixed start, so the history is the same on every run. */
const T0 = Date.UTC(2026, 9, 17, 9, 0, 0);
const TURNS = Number(process.env.MOCK_TURNS ?? 40);
const LIVE_STEPS = Number(process.env.MOCK_LIVE_STEPS ?? 0);

const FILES = ["crates/kiln/src/http/client.rs", "crates/kiln/src/http/retry.rs", "crates/kiln/src/rt/timer.rs", "crates/kiln/tests/retry.rs", "ui/src/api/client.ts"];
const ASKS = [
  "Why does the retry test fail one run in ten?",
  "Make the backoff jitter deterministic under test.",
  "Add a test for the Retry-After header.",
  "Check the timer wheel for the same race.",
  "Write it up for the changelog.",
  "Run the whole suite again.",
  "Is the client still cancel-safe?",
  "Tighten the error messages.",
];

/** An event before it's numbered (each kind of event keeps its own fields). */
type Unnumbered = HEvent extends infer E ? (E extends HEvent ? Omit<E, "seq"> : never) : never;

let seq = 0;
const events: HEvent[] = [];
const add = (e: Unnumbered) => {
  seq += 1;
  const ev = { ...e, seq } as HEvent;
  events.push(ev);
  return ev;
};

function answer(i: number): string {
  const f = FILES[i % FILES.length]!;
  const long = i % 3 === 0;
  return [
    `### What I found (${i})`,
    "",
    `The retry loop in \`${f}\` read the clock twice, so a slow tick could put the deadline before the first attempt.`,
    "",
    "- The first attempt now takes its deadline from the same instant as the backoff.",
    "- The jitter comes from the test's seeded source when one is set.",
    long ? "- `Retry-After` wins over the backoff when the server sends one, capped at the deadline." : "",
    "",
    long ? "```rust\nlet now = clock.now();\nlet deadline = now + budget;\nlet wait = backoff.next(now).min(deadline - now);\n```" : "",
    "",
    `All ${120 + i} tests pass, ten runs in a row.`,
  ]
    .filter((l, k, a) => l !== "" || a[k - 1] !== "")
    .join("\n");
}

for (let i = 1; i <= TURNS; i++) {
  const at = T0 + i * 90_000;
  add({ id: `L-u${i}`, at, kind: "user", text: ASKS[i % ASKS.length]! + (i % 4 === 0 ? "\n\nKeep the change small; the release is tomorrow." : "") });
  add({ id: `L-t${i}`, at: at + 2_000, kind: "thinking", text: `Looking at how the deadline is computed in turn ${i}, and whether the clock is read once.` });
  add({
    id: `L-r${i}a`,
    at: at + 4_000,
    kind: "tool",
    tool_id: `tr${i}a`,
    name: "Read",
    tool: "read",
    title: `/home/ada/src/kiln/${FILES[i % FILES.length]}`,
    input: {},
    status: "done",
    started: at + 4_000,
    ended: at + 4_200,
  });
  add({
    id: `L-r${i}b`,
    at: at + 4_500,
    kind: "tool",
    tool_id: `tr${i}b`,
    name: "Grep",
    tool: "search",
    title: "deadline",
    detail: "crates/kiln/src",
    input: {},
    status: "done",
    started: at + 4_500,
    ended: at + 4_900,
  });
  if (i % 2 === 0)
    add({
      id: `L-e${i}`,
      at: at + 8_000,
      kind: "tool",
      tool_id: `te${i}`,
      name: "Edit",
      tool: "edit",
      title: `/home/ada/src/kiln/${FILES[(i + 1) % FILES.length]}`,
      input: {},
      status: "done",
      started: at + 8_000,
      ended: at + 8_100,
      diffs: [
        {
          path: `/home/ada/src/kiln/${FILES[(i + 1) % FILES.length]}`,
          hunks: [
            {
              old_start: 40 + i,
              old_lines: 3,
              new_start: 40 + i,
              new_lines: 4,
              lines: [" let now = clock.now();", "-let deadline = clock.now() + budget;", "+let deadline = now + budget;", "+let first = now;", " loop {"],
            },
          ],
        },
      ],
    });
  add({
    id: `L-s${i}`,
    at: at + 12_000,
    kind: "tool",
    tool_id: `ts${i}`,
    name: "Bash",
    tool: "shell",
    title: i % 5 === 0 ? "cargo test -p kiln retry" : "cargo test -p kiln http::client",
    input: {},
    status: "done",
    exit_code: i % 7 === 0 ? 101 : 0,
    output: Array.from({ length: 6 + (i % 5) }, (_, k) => `test http::client::retry_${k} ... ok`).join("\n") + `\n\ntest result: ${i % 7 === 0 ? "FAILED" : "ok"}. ${6 + (i % 5)} passed`,
    started: at + 12_000,
    ended: at + 19_000,
  });
  // Now and then the same tests on the Mac, through kiln reach: a command on another machine.
  if (i % 4 === 1)
    add({
      id: `L-r${i}`,
      at: at + 19_500,
      kind: "tool",
      tool_id: `tr${i}`,
      name: "mcp__kiln__run",
      tool: "shell",
      title: "cargo test -p kiln http::client --target aarch64-apple-darwin",
      detail: "on adas-macbook-pro-2.local",
      runner: "adas-macbook-pro-2.local",
      input: { on: "macos", command: "cargo test -p kiln http::client --target aarch64-apple-darwin" },
      status: "done",
      exit_code: 0,
      output: `test http::client::retry_0 ... ok\ntest http::client::retry_1 ... ok\n\ntest result: ok. 2 passed`,
      started: at + 19_500,
      ended: at + 20_800,
    });
  add({ id: `L-a${i}`, at: at + 21_000, kind: "assistant", text: answer(i) });
}

// The turn still going.
const LIVE_AT = T0 + (TURNS + 1) * 90_000;

/* ---- files a message mentions (docs/agents.md, "Files in messages") ---- */

/** A PNG of a phone's head, drawn here: bands like the console's, so a check can see it. */
function phonePng(w: number, h: number): Buffer {
  const chunk = (type: string, data: Buffer) => {
    const head = Buffer.alloc(4);
    head.writeUInt32BE(data.length);
    const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(body) >>> 0);
    return Buffer.concat([head, body, crc]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr.set([8, 2, 0, 0, 0], 8);
  const rows = Buffer.alloc((w * 3 + 1) * h);
  for (let y = 0; y < h; y++) {
    const band = y < 140 ? [12, 13, 16] : y < 190 ? [26, 24, 34] : (y >> 5) % 2 ? [16, 17, 20] : [20, 21, 25];
    for (let x = 0; x < w; x++) rows.set(band, y * (w * 3 + 1) + 1 + x * 3);
  }
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk("IHDR", ihdr), chunk("IDAT", deflateSync(rows)), chunk("IEND", Buffer.alloc(0))]);
}
const MOCK_DIR = "/tmp/kiln-mock";
const PHONE = phonePng(390, 844);
const NOTES = Buffer.from("# Notes\n\n- The head's clock holds the last turn's length.\n- The usage panel reads Cost $4.21.\n");
const RUN_LOG = Buffer.from(Array.from({ length: 40 }, (_, i) => `[${i}] test http::client::retry_${i} ... ok`).join("\n") + "\n");
const sha256 = (b: Buffer) => createHash("sha256").update(b).digest("hex");
add({ id: "L-u-files", at: LIVE_AT - 60_000, kind: "user", text: "Show me the phone view, and the notes." });
const filesSaid = add({
  id: "L-a-files",
  at: LIVE_AT - 50_000,
  kind: "assistant",
  text: `Here's the head at a phone's width:\n\n![phone view](${MOCK_DIR}/phone.png)\n\nThe notes are in [notes](${MOCK_DIR}/notes.md), the run's log is ${MOCK_DIR}/run.log, and the recording was \`${MOCK_DIR}/flow.webm\`.`,
});
const FILES_AT = LIVE_AT - 120_000;
/** What the server kept of them: the picture and the notes (by content), the log only listed (read now), the recording gone. */
const kept: KeptFile[] = [
  { path: `${MOCK_DIR}/phone.png`, seq: filesSaid.seq, meta: { size: PHONE.length, mtime: FILES_AT, mime: "image/png", width: 390, height: 844 }, sha: sha256(PHONE) },
  { path: `${MOCK_DIR}/notes.md`, seq: filesSaid.seq, meta: { size: NOTES.length, mtime: FILES_AT, mime: "text/markdown; charset=utf-8" }, sha: sha256(NOTES) },
  { path: `${MOCK_DIR}/run.log`, seq: filesSaid.seq, meta: { size: RUN_LOG.length, mtime: FILES_AT, mime: "text/plain; charset=utf-8" } },
  { path: `${MOCK_DIR}/flow.webm`, seq: filesSaid.seq, error: `${MOCK_DIR}/flow.webm isn't there any more` },
];
const blobs = new Map<string, Buffer>([
  [sha256(PHONE), PHONE],
  [sha256(NOTES), NOTES],
]);
const onDisk = new Map<string, Buffer>([
  [`${MOCK_DIR}/phone.png`, PHONE],
  [`${MOCK_DIR}/notes.md`, NOTES],
  [`${MOCK_DIR}/run.log`, RUN_LOG],
]);

/** The long session's files, as the server serves them: a kept copy by its content (for good), or
 *  a mentioned path read now; both take `Range`. */
export function handleLongFiles(path: string, url: URL, req: IncomingMessage, res: ServerResponse): boolean {
  const byContent = /^\/api\/harness\/sessions\/demo-long\/files\/([0-9a-f]{64})$/.exec(path);
  if (!byContent && path !== "/api/harness/sessions/demo-long/file") return false;
  const sha = byContent?.[1];
  const asked = url.searchParams.get("path") ?? "";
  const entry = kept.find((k) => (sha ? k.sha === sha : k.path === asked));
  const body = sha ? blobs.get(sha) : onDisk.get(asked);
  if (!entry?.meta || !body) {
    res.writeHead(404, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ error: entry?.error ?? "no message of this conversation mentions that file" }));
    return true;
  }
  const size = body.length;
  let [start, end] = [0, size - 1];
  const range = /^bytes=(\d*)-(\d*)$/.exec(req.headers.range ?? "");
  if (range) {
    if (range[1] === "") start = Math.max(0, size - Number(range[2]));
    else {
      start = Number(range[1]);
      if (range[2] !== "") end = Math.min(end, Number(range[2]));
    }
    if (start > end) {
      res.writeHead(416, { "Content-Range": `bytes */${size}` });
      res.end();
      return true;
    }
  }
  res.writeHead(range ? 206 : 200, {
    "Content-Type": entry.meta.mime,
    "Content-Length": end - start + 1,
    "Accept-Ranges": "bytes",
    "Cache-Control": sha ? "private, max-age=31536000, immutable" : "private, no-cache",
    ...(range ? { "Content-Range": `bytes ${start}-${end}/${size}` } : {}),
  });
  res.end(body.subarray(start, end + 1));
  return true;
}

// A page the agent showed (`show_page`): live in the conversation, sized before it loads.
export const PAGE_ID = "3f1c0de5a2b94e7a8c6d0b1e2f3a4b5c";
/** Its height at each width, as kiln's browser on the agent's machine measured it. */
const PAGE_HEIGHT = Number(process.env.MOCK_PAGE_HEIGHT ?? 310);
const PAGE_HTML = `<!doctype html><html><head><meta charset="utf-8"><title>Retry latency</title>
<style>
  body { font: 13px/1.5 var(--font-sans); padding: 12px 14px 14px; }
  h1 { font: 500 10px/18px var(--font-mono); letter-spacing: .09em; text-transform: uppercase; color: var(--muted-foreground); margin: 0 0 8px; }
  canvas { display: block; width: 100%; height: 220px; }
  .row { display: flex; gap: 8px; margin-top: 10px; align-items: center; height: 28px; }
  button { font: 12px/26px var(--font-mono); color: var(--foreground); background: var(--card); border: 1px solid var(--border); border-radius: 6px; height: 28px; padding: 0 10px; }
  a { color: var(--accent); font: 12px/28px var(--font-mono); }
  #more div { padding: 6px 0; border-top: 1px solid var(--border); }
</style></head>
<body>
<h1>p50 / p95 by attempt</h1>
<canvas id="c"></canvas>
<div class="row"><button id="pick">Use attempt 3</button><button id="count">Clicked 0</button><button id="add">More</button><a id="docs" href="https://example.com/docs">Docs</a></div>
<div id="more"></div>
<script>
  const c = document.getElementById("c"), g = c.getContext("2d");
  window.__frames = 0;
  const p50 = [42, 61, 88, 130, 190], p95 = [90, 140, 210, 330, 480];
  const draw = (t) => {
    window.__frames++;
    const w = (c.width = c.clientWidth * devicePixelRatio), h = (c.height = c.clientHeight * devicePixelRatio);
    const css = getComputedStyle(document.documentElement);
    const bar = w / (p50.length * 3), grow = Math.min(1, t / 900);
    p50.forEach((v, i) => {
      const x = bar * (1 + i * 3);
      g.fillStyle = css.getPropertyValue("--chart-2");
      g.fillRect(x, h - (v / 500) * h * grow, bar, (v / 500) * h * grow);
      g.fillStyle = css.getPropertyValue("--chart-4");
      g.fillRect(x + bar, h - (p95[i] / 500) * h * grow, bar * 0.9, (p95[i] / 500) * h * grow);
    });
    requestAnimationFrame(draw);
  };
  requestAnimationFrame(draw);
  let n = 0;
  const count = document.getElementById("count");
  count.onclick = () => { n++; count.textContent = "Clicked " + n; kiln.save({ n }); };
  kiln.onrestore = (s) => { n = (s && s.n) || 0; count.textContent = "Clicked " + n; };
  document.getElementById("pick").onclick = () => kiln.reply({ attempt: 3 }, "Use attempt 3");
  document.getElementById("add").onclick = () => {
    const d = document.createElement("div");
    d.textContent = "Attempt " + (document.querySelectorAll("#more div").length + 6) + ": 690 ms";
    document.getElementById("more").append(d);
  };
</script></body></html>`;
add({ id: "L-u-page", at: LIVE_AT - 45_000, kind: "user", text: "Chart the retry latencies by attempt." });
add({
  id: "L-page",
  at: LIVE_AT - 40_000,
  kind: "tool",
  tool_id: "tpage",
  name: "mcp__kiln__show_page",
  tool: "mcp",
  title: "Retry latency by attempt",
  input: { title: "Retry latency by attempt", html: PAGE_HTML },
  status: "done",
  output: JSON.stringify({
    kiln_page: { id: PAGE_ID, title: "Retry latency by attempt", path: `/v/${PAGE_ID}/`, files: 0, heights: [320, 360, 390, 430, 520, 640, 728, 860, 1000, 1144].map((w) => [w, PAGE_HEIGHT]) },
    seen: { width: 390, height: 844, content_height: PAGE_HEIGHT, said: [], layout_shift: 0, long_tasks: 0, longest_task_ms: 0, fps: 60 },
  }),
  started: LIVE_AT - 40_000,
  ended: LIVE_AT - 38_000,
});
add({ id: "L-a-page", at: LIVE_AT - 37_000, kind: "assistant", text: "Attempt 3 is where p95 crosses 200 ms; the chart is above." });

/** kiln's bridge and theme, as the server puts them first in a page (crates/kiln/src/harness). */
const BRIDGE = readFileSync(new URL("../../crates/kiln/src/harness/bridge.js", import.meta.url), "utf8");
const PAGE_HEAD = `<meta name="color-scheme" content="dark light"><style data-kiln>:root{--background:transparent;--foreground:#e9ebee;--muted-foreground:#8f929b;--card:#0c0d10;--border:#2b2d32;--accent:#bab0e3;--chart-2:#a6c4ed;--chart-4:#d6a2aa;--font-sans:"Geist Variable",ui-sans-serif,system-ui,sans-serif;--font-mono:"Geist Mono Variable",ui-monospace,monospace;color-scheme:dark}html,body{margin:0;background:var(--background);color:var(--foreground);font-family:var(--font-sans)}html{scrollbar-width:none}</style><script data-kiln>${BRIDGE}</script>`;

/** Pages, as the server serves them: sandboxed, framed only by kiln, never cached by anyone else. */
export function handleShownPages(path: string, res: ServerResponse): boolean {
  const m = /^\/v\/([0-9a-f]{32})(\/?)$/.exec(path);
  if (!m) return false;
  if (m[1] !== PAGE_ID) {
    res.writeHead(404, { "Content-Type": "text/plain" });
    res.end("no such page");
    return true;
  }
  if (!m[2]) {
    res.writeHead(308, { Location: `/v/${PAGE_ID}/` });
    res.end();
    return true;
  }
  const html = PAGE_HTML.replace("<head>", `<head>${PAGE_HEAD}`);
  res.writeHead(200, {
    "Content-Type": "text/html; charset=utf-8",
    "Content-Security-Policy":
      "sandbox allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox allow-downloads allow-pointer-lock; default-src 'self' data: blob:; script-src 'self' 'unsafe-inline' 'unsafe-eval' data: blob:; style-src 'self' 'unsafe-inline' data:; img-src 'self' data: blob:; media-src 'self' data: blob:; font-src 'self' data:; connect-src 'self' data: blob:; worker-src 'self' data: blob:; frame-src 'self' data: blob:; form-action 'none'; base-uri 'none'; frame-ancestors 'self'",
    "Referrer-Policy": "no-referrer",
    "X-Content-Type-Options": "nosniff",
    "Cache-Control": "private, max-age=31536000, immutable",
  });
  res.end(html);
  return true;
}

add({ id: "L-u-live", at: LIVE_AT, kind: "user", text: "Now do the same for the websocket reconnects, and keep going until the suite is green." });
// A long turn already under way: what it said and ran, step after step.
for (let k = 1; k <= LIVE_STEPS; k++) {
  const at = LIVE_AT + 1_000 + k * 20;
  add({ id: `L-live-n${k}`, at, kind: "assistant", text: `Step ${k}: the reconnect path in \`${FILES[k % FILES.length]}\` reads its backoff from the same clock as the deadline, so I'm checking it next.` });
  add({
    id: `L-live-s${k}`,
    at: at + 5,
    kind: "tool",
    tool_id: `tl${k}`,
    name: "Bash",
    tool: "shell",
    title: `cargo test -p kiln ws::reconnect_${k}`,
    input: {},
    status: "done",
    exit_code: 0,
    output: Array.from({ length: 8 }, (_, j) => `test ws::reconnect_${k}_${j} ... ok`).join("\n") + "\n\ntest result: ok. 8 passed",
    started: at + 5,
    ended: at + 15,
  });
}
add({
  id: "L-s-live",
  at: LIVE_AT + 3_000,
  kind: "tool",
  tool_id: "ts-live",
  name: "Bash",
  tool: "shell",
  title: "cargo test -p kiln ws::reconnect",
  input: {},
  status: "running",
  started: LIVE_AT + 3_000,
});
// And on Linux x86-64, while kiln picks the runner.
add({
  id: "L-r-live",
  at: LIVE_AT + 3_500,
  kind: "tool",
  tool_id: "tr-live",
  name: "mcp__kiln__run",
  tool: "shell",
  title: "cargo test -p kiln ws::reconnect --target x86_64-unknown-linux-musl",
  detail: "on linux/amd64",
  runner: "linux/amd64",
  input: { on: "linux/amd64" },
  status: "running",
  started: LIVE_AT + 3_500,
});
let live = add({ id: "L-a-live", at: LIVE_AT + 5_000, kind: "assistant", text: "Working through the reconnect path:", streaming: true }) as Extract<HEvent, { kind: "assistant" }>;

const PAUSED = process.env.MOCK_PAUSED === "1";
const PAUSED_UNTIL = Date.now() + 47 * 60_000;
if (PAUSED) {
  live.streaming = false;
  for (let i = 0; i < 7; i++) add({ id: `L-limit-${i}`, at: LIVE_AT + 6_000 + i, kind: "notice", level: "error", text: "You've hit your session limit · resets 10:30pm (America/Los_Angeles)" });
  add({
    id: "pause-1",
    at: LIVE_AT + 7_000,
    kind: "notice",
    level: "warn",
    text: "Paused: You've hit your session limit · resets 10:30pm (America/Los_Angeles). Queued messages and goals wait; it goes on by itself once the limit resets.",
  });
}
/** MOCK_STOP_POINT=1: paused at its stop point for the weekly limit, not at the session limit. */
const STOP_POINT = process.env.MOCK_STOP_POINT === "1";
/** When it paused (an old runner's stop point pauses it again as it resumes), and whether a resume took. */
let pauseSince = LIVE_AT + 7_000;
let resumed = false;
let stopOff = false;
const pause = (): State["pause"] =>
  PAUSED && !resumed
    ? {
        reason: STOP_POINT ? "threshold" : "limit",
        window: STOP_POINT ? "seven_day" : "five_hour",
        label: STOP_POINT ? "Weekly" : "Session",
        said: STOP_POINT ? "Weekly 95% used: its stop point" : "You've hit your session limit · resets 10:30pm (America/Los_Angeles)",
        until: PAUSED_UNTIL,
        since: pauseSince,
        auto: true,
        by: "harness",
        cut: { message: "L-u-live", text: "Is the client still cancel-safe?", at: LIVE_AT + 7_000 },
        checkpoint: { git_ref: "refs/kiln/checkpoints/demo-long/1792300000000", commit: "4f1c2a9e07b3d5c6a8e9f0b1c2d3e4f5a6b7c8d9", at: LIVE_AT + 8_000 },
      }
    : undefined;
// MOCK_NEAR=1: close to the 5-hour limit, with a weekly one and a model's own (as a Max plan shows them).
const NEAR = process.env.MOCK_NEAR === "1";
const limits = (): State["limits"] =>
  PAUSED
    ? [
        { id: "five_hour", label: "Session", used_percent: 100, resets_at: PAUSED_UNTIL, window: "5h", status: "rejected" },
        { id: "seven_day", label: "Weekly", used_percent: 62, resets_at: PAUSED_UNTIL + 3 * 86_400_000, window: "7d" },
      ]
    : NEAR
      ? [
          { id: "five_hour", label: "Session", used_percent: 99, resets_at: Date.now() + 82 * 60_000, window: "5h", status: "warning" },
          { id: "seven_day", label: "Weekly", used_percent: 80, resets_at: Date.now() + 5 * 86_400_000 + 13 * 3_600_000, window: "7d", status: "warning" },
          { id: "seven_day_fable", label: "Weekly · Fable", used_percent: 0, resets_at: Date.now() + 5 * 86_400_000 + 13 * 3_600_000, window: "7d" },
        ]
      : [{ id: "seven_day", label: "Weekly", used_percent: 62, resets_at: LIVE_AT + 3 * 86_400_000, window: "7d" }];
// MOCK_OFFLINE_MS: its machine is away that long from the start, then back.
let offlineUntil = Date.now() + Number(process.env.MOCK_OFFLINE_MS ?? 0);
const offline = () => Date.now() < offlineUntil;
let pauseAt: number | undefined;
/** The most subagents it runs at once (`max_subagents`). */
let maxSubagents: number | undefined;
let moveOn = false;
/** Where it goes on at a limit (`State.move_plan`). */
let movePlan: { to?: string; kind?: string; model?: string } = {};
/** Its runner from before agents went on elsewhere (MOCK_OLD_RUNNER=1): its state says nothing of it. */
const OLD_RUNNER = process.env.MOCK_OLD_RUNNER === "1";

const info: AgentInfo = {
  id: ID,
  kind: "claude",
  kind_name: "Claude Code",
  // MOCK_LONG_TARGET=agent:<name>: on one of the machines (it can move); else on the server.
  target: process.env.MOCK_LONG_TARGET ?? "local",
  name: "Retry storm",
  cwd: "/home/ada/src/kiln",
  created_at: T0,
  status: PAUSED ? "idle" : "running",
  title: "Retry backoff rework",
  model: "claude-opus-5-5",
  mode: "acceptEdits",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 4.21,
  last_activity: LIVE_AT,
  pause: pause(),
  limits: limits(),
};

const meta: Meta = {
  version: "2.3.1",
  modes: [
    { id: "plan", label: "Plan", description: "Reads and plans; changes nothing" },
    { id: "default", label: "Ask", description: "Asks before edits and commands" },
    { id: "acceptEdits", label: "Accept edits", description: "Edits files without asking; asks before commands" },
  ],
  models: [
    { id: "default", label: "Default (recommended)", description: "Opus 5.5" },
    { id: "claude-opus-5-5", label: "claude-opus-5-5" },
    { id: "claude-sonnet-5", label: "claude-sonnet-5" },
  ],
  efforts: ["low", "medium", "high", "max"].map((id) => ({ id, label: id.charAt(0).toUpperCase() + id.slice(1) })),
  commands: [
    { name: "compact", description: "Summarize the conversation to free context", group: "Context", args: "optional", route: { kind: "prompt" } },
    { name: "login", description: "Sign in to Claude Code on this machine", group: "Account", args: "none", route: { kind: "view", view: "login" } },
  ],
  agents: [],
  features: ["interrupt", "set_mode", "set_model", "set_effort", "steer", "send_now", "queue_cancel", "attachments", "switch_harness"],
};

let activity = "Running cargo test";
let effort = "high";
// A long turn under way, as the harness reports it (`doing`, since `turn_started`): it thinks, then
// takes a step, and thinks again. An interrupt doesn't take (so a stop can be forced), a forced stop
// ends the turn, and a message sent to it when it's done starts the next.
let running = !PAUSED;
let doing = "Thinking";
let turnStarted = Date.now() - Number(process.env.MOCK_TURN_AGE_MS ?? 134_000);
let lastTurn: State["last_turn"];
const state = (): State => ({
  status: offline() ? "disconnected" : running ? "running" : "idle",
  pause_at: pauseAt,
  max_subagents: maxSubagents,
  ...(OLD_RUNNER ? {} : { move_on_limit: moveOn, move_plan: { ...movePlan } }),
  doing: running ? doing : undefined,
  turn_started: running ? turnStarted : undefined,
  last_turn: running ? undefined : lastTurn,
  cwd: info.cwd,
  model: info.model,
  mode: info.mode,
  effort,
  title: info.title,
  activity,
  approvals: [],
  queue: PAUSED
    ? [
        { id: "q1", text: "Then run the whole suite again.", at: LIVE_AT + 7_500 },
        { id: "q2", text: "Write it up for the changelog.", at: LIVE_AT + 7_600 },
      ]
    : [],
  delivery: {},
  tasks: [],
  todos: [],
  usage: { cost_usd: 4.21, input_tokens: 812_000, output_tokens: 64_000, cache_read_tokens: 5_100_000, turns: TURNS + 1 },
  plan: "Max",
  dropped: DROPPED,
  pause: pause(),
  limits: limits(),
});

const viewers = new Set<WebSocket>();
/** Viewers that take a state as what changed in it (`?patch=state`), with the state each has, as the server keeps it. */
const had = new WeakMap<WebSocket, object>();
const send = (ws: WebSocket, u: unknown) => {
  if (ws.readyState !== ws.OPEN) return;
  const was = had.get(ws) as Record<string, unknown> | undefined;
  const f = u as { t?: string; state?: Record<string, unknown> };
  if (was && f.t === "state" && f.state) {
    // As it goes over the wire: a field left undefined isn't there.
    const now = JSON.parse(JSON.stringify(f.state)) as Record<string, unknown>;
    had.set(ws, now);
    const set = Object.fromEntries(Object.entries(now).filter(([k, v]) => JSON.stringify(was[k]) !== JSON.stringify(v)));
    const gone = Object.keys(was).filter((k) => !(k in now));
    ws.send(JSON.stringify({ t: "state_patch", set, ...(gone.length ? { gone } : {}) }));
    return;
  }
  ws.send(JSON.stringify(u));
};
const broadcast = (u: unknown) => viewers.forEach((ws) => send(ws, u));

const WORDS =
  "the reconnect path reads its backoff from the same clock as the deadline so a slow tick can no longer put the deadline before the first attempt and the socket comes back within one interval".split(
    " ",
  );
let w = 0;
let step = 0;
let timer: NodeJS.Timeout | null = null;

/** While anyone watches: the reply streams, and now and then it stops to take a step and goes on after it (as a harness's turn does). */
/** After a step it thinks a while, saying nothing (as a model does after a tool call). */
let thinkingUntil = 0;

function tick() {
  if (viewers.size === 0 || PAUSED) {
    timer = null;
    return;
  }
  if (Date.now() < thinkingUntil) {
    timer = setTimeout(tick, 140);
    return;
  }
  if (!live.streaming) {
    live = add({ id: `L-a-live-${step}`, at: Date.now(), kind: "assistant", text: "", streaming: true }) as Extract<HEvent, { kind: "assistant" }>;
    broadcast({ t: "events", events: [{ ...live }] });
    doing = "Writing";
    broadcast({ t: "state", state: state() });
  }
  const word = `${WORDS[w % WORDS.length]} `;
  w += 1;
  live.text += word;
  broadcast({ t: "stream", id: live.id, text: word });
  if (w % 40 === 0) {
    step += 1;
    const at = Date.now();
    // The words so far are done; a step; then the reply goes on, in a note of its own.
    live.streaming = false;
    const e = add({
      id: `L-step-${step}`,
      at,
      kind: "tool",
      tool_id: `tstep${step}`,
      name: "Read",
      tool: "read",
      title: `/home/ada/src/kiln/${FILES[step % FILES.length]}`,
      input: {},
      status: "done",
      started: at,
      ended: at + 120,
    });
    const done = { ...live };
    broadcast({ t: "events", events: [done, e] });
    activity = step % 2 ? "Reading files" : "Running cargo test";
    doing = "Thinking";
    thinkingUntil = Date.now() + 4_000;
    broadcast({ t: "state", state: state() });
  }
  timer = setTimeout(tick, 140);
}

/** The server's event stream (set by the mock's server). */
let emit: (ev: { type: "harness"; info: AgentInfo }) => void = () => {};
export function setLongEmit(fn: typeof emit) {
  emit = fn;
}

/* ------------------------------------------------------------------ the conversation store's copy */

const DROPPED = Number(process.env.MOCK_DROPPED ?? 0);
/** What the timeline let go of (MOCK_DROPPED): the store still has it. Before the forty turns. */
const earlier: HEvent[] = Array.from({ length: Math.min(DROPPED, 600) }, (_, i) => {
  const at = T0 - (DROPPED - i) * 60_000;
  const id = `E-${i}`;
  return i % 3 === 0
    ? { id, seq: -DROPPED + i, at, kind: "user", text: `${ASKS[i % ASKS.length]} (earlier, ${i})` }
    : { id, seq: -DROPPED + i, at, kind: "assistant", text: `Earlier: the retry loop in \`${FILES[i % FILES.length]}\` still read the clock twice; the backoff test failed on tick ${i}.` };
});

const wordsOf = (e: HEvent) => (e.kind === "tool" ? [e.title, e.detail, e.output].filter(Boolean).join("\n") : e.kind === "thinking" && e.redacted ? "" : e.text);

/** `GET /api/conversations/search` as the server answers it from its store: every word, any case, newest first. */
export function searchSaid(q: string, agent: string | null, until: number | null, limit: number): SaidHit[] {
  const words = q.toLowerCase().split(/\s+/).filter(Boolean);
  const all = [...earlier, ...events].filter((e) => (until === null || e.at < until) && words.every((w) => wordsOf(e).toLowerCase().includes(w)));
  const hits: SaidHit[] = (agent === null || agent === ID ? all : []).map((event) => ({ agent: ID, name: info.name, machine: info.target, event }));
  // One from an agent that ended and is forgotten (no name: it can't be opened).
  if (agent === null && words.every((w) => "the retry test passed on the old runner".includes(w)))
    hits.push({ agent: "gone-1", name: null, machine: null, event: { id: "G-1", seq: 1, at: T0 - 86_400_000, kind: "assistant", text: "The retry test passed on the old runner." } });
  return hits.sort((a, b) => b.event.at - a.event.at).slice(0, limit);
}

const late = Number(process.env.MOCK_LATE_SESSION ?? 0);
const knownAt = Date.now() + late;
if (late > 0) setTimeout(() => emit({ type: "harness", info: longInfo() }), late);
/** Agents at work send a stream of news (MOCK_CHURN=n): n of its entry from 4 s on, 100 ms apart, the last with an approval waiting. */
const CHURN = Number(process.env.MOCK_CHURN ?? 0);
for (let i = 1; i <= CHURN; i++) {
  const waits = i === CHURN ? [{ id: "churn", tool: "shell" as const, tool_name: "Bash", title: "rm -rf build", at: Date.now() }] : [];
  setTimeout(() => emit({ type: "harness", info: { ...longInfo(), activity: `Step ${i}`, approvals: waits } }), 4_000 + i * 100);
}
// Its machine back: viewers hear it, as the server tells them.
if (offline())
  setTimeout(() => {
    broadcast({ t: "state", state: state() });
    emit({ type: "harness", info: longInfo() });
  }, offlineUntil - Date.now() + 50);
/** Send its machine away again for `ms` (the browser checks drive it, `POST /mock/away`). */
export function sendAway(ms: number) {
  offlineUntil = Date.now() + ms;
  broadcast({ t: "state", state: state() });
  setTimeout(() => broadcast({ t: "state", state: state() }), ms + 50);
}

export function handleLongSession(ws: WebSocket, id: string, packed = false, patches = false): boolean {
  if (id !== ID) return false;
  if (Date.now() < knownAt) {
    // Not known here yet (its machine hasn't reconnected): what the server says, then it hangs up.
    send(ws, { t: "closed" });
    ws.close();
    return true;
  }
  const slow = Number(process.env.MOCK_SLOW_SNAPSHOT ?? 0);
  const first = state();
  const frames: unknown[] = [
    { t: "open", info },
    { t: "meta", meta },
    { t: "state", state: first },
  ];
  // What's kept of the files its messages mention, ahead of the events (MOCK_FILES=0: none yet).
  if (process.env.MOCK_FILES !== "0") frames.push({ t: "files", files: kept });
  // Events in batches, as the server cuts them (by size); the last frame says it's complete.
  for (let i = 0; i < events.length; i += 120) frames.push({ t: "events", events: events.slice(i, i + 120) });
  if (process.env.MOCK_NO_SYNCED !== "1") frames.push({ t: "synced" });
  // A viewer that unpacks gzip (`?z=gzip`): the whole snapshot packed in one message, a frame a line, as the server sends it.
  if (packed && slow === 0) {
    ws.send(gzipSync(frames.map((f) => JSON.stringify(f)).join("\n")));
    if (patches) had.set(ws, first);
    viewers.add(ws);
    if (!timer) timer = setTimeout(tick, 140);
    frames.length = 0;
  }
  const go = (i: number) => {
    if (i >= frames.length) {
      if (patches && !had.has(ws)) had.set(ws, first);
      viewers.add(ws);
      if (!timer) timer = setTimeout(tick, 140);
      return;
    }
    send(ws, frames[i]);
    if (slow > 0) setTimeout(() => go(i + 1), slow);
    else go(i + 1);
  };
  go(0);
  ws.on("message", (raw) => {
    let c: { type?: string; text?: string; id?: string; mode?: string; model?: string; effort?: string; percent?: number | null; count?: number | null; on?: boolean; to?: string; kind?: string };
    try {
      c = JSON.parse(raw.toString()) as typeof c;
    } catch {
      return;
    }
    // Its machine is away: the server can't hand it anything (it says so; the message is lost).
    if (offline() && c.type === "send") {
      send(ws, { t: "error", error: "its machine is away" });
      return;
    }
    if (c.type === "pause_at") {
      pauseAt = c.percent ?? undefined;
      if (c.percent === null) stopOff = true;
    } else if (c.type === "max_subagents") {
      maxSubagents = c.count ?? undefined;
      broadcast({ t: "state", state: state() });
    } else if (c.type === "resume" && PAUSED) {
      // A runner from before a stop point could be resumed past: it stops again at once, unless
      // the stop point is off. A newer one goes on.
      if (OLD_RUNNER && STOP_POINT && !stopOff)
        setTimeout(() => {
          pauseSince = Date.now();
          broadcast({ t: "state", state: state() });
        }, 600);
      else if (!OLD_RUNNER || stopOff) {
        resumed = true;
        broadcast({ t: "state", state: state() });
      }
    } else if (c.type === "move_on_limit" && !OLD_RUNNER) {
      moveOn = Boolean(c.on);
      movePlan = moveOn ? { ...(c.to ? { to: c.to } : {}), ...(c.kind ? { kind: c.kind } : {}), ...(c.model ? { model: c.model } : {}) } : {};
    }
    else if (c.type === "set_mode" && c.mode) info.mode = c.mode;
    else if (c.type === "set_model" && c.model) info.model = c.model;
    else if (c.type === "set_effort" && c.effort) effort = c.effort;
    else if (c.type === "force_stop") {
      running = false;
      lastTurn = { at: Date.now(), how: "stopped", kept: 0 };
    }
    else if (c.type === "send" && c.text) {
      const e = add({ id: `L-sent-${seq}`, at: Date.now(), kind: "user", text: c.text, client_id: c.id });
      if (!running) {
        running = true;
        doing = "Thinking";
        turnStarted = Date.now();
        broadcast({ t: "state", state: state() });
      }
      // Steered and sent-now messages show where they went in; a queued one would wait (not modeled).
      broadcast({ t: "events", events: [e] });
      return;
    } else return;
    broadcast({ t: "state", state: state() });
  });
  ws.on("close", () => viewers.delete(ws));
  return true;
}

/** Its row in the agents list. */
export const longInfo = () => ({ ...info, status: PAUSED ? ("idle" as const) : ("running" as const), activity, last_activity: Date.now(), pause: pause(), limits: limits() });
