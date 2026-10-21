/**
 * An agent with fast output, for the mock: /agents/demo-fast.
 *
 * Each viewer gets the same script from the moment its socket opens (seeded, so one run compares
 * with the next): a history of finished turns, then a turn under way where the agent works as fast
 * as harnesses do. Its replies stream in deltas of a word or three, 30 to 60 a second; its tool
 * calls start and finish every 100 to 300 ms (reads and searches in runs, a few at once, commands
 * one after another, edits, now and then a long command whose output streams in); thinking blocks
 * start and end; subagents start, work in their own threads (their calls and words as fast) and
 * finish; its usage, activity and tasks change as it goes. After some thirty rounds of that the
 * turn ends with an answer and, a moment later, the next one begins.
 *
 *   MOCK_FAST=1          it's in the agents list (its page works without)
 *   MOCK_FAST_SEED=n     another script (the same one every time for the same n)
 *   MOCK_FAST_TURNS=n    finished turns before the one under way (40)
 *   MOCK_FAST_STEPS=n    things it already did in the turn under way (40)
 *   MOCK_FAST_RATE=x     everything x times as fast (0.5: half as fast)
 */
import type { WebSocket } from "ws";
import type { AgentInfo, FileChange, FileDiff, HEvent, Meta, State, Task, Todo } from "../src/api/harness.ts";
import { mulberry32 } from "./data.ts";

const ID = "demo-fast";
const LISTED = process.env.MOCK_FAST === "1";
const SEED = Number(process.env.MOCK_FAST_SEED ?? 7);
const TURNS = Number(process.env.MOCK_FAST_TURNS ?? 40);
const STEPS = Number(process.env.MOCK_FAST_STEPS ?? 40);
const RATE = Number(process.env.MOCK_FAST_RATE ?? 1) || 1;
/** Rounds of work (think, calls, a reply) in a turn before it ends. */
const ROUNDS = 30;
/** Between a turn's end and the next one. */
const BETWEEN_MS = 2_500;
/** Subagents at work at once, at most. */
const MOST_SUBAGENTS = 2;

const CWD = "/home/ada/src/kiln";
const FILES = [
  "crates/kiln/src/http/client.rs",
  "crates/kiln/src/http/retry.rs",
  "crates/kiln/src/rt/timer.rs",
  "crates/kiln/src/ws/reconnect.rs",
  "crates/kiln/tests/retry.rs",
  "ui/src/api/client.ts",
  "ui/src/api/harness.ts",
];
const SEARCHES = ["deadline", "backoff", "fn reconnect", "Retry-After", "clock.now()", "jitter", "MAX_ATTEMPTS"];
const COMMANDS = [
  "cargo check -p kiln",
  "cargo test -p kiln http::client",
  "cargo test -p kiln ws::reconnect",
  "git status --short",
  "rg -n 'clock.now' crates/kiln/src",
  "cargo clippy -p kiln -- -D warnings",
];
const ASKS = [
  "Make the reconnect backoff deterministic under test.",
  "Now the same for the retry loop, and keep going until the suite is green.",
  "Tighten the error messages while you're there.",
  "Run the whole suite again.",
];
const PHRASES = [
  "the reconnect path reads its backoff from the same clock as the deadline",
  "so a slow tick can no longer put the deadline before the first attempt",
  "and the socket comes back within one interval",
  "the jitter comes from the test's seeded source when one is set",
  "the timer wheel had the same race, one level down",
  "which is why the test failed one run in ten",
  "and `Retry-After` wins over the backoff when the server sends one",
  "capped at the deadline so a long header can't stall the client",
  "I checked the cancel path as well",
  "the client stays cancel-safe because every await holds no lock",
];
const THOUGHTS = [
  "The deadline is computed before the first attempt, so if the clock ticks between the two reads the budget is already short.",
  "If I take one instant and derive both from it the race goes away, but the timer wheel also reads the clock on its own.",
  "The test seeds the jitter source, yet the backoff ignores it unless the builder is told; that's the other half.",
  "Running the suite in a loop will show whether it's gone; ten runs in a row should be enough to trust it.",
];

