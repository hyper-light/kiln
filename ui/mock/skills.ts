/**
 * Skills kiln installs (docs/agents.md, "Skills kiln installs"), as the server answers them:
 *   GET    /api/skills?agent=                 the packages an agent has, each machine's state
 *   POST   /api/skills/look                   what a link, folder or written skill holds (a draft)
 *   POST   /api/skills/upload?agent=&name=    what an upload holds
 *   POST   /api/skills                        install a draft
 *   PATCH  /api/skills/:id                    on / off
 *   POST   /api/skills/:id/update             to its branch's newer commit
 *   DELETE /api/skills/:id                    uninstall
 *   GET    /api/skills/:id/skills/:skill      a skill's SKILL.md
 * Links it knows: cursor/plugins (pstack), t3dotgg/tstack, mattpocock/skills; any folder is a
 * Python and a Go skill with hooks and an MCP server. A package installed is fetched, set up, then
 * ready on each machine over a few seconds after it's installed.
 */
import type http from "node:http";
import type { SkillDraft, SkillItem, SkillMachine, SkillPackage, SkillScope } from "../src/api/skills.ts";

type Ctx = {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  err: (res: http.ServerResponse, status: number, message: string) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
};

const names = (s: string): SkillItem[] => s.split(" ").map((name) => ({ name }));
const MACHINES = ["metal-ryzen-02", "jupiter", "adas-macbook-pro-2"];

type Found = Omit<SkillDraft, "draft">;

const PSTACK: Found = {
  name: "pstack",
  version: "0.15.36",
  license: "MIT",
  description: "if you want to go fast, go deep first.",
  shape: "marketplace",
  source: { kind: "link", label: "cursor/plugins", url: "https://github.com/cursor/plugins.git", git_ref: "main", path: "pstack" },
  sha: "8ecfb19f844b67a4bc7af7f66c366d57bc7b6d2b",
  skills: names(
    "architect arena automate-me benchmark-checklist blast-radius bro correct create-formal-verification-skill create-verification-skill figure-it-out how in-your-own-words interrogate ketchup ketchup-video maintain-formal-verification-skill maintain-verification-skill make-bot-ui no-comments poteto-help poteto-mode principle-attack-the-premise principle-boundary-discipline principle-build-the-lever principle-encode-lessons-in-structure principle-exhaust-the-design-space principle-experience-first principle-explain-the-number principle-fix-root-causes principle-foundational-thinking principle-guard-the-context-window principle-laziness-protocol principle-make-operations-idempotent principle-migrate-callers-then-delete-legacy-apis principle-minimize-reader-load principle-model-the-domain principle-never-block-on-the-human principle-outcome-oriented-execution principle-prove-it-works principle-redesign-from-first-principles principle-separate-before-serializing-shared-state principle-sequence-verifiable-units principle-subtract-before-you-add principle-test-behavior-not-implementation principle-type-system-discipline recall reflect setup-pstack sharpen show-me-your-work statechart swarm tdd teach technical-writing typescript-best-practices unslop why",
  ),
  agents: names("comment-sicko poteto-agent"),
  hooks: [],
  mcp: [],
  setup: [],
  needs: [],
};

const firstSkill = PSTACK.skills[0];
if (firstSkill) firstSkill.description = "Use this skill to reason through a design before implementation, keeping constraints and tradeoffs explicit.";

const TSTACK: Found = {
  name: "tstack",
  version: "",
  license: "MIT",
  description: "",
  shape: "skill folders",
  source: { kind: "link", label: "t3dotgg/tstack", url: "https://github.com/t3dotgg/tstack.git" },
  sha: "10850501c2f4e9b1c7a0d3e2f6b8a4c9d1e0f2a3",
  skills: [
    { name: "babysit-pr", description: "Monitor a pull request through review and CI." },
    { name: "council", description: "Review from three models until they agree." },
    { name: "file-pr", description: "File a short PR that leads with the problem." },
    { name: "full-send", description: "Take a change to a merged PR." },
    { name: "full-stop", description: "Full send, stopping when the PR is ready to merge." },
    { name: "leaving-pr-comments", description: "Format comments posted on your behalf." },
    { name: "modernize", description: "Rebase an old PR and decide if it should exist." },
    { name: "sol-loop", description: "Review a change in a loop until both agents agree." },
    { name: "takeover", description: "Finish someone else's PR, keeping their credit." },
  ],
  agents: [],
  hooks: [],
  mcp: [],
  setup: [],
  needs: ["gh", "python3"],
};

