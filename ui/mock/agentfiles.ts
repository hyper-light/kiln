/**
 * Agentfiles in the mock (docs/api.md, "Agentfiles"), answered as the server's routes answer: the
 * mock's one person's own (the server's owner), each with its versions; kiln's built-ins (none yet);
 * and, for any repository asked for, one `.kiln/images/claude/Agentfile`. No Shards runs here: a text
 * is kept unchecked (`no_shards`), but one with a `FORM …` line is refused as Shards refuses it, so a
 * page's refusal can be seen.
 *
 *   GET    /api/agentfiles?repo=&all=       the list
 *   POST   /api/agentfiles                  {name, text}: keep one
 *   POST   /api/agentfiles/check            {text}: what keeping it would say
 *   GET    /api/agentfiles/:id              one
 *   PATCH  /api/agentfiles/:id              {name?, text?}: a new text is a new version
 *   GET    /api/agentfiles/:id/versions     its versions, newest first
 *   POST   /api/agentfiles/:id/restore      {version}: that version's text, as a new version
 *   DELETE /api/agentfiles/:id              it and its versions
 */
import type http from "node:http";
import { randomBytes } from "node:crypto";
import type { Agentfile, AgentfileBrief, AgentfileCheck, AgentfileListing, AgentfileSaved, AgentfileVersion, ServerEvent } from "../src/api/wire/index.ts";

type Ctx = {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  err: (res: http.ServerResponse, status: number, message: string) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
  emit: (ev: ServerEvent) => void;
};