/** An event before it's numbered (each kind of event keeps its own fields). */
type Unnumbered = HEvent extends infer E ? (E extends HEvent ? Omit<E, "seq"> : never) : never;
type Tool = Extract<HEvent, { kind: "tool" }>;
type Said = Extract<HEvent, { kind: "assistant" }> | Extract<HEvent, { kind: "thinking" }>;

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
  ],
  efforts: ["low", "medium", "high", "max"].map((id) => ({ id, label: id.charAt(0).toUpperCase() + id.slice(1) })),
  commands: [],
  agents: [],
  features: ["interrupt", "set_mode", "set_model", "set_effort", "steer", "send_now", "queue_cancel"],
};

const info = (): AgentInfo => ({
  id: ID,
  kind: "claude",
  kind_name: "Claude Code",
  target: "local",
  name: "Fast output",
  cwd: CWD,
  created_at: Date.now() - 3_600_000,
  status: "running",
  title: "Reconnect backoff, at full speed",
  model: "claude-opus-5-5",
  mode: "acceptEdits",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 2.4,
  last_activity: Date.now(),
});

/** Its row in the agents list: there only with MOCK_FAST=1 (its page works without). */
export const fastInfos = (): AgentInfo[] => (LISTED ? [info()] : []);

const pick = <T>(r: () => number, xs: readonly T[]): T => xs[Math.floor(r() * xs.length)]!;
const between = (r: () => number, a: number, b: number) => a + r() * (b - a);
const int = (r: () => number, a: number, b: number) => Math.floor(between(r, a, b + 1));

/** A reply as markdown: a paragraph, then now and then a list, a code block, a closing line. */
function replyOf(r: () => number): string {
  const sentence = () => {
    const s = `${pick(r, PHRASES)}, ${pick(r, PHRASES)}.`;
    return s.charAt(0).toUpperCase() + s.slice(1);
  };
  // One in five goes on for several paragraphs (an answer, a plan).
  const parts: string[] = [];
  for (let n = r() < 0.2 ? int(r, 4, 7) : 1; n > 0; n--) {
    parts.push(Array.from({ length: int(r, 1, 3) }, sentence).join(" "));
    if (r() < 0.45) parts.push(Array.from({ length: int(r, 2, 4) }, () => `- \`${pick(r, FILES).split("/").pop()}\`: ${pick(r, PHRASES)}.`).join("\n"));
    if (r() < 0.2) parts.push(["```rust", "let now = clock.now();", "let deadline = now + budget;", "let wait = backoff.next(now).min(deadline - now);", "```"].join("\n"));
  }
  if (r() < 0.4) parts.push(sentence());
  return parts.join("\n\n");
}

/** Text as it streams: a word to three at a time, whitespace kept with the word before it. */
function deltasOf(r: () => number, text: string, most = 3): string[] {
  const words = text.match(/\s*\S+\s*/g) ?? [text];
  const out: string[] = [];
  for (let i = 0; i < words.length;) {
    const n = int(r, 1, most);
    out.push(words.slice(i, i + n).join(""));
    i += n;
  }
  return out;
}

const diffOf = (r: () => number, path: string, at: number): FileDiff => {
  const n = int(r, 1, 3);
  const line = int(r, 20, 400);
  return {
    path: `${CWD}/${path}`,
    hunks: [
      {
        old_start: line,
        old_lines: 2 + n,
        new_start: line,
        new_lines: 2 + 2 * n,
        lines: [
          ` // ${at % 997}`,
          ...Array.from({ length: n }, (_, k) => `-let deadline_${k} = clock.now() + budget;`),
          ...Array.from({ length: 2 * n }, (_, k) => `+let deadline_${k} = now + budget;`),
          " loop {",
        ],
      },
    ],
  };
};

const outputOf = (r: () => number, cmd: string, lines: number) =>
  cmd.startsWith("git")
    ? Array.from({ length: Math.min(lines, 4) }, () => ` M ${pick(r, FILES)}`).join("\n")
    : Array.from({ length: lines }, (_, k) => `test ${cmd.split(" ").pop()}::case_${k} ... ok`).join("\n") + `\n\ntest result: ok. ${lines} passed`;