const MATT: Found = {
  name: "mattpocock-skills",
  version: "1.3.2",
  license: "MIT",
  description: "Matt Pocock's agent skills for real engineering.",
  shape: "plugin",
  source: { kind: "link", label: "mattpocock/skills", url: "https://github.com/mattpocock/skills.git" },
  sha: "4ae2317c90b1d2e3f4a5b6c7d8e9f0a1b2c3d4e5",
  skills: names(
    "ask-matt diagnosing-bugs grill-with-docs triage improve-codebase-architecture setup-matt-pocock-skills tdd to-spec to-tickets wayfinder implement implement-spec prototype research domain-modeling codebase-design code-review pr retro wizard grill-me grilling handoff teach to-questionnaire wait-what writing-for-agents",
  ),
  agents: [],
  hooks: [],
  mcp: [],
  setup: [],
  needs: [],
};

const PERF = (path: string): Found => ({
  name: path.replace(/\/+$/, "").split("/").pop() || "skills",
  version: "",
  license: "",
  description: "",
  shape: "skill folders",
  source: { kind: "folder", label: `metal-ryzen-02:${path}`, path, machine: "metal-ryzen-02" },
  sha: "c41d07e9a2b3",
  skills: [
    { name: "bench-report", description: "Turn criterion output into a p50/p99/p99.9 table against main." },
    { name: "trace-diff", description: "Diff two traces span by span." },
  ],
  agents: [],
  hooks: ["PostToolUse"],
  mcp: ["traces"],
  setup: [
    { skill: "bench-report", stack: "Python", command: "uv sync" },
    { skill: "trace-diff", stack: "Go", command: "go build -o bin/ ./..." },
  ],
  needs: ["go", "uv"],
});

const one = (name: string, description: string, label: string, kind: string): Found => ({
  name,
  version: "",
  license: "",
  description,
  shape: "one skill",
  source: { kind, label },
  sha: "5d9a2b0e7f31",
  skills: [{ name, description }],
  agents: [],
  hooks: [],
  mcp: [],
  setup: [],
  needs: [],
});

type Kept = { p: SkillPackage; at: number; scopeOf: SkillScope; agent: string };
const drafts = new Map<string, Found>();
let installed: Kept[] = [];
let seeded = false;
let n = 0;

const SCOPE_WORD: Record<SkillScope, string> = { agent: "This agent", mine: "All my agents", repo: "This repo" };

/** Each machine's state, from how long ago it was installed: fetched, set up (when it has setup), ready. */
function machinesOf(k: Kept, agentMachine: string): SkillMachine[] {
  const age = Date.now() - k.at;
  const at = k.scopeOf === "mine" ? MACHINES : [agentMachine];
  return at.map((machine) => {
    const missing = k.p.needs.filter((t) => (t === "gh" && machine === "jupiter") || (t === "go" && machine === "adas-macbook-pro-2"));
    if (age < 1500) return { machine, state: "fetching", note: "", log: "" };
    if (age < 4000 && k.p.setup.length) return { machine, state: "setting-up", note: "", log: "" };
    if (missing.length) return { machine, state: "needs", note: `${missing.join(", ")} missing`, log: "" };
    return { machine, state: "ready", note: "", log: k.p.setup.map((s) => `$ ${s.command}\nok`).join("\n") };
  });
}

function seed() {
  if (seeded) return;
  seeded = true;
  const old = Date.now() - 3_600_000;
  installed = [
    { p: pkg(TSTACK, "mine"), at: old, scopeOf: "mine", agent: "" },
    { p: { ...pkg(MATT, "agent"), update: "9c1e0aa4b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7" }, at: old, scopeOf: "agent", agent: "fam-docs" },
  ];
}

function pkg(f: Found, scope: SkillScope): SkillPackage {
  n += 1;
  return { id: `pkg-${n}`, ...f, scope: SCOPE_WORD[scope], on: true, update: null, machines: [], by: "admin" };
}

function look(found: Found | string) {
  if (typeof found === "string") return { status: 400, body: found };
  n += 1;
  const draft = `draft-${n}`;
  drafts.set(draft, found);
  return { status: 200, body: { draft, ...found } };
}

