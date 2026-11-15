/**
 * Agent definitions (GET /api/agentdefs; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P5.1): an agent
 * as you define it (one of kiln's ten harnesses, its model, its Agentfile, instructions, tools,
 * skills and limits), the sessions started from it (ids of the mock's own harness sessions, so each
 * opens its real agent page), and a live experiment splitting its new sessions between microVM
 * deployments (P7.1). kiln's own definitions ride along, read only.
 */
import type http from "node:http";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
}

const variant = (key: string, agentfile: string, cpus: number, memory: number, weight: number, sessions: number, warm: [number, number, number], peak: [number, number, number], failed: [number, number, number], current = false) => ({
  key,
  agentfile,
  engine: "microvm",
  cpus,
  memory: memory * 2 ** 30,
  weight,
  sessions,
  current,
  measures: { warm_ms: warm, peak_gib: peak, failed_pct: failed },
});

const DEFS = [
  {
    id: "reviewer",
    name: "reviewer",
    own: true,
    harness: "claude",
    model: "claude-opus-5-5",
    agentfile: "rust-dev",
    version: 4,
    instructions: "Review the change against AGENTS.md; findings with file and line.",
    tools: 3,
    mcp: 1,
    skills: 3,
    limit: "45m",
    sessions: [
      { id: "fam-orch", version: 4, variant: "A" },
      { id: "fan-lead", version: 4, variant: "B" },
      { id: "rev-big", version: 3, variant: "A" },
    ],
    experiment: {
      id: "reviewer-size",
      state: "live",
      version: 1,
      sessions: 96,
      variants: [variant("A", "rust-dev", 2, 4, 50, 49, [208, 215, 223], [2.24, 2.29, 2.35], [6, 12, 20], true), variant("B", "rust-dev", 4, 8, 50, 47, [212, 221, 229], [2.94, 3.0, 3.07], [0, 2, 8])],
    },
  },
  {
    id: "pr-triage",
    name: "pr-triage",
    own: true,
    harness: "codex",
    model: "gpt-6.1-sol-mini",
    agentfile: "rust-dev",
    version: 2,
    instructions: "Label and route new pull requests.",
    tools: 1,
    mcp: 0,
    skills: 0,
    limit: "10m",
    conflict: { field: "model", here: "gpt-6.1-sol-mini", there: "gpt-6.1-sol", from: "rust-dev v9" },
    experiment: {
      id: "triage-sizes",
      state: "live",
      version: 3,
      sessions: 214,
      variants: [
        variant("A", "rust-dev", 1, 2, 20, 44, [196, 204, 213], [1.71, 1.76, 1.82], [8, 15, 24], true),
        variant("B", "rust-dev", 2, 2, 20, 41, [199, 207, 216], [1.74, 1.79, 1.85], [5, 11, 19]),
        variant("C", "rust-dev", 2, 4, 20, 43, [201, 209, 218], [2.21, 2.27, 2.33], [1, 4, 10]),
        variant("D", "rust-dev", 4, 4, 15, 31, [204, 212, 221], [2.26, 2.31, 2.38], [0, 3, 10]),
        variant("E", "rust-dev", 4, 8, 15, 33, [207, 216, 226], [2.93, 2.99, 3.06], [0, 2, 9]),
        variant("F", "docs-site", 2, 4, 10, 22, [188, 199, 211], [1.95, 2.02, 2.1], [2, 8, 21]),
      ],
    },
    sessions: [
      { id: "sub-codex", version: 2 },
      { id: "fam-tests", version: 2 },
    ],
  },
  {
    id: "release-notes",
    name: "release-notes",
    own: true,
    harness: "gemini",
    model: "",
    agentfile: "docs-site",
    version: 1,
    instructions: "Notes from landed diffs.",
    tools: 0,
    mcp: 0,
    skills: 0,
    limit: "",
    sessions: [{ id: "sub-done", version: 1 }],
  },
  {
    id: "about-terse",
    name: "about-terse",
    own: true,
    replaces: "about",
    harness: "claude",
    model: "claude-sonnet-5-5",
    agentfile: "harness-claude",
    version: 1,
    instructions: "Explain the code in three lines.",
    tools: 1,
    mcp: 0,
    skills: 0,
    limit: "2m",
    sessions: [],
  },
  ...["claude", "codex", "devin", "gemini", "opencode", "goose", "amp", "antigravity", "cursor", "pi"].map((h) => ({ id: h, name: h, own: false, kind: "harness", harness: h, model: "", agentfile: `harness-${h}`, version: 1, instructions: "", tools: 0, mcp: 0, skills: 0, limit: "", sessions: [] as { id: string; version: number }[] })),
  ...["ci-diagnose", "ci-review", "ci-performance", "ci-migrate", "ci-author", "ci-secrets", "ci-runners"].map((n) => ({ id: n, name: n, own: false, kind: "skill", harness: n === "ci-performance" || n === "ci-runners" ? "codex" : "claude", model: "", agentfile: "harness-claude", version: 1, instructions: "", tools: 2, mcp: 0, skills: 1, limit: "", sessions: n === "ci-diagnose" ? [{ id: "att-ci", version: 1 }] : [] })),
  ...["about", "ask", "review", "attach"].map((n) => ({ id: `ui-${n}`, name: n, own: false, kind: "ui", harness: "claude", model: "claude-sonnet-5-5", agentfile: "harness-claude", version: 1, instructions: "", tools: 1, mcp: 0, skills: 0, limit: "", sessions: [] as { id: string; version: number }[], replaced_by: n === "about" ? "about-terse" : undefined })),
];

/** Handles the routes above; false when it doesn't know the route. */
export async function handleAgentdefs(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/agentdefs" && m === "GET") return (ctx.json(res, 200, { agentdefs: DEFS }), true);
  const one = p.match(/^\/api\/agentdefs\/([^/]+)$/);
  if (one && m === "GET") {
    const d = DEFS.find((x) => x.id === decodeURIComponent(one[1]!));
    return (d ? ctx.json(res, 200, { agentdef: d }) : ctx.json(res, 404, { error: "no such agent definition" }), true);
  }
  return false;
}
