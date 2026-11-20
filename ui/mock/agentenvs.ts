/**
 * A person's environments for agent sessions in the mock, as the server's routes answer them
 * (docs/agents.md, "Environments", "Operations"): fake values only, secrets by name, refusals typed
 * (`{error, code}`, the error's words the UI's to show).
 *
 *   GET    /api/agentenvs                    {agentenvs: [...]}, by name, each with its `sessions`
 *   POST   /api/agentenvs                    {name, description?, base, vars} → the environment; 409 name taken; 400 a bad variable or a secret the caller hasn't
 *   GET    /api/agentenvs/machine?target=    {vars: [{name, sources}]}: what that machine's user has, by name; 502 while it's away
 *   GET    /api/agentenvs/:id                the environment; 404
 *   PATCH  /api/agentenvs/:id                {version, name?, description?, base?, vars?} → the environment; 409 when `version` isn't its current one
 *   DELETE /api/agentenvs/:id                204; 409 naming the sessions that run with it
 *   PUT    /api/harness/sessions/:id/agentenv {agentenv: id | null, restart?} → {agent, agentenv, restarted}; a `secret` variable refused in a microVM
 *   POST   /api/mock/agentenvs               put them back as they were (a browser check's start)
 */
import type http from "node:http";
import type { Agentenv, AgentenvVar, MachineVar } from "../src/api/agentenvs.ts";
import { familyInfos } from "./agents.ts";
import { MAC_MINI } from "./data.ts";
import { sessionOf } from "./microvms.ts";
import { mySecretNames } from "./secrets-mine.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
}
const now = () => Math.floor(Date.now() / 1000);
const DAY = 86_400;
const NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

function seed(): Map<string, Agentenv> {
  const t = now();
  const all: Agentenv[] = [
    {
      id: "env-release",
      owner: "ada",
      name: "release-tools",
      description: "GitHub token, quiet logs, cargo",
      base: "user",
      vars: [
        { name: "GH_TOKEN", from: "secret", secret: "GH_TOKEN" },
        { name: "RUST_LOG", from: "value", value: "info" },
        { name: "CARGO_HOME", from: "machine" },
      ],
      version: 3,
      created_at: t - 5 * DAY,
      updated_at: t - 2 * DAY,
      tags: ["release", "github"],
      sessions: ["fam-docs", "fam-orch", "fam-tests"],
    },
    {
      id: "env-aws",
      owner: "ada",
      name: "aws-agents",
      base: "user",
      vars: [
        { name: "AWS_ACCESS_KEY_ID", from: "secret", secret: "AWS_AGENTS_KEY_ID" },
        { name: "AWS_SECRET_ACCESS_KEY", from: "secret", secret: "AWS_AGENTS_SECRET" },
        { name: "AWS_REGION", from: "value", value: "us-west-2" },
        { name: "AWS_PROFILE", from: "machine", of: "AWS_DEFAULT_PROFILE" },
      ],
      version: 2,
      created_at: t - 12 * DAY,
      updated_at: t - 9 * DAY,
      tags: ["aws"],
      sessions: ["fam-migr"],
    },
    {
      id: "env-clean",
      owner: "ada",
      name: "clean-ci",
      description: "Like CI: only PATH",
      base: "clean",
      vars: [
        { name: "PATH", from: "machine" },
        { name: "LANG", from: "value", value: "C.UTF-8" },
        { name: "NODE_OPTIONS", from: "unset" },
      ],
      version: 1,
      created_at: t - 1 * DAY,
      updated_at: t - 1 * DAY,
      tags: ["ci"],
      sessions: [],
    },
  ];
  return new Map(all.map((e) => [e.id, e]));
}
let kept = seed();