/** One viewer's run of the script: its own clock, its own seeded choices. */
class Run {
  private alive = true;
  /** The turn was stopped (an interrupt): the script holds until it's asked something again. */
  private halted = false;
  private seq = 0;
  private n = 0;
  private status: State["status"] = "running";
  private doing = "Thinking";
  private activity = "Thinking";
  private turnStarted = Date.now() - 95_000;
  private lastTurn: State["last_turn"];
  private tasks: Task[] = [];
  private todos: Todo[] = [
    { text: "Read the reconnect path", status: "completed" },
    { text: "Take one instant for the deadline and the backoff", status: "in_progress" },
    { text: "Seed the jitter under test", status: "pending" },
    { text: "Run the suite ten times", status: "pending" },
  ];
  private usage = { cost_usd: 2.4, input_tokens: 512_000, output_tokens: 38_000, cache_read_tokens: 3_100_000, turns: TURNS + 1 };
  private changes = new Map<string, FileChange>();
  private had: Record<string, unknown> | null = null;
  private subagents = 0;
  private spawned = 0;
  private history: HEvent[] = [];

  constructor(
    private ws: WebSocket,
    private patches: boolean,
  ) {}

  private state(): State {
    const running = this.status === "running";
    return {
      status: this.status,
      doing: running ? this.doing : undefined,
      turn_started: running ? this.turnStarted : undefined,
      last_turn: running ? undefined : this.lastTurn,
      cwd: CWD,
      model: "claude-opus-5-5",
      mode: "acceptEdits",
      effort: "high",
      title: "Reconnect backoff, at full speed",
      activity: this.activity,
      approvals: [],
      queue: [],
      delivery: {},
      tasks: this.tasks.map((t) => ({ ...t })),
      todos: this.todos.map((t) => ({ ...t })),
      usage: { ...this.usage },
      plan: "Max",
      dropped: 0,
      limits: [{ id: "seven_day", label: "Weekly", used_percent: 41, resets_at: Date.now() + 3 * 86_400_000, window: "7d" }],
    };
  }

  private send(u: unknown) {
    if (this.alive && this.ws.readyState === this.ws.OPEN) this.ws.send(JSON.stringify(u));
  }

  /** The state as it goes over the wire: what changed in it, for a viewer that takes patches (as the server keeps it). */
  private sendState() {
    const now = JSON.parse(JSON.stringify(this.state())) as Record<string, unknown>;
    const was = this.had;
    if (!this.patches || !was) {
      this.had = now;
      this.send({ t: "state", state: now });
      return;
    }
    this.had = now;
    const set = Object.fromEntries(Object.entries(now).filter(([k, v]) => JSON.stringify(was[k]) !== JSON.stringify(v)));
    const gone = Object.keys(was).filter((k) => !(k in now));
    if (Object.keys(set).length === 0 && gone.length === 0) return;
    this.send({ t: "state_patch", set, ...(gone.length ? { gone } : {}) });
  }

  private ev<E extends Unnumbered>(e: E): E & { seq: number } {
    this.seq += 1;
    return { ...e, seq: this.seq };
  }

  private emit(...events: HEvent[]) {
    this.send({ t: "events", events });
  }

  private sleep(ms: number) {
    return new Promise<boolean>((resolve) => setTimeout(() => resolve(this.alive && !this.halted), ms / RATE));
  }

  private id(prefix: string) {
    this.n += 1;
    return `${prefix}${this.n}`;
  }

  /* ---------------------------------------------------------------- the snapshot */