export async function handleSkills(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (!p.startsWith("/api/skills")) return false;
  seed();
  const agentMachine = "metal-ryzen-02";
  const view = (k: Kept) => ({ ...k.p, machines: k.p.on ? machinesOf(k, agentMachine) : [] });
  if (p === "/api/skills" && m === "GET") {
    const agent = url.searchParams.get("agent") ?? "";
    const mine = installed.filter((k) => k.scopeOf !== "agent" || k.agent === agent);
    ctx.json(res, 200, { packages: mine.map(view), machine: agentMachine, installs: true });
    return true;
  }
  if (p === "/api/skills/look" && m === "POST") {
    const b = await ctx.body<{ from?: string; link?: string; path?: string; plugin?: string; name?: string; description?: string; body?: string }>(req);
    const link = (b.link ?? "").trim().replace(/\/+$/, "");
    const found: Found | string =
      b.from === "folder"
        ? PERF(b.path ?? "~/skills")
        : b.from === "write"
          ? one((b.name ?? "skill").toLowerCase().replace(/[^a-z0-9]+/g, "-"), b.description ?? "", "written here", "write")
          : /github\.com\/cursor\/plugins(\/tree\/[^/]+\/pstack)?$/.test(link)
            ? PSTACK
            : /github\.com\/t3dotgg\/tstack$/.test(link)
              ? TSTACK
              : /github\.com\/mattpocock\/skills$/.test(link)
                ? MATT
                : "no skills or plugin there";
    const r = look(found);
    if (r.status !== 200) ctx.err(res, r.status, r.body as string);
    else ctx.json(res, 200, r.body);
    return true;
  }
  if (p === "/api/skills/upload" && m === "POST") {
    for await (const _ of req) void _;
    const name = (url.searchParams.get("name") ?? "release-notes.md").replace(/\.(md|zip|tar\.gz|tgz)$/, "");
    if (name === "marketplace") {
      const plugin = url.searchParams.get("plugin");
      if (!plugin) ctx.json(res, 200, { choose: [{ name: "pstack" }, { name: "tstack" }] });
      else ctx.json(res, 200, look({ ...(plugin === "pstack" ? PSTACK : TSTACK), source: { kind: "upload", label: "marketplace.zip" } }).body);
    } else ctx.json(res, 200, look(one(name, "Write release notes from merged changes.", url.searchParams.get("name") ?? name, "upload")).body);
    return true;
  }
  if (p === "/api/skills" && m === "POST") {
    const b = await ctx.body<{ agent: string; draft: string; scope: SkillScope; picks: { skills: boolean; agents: boolean; hooks: boolean; mcp: boolean; setup: boolean } }>(req);
    const f = drafts.get(b.draft);
    if (!f) return (ctx.err(res, 404, "no such draft: look at its source again"), true);
    if (!b.picks.skills && !b.picks.agents) return (ctx.err(res, 400, "pick its skills or its agents: nothing else installs alone"), true);
    const p2 = pkg(
      { ...f, skills: b.picks.skills ? f.skills : [], agents: b.picks.agents ? f.agents : [], hooks: b.picks.hooks ? f.hooks : [], mcp: b.picks.mcp ? f.mcp : [], setup: b.picks.setup ? f.setup : [] },
      b.scope,
    );
    installed = [{ p: p2, at: Date.now(), scopeOf: b.scope, agent: b.agent }, ...installed.filter((k) => !(k.p.name === p2.name && k.scopeOf === b.scope))];
    drafts.delete(b.draft);
    ctx.json(res, 200, view(installed[0]!));
    return true;
  }
  const mm = p.match(/^\/api\/skills\/([^/]+)(\/update|\/skills\/([^/]+))?$/);
  const k = mm ? installed.find((x) => x.p.id === decodeURIComponent(mm[1]!)) : undefined;
  if (!mm || !k) return (ctx.err(res, 404, "no such package"), true);
  if (m === "PATCH" && !mm[2]) {
    const b = await ctx.body<{ on: boolean }>(req);
    k.p = { ...k.p, on: b.on };
    if (b.on) k.at = Date.now();
    ctx.json(res, 200, view(k));
    return true;
  }
  if (m === "DELETE" && !mm[2]) {
    installed = installed.filter((x) => x !== k);
    ctx.json(res, 200, { removed: k.p.id });
    return true;
  }
  if (m === "POST" && mm[2] === "/update") {
    k.p = { ...k.p, sha: k.p.update ?? k.p.sha, update: null };
    k.at = Date.now();
    ctx.json(res, 200, view(k));
    return true;
  }
  if (m === "GET" && mm[3]) {
    const s = k.p.skills.find((x) => x.name === decodeURIComponent(mm[3]!));
    if (!s) return (ctx.err(res, 404, `${k.p.name} has no skill ${mm[3]}`), true);
    ctx.json(res, 200, { content: `---\nname: ${s.name}\ndescription: ${s.description ?? "…"}\n---\n\n# ${s.name}\n\n## When to use\n\n${s.description ?? "…"}\n` });
    return true;
  }
  ctx.err(res, 404, `no route for ${m} ${p}`);
  return true;
}