/** What a machine's user has, by name: a Mac's from its login shell and launchd, a Linux machine's from its login shell, systemd and PAM. */
function machineVars(target: string): MachineVar[] | null {
  const machine = target.replace(/^agent:/, "");
  if (machine === "metal-pi-04") return null;
  const mac = machine === MAC_MINI;
  const vars: MachineVar[] = mac
    ? [
        { name: "PATH", sources: ["login_shell", "runner"] },
        { name: "HOME", sources: ["login_shell", "runner"] },
        { name: "SSH_AUTH_SOCK", sources: ["launchd", "login_shell"] },
        { name: "HOMEBREW_PREFIX", sources: ["login_shell"] },
        { name: "CARGO_HOME", sources: ["login_shell"] },
        { name: "AWS_DEFAULT_PROFILE", sources: ["login_shell"] },
        { name: "ANTHROPIC_BASE_URL", sources: ["launchd"] },
      ]
    : [
        { name: "PATH", sources: ["login_shell", "runner"] },
        { name: "HOME", sources: ["login_shell", "runner"] },
        { name: "LANG", sources: ["systemd_user", "pam"] },
        { name: "SSH_AUTH_SOCK", sources: ["systemd_user"] },
        { name: "XDG_RUNTIME_DIR", sources: ["pam", "runner"] },
        { name: "CARGO_HOME", sources: ["login_shell"] },
        { name: "GOPATH", sources: ["login_shell"] },
        { name: "AWS_DEFAULT_PROFILE", sources: ["login_shell"] },
        { name: "EDITOR", sources: ["pam"] },
      ];
  return vars.sort((a, b) => a.name.localeCompare(b.name));
}

/** A session by the name people see. */
const sessionName = (id: string) => familyInfos().find((a) => a.id === id)?.name ?? id;

/** A refusal: its words, and its kind. */
const refuse = (ctx: Ctx, res: http.ServerResponse, status: number, code: string, error: string, more: Record<string, unknown> = {}) => (ctx.json(res, status, { error, code, ...more }), true);

/** What's wrong with these variables, as the server says it (none: they're fine). */
function badVars(vars: unknown): { code: string; error: string } | null {
  if (!Array.isArray(vars)) return { code: "agentenv_bad_var", error: "`vars` is a list of variables" };
  const secrets = mySecretNames();
  for (const v of vars as Partial<AgentenvVar>[]) {
    if (typeof v.name !== "string" || !NAME.test(v.name))
      return { code: "agentenv_bad_var", error: `\`${String(v.name ?? "")}\` isn't a variable's name: letters, digits and _ only, not starting with a digit` };
    if (v.name.startsWith("KILN_")) return { code: "agentenv_bad_var", error: `${v.name}: kiln's own variables (KILN_*) come from kiln alone` };
    if (!["machine", "value", "secret", "unset"].includes(String(v.from))) return { code: "agentenv_bad_var", error: `${v.name}: \`from\` is machine, value, secret or unset` };
    if (v.from === "value" && typeof v.value !== "string") return { code: "agentenv_bad_var", error: `${v.name}: a value variable needs its \`value\`` };
    if (v.from === "secret" && !secrets.includes(String(v.secret ?? ""))) return { code: "agentenv_secret_missing", error: `${v.name}: you have no secret ${String(v.secret ?? "")}` };
  }
  return null;
}

/** Tags as the server keeps them: each trimmed, the empty dropped, once whatever their case. */
const tagsOf = (tags: string[]): string[] => tags.map((t) => t.trim()).filter((t, i, all) => t && all.findIndex((x) => x.toLowerCase() === t.toLowerCase()) === i);

const clean = (vars: AgentenvVar[]): AgentenvVar[] =>
  vars.map((v) => ({
    name: v.name,
    from: v.from,
    ...(v.from === "machine" && v.of ? { of: v.of } : {}),
    ...(v.from === "value" ? { value: v.value ?? "" } : {}),
    ...(v.from === "secret" ? { secret: v.secret } : {}),
  }));