  private build() {
    const r = mulberry32(SEED * 7919 + 1);
    const now = Date.now();
    const t0 = now - (TURNS + 2) * 60_000;
    const push = (e: Unnumbered) => {
      const x = this.ev(e) as HEvent;
      this.history.push(x);
      return x;
    };
    for (let i = 1; i <= TURNS; i++) {
      const at = t0 + i * 60_000;
      push({ id: `H-u${i}`, at, kind: "user", text: pick(r, ASKS) });
      push({ id: `H-t${i}`, at: at + 1_000, kind: "thinking", text: pick(r, THOUGHTS) });
      for (let k = 0; k < 3; k++) {
        const read = k < 2;
        push({
          id: `H-x${i}-${k}`,
          at: at + 2_000 + k * 300,
          kind: "tool",
          tool_id: `hx${i}-${k}`,
          name: read ? "Read" : "Grep",
          tool: read ? "read" : "search",
          title: read ? `${CWD}/${pick(r, FILES)}` : pick(r, SEARCHES),
          detail: read ? undefined : "crates/kiln/src",
          input: {},
          status: "done",
          started: at + 2_000 + k * 300,
          ended: at + 2_200 + k * 300,
        });
      }
      if (i % 2 === 0) {
        const path = pick(r, FILES);
        push({
          id: `H-e${i}`,
          at: at + 4_000,
          kind: "tool",
          tool_id: `he${i}`,
          name: "Edit",
          tool: "edit",
          title: `${CWD}/${path}`,
          input: {},
          status: "done",
          started: at + 4_000,
          ended: at + 4_100,
          diffs: [diffOf(r, path, at)],
        });
      }
      const cmd = pick(r, COMMANDS);
      push({
        id: `H-s${i}`,
        at: at + 6_000,
        kind: "tool",
        tool_id: `hs${i}`,
        name: "Bash",
        tool: "shell",
        title: cmd,
        input: {},
        status: "done",
        exit_code: 0,
        output: outputOf(r, cmd, int(r, 4, 9)),
        started: at + 6_000,
        ended: at + 9_000,
      });
      push({ id: `H-a${i}`, at: at + 10_000, kind: "assistant", text: replyOf(r) });
    }
    // The turn under way, and what it already did.
    push({ id: "F-ask", at: this.turnStarted, kind: "user", text: "Make the reconnect backoff deterministic under test, then keep going until the whole suite is green. Work fast." });
    for (let k = 1; k <= STEPS; k++) {
      const at = this.turnStarted + 1_000 + k * 1_500;
      const kind = k % 5;
      if (kind === 0) push({ id: `F-p${k}`, at, kind: "assistant", text: replyOf(r) });
      else if (kind === 1) push({ id: `F-p${k}`, at, kind: "thinking", text: pick(r, THOUGHTS) });
      else if (kind === 2)
        for (let j = 0; j < int(r, 2, 4); j++)
          push({
            id: `F-p${k}-${j}`,
            at: at + j * 100,
            kind: "tool",
            tool_id: `fp${k}-${j}`,
            name: "Read",
            tool: "read",
            title: `${CWD}/${pick(r, FILES)}`,
            input: {},
            status: "done",
            started: at + j * 100,
            ended: at + j * 100 + 80,
          });
      else if (kind === 3) {
        const cmd = pick(r, COMMANDS);
        push({
          id: `F-p${k}`,
          at,
          kind: "tool",
          tool_id: `fp${k}`,
          name: "Bash",
          tool: "shell",
          title: cmd,
          input: {},
          status: "done",
          exit_code: 0,
          output: outputOf(r, cmd, int(r, 3, 8)),
          started: at,
          ended: at + 400,
        });
      } else {
        const path = pick(r, FILES);
        const diff = diffOf(r, path, at);
        push({ id: `F-p${k}`, at, kind: "tool", tool_id: `fp${k}`, name: "Edit", tool: "edit", title: `${CWD}/${path}`, input: {}, status: "done", started: at, ended: at + 90, diffs: [diff] });
        this.change(diff, at);
      }
    }
  }

  /** An edit joins the changeset (as the server keeps it). */
  private change(d: FileDiff, at: number) {
    const was = this.changes.get(d.path);
    const added = d.hunks.reduce((n, h) => n + h.lines.filter((l) => l.startsWith("+")).length, 0);
    const removed = d.hunks.reduce((n, h) => n + h.lines.filter((l) => l.startsWith("-")).length, 0);
    this.changes.set(d.path, { path: d.path, added: (was?.added ?? 0) + added, removed: (was?.removed ?? 0) + removed, hunks: [...(was?.hunks ?? []), ...d.hunks].slice(-6), at });
  }

  start() {
    this.build();
    this.send({ t: "open", info: info() });
    this.send({ t: "meta", meta });
    this.sendState();
    this.send({ t: "changes", files: [...this.changes.values()] });
    for (let i = 0; i < this.history.length; i += 120) this.emit(...this.history.slice(i, i + 120));
    this.send({ t: "synced" });
    void this.main();
  }

  stop() {
    this.alive = false;
  }

