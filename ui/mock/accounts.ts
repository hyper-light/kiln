/**
 * Harness accounts in the mock: each machine's Claude Code and Codex sign-in, and their flows as the
 * real CLIs run them.
 *
 *   Claude Code   a page to open (after a moment), then the code it shows pasted back.
 *                 A code starting "bad" is turned down and asked for again; "fail…" ends it failed.
 *   Codex         ChatGPT: a page and a one-time code to enter there, then it finishes by itself.
 *                 API key: asked for at once (one not starting "sk-" is turned down).
 *
 * The local machine starts with Claude signed out and Codex signed in; every other machine the
 * other way round, except a "pi" one, where kiln can't tell. Codes and keys are never logged.
 *
 * Also two demo agents to see the card from, each stopped by its harness's "not signed in" error:
 * /agents/demo-claude (on the server) and /agents/demo-codex (on metal-ryzen-01).
 */
import type http from "node:http";
import { randomBytes } from "node:crypto";
import type { WebSocket } from "ws";
import type { Account, AccountFlow, AccountMethod, AgentInfo, HEvent, Meta, SlashCommand, State } from "../src/api/harness.ts";

const now = () => Math.floor(Date.now() / 1000);
const EMAIL = "ada@lundhe.audio";
/** How long a machine takes to say who's signed in (the CLIs are asked each time). */
const STATUS_MS = 450;

function json(res: http.ServerResponse, status: number, body: unknown) {
  res.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
  res.end(JSON.stringify(body));
}
async function body<T>(req: http.IncomingMessage): Promise<T> {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(c as Buffer);
  const s = Buffer.concat(chunks).toString("utf8");
  return (s ? JSON.parse(s) : {}) as T;
}
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/* ------------------------------------------------------------------ accounts */

const METHODS: Record<string, AccountMethod[]> = {
  claude: [
    { id: "claudeai", label: "Claude subscription", asks: "code" },
    { id: "console", label: "Anthropic Console", asks: "code" },
  ],
  codex: [
    { id: "chatgpt", label: "ChatGPT" },
    { id: "api_key", label: "API key", asks: "api_key" },
  ],
};
const NAMES: Record<string, string> = { claude: "Claude Code", codex: "Codex" };

const out = (kind: string): Account => ({ kind, name: NAMES[kind] ?? kind, signed_in: false, methods: METHODS[kind] ?? [] });
const signedIn = (kind: string, how: Partial<Account>): Account => ({ ...out(kind), signed_in: true, ...how });
const CLAUDE_IN = { method: "Claude subscription", email: EMAIL, plan: "max" };
const CODEX_IN = { method: "ChatGPT", email: EMAIL, plan: "plus" };

const machines = new Map<string, Map<string, Account>>();
function accountsOn(target: string): Map<string, Account> {
  let m = machines.get(target);
  if (!m) {
    m = new Map();
    if (target === "local") {
      m.set("claude", out("claude"));
      m.set("codex", signedIn("codex", CODEX_IN));
    } else if (target.includes("pi")) {
      m.set("claude", { ...out("claude"), signed_in: null, error: "claude --version: command not found" });
      m.set("codex", signedIn("codex", CODEX_IN));
    } else {
      m.set("claude", signedIn("claude", CLAUDE_IN));
      m.set("codex", out("codex"));
    }
    machines.set(target, m);
  }
  return m;
}

/* ------------------------------------------------------------------ flows */

interface Sim {
  flow: AccountFlow;
  t0: number;
  /** What was given back, until the "CLI" has read it. Never logged. */
  input?: string;
  inputAt?: number;
}
const flows = new Map<string, Sim>();

const deviceCode = () => {
  const a = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
  const pick = (n: number) => Array.from(randomBytes(n), (b) => a[b % a.length]).join("");
  return `${pick(4)}-${pick(5)}`;
};

function end(s: Sim, state: AccountFlow["state"], how?: { account?: Account; error?: string }) {
  const f = s.flow;
  f.state = state;
  f.ended_at = now();
  f.asks = undefined;
  if (how?.error) f.error = how.error;
  if (how?.account) {
    accountsOn(f.target).set(f.kind, how.account);
    f.account = how.account;
  } else f.account = accountsOn(f.target).get(f.kind);
}

