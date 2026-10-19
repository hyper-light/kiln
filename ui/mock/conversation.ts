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
import { gzipSync } from "node:zlib";
import type { WebSocket } from "ws";
import type { AgentInfo, HEvent, Meta, SaidHit, State } from "../src/api/harness.ts";

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
  add({ id: `L-a${i}`, at: at + 21_000, kind: "assistant", text: answer(i) });
}

// The turn still going.
const LIVE_AT = T0 + (TURNS + 1) * 90_000;
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
const pause = (): State["pause"] =>
  PAUSED
    ? {
        reason: "limit",
        window: "five_hour",
        label: "Session",
        said: "You've hit your session limit · resets 10:30pm (America/Los_Angeles)",
        until: PAUSED_UNTIL,
        since: LIVE_AT + 7_000,
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
let moveOn = false;

const info: AgentInfo = {
  id: ID,
  kind: "claude",
  kind_name: "Claude Code",
  target: "local",
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
  features: ["interrupt", "set_mode", "set_model", "set_effort", "steer", "send_now", "queue_cancel", "attachments"],
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
  move_on_limit: moveOn || undefined,
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
    const now = f.state;
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
    let c: { type?: string; text?: string; id?: string; mode?: string; model?: string; effort?: string; percent?: number | null; on?: boolean };
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
    if (c.type === "pause_at") pauseAt = c.percent ?? undefined;
    else if (c.type === "move_on_limit") moveOn = Boolean(c.on);
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
