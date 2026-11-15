/**
 * Agentfiles in the mock (docs/api.md, "Agentfiles"), answered as the server's routes answer: the
 * mock's one person's own (the server's owner), each with its versions; kiln's built-ins, the
 * repository's own `.kiln/images/harness-<id>/Agentfile` files, which kiln embeds
 * (`crate::agentfiles::builtin`); and, for any repository asked for, one `.kiln/images/claude/Agentfile`.
 * No Shards runs here: a text is kept unchecked (`no_shards`), but one with a `FORM …` line is refused
 * as Shards refuses it, so a page's refusal can be seen.
 *
 *   GET    /api/agentfiles?repo=&all=       the list
 *   POST   /api/agentfiles                  {name, text}: keep one
 *   POST   /api/agentfiles/check            {text}: what keeping it would say
 *   GET    /api/agentfiles/:id              one
 *   PATCH  /api/agentfiles/:id              {name?, text?}: a new text is a new version
 *   GET    /api/agentfiles/:id/versions     its versions, newest first
 *   POST   /api/agentfiles/:id/restore      {version}: that version's text, as a new version
 *   DELETE /api/agentfiles/:id              it and its versions
 *   GET    /api/agentfiles/:id/builds       what it installs from (each source and its pin), its image on each runner, its image versions
 *   POST   /api/agentfiles/:id/builds       {runner?}: build its current version there (every runner that can, without one)
 */
import type http from "node:http";
import { createHash, randomBytes } from "node:crypto";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import type { Agentfile, AgentfileBrief, AgentfileBuiltin, AgentfileCheck, AgentfileListing, AgentfileSaved, AgentfileVersion, ServerEvent } from "../src/api/wire/index.ts";

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

/** Where kiln keeps image definitions (W13-R11), its built-ins' among them. */
const IMAGES = new URL("../../.kiln/images/", import.meta.url);

/** kiln's built-ins, one per harness: `harness-<id>/Agentfile`, as kiln embeds them. */
function builtins(): AgentfileBuiltin[] {
  if (!existsSync(IMAGES)) return [];
  return readdirSync(IMAGES, { withFileTypes: true })
    .filter((d) => d.isDirectory() && d.name.startsWith("harness-") && existsSync(new URL(`${d.name}/Agentfile`, IMAGES)))
    .map((d) => ({ name: d.name, harness: d.name.slice("harness-".length), text: readFileSync(new URL(`${d.name}/Agentfile`, IMAGES), "utf8") }))
    .sort((a, b) => a.name.localeCompare(b.name));
}
let shipped: AgentfileBuiltin[] | null = null;

const kept = new Map<string, Agentfile>();
const versions = new Map<string, AgentfileVersion[]>();

/** A seeded one's id comes from its name, so its links hold across the mock's reloads; one kept since gets a random id, as the server's. */
const seededId = (name: string) => createHash("sha256").update(name).digest("hex").slice(0, 32);
function put(name: string, text: string, at: number, id = randomBytes(16).toString("hex")): Agentfile {
  const a: Agentfile = { id, owner: OWNER, name, text, version: 1, created_at: at, updated_at: at };
  kept.set(a.id, a);
  versions.set(a.id, [{ version: 1, text, at, by: BY }]);
  return a;
}

/**
 * rust-dev at version 9: every kind of install source (an https tarball pinned by its SHA-256, Git
 * over https and over ssh pinned by the commit a ref names, an S3 object pinned by its version, a path
 * kiln fills), the agents it carries and what they reach. Line 13's checksum names another download
 * than the one served, so Shards refuses the build where it fetches it (the runners' fixtures).
 */