/** Moves a flow along as time passes, the way its CLI would. */
function advance(s: Sim) {
  const f = s.flow;
  if (f.state !== "running") return;
  const dt = Date.now() - s.t0;
  const read = s.input !== undefined && s.inputAt !== undefined && Date.now() - s.inputAt > 1500 ? s.input : undefined;
  if (read !== undefined) s.input = undefined;

  if (f.kind === "claude") {
    if (!f.url && dt > 900) {
      f.url =
        "https://claude.ai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code" +
        "&redirect_uri=https%3A%2F%2Fconsole.anthropic.com%2Foauth%2Fcode%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference" +
        `&code_challenge=${randomBytes(24).toString("base64url")}&code_challenge_method=S256&state=${randomBytes(16).toString("base64url")}`;
      f.asks = "code";
      f.lines.push("Opening browser to sign in…", "Browser didn't open? Use the url below to sign in:", "", f.url, "", "Paste code here if prompted >");
    }
    if (read !== undefined) {
      if (/^bad/i.test(read)) {
        f.lines.push("OAuth error: Invalid code", "Paste code here if prompted >");
        f.asks = "code";
      } else if (/^fail/i.test(read)) {
        f.lines.push("OAuth error: authorization code expired");
        end(s, "failed", { error: "the code expired before it was used. Sign in again for a new one." });
      } else {
        f.lines.push("Login successful.");
        end(s, "done", { account: signedIn("claude", f.method === "console" ? { method: "Anthropic Console", email: EMAIL, org: "Lundhe Audio" } : CLAUDE_IN) });
      }
    }
    return;
  }

  if (f.method === "api_key") {
    if (read !== undefined) {
      if (!read.startsWith("sk-")) {
        f.lines.push("Error: invalid API key format");
        end(s, "failed", { error: "that isn't an OpenAI API key (they start with sk-)." });
      } else {
        f.lines.push("Successfully logged in");
        end(s, "done", { account: signedIn("codex", { method: "API key" }) });
      }
    }
    return;
  }

  // Codex, device sign-in.
  if (!f.url && dt > 700) {
    f.url = "https://auth.openai.com/codex/device";
    f.code = deviceCode();
    f.lines.push(
      "Welcome to Codex [v0.46.0]",
      "Follow these steps to sign in with ChatGPT using device code authorization:",
      "",
      "1. Open this link in your browser and sign in to your account",
      `   ${f.url}`,
      "",
      "2. Enter this one-time code (expires in 15 minutes)",
      `   ${f.code}`,
      "",
      "Device codes are a common phishing target. Never share this code.",
    );
  }
  if (f.url && dt > 7500) {
    f.lines.push("Successfully logged in");
    end(s, "done", { account: signedIn("codex", CODEX_IN) });
  }
}

/* ------------------------------------------------------------------ demo agents */

const COMMANDS: SlashCommand[] = [
  { name: "login", description: "Sign in, or switch accounts", group: "Session", args: "none", route: { kind: "view", view: "login" } },
  { name: "logout", description: "Sign out on this machine", group: "Session", args: "none", route: { kind: "view", view: "logout" } },
  { name: "model", description: "Change the model", group: "Model & mode", args: "optional", route: { kind: "view", view: "model" } },
  { name: "compact", description: "Summarize the conversation to free context", group: "Context", args: "optional", route: { kind: "prompt" } },
];

interface Demo {
  info: AgentInfo;
  meta: Meta;
  events: HEvent[];
  /** What the harness says when it can't reach its account. */
  refusal: string;
}

function demo(id: string, kind: string, target: string, title: string, model: string, ask: string, refusal: string): Demo {
  const t = Date.now() - 6 * 60_000;
  const info: AgentInfo = {
    id,
    kind,
    kind_name: NAMES[kind] ?? kind,
    target,
    name: NAMES[kind] ?? kind,
    cwd: "/home/ada/src/kiln",
    created_at: t,
    status: "idle",
    title,
    model,
    mode: "default",
    approvals: [],
    queued: 0,
    tasks_running: 0,
    cost_usd: 0,
    last_activity: t + 4000,
  };
  const meta: Meta = {
    version: kind === "claude" ? "2.3.1" : "0.46.0",
    modes: [
      { id: "plan", label: "Plan", description: "Reads and plans; changes nothing" },
      { id: "default", label: "Ask", description: "Asks before edits and commands" },
      { id: "acceptEdits", label: "Accept edits", description: "Edits files without asking; asks before commands" },
    ],
    models: [{ id: model, label: model }],
    commands: COMMANDS,
    agents: [],
    features: ["interrupt", "set_mode", "steer"],
  };
  const events: HEvent[] = [
    { id: `${id}-u1`, seq: 1, at: t, kind: "user", text: ask },
    { id: `${id}-n1`, seq: 2, at: t + 2500, kind: "notice", level: "error", text: refusal },
  ];
  return { info, meta, events, refusal };
}

const demos = new Map<string, Demo>(
  [
    demo(
      "demo-claude",
      "claude",
      "local",
      "Flaky retry test",
      "claude-opus-5-5",
      "The retry test in crates/kiln/src/http/client.rs fails about one run in ten. Find out why and fix it.",
      "Invalid API key · Please run /login",
    ),
    demo(
      "demo-codex",
      "codex",
      "agent:metal-ryzen-01",
      "Scheduler changes",
      "gpt-5.5-codex",
      "Summarize what changed in the scheduler this week.",
      "unexpected status 401 Unauthorized: Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.",
    ),
  ].map((d) => [d.info.id, d]),
);

function stateOf(d: Demo): State {
  return {
    status: "idle",
    cwd: d.info.cwd,
    model: d.info.model,
    mode: d.info.mode,
    title: d.info.title,
    approvals: [],
    queue: [],
    delivery: {},
    tasks: [],
    todos: [],
    usage: { cost_usd: 0, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, turns: 1 },
    dropped: 0,
  };
}

