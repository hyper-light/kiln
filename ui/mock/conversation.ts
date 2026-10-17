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
 *   MOCK_LATE_SESSION=ms   for this long after the mock starts, it doesn't know the session (as a
 *                          restarted server before the session's machine reconnects): its socket
 *                          says {"t":"closed"}; then the session is announced on /api/events
 */
import type { WebSocket } from "ws";
import type { AgentInfo, HEvent, Meta, State } from "../src/api/harness.ts";

const ID = "demo-long";
/** A fixed start, so the history is the same on every run. */
const T0 = Date.UTC(2026, 9, 17, 9, 0, 0);
const TURNS = 40;

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
  add({ id: `L-r${i}a`, at: at + 4_000, kind: "tool", tool_id: `tr${i}a`, name: "Read", tool: "read", title: `/home/ada/src/kiln/${FILES[i % FILES.length]}`, input: {}, status: "done", started: at + 4_000, ended: at + 4_200 });
  add({ id: `L-r${i}b`, at: at + 4_500, kind: "tool", tool_id: `tr${i}b`, name: "Grep", tool: "search", title: "deadline", detail: "crates/kiln/src", input: {}, status: "done", started: at + 4_500, ended: at + 4_900 });
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
          hunks: [{ old_start: 40 + i, old_lines: 3, new_start: 40 + i, new_lines: 4, lines: [" let now = clock.now();", "-let deadline = clock.now() + budget;", "+let deadline = now + budget;", "+let first = now;", " loop {"] }],
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
  add({ id: `L-a${i}`, at: at + 21_000, kind: "assistant", text: answer(i) });
}

// The turn still going.
const LIVE_AT = T0 + (TURNS + 1) * 90_000;
add({ id: "L-u-live", at: LIVE_AT, kind: "user", text: "Now do the same for the websocket reconnects, and keep going until the suite is green." });
add({ id: "L-s-live", at: LIVE_AT + 3_000, kind: "tool", tool_id: "ts-live", name: "Bash", tool: "shell", title: "cargo test -p kiln ws::reconnect", input: {}, status: "running", started: LIVE_AT + 3_000 });
let live = add({ id: "L-a-live", at: LIVE_AT + 5_000, kind: "assistant", text: "Working through the reconnect path:", streaming: true }) as Extract<HEvent, { kind: "assistant" }>;

const info: AgentInfo = {
  id: ID,
  kind: "claude",
  kind_name: "Claude Code",
  target: "local",
  name: "Retry storm",
  cwd: "/home/ada/src/kiln",
  created_at: T0,
  status: "running",
  title: "Retry backoff rework",
  model: "claude-opus-5-5",
  mode: "acceptEdits",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 4.21,
  last_activity: LIVE_AT,
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
  commands: [{ name: "compact", description: "Summarize the conversation to free context", group: "Context", args: "optional", route: { kind: "prompt" } }],
  agents: [],
  features: ["interrupt", "set_mode", "set_model", "set_effort", "steer", "send_now", "queue_cancel"],
};

let activity = "Running cargo test";
let effort = "high";
const state = (): State => ({
  status: "running",
  cwd: info.cwd,
  model: info.model,
  mode: info.mode,
  effort,
  title: info.title,
  activity,
  approvals: [],
  queue: [],
  delivery: {},
  tasks: [],
  todos: [],
  usage: { cost_usd: 4.21, input_tokens: 812_000, output_tokens: 64_000, cache_read_tokens: 5_100_000, turns: TURNS + 1 },
  dropped: 0,
});

const viewers = new Set<WebSocket>();
const send = (ws: WebSocket, u: unknown) => {
  if (ws.readyState === ws.OPEN) ws.send(JSON.stringify(u));
};
const broadcast = (u: unknown) => viewers.forEach((ws) => send(ws, u));

const WORDS = "the reconnect path reads its backoff from the same clock as the deadline so a slow tick can no longer put the deadline before the first attempt and the socket comes back within one interval".split(" ");
let w = 0;
let step = 0;
let timer: NodeJS.Timeout | null = null;

/** While anyone watches: the reply streams, and now and then it stops to take a step and goes on after it (as a harness's turn does). */
function tick() {
  if (viewers.size === 0) {
    timer = null;
    return;
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
    const e = add({ id: `L-step-${step}`, at, kind: "tool", tool_id: `tstep${step}`, name: "Read", tool: "read", title: `/home/ada/src/kiln/${FILES[step % FILES.length]}`, input: {}, status: "done", started: at, ended: at + 120 });
    const done = { ...live };
    live = add({ id: `L-a-live-${step}`, at: at + 200, kind: "assistant", text: "", streaming: true }) as Extract<HEvent, { kind: "assistant" }>;
    broadcast({ t: "events", events: [done, e, { ...live }] });
    activity = step % 2 ? "Reading files" : "Running cargo test";
    broadcast({ t: "state", state: state() });
  }
  timer = setTimeout(tick, 140);
}

/** The server's event stream (set by the mock's server). */
let emit: (ev: { type: "harness"; info: AgentInfo }) => void = () => {};
export function setLongEmit(fn: typeof emit) {
  emit = fn;
}

const late = Number(process.env.MOCK_LATE_SESSION ?? 0);
const knownAt = Date.now() + late;
if (late > 0) setTimeout(() => emit({ type: "harness", info: longInfo() }), late);

export function handleLongSession(ws: WebSocket, id: string): boolean {
  if (id !== ID) return false;
  if (Date.now() < knownAt) {
    // Not known here yet (its machine hasn't reconnected): what the server says, then it hangs up.
    send(ws, { t: "closed" });
    ws.close();
    return true;
  }
  const slow = Number(process.env.MOCK_SLOW_SNAPSHOT ?? 0);
  const frames: unknown[] = [{ t: "open", info }, { t: "meta", meta }, { t: "state", state: state() }];
  // Events in batches, as the server cuts them (by size); the last frame says it's complete.
  for (let i = 0; i < events.length; i += 120) frames.push({ t: "events", events: events.slice(i, i + 120) });
  if (process.env.MOCK_NO_SYNCED !== "1") frames.push({ t: "synced" });
  const go = (i: number) => {
    if (i >= frames.length) {
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
    let c: { type?: string; text?: string; id?: string; mode?: string; model?: string; effort?: string };
    try {
      c = JSON.parse(raw.toString()) as typeof c;
    } catch {
      return;
    }
    if (c.type === "set_mode" && c.mode) info.mode = c.mode;
    else if (c.type === "set_model" && c.model) info.model = c.model;
    else if (c.type === "set_effort" && c.effort) effort = c.effort;
    else if (c.type === "send" && c.text) {
      const e = add({ id: `L-sent-${seq}`, at: Date.now(), kind: "user", text: c.text, client_id: c.id });
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
export const longInfo = () => ({ ...info, status: "running" as const, activity, last_activity: Date.now() });
