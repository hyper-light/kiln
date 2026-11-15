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

/**
 * A variant: an opaque id (never shown: the UI names a variant by what sets it apart, or by the name
 * a person gave it), the deployment it starts sessions in, its share of new sessions, and its measures.
 */
const variant = (
  id: string,
  agentfile: string,
  cpus: number,
  memory: number,
  weight: number,
  sessions: number,
  warm: [number, number, number],
  peak: [number, number, number],
  failed: [number, number, number],
  current = false,
) => ({
  id,
  name: null as string | null,
  agentfile,
  engine: "microvm",
  cpus,
  memory: memory * 2 ** 30,
  weight,
  sessions,
  current,
  measures: { warm_ms: warm, peak_gib: peak, failed_pct: failed },
});

/** Where a definition's sessions run and how fast they start: its engine, warm hosts kept (of the target), and start p50s by kind. */
const runs = (engine: "microvm" | "host", warm: number, target: number, p50: { warm?: number; template?: number; cold?: number }) => ({ engine, warm, target, p50_ms: p50 });

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
    runs: runs("microvm", 4, 7, { warm: 214, template: 438, cold: 1890 }),
    sessions: [
      { id: "fam-orch", version: 4, variant: "5e1c" },
      { id: "fan-lead", version: 4, variant: "a37d" },
      { id: "fan-s2", version: 4, variant: "5e1c" },
      { id: "fam-migr", version: 4, variant: "a37d" },
      { id: "rev-big", version: 3, variant: "5e1c" },
    ],
    experiment: {
      id: "reviewer-size",
      state: "live",
      version: 1,
      sessions: 96,
      variants: [
        variant("5e1c", "rust-dev", 2, 4, 50, 49, [208, 215, 223], [2.24, 2.29, 2.35], [6, 12, 20], true),
        variant("a37d", "rust-dev", 4, 8, 50, 47, [212, 221, 229], [2.94, 3.0, 3.07], [0, 2, 8]),
      ],
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
        variant("0b9e", "rust-dev", 1, 2, 20, 44, [196, 204, 213], [1.71, 1.76, 1.82], [8, 15, 24], true),
        variant("c4a1", "rust-dev", 2, 2, 20, 41, [199, 207, 216], [1.74, 1.79, 1.85], [5, 11, 19]),
        variant("7f20", "rust-dev", 2, 4, 20, 43, [201, 209, 218], [2.21, 2.27, 2.33], [1, 4, 10]),
        variant("e853", "rust-dev", 4, 4, 15, 31, [204, 212, 221], [2.26, 2.31, 2.38], [0, 3, 10]),
        variant("19d6", "rust-dev", 4, 8, 15, 33, [207, 216, 226], [2.93, 2.99, 3.06], [0, 2, 9]),
        variant("d2fb", "docs-site", 2, 4, 10, 22, [188, 199, 211], [1.95, 2.02, 2.1], [2, 8, 21]),
      ],
    },
    runs: runs("microvm", 0, 1, { warm: 205, template: 455, cold: 1940 }),
    sessions: [
      { id: "sub-codex", version: 2, variant: "0b9e" },
      { id: "fam-tests", version: 2, variant: "7f20" },
      { id: "fan-s3", version: 2, variant: "c4a1" },
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
    runs: runs("microvm", 0, 0, { warm: 198 }),
    sessions: [
      { id: "sub-busy", version: 1 },
      { id: "sub-done", version: 1 },
    ],
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
    runs: runs("microvm", 0, 0, {}),
    sessions: [],
  },
  ...["claude", "codex", "devin", "gemini", "opencode", "goose", "amp", "antigravity", "cursor", "pi"].map((h) => ({
    id: h,
    name: h,
    own: false,
    kind: "harness",
    harness: h,
    model: "",
    agentfile: `harness-${h}`,
    version: 1,
    instructions: "",
    tools: 0,
    mcp: 0,
    skills: 0,
    limit: "",
    runs: runs("microvm", 0, 0, h === "claude" ? { template: 612, cold: 1884 } : h === "codex" ? { template: 640, cold: 1930 } : h === "devin" ? { cold: 2104 } : {}),
    sessions: [] as { id: string; version: number }[],
  })),
  ...["ci-diagnose", "ci-review", "ci-performance", "ci-migrate", "ci-author", "ci-secrets", "ci-runners"].map((n) => ({
    id: n,
    name: n,
    own: false,
    kind: "skill",
    harness: n === "ci-performance" || n === "ci-runners" ? "codex" : "claude",
    model: "",
    agentfile: "harness-claude",
    version: 1,
    instructions: "",
    tools: 2,
    mcp: 0,
    skills: 1,
    limit: "",
    runs: runs("microvm", n === "ci-diagnose" ? 1 : 0, n === "ci-diagnose" ? 1 : 0, n === "ci-diagnose" ? { warm: 230, cold: 1912 } : {}),
    sessions: n === "ci-diagnose" ? [{ id: "att-ci", version: 1 }] : [],
  })),
  ...["about", "ask", "review", "attach"].map((n) => ({
    id: `ui-${n}`,
    name: n,
    own: false,
    kind: "ui",
    harness: "claude",
    model: "claude-sonnet-5-5",
    agentfile: "harness-claude",
    version: 1,
    instructions: "",
    tools: 1,
    mcp: 0,
    skills: 0,
    limit: "",
    runs: runs("microvm", 0, 0, { template: 590 }),
    sessions: [] as { id: string; version: number }[],
    replaced_by: n === "about" ? "about-terse" : undefined,
  })),
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

/** The definition a session started from and the deployment its variant gave it: what its microVM was sized and built from. */
export function sessionDef(session: string): { agent: string; agentfile: string; cpus: number; memory: number } | null {
  for (const d of DEFS as {
    name: string;
    agentfile: string;
    sessions: { id: string; variant?: string }[];
    experiment?: { variants: { id: string; agentfile: string; cpus: number; memory: number }[] };
  }[]) {
    const s = d.sessions.find((x) => x.id === session);
    if (!s) continue;
    const v = s.variant ? d.experiment?.variants.find((x) => x.id === s.variant) : undefined;
    return { agent: d.name, agentfile: v?.agentfile ?? d.agentfile, cpus: v?.cpus ?? 2, memory: v?.memory ?? 4 * 2 ** 30 };
  }
  return null;
}