  /** A message from the page: a new ask starts a turn (or steers the one under way); a stop ends it. */
  take(c: { type?: string; text?: string; id?: string }) {
    if (c.type === "interrupt" || c.type === "force_stop") {
      if (this.status !== "running") return;
      this.halted = true;
      this.endTurn("stopped");
      return;
    }
    if (c.type !== "send" || !c.text) return;
    this.emit(this.ev({ id: this.id("F-sent-"), at: Date.now(), kind: "user", text: c.text, client_id: c.id }));
    if (this.halted) {
      this.halted = false;
      this.beginTurn();
      void this.main();
    }
  }

  private endTurn(how: "done" | "stopped") {
    this.status = "idle";
    this.lastTurn = { at: Date.now(), how, kept: 0 };
    this.tasks = this.tasks.map((t) => (t.status === "running" ? { ...t, status: "stopped", ended: Date.now() } : t));
    this.sendState();
  }

  private beginTurn() {
    this.status = "running";
    this.turnStarted = Date.now();
    this.doing = "Thinking";
    this.usage = { ...this.usage, turns: this.usage.turns + 1 };
    this.sendState();
  }

  /* ---------------------------------------------------------------- the turn under way */

  private async main() {
    const r = mulberry32(SEED);
    for (;;) {
      for (let round = 0; round < ROUNDS; round++) if (!(await this.round(r))) return;
      if (!(await this.say(r, "assistant"))) return;
      this.endTurn("done");
      if (!(await this.sleep(BETWEEN_MS))) return;
      this.emit(this.ev({ id: this.id("F-ask-"), at: Date.now(), kind: "user", text: pick(r, ASKS) }));
      this.beginTurn();
    }
  }

  /** One round of work: it thinks, takes a few runs of calls, maybe starts a subagent, says something, and maybe goes quiet a while. */
  private async round(r: () => number): Promise<boolean> {
    if (r() < 0.7 && !(await this.say(r, "thinking"))) return false;
    for (let g = int(r, 1, 3); g > 0; g--) {
      const what = r();
      const ok = what < 0.4 ? await this.explore(r) : what < 0.7 ? await this.shell(r) : what < 0.9 ? await this.edit(r) : what < 0.95 ? await this.plan(r) : await this.web(r);
      if (!ok || !(await this.sleep(between(r, 20, 120)))) return false;
    }
    if (r() < 0.15 && this.subagents < MOST_SUBAGENTS) await this.spawn(r);
    if (r() < 0.8 && !(await this.say(r, "assistant"))) return false;
    this.usage = {
      ...this.usage,
      output_tokens: this.usage.output_tokens + int(r, 200, 900),
      input_tokens: this.usage.input_tokens + int(r, 2_000, 9_000),
      cost_usd: Math.round((this.usage.cost_usd + 0.02) * 100) / 100,
    };
    this.sendState();
    // Now and then it thinks a while with nothing to show (as a model does after a call).
    if (r() < 0.25) {
      this.doing = "Thinking";
      this.sendState();
      return this.sleep(between(r, 800, 2_000));
    }
    return this.sleep(between(r, 30, 200));
  }

  /** Words streaming, 30 to 60 deltas a second: a reply, or a thought. */
  private async say(r: () => number, kind: "assistant" | "thinking", parent?: string): Promise<boolean> {
    const text = kind === "thinking" ? `${pick(r, THOUGHTS)} ${pick(r, THOUGHTS)}` : replyOf(r);
    if (!parent) {
      this.doing = kind === "thinking" ? "Thinking" : "Writing";
      this.activity = this.doing;
      this.sendState();
    }
    const e = this.ev({ id: this.id(parent ? `F-${parent}-s` : "F-s"), at: Date.now(), kind, text: "", streaming: true, ...(parent ? { parent } : {}) }) as Said;
    this.emit({ ...e });
    for (const d of deltasOf(r, text, kind === "thinking" ? 6 : 3)) {
      if (!(await this.sleep(between(r, 16, 33)))) return false;
      e.text += d;
      this.send({ t: "stream", id: e.id, text: d });
    }
    e.streaming = false;
    this.emit({ ...e });
    return this.sleep(between(r, 20, 90));
  }