const RUST_DEV = `# rust-dev: Rust, Claude Code and Codex, with the reviewer and triage agents
FROM rust:1.91-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends clang lld pkg-config libssl-dev \\
 && rm -rf /var/lib/apt/lists/*
ENV CARGO_HOME=/root/.cargo

HARNESS claude --checksum=sha256:2b7d4f0e9c1a8e3f5d6b0a7c9e2f4d1b8a6c3e5f7d9b1a0c2e4f6a8b0d2c4e6f FROM https://downloads.claude.ai/claude-code/2.1.311/claude-linux.tar.gz
LABEL kiln.harness.claude.model=claude-opus-5-5
LABEL kiln.harness.claude.sign-in=home
NETWORK --egress=api.anthropic.com:443 claude-out FOR claude

# Codex, from its release
HARNESS codex --checksum=sha256:9f2c55d0b7a1e4c8f3d6a9b2e5c8f1a4d7b0e3c6f9a2d5b8e1c4f7a0d3b6e9c2 FROM https://github.com/openai/codex/releases/download/rust-v0.68.0/codex-linux.tar.gz
LABEL kiln.harness.codex.model=gpt-6.1-sol
LABEL kiln.harness.codex.sign-in=secret:OPENAI_API_KEY
NETWORK --egress=api.openai.com:443 codex-out FOR codex

AGENT reviewer FROM git@github.com:hyper-light/agents.git#v4:reviewer
AGENT triage --version-id=3HL4kqtJlcpXroDTDmJ.rmSpXd3dIbrHY --region=us-east-1 FROM s3://kiln-agents/pr-triage/v2.tar.gz
AGENT notes FROM https://github.com/hyper-light/notes-agent.git#main
AGENT lint FROM ./agents/lint
ATTACH reviewer FOR claude
ATTACH triage FOR codex
ATTACH notes FOR claude
ATTACH lint FOR claude
NETWORK --egress=github.com:443,index.crates.io:443,static.crates.io:443 deps FOR reviewer,triage
VOLUME /root/.cargo/registry FOR reviewer,triage
CMD ["claude"]
`;
const DOCS_SITE = `# docs-site: Node and Claude Code, for the docs
FROM node:24-bookworm
HARNESS claude FROM ./harness
LABEL kiln.harness.claude.sign-in=home
AGENT notes FROM https://github.com/hyper-light/notes-agent.git#main
ATTACH notes FOR claude
NETWORK --egress=github.com:443,registry.npmjs.org:443 site FOR notes
CMD ["claude"]
`;

let seeded = false;
function seed() {
  if (seeded) return;
  seeded = true;
  const day = 86_400;
  const a = put("claude-rust", CLAUDE, now() - 3 * day, seededId("claude-rust"));
  const v2 = CLAUDE.replace("FROM ubuntu:24.04", "FROM rust:1.91");
  a.text = v2;
  a.version = 2;
  a.updated_at = now() - day;
  versions.get(a.id)?.unshift({ version: 2, text: v2, at: a.updated_at, by: BY });
  // Older versions of each: the base and the pins moved forward, a line at a time.
  const grow = (name: string, last: string, older: (v: number) => string, n: number, since: number) => {
    const f = put(name, older(1), now() - since, seededId(name));
    for (let v = 2; v <= n; v++) {
      f.text = v === n ? last : older(v);
      f.version = v;
      f.updated_at = now() - since + Math.round(((since - 600) * (v - 1)) / (n - 1));
      versions.get(f.id)?.unshift({ version: v, text: f.text, at: f.updated_at, by: BY });
    }
  };
  grow("rust-dev", RUST_DEV, (v) => RUST_DEV.replace("rust:1.91-bookworm", `rust:1.${82 + v}-bookworm`).replace("2.1.311", `2.1.${300 + v}`), 9, 20 * day);
  grow("docs-site", DOCS_SITE, (v) => DOCS_SITE.replace("node:24", `node:${21 + v}`), 3, 9 * day);
}

/* ------------------------------------------------ what it installs from, and its builds */