/** A demo agent's socket: its snapshot, and an answer to what's sent (refused until its harness is signed in). */
export function handleDemoSession(ws: WebSocket, id: string): boolean {
  const d = demos.get(id);
  if (!d) return false;
  const send = (u: unknown) => ws.send(JSON.stringify(u));
  send({ t: "open", info: d.info });
  send({ t: "meta", meta: d.meta });
  send({ t: "state", state: stateOf(d) });
  send({ t: "events", events: d.events });
  ws.on("message", (raw) => {
    let c: { type?: string; text?: string; id?: string };
    try {
      c = JSON.parse(raw.toString()) as typeof c;
    } catch {
      return;
    }
    if (c.type !== "send" || !c.text) return;
    const seq = (d.events.at(-1)?.seq ?? 0) + 1;
    const at = Date.now();
    const ok = accountsOn(d.info.target).get(d.info.kind)?.signed_in === true;
    const added: HEvent[] = [
      { id: `${id}-u${seq}`, seq, at, kind: "user", text: c.text, client_id: c.id },
      ok
        ? { id: `${id}-a${seq}`, seq: seq + 1, at: at + 900, kind: "assistant", text: "Signed in again, so I can carry on. (This is the mock: nothing ran.)" }
        : { id: `${id}-n${seq}`, seq: seq + 1, at: at + 900, kind: "notice", level: "error", text: d.refusal },
    ];
    d.events.push(...added);
    setTimeout(() => {
      if (ws.readyState === ws.OPEN) send({ t: "events", events: added });
    }, 900);
  });
  return true;
}

/* ------------------------------------------------------------------ routes */

/** The accounts API and the demo agents' list. True when it answered. */
export async function handleAccounts(req: http.IncomingMessage, res: http.ServerResponse, url: URL): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";

  if (p === "/api/harness/sessions" && m === "GET") {
    json(
      res,
      200,
      [...demos.values()].map((d) => d.info),
    );
    return true;
  }

  if (p === "/api/accounts" && m === "GET") {
    await sleep(STATUS_MS);
    json(res, 200, [...accountsOn(url.searchParams.get("target") ?? "local").values()]);
    return true;
  }

  if (p === "/api/accounts/login" && m === "POST") {
    const b = await body<{ target?: string; kind?: string; method?: string }>(req);
    const target = b.target ?? "local";
    const kind = b.kind ?? "";
    const methods = METHODS[kind];
    if (!methods) return (json(res, 400, { error: `kiln can't sign ${kind || "that"} in` }), true);
    const method = methods.find((x) => x.id === b.method) ?? methods[0]!;
    // One sign-in per harness per machine: a new one replaces the last.
    for (const s of flows.values()) if (s.flow.target === target && s.flow.kind === kind && s.flow.state === "running") end(s, "cancelled");
    const flow: AccountFlow = { id: `af_${randomBytes(6).toString("hex")}`, target, kind, action: "login", method: method.id, state: "running", lines: [], started_at: now() };
    if (method.asks === "api_key") {
      flow.asks = "api_key";
      flow.lines.push("Reading an API key from stdin…");
    }
    flows.set(flow.id, { flow, t0: Date.now() });
    json(res, 200, flow);
    return true;
  }

  if (p === "/api/accounts/logout" && m === "POST") {
    const b = await body<{ target?: string; kind?: string }>(req);
    const target = b.target ?? "local";
    const kind = b.kind ?? "";
    if (!METHODS[kind]) return (json(res, 400, { error: `kiln can't sign ${kind || "that"} out` }), true);
    await sleep(600);
    const flow: AccountFlow = {
      id: `af_${randomBytes(6).toString("hex")}`,
      target,
      kind,
      action: "logout",
      state: "running",
      lines: [kind === "claude" ? "Successfully logged out from your Anthropic account." : "Successfully logged out"],
      started_at: now(),
    };
    const s: Sim = { flow, t0: Date.now() };
    flows.set(flow.id, s);
    end(s, "done", { account: out(kind) });
    json(res, 200, flow);
    return true;
  }

  const fm = p.match(/^\/api\/accounts\/flows\/([^/]+)(\/input)?$/);
  if (fm) {
    const s = flows.get(decodeURIComponent(fm[1]!));
    if (!s) return (json(res, 404, { error: "no such sign-in" }), true);
    if (fm[2] && m === "POST") {
      const b = await body<{ text?: string }>(req);
      if (s.flow.state !== "running" || !s.flow.asks) return (json(res, 409, { error: "it isn't asking for anything" }), true);
      s.input = (b.text ?? "").trim();
      s.inputAt = Date.now();
      s.flow.asks = undefined;
      res.writeHead(204);
      res.end();
      return true;
    }
    if (!fm[2] && m === "GET") {
      advance(s);
      json(res, 200, s.flow);
      return true;
    }
    if (!fm[2] && m === "DELETE") {
      if (s.flow.state === "running") end(s, "cancelled");
      res.writeHead(204);
      res.end();
      return true;
    }
  }
  return false;
}