  private call(r: () => number, t: Omit<Tool, "id" | "seq" | "at" | "kind" | "tool_id" | "status" | "started" | "input"> & { input?: unknown }, parent?: string): Tool {
    void r;
    const id = this.id(parent ? `F-${parent}-c` : "F-c");
    const at = Date.now();
    return this.ev({ id, at, kind: "tool", tool_id: `tu-${id}`, status: "running", started: at, input: {}, ...t, ...(parent ? { parent } : {}) }) as Tool;
  }

  private finish(c: Tool, rest: Partial<Tool> = {}) {
    Object.assign(c, { status: "done", ended: Date.now() }, rest);
    this.emit({ ...c });
  }

  /** A run of reads and searches: one after another, or a few at once. */
  private async explore(r: () => number, parent?: string): Promise<boolean> {
    if (!parent) {
      this.activity = "Reading files";
      this.sendState();
    }
    const make = () => {
      const read = r() < 0.65;
      return this.call(
        r,
        read ? { name: "Read", tool: "read", title: `${CWD}/${pick(r, FILES)}` } : { name: "Grep", tool: "search", title: pick(r, SEARCHES), detail: `${CWD}/crates/kiln/src` },
        parent,
      );
    };
    if (r() < 0.3) {
      const all = Array.from({ length: int(r, 2, 4) }, make);
      this.emit(...all.map((c) => ({ ...c })));
      for (const c of all) {
        if (!(await this.sleep(between(r, 60, 200)))) return false;
        this.finish(c);
      }
      return true;
    }
    for (let k = int(r, 2, 6); k > 0; k--) {
      const c = make();
      this.emit({ ...c });
      if (!(await this.sleep(between(r, 60, 250)))) return false;
      this.finish(c);
      if (k > 1 && !(await this.sleep(between(r, 20, 120)))) return false;
    }
    return true;
  }

  /** Commands one after another, now and then a long one whose output streams in. */
  private async shell(r: () => number, parent?: string): Promise<boolean> {
    const n = int(r, 1, 4);
    const long = r() < 0.25 ? int(r, 0, n - 1) : -1;
    for (let k = 0; k < n; k++) {
      const cmd = pick(r, COMMANDS);
      if (!parent) {
        this.activity = `Running ${cmd.split(" ").slice(0, 2).join(" ")}`;
        this.sendState();
      }
      const c = this.call(r, { name: "Bash", tool: "shell", title: cmd }, parent);
      this.emit({ ...c });
      if (k === long) {
        const lines: string[] = [];
        const until = Date.now() + between(r, 1_500, 4_000) / RATE;
        while (Date.now() < until) {
          if (!(await this.sleep(between(r, 200, 300)))) return false;
          for (let j = int(r, 1, 3); j > 0; j--) lines.push(`test ${cmd.split(" ").pop()}::case_${lines.length} ... ok`);
          c.output = lines.join("\n");
          this.emit({ ...c });
        }
        this.finish(c, { exit_code: 0, output: `${lines.join("\n")}\n\ntest result: ok. ${lines.length} passed` });
      } else {
        if (!(await this.sleep(between(r, 100, 300)))) return false;
        const failed = r() < 0.08;
        this.finish(
          c,
          failed ? { exit_code: 101, status: "error", output: "error[E0308]: mismatched types\n  --> crates/kiln/src/http/retry.rs:88:17" } : { exit_code: 0, output: outputOf(r, cmd, int(r, 3, 10)) },
        );
      }
      if (k < n - 1 && !(await this.sleep(between(r, 20, 100)))) return false;
    }
    return true;
  }

  private async edit(r: () => number, parent?: string): Promise<boolean> {
    if (!parent) {
      this.activity = "Editing files";
      this.sendState();
    }
    for (let k = int(r, 1, 3); k > 0; k--) {
      const path = pick(r, FILES);
      const c = this.call(r, { name: "Edit", tool: "edit", title: `${CWD}/${path}` }, parent);
      this.emit({ ...c });
      if (!(await this.sleep(between(r, 80, 200)))) return false;
      const diff = diffOf(r, path, Date.now());
      this.finish(c, { diffs: [diff] });
      if (!parent) {
        this.change(diff, Date.now());
        this.send({ t: "changes", files: [...this.changes.values()] });
      }
      if (k > 1 && !(await this.sleep(between(r, 20, 100)))) return false;
    }
    return true;
  }