type SourceKind = "path" | "git-https" | "git-ssh" | "s3" | "https" | "oci";
/** An install source as Shards reads a `HARNESS` or `AGENT` line: its kind, what pins it, and the credential it fetches with, by name. */
interface Source {
  line: number;
  directive: "HARNESS" | "AGENT";
  name: string;
  kind: SourceKind;
  source: string;
  /** What it's pinned by: a SHA-256, an object version, a ref (resolved to its commit at the last build). */
  pin: { by: "sha256" | "version-id" | "ref" | "kiln"; value: string; resolved: string | null } | null;
  credential: { name: string; scope: string } | null;
  /** The origin its fetch is granted, that step alone. */
  grant: string | null;
}
const kindOf = (src: string): SourceKind =>
  src.startsWith("s3://")
    ? "s3"
    : /^(git@|ssh:\/\/)/.test(src)
      ? "git-ssh"
      : /^https:\/\/[^#]+\.git(#|$)/.test(src)
        ? "git-https"
        : src.startsWith("https://")
          ? "https"
          : /^(\.{1,2}\/|\/)/.test(src)
            ? "path"
            : "oci";
/** A commit a ref resolved to at the last build (a stand-in for the fetch's own answer). */
const commitOf = (ref: string) => {
  let h = 2166136261;
  for (const c of ref) h = Math.imul(h ^ c.charCodeAt(0), 16777619);
  return `${(h >>> 0).toString(16).padStart(8, "0")}${((h * 31) >>> 0).toString(16).padStart(8, "0")}`.slice(0, 12);
};
/** The credentials the person chose for sources, by reference (kept beside the version, never in its text). */
const CREDENTIALS: Record<string, { name: string; scope: string }> = {
  "git@github.com:hyper-light/agents.git": { name: "GH_DEPLOY_KEY", scope: "user" },
  "s3://kiln-agents/pr-triage/v2.tar.gz": { name: "AWS_AGENTS_READ", scope: "user" },
};
function sourcesOf(text: string): Source[] {
  return text.split("\n").flatMap((l, i) => {
    const m = /^\s*(HARNESS|AGENT)\s+(\S+)((?:\s+--\S+)*)\s+FROM\s+(\S+)/.exec(l);
    if (!m) return [];
    const [, directive, name, flags = "", source = ""] = m;
    const flag = (f: string) => new RegExp(`--${f}=(\\S+)`).exec(flags)?.[1] ?? null;
    const kind = kindOf(source);
    const ref = source.includes("#") ? source.slice(source.indexOf("#") + 1).split(":")[0]! : "";
    const pin =
      kind === "git-https" || kind === "git-ssh"
        ? { by: "ref" as const, value: ref || "HEAD", resolved: commitOf(source) }
        : flag("version-id")
          ? { by: "version-id" as const, value: flag("version-id")!, resolved: null }
          : flag("checksum")
            ? { by: "sha256" as const, value: flag("checksum")!.replace(/^sha256:/, ""), resolved: null }
            : kind === "path"
              ? { by: "kiln" as const, value: "kiln fills it, checked by SHA-256", resolved: null }
              : null;
    const origin =
      kind === "s3"
        ? `s3.${flag("region") ?? "us-east-1"}.amazonaws.com:443`
        : kind === "git-ssh"
          ? `${source.replace(/^git@|^ssh:\/\/(\w+@)?/, "").split(/[:/]/)[0]}:22`
          : kind === "path" || kind === "oci"
            ? null
            : `${new URL(source.split("#")[0]!).host}:443`;
    return [{ line: i + 1, directive: directive as "HARNESS" | "AGENT", name: name!, kind, source: source.split("#")[0]!, pin, credential: CREDENTIALS[source.split("#")[0]!] ?? null, grant: origin }];
  });
}

type BuildState = "built" | "building" | "queued" | "failed" | "none";
/** Its image on one runner: built from which version, or building (which step), or why not. */
interface Build {
  runner: string;
  platform: "linux/amd64" | "linux/arm64" | null;
  state: BuildState;
  version: number;
  digest: string | null;
  size: number | null;
  took_s: number | null;
  at: number | null;
  step: [number, number] | null;
  /** Shards' words when it refused or failed; kiln's when a runner doesn't build (can't run microVMs, runs on the host). */
  why: string | null;
  /** While a newer one isn't built: the version its sessions there still start from. */
  serving: number | null;
}
/** Its runners as the runners' fixtures have them: microVM runners build for their own platform; the others say why not. */
const RUNNERS: { runner: string; platform: "linux/amd64" | "linux/arm64" | null; why: string | null }[] = [
  { runner: "metal-ryzen-01", platform: "linux/amd64", why: null },
  { runner: "metal-epyc-01", platform: "linux/amd64", why: null },
  { runner: "9c4e2f7a1b8d43c6a0e5f2b7d1c8a4e3", platform: "linux/arm64", why: null },
  { runner: "metal-ryzen-02", platform: null, why: "runs sessions on the host" },
  { runner: "metal-pi-04", platform: null, why: "can't run microVMs" },
];
const G = 2 ** 30;
/** An image's digest: those the runners' fixtures (microvms.ts) show for the versions they hold, else one made from what it is. */
const DIGESTS: Record<string, Record<string, string>> = {
  "rust-dev/8": { "linux/amd64": "sha256:7c1e0b9a44d2", "linux/arm64": "sha256:5d2a1e9c07f3" },
  "docs-site/3": { "linux/amd64": "sha256:91fe72b04c3d", "linux/arm64": "sha256:e04b7a1c9d25" },
};
const digestOf = (a: Agentfile, version: number, platform: string) => DIGESTS[`${a.name}/${version}`]?.[platform] ?? `sha256:${commitOf(`${a.id}/${version}/${platform}`)}`;
/** Builds asked for since the mock started: runner → when, for which version. */
const asked = new Map<string, Map<string, { at: number; version: number }>>();
/** A build takes this long in the mock, its 9 steps a share each. */
const BUILD_S = 36;
function buildsOf(a: Agentfile): Build[] {
  const mine = asked.get(a.id);
  return RUNNERS.map(({ runner, platform, why }): Build => {
    if (!platform) return { runner, platform, state: "none", version: a.version, digest: null, size: null, took_s: null, at: null, step: null, why, serving: null };
    const ask = mine?.get(runner);
    if (ask && ask.version === a.version) {
      const s = now() - ask.at;
      if (s < 4) return { runner, platform, state: "queued", version: a.version, digest: null, size: null, took_s: null, at: ask.at, step: null, why: null, serving: a.version - 1 || null };
      if (s < BUILD_S)
        return {
          runner,
          platform,
          state: "building",
          version: a.version,
          digest: null,
          size: null,
          took_s: null,
          at: ask.at,
          step: [Math.min(9, 1 + Math.floor(((s - 4) / (BUILD_S - 4)) * 9)), 9],
          why: null,
          serving: a.version - 1 || null,
        };
      return {
        runner,
        platform,
        state: "built",
        version: a.version,
        digest: digestOf(a, a.version, platform),
        size: 1.4 * G,
        took_s: BUILD_S - 4,
        at: ask.at + BUILD_S,
        step: null,
        why: null,
        serving: null,
      };
    }
    // As the runners' fixtures have them: rust-dev v9 building on ryzen-01, refused on epyc-01, waiting its turn on the Mac.
    if (a.name === "rust-dev" && a.version === 9) {
      if (runner === "metal-ryzen-01") return { runner, platform, state: "building", version: 9, digest: null, size: null, took_s: null, at: now() - 200, step: [6, 9], why: null, serving: 8 };
      if (runner === "metal-epyc-01")
        return {
          runner,
          platform,
          state: "failed",
          version: 9,
          digest: null,
          size: null,
          took_s: 48,
          at: now() - 240,
          step: null,
          why: "Shards refused the build at line 13 (HARNESS codex): the download for linux/amd64 has SHA-256 4b07e1a9c3d5…, and --checksum=sha256:9f2c55d0b7a1… names another.",
          serving: 8,
        };
      return { runner, platform, state: "queued", version: 9, digest: null, size: null, took_s: null, at: now() - 30, step: null, why: null, serving: 8 };
    }
    return {
      runner,
      platform,
      state: "built",
      version: a.version,
      digest: digestOf(a, a.version, platform),
      size: (a.name === "docs-site" ? 0.88 : 1.4) * G,
      took_s: 94,
      at: a.updated_at + 120,
      step: null,
      why: null,
      serving: null,
    };
  });
}
/** Its image versions: what each version built to, on which runners its sessions start from it now. */
function imagesOf(a: Agentfile, builds: Build[]) {
  const vs = (versions.get(a.id) ?? []).slice(0, 4);
  return vs.map((v) => {
    const here = builds.filter((b) => (b.state === "built" && b.version === v.version) || b.serving === v.version);
    const built = v.version < a.version || here.some((b) => b.state === "built");
    return {
      version: v.version,
      built,
      digests: built ? Object.fromEntries(["linux/amd64", "linux/arm64"].map((pl) => [pl, digestOf(a, v.version, pl)])) : {},
      size: built ? (a.name === "docs-site" ? 0.88 : 1.4 - (a.version - v.version) * 0.02) * G : null,
      at: v.at + 120,
      runners: here.map((b) => b.runner),
      current: v.version === a.version,
    };
  });
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
      builtin: (shipped ??= builtins()),
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
  if (rest === "builds" && m === "GET") {
    const builds = buildsOf(a);
    return answer({ version: a.version, sources: sourcesOf(a.text), builds, images: imagesOf(a, builds) });
  }
  if (rest === "builds" && m === "POST") {
    const { runner } = await ctx.body<{ runner?: string }>(req);
    const can = RUNNERS.filter((r) => r.platform && (!runner || r.runner === runner));
    if (runner && can.length === 0) return fail(409, RUNNERS.find((r) => r.runner === runner)?.why ?? "no such runner");
    const mine = asked.get(a.id) ?? new Map<string, { at: number; version: number }>();
    for (const r of can) mine.set(r.runner, { at: now(), version: a.version });
    asked.set(a.id, mine);
    const builds = buildsOf(a);
    return answer({ version: a.version, sources: sourcesOf(a.text), builds, images: imagesOf(a, builds) });
  }
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
