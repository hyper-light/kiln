/** Research (docs/research.md): what an agent found out, each finding graded by kiln from its sources. */
import { request } from "./client";

/** A source's kind of thing: inside (the machines and systems the agent works in) or outside. */
export type SourceType =
  "measurement" | "code" | "data" | "log" | "doc" | "media" | "record" | "standard" | "paper" | "docs" | "book" | "preprint" | "news" | "blog" | "wiki" | "forum" | "vendor" | "social" | "web";

export const INSIDE: SourceType[] = ["measurement", "code", "data", "log", "doc", "media", "record"];

export const SOURCE_WORDS: Record<SourceType, string> = {
  measurement: "measurement",
  code: "code",
  data: "data",
  log: "logs",
  doc: "document",
  media: "media",
  record: "kiln record",
  standard: "standard",
  paper: "paper",
  docs: "official docs",
  book: "book",
  preprint: "preprint",
  news: "news",
  blog: "blog",
  wiki: "wiki",
  forum: "forum",
  vendor: "vendor",
  social: "social",
  web: "web page",
};

export interface ResearchSource {
  id: string;
  type: SourceType;
  title: string;
  locator: string;
  machine?: string;
  author?: string;
  published?: string;
  origin?: string;
  dated?: boolean;
  retrieved_at: number;
}

/** kiln's look for a quote where the citation says it is. */
export type CheckState = "pending" | "found" | "not_found" | "gone" | "exists" | "unchecked";

export interface Cite {
  source: string;
  quote?: string;
  at?: string;
  check?: { state: CheckState; at: number; said?: string };
}

export interface Counterclaim {
  claim: string;
  cites: Cite[];
  strength?: "strong" | "moderate" | "weak";
}

export type Grade = "A" | "B" | "C" | "D" | "F";

export interface Confidence {
  score: number;
  grade: Grade;
  words: string;
  support: number;
  against: number;
  reliability: number;
  citation: number;
  independent: number;
  cites: number;
  found: number;
  failed: number;
  why: string;
}

export interface Finding {
  id: string;
  claim: string;
  cites: Cite[];
  against: Counterclaim[];
  inferred?: boolean;
  note?: string;
  at: number;
  confidence: Confidence;
  against_weights: number[];
}

export interface Research {
  id: string;
  question: string;
  agent: string | null;
  by: string;
  plan: string | null;
  sources: ResearchSource[];
  findings: Finding[];
  skipped: { internal?: string; external?: string };
  summary: { findings: number; grades: Partial<Record<Grade, number>>; sources: number; internal: number; external: number; checking: number };
  created_at: number;
  updated_at: number;
  markdown?: string;
}

const enc = encodeURIComponent;

export const researchApi = {
  get: (id: string, signal?: AbortSignal) => request<Research>("GET", `/api/research/${enc(id)}?markdown=false`, undefined, { signal }),
  /** Research, newest change first: one agent's (`agent`), one plan's (`plan`), or all. */
  list: (of: { agent?: string; plan?: string } = {}, signal?: AbortSignal) => {
    const q = new URLSearchParams(Object.entries(of).filter((e): e is [string, string] => !!e[1])).toString();
    return request<{ research: Research[] }>("GET", `/api/research${q ? `?${q}` : ""}`, undefined, { signal });
  },
};

export const researchHref = (id: string) => `/agents/research/${enc(id)}`;

const parsed = (s: string): unknown => {
  try {
    return JSON.parse(s);
  } catch {
    return null;
  }
};

/**
 * The research an agent's tool call starts, adds to, reads or skips a side of (kiln's `research_*`
 * tools), and its id, from the call as any harness reports it: an MCP call of kiln's (Claude Code,
 * Codex: its server and the tool's own name), or one an ACP agent reports in words ("Called
 * research_add from kiln", Devin), its input as text. The id is the one it names, else the one its
 * answer opens with. Null for any other call.
 */
export function researchCall(c: { server?: string; tool_short?: string; title?: string; input?: unknown; output?: string }): { verb: string; id: string | null } | null {
  const tool = c.server?.startsWith("kiln") && c.tool_short?.startsWith("research_") ? c.tool_short : /^Called (research_\w+) from kiln\b/.exec(c.title ?? "")?.[1];
  const verb = tool?.slice("research_".length) ?? "";
  if (!["start", "add", "get", "skip"].includes(verb)) return null;
  const input = typeof c.input === "string" ? parsed(c.input) : c.input;
  const named = input && typeof input === "object" && "id" in input && typeof input.id === "string" ? input.id : null;
  // Its answer is the research (JSON, or JSON quoted as text), its id first.
  const answered = /\\?"id\\?"\s*:\s*\\?"([^"\\]+)\\?"/.exec(c.output ?? "")?.[1] ?? null;
  return { verb, id: named || answered };
}