export async function handleAgentenvs(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/mock/agentenvs" && m === "POST") {
    kept = seed();
    return (ctx.json(res, 200, { reset: true }), true);
  }
  const applying = p.match(/^\/api\/harness\/sessions\/([^/]+)\/agentenv$/);
  if (applying && m === "PUT") {
    const session = decodeURIComponent(applying[1]!);
    const b = await ctx.body<{ agentenv?: string | null; restart?: boolean }>(req);
    const to = b.agentenv ? kept.get(b.agentenv) : null;
    if (b.agentenv && !to) return refuse(ctx, res, 404, "agentenv_unknown", `you have no environment ${b.agentenv}`);
    const secrets = to?.vars.filter((v) => v.from === "secret").map((v) => v.name) ?? [];
    if (to && secrets.length > 0 && sessionOf(session)?.engine === "microvm")
      return refuse(
        ctx,
        res,
        409,
        "agentenv_secret_in_microvm",
        `${sessionName(session)} runs in a microVM: ${to.name}'s secret ${secrets.length === 1 ? "variable" : "variables"} ${secrets.join(", ")} can't go in by value. Grant a secret to a microVM by name instead.`,
      );
    for (const e of kept.values()) e.sessions = e.sessions.filter((s) => s !== session);
    if (to) to.sessions.push(session);
    const restarted = b.restart !== false;
    return (ctx.json(res, 200, { agent: { id: session }, agentenv: to ? { id: to.id, name: to.name, version: to.version } : null, restarted }), true);
  }
  if (!p.startsWith("/api/agentenvs")) return false;
  if (p === "/api/agentenvs" && m === "GET") {
    const agentenvs = [...kept.values()].sort((a, b) => a.name.localeCompare(b.name));
    return (ctx.json(res, 200, { agentenvs }), true);
  }
  if (p === "/api/agentenvs" && m === "POST") {
    const b = await ctx.body<{ name?: string; description?: string; base?: string; vars?: AgentenvVar[]; tags?: string[] }>(req);
    const name = (b.name ?? "").trim();
    if (!name) return refuse(ctx, res, 400, "agentenv_bad_var", "an environment needs a name");
    if ([...kept.values()].some((e) => e.name === name)) return refuse(ctx, res, 409, "agentenv_name_taken", `you have an environment named ${name} already`);
    const bad = badVars(b.vars ?? []);
    if (bad) return refuse(ctx, res, 400, bad.code, bad.error);
    const t = now();
    const e: Agentenv = {
      id: `env-${Math.random().toString(16).slice(2, 10)}`,
      owner: "ada",
      name,
      ...(b.description?.trim() ? { description: b.description.trim() } : {}),
      base: b.base === "clean" ? "clean" : "user",
      vars: clean(b.vars ?? []),
      tags: tagsOf(b.tags ?? []),
      version: 1,
      created_at: t,
      updated_at: t,
      sessions: [],
    };
    kept.set(e.id, e);
    return (ctx.json(res, 200, e), true);
  }
  if (p === "/api/agentenvs/machine" && m === "GET") {
    const target = url.searchParams.get("target") ?? "local";
    const vars = machineVars(target);
    if (!vars) return refuse(ctx, res, 502, "runner_away", `${target.replace(/^agent:/, "")} is away: its user's variables can't be read until it's back`);
    return (ctx.json(res, 200, { vars }), true);
  }
  const one = p.match(/^\/api\/agentenvs\/([^/]+)$/);
  if (!one) return false;
  const e = kept.get(decodeURIComponent(one[1]!));
  if (!e) return refuse(ctx, res, 404, "agentenv_unknown", `you have no environment ${decodeURIComponent(one[1]!)}`);
  if (m === "GET") return (ctx.json(res, 200, e), true);
  if (m === "PATCH") {
    const b = await ctx.body<{ version?: number; name?: string; description?: string | null; base?: string; vars?: AgentenvVar[]; tags?: string[] }>(req);
    if (b.version !== e.version) return refuse(ctx, res, 409, "agentenv_stale", `${e.name} is at v${e.version} now: it was saved since you read v${b.version ?? "?"}`, { current: e });
    const name = b.name?.trim();
    if (name !== undefined && !name) return refuse(ctx, res, 400, "agentenv_bad_var", "an environment needs a name");
    if (name && name !== e.name && [...kept.values()].some((x) => x.name === name)) return refuse(ctx, res, 409, "agentenv_name_taken", `you have an environment named ${name} already`);
    if (b.vars !== undefined) {
      const bad = badVars(b.vars);
      if (bad) return refuse(ctx, res, 400, bad.code, bad.error);
    }
    const changed = name !== undefined || b.description !== undefined || b.base !== undefined || b.vars !== undefined || b.tags !== undefined;
    if (name) e.name = name;
    if (b.description !== undefined) {
      if (b.description?.trim()) e.description = b.description.trim();
      else delete e.description;
    }
    if (b.base === "user" || b.base === "clean") e.base = b.base;
    if (b.vars) e.vars = clean(b.vars);
    if (b.tags) e.tags = tagsOf(b.tags);
    if (changed) {
      e.version += 1;
      e.updated_at = now();
    }
    return (ctx.json(res, 200, e), true);
  }
  if (m === "DELETE") {
    if (e.sessions.length > 0) {
      const names = e.sessions.map(sessionName);
      return refuse(
        ctx,
        res,
        409,
        "agentenv_in_use",
        `${e.name} can't be deleted: ${names.map((n) => `“${n}”`).join(", ")} ${names.length === 1 ? "runs" : "run"} with it. Put ${names.length === 1 ? "it" : "them"} on another environment first.`,
        { sessions: e.sessions },
      );
    }
    kept.delete(e.id);
    res.writeHead(204);
    res.end();
    return true;
  }
  return false;
}