const OWNER = "owner";
const BY = "admin";
/** The most an Agentfile's text is: 2 MiB, as the server's. */
const MOST = 2 << 20;
/** An image's name, as the registry's: lowercase letters and digits joined by `.`, `_`, `__` or runs of `-`. */
const NAME = /^[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*$/;

const now = () => Math.floor(Date.now() / 1000);

const CLAUDE = `FROM ubuntu:24.04
HARNESS claude FROM ./harness
AGENT main FROM ./agent
ATTACH main FOR claude
CMD ["claude"]
`;

const kept = new Map<string, Agentfile>();
const versions = new Map<string, AgentfileVersion[]>();

function put(name: string, text: string, at: number): Agentfile {
  const a: Agentfile = { id: randomBytes(16).toString("hex"), owner: OWNER, name, text, version: 1, created_at: at, updated_at: at };
  kept.set(a.id, a);
  versions.set(a.id, [{ version: 1, text, at, by: BY }]);
  return a;
}

let seeded = false;
function seed() {
  if (seeded) return;
  seeded = true;
  const day = 86_400;
  const a = put("claude-rust", CLAUDE, now() - 3 * day);
  const v2 = CLAUDE.replace("FROM ubuntu:24.04", "FROM rust:1.91");
  a.text = v2;
  a.version = 2;
  a.updated_at = now() - day;
  versions.get(a.id)?.unshift({ version: 2, text: v2, at: a.updated_at, by: BY });
}

function brief(a: Agentfile): AgentfileBrief {
  return { id: a.id, owner: a.owner, name: a.name, version: a.version, created_at: a.created_at, updated_at: a.updated_at };
}

/** What Shards would say: a `FORM` line refused in its words, else unchecked (no Shards here). */
function check(text: string): AgentfileCheck {
  const at = text.split("\n").findIndex((l) => /^\s*FORM\s/i.test(l));
  if (at < 0) return { checked: false, unchecked: { reason: "no_shards" }, diagnostics: [] };
  const line = at + 1;
  const message = `dockerfile parse error on line ${line}: unknown instruction: FORM (did you mean FROM?)`;
  return { checked: true, diagnostics: [{ code: "shards", severity: "error", message, path: "Agentfile", range: { start: { line, col: 1 }, end: { line, col: 1 } } }] };
}

function refusal(c: AgentfileCheck): string | null {
  const e = c.diagnostics.find((d) => d.severity === "error");
  return e ? `Shards refuses this Agentfile: ${e.message}` : null;
}

function told(ctx: Ctx, a: Agentfile, deleted = false) {
  ctx.emit({ type: "agentfile", owner: a.owner, id: a.id, name: a.name, version: a.version, ...(deleted ? { deleted } : {}) });
}

/** A text's problem before Shards reads it, as the server says it. */
function textProblem(text: string): string | null {
  if (!text.trim()) return "an Agentfile needs its instructions";
  return text.length > MOST ? "an Agentfile is 2 MiB at the most" : null;
}

function nameProblem(name: string): string | null {
  return NAME.test(name) && name.length <= 255
    ? null
    : `\`${name}\` isn't an Agentfile's name: an image's, lowercase letters and digits joined by \`.\`, \`_\` or \`-\` (\`claude-rust\`), 255 at the most`;
}

export async function handleAgentfiles(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p !== "/api/agentfiles" && !p.startsWith("/api/agentfiles/")) return false;
  seed();
  const fail = (status: number, message: string) => {
    ctx.err(res, status, message);
    return true;
  };
  const answer = (body: unknown) => {
    ctx.json(res, 200, body);
    return true;
  };
  if (p === "/api/agentfiles" && m === "GET") {
    const repo = url.searchParams.get("repo")?.trim();
    const listing: AgentfileListing = {
      agentfiles: [...kept.values()].sort((a, b) => b.updated_at - a.updated_at || a.id.localeCompare(b.id)).map(brief),
      reaches_all: true,
      builtin: [],
      ...(repo ? { project: { repo, sha: "4f1c9a7e2b3d5f60718293a4b5c6d7e8f9012345", agentfiles: [{ name: "claude", path: ".kiln/images/claude/Agentfile", text: CLAUDE }] } } : {}),
    };
    return answer(listing);
  }
  if (p === "/api/agentfiles/check" && m === "POST") {
    const { text = "" } = await ctx.body<{ text?: string }>(req);
    const problem = textProblem(text);
    return problem ? fail(400, problem) : answer(check(text));
  }
  if (p === "/api/agentfiles" && m === "POST") {
    const { name = "", text = "" } = await ctx.body<{ name?: string; text?: string }>(req);
    const problem = nameProblem(name.trim()) ?? textProblem(text);
    if (problem) return fail(400, problem);
    const c = check(text);
    const refused = refusal(c);
    if (refused) return fail(422, refused);
    if ([...kept.values()].some((a) => a.name === name.trim())) return fail(409, `you have an Agentfile named ${name.trim()} already: give this one another name`);
    const a = put(name.trim(), text, now());
    told(ctx, a);
    const saved: AgentfileSaved = { agentfile: a, check: c };
    return answer(saved);
  }
  const [, , , id, rest] = p.split("/");
  const a = id ? kept.get(id) : undefined;
  if (!a) return fail(404, "no such Agentfile");
  if (!rest && m === "GET") return answer(a);
  if (rest === "versions" && m === "GET") return answer({ versions: versions.get(a.id) ?? [] });
  if (!rest && m === "DELETE") {
    kept.delete(a.id);
    versions.delete(a.id);
    told(ctx, a, true);
    return answer({ deleted: true });
  }
  if ((!rest && m === "PATCH") || (rest === "restore" && m === "POST")) {
    const b = await ctx.body<{ name?: string; text?: string; version?: number }>(req);
    let text = b.text;
    if (rest === "restore") {
      const v = (versions.get(a.id) ?? []).find((x) => x.version === Number(b.version));
      if (!v) return fail(400, `the Agentfile has no version ${b.version}`);
      text = v.text;
    }
    const name = b.name?.trim() ?? a.name;
    const problem = nameProblem(name) ?? (text === undefined ? null : textProblem(text));
    if (problem) return fail(400, problem);
    const c = check(text ?? a.text);
    const refused = text !== undefined && text !== a.text ? refusal(c) : null;
    if (refused) return fail(422, refused);
    if (name !== a.name && [...kept.values()].some((x) => x.name === name)) return fail(409, `you have an Agentfile named ${name} already: give this one another name`);
    const changed = text !== undefined && text !== a.text;
    if (changed || name !== a.name) {
      a.name = name;
      a.updated_at = now();
      if (changed) {
        a.text = text ?? a.text;
        a.version += 1;
        versions.get(a.id)?.unshift({ version: a.version, text: a.text, at: a.updated_at, by: BY });
      }
      told(ctx, a);
    }
    const saved: AgentfileSaved = { agentfile: a, check: c };
    return answer(saved);
  }
  return fail(404, `no route ${m} ${p}`);
}