  private async plan(r: () => number): Promise<boolean> {
    const at = this.todos.findIndex((t) => t.status !== "completed");
    this.todos = this.todos.map((t, i) => (i === at ? { ...t, status: "completed" } : i === at + 1 ? { ...t, status: "in_progress" } : t));
    if (at < 0) this.todos = this.todos.map((t, i) => ({ ...t, status: i === 0 ? "in_progress" : "pending" }));
    const c = this.call(r, { name: "TodoWrite", tool: "todo", title: "Update the plan", todos: this.todos.map((t) => ({ ...t })) });
    this.emit({ ...c });
    this.sendState();
    if (!(await this.sleep(between(r, 30, 80)))) return false;
    this.finish(c);
    return true;
  }

  private async web(r: () => number): Promise<boolean> {
    for (let k = int(r, 1, 2); k > 0; k--) {
      const c = this.call(r, { name: "WebSearch", tool: "web", title: "tokio interval missed tick behavior" });
      this.emit({ ...c });
      if (!(await this.sleep(between(r, 150, 300)))) return false;
      this.finish(c, { output: "MissedTickBehavior::Delay waits a whole period after a late tick." });
    }
    return true;
  }

  /** A subagent: its spawn call, its task, and its own work streaming into its thread until it's done. */
  private async spawn(r: () => number) {
    this.spawned += 1;
    this.subagents += 1;
    const k = this.spawned;
    const description = k % 2 ? "Audit the timer wheel for the same race" : "Check every caller of the backoff";
    const c = this.call(r, { name: "Task", tool: "agent", title: description, input: { description, prompt: `${description}. Report what you find.`, subagent_type: "general-purpose" } });
    const task: Task = {
      id: `task-${k}`,
      tool_id: c.tool_id,
      description,
      kind: "general-purpose",
      status: "running",
      started: Date.now(),
      tool_uses: 0,
      class: "agent",
      prompt: `${description}. Report what you find.`,
    };
    this.tasks = [...this.tasks.filter((t) => t.status === "running"), task];
    this.emit({ ...c });
    this.sendState();
    // It works on by itself, with its own choices, while the main thread goes on.
    void (async () => {
      const s = mulberry32(SEED * 1000 + k);
      const parent = c.tool_id;
      const until = Date.now() + between(s, 4_000, 9_000) / RATE;
      const tick = (last: string) => {
        this.tasks = this.tasks.map((t) => (t.id === task.id ? { ...t, tool_uses: (t.tool_uses ?? 0) + 1, last_tool: last } : t));
        this.sendState();
      };
      while (Date.now() < until) {
        const what = s();
        const ok = what < 0.45 ? await this.explore(s, parent) : what < 0.7 ? await this.shell(s, parent) : what < 0.85 ? await this.edit(s, parent) : await this.say(s, "assistant", parent);
        if (!ok) break;
        tick(what < 0.45 ? "Read" : what < 0.7 ? "Bash" : what < 0.85 ? "Edit" : "Writing");
        if (!(await this.sleep(between(s, 40, 150)))) break;
      }
      this.subagents -= 1;
      if (!this.alive) return;
      const summary = `${description}: the same clock read twice; fixed in one place, ${int(s, 3, 9)} callers checked.`;
      this.emit(this.ev({ id: this.id(`F-${parent}-a`), at: Date.now(), kind: "assistant", text: summary, parent }));
      const ended = Date.now();
      this.tasks = this.tasks.map((t) => (t.id === task.id ? { ...t, status: "completed", ended, duration_ms: ended - t.started, summary } : t));
      this.finish(c, { output: summary });
      this.sendState();
    })();
  }
}

/** Its socket: the snapshot, then the script, for as long as the page is open. */
export function handleFastSession(ws: WebSocket, id: string, patches = false): boolean {
  if (id !== ID) return false;
  const run = new Run(ws, patches);
  run.start();
  ws.on("message", (raw) => {
    try {
      run.take(JSON.parse(raw.toString()) as { type?: string; text?: string; id?: string });
    } catch {
      // Not a command.
    }
  });
  ws.on("close", () => run.stop());
  return true;
}
