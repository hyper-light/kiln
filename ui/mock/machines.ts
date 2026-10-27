/**
 * Your machines as moving an agent sees them (GET /api/reach/runners, /api/harness/kinds with each
 * harness's models), and the moves asked for (POST /api/harness/sessions/:id/move), kept for the
 * browser checks to read (GET /api/mock/moves).
 */
import type http from "node:http";
import type { KindInfo } from "../src/api/harness.ts";
import type { ReachRunner } from "../src/api/types.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
}

const runner = (name: string, os: string, arch: string, harnesses: string[], over: Partial<ReachRunner> = {}): ReachRunner => ({
  name,
  hostname: `${name.replace(/^metal-/, "")}.lan`,
  os,
  arch,
  labels: [],
  connected: true,
  reach: true,
  fingerprint: `fp-${name}`,
  cpus: 16,
  cpu_pct: 12,
  mem_total: 64e9,
  mem_used: 20e9,
  harnesses,
  ...over,
});

const RUNNERS: ReachRunner[] = [
  runner("adacachy", "linux", "x86_64", ["claude", "codex", "devin", "antigravity", "gemini"]),
  runner("metal-ryzen-01", "linux", "x86_64", ["claude", "codex"]),
  runner("metal-mac-mini", "macos", "aarch64", ["claude"]),
  runner("metal-pi-04", "linux", "aarch64", ["codex"]),
  runner("metal-nuc-old", "linux", "x86_64", ["claude"], { connected: false }),
  runner("metal-old", "linux", "x86_64", ["claude"], { fingerprint: null }),
];

const claude: KindInfo = {
  id: "claude",
  name: "Claude Code",
  verified: true,
  available: true,
  switch: true,
  models: [
    { id: "claude-opus-5-5", label: "Opus 5.5", description: "" },
    { id: "claude-sonnet-5-5", label: "Sonnet 5.5", description: "" },
  ],
};
const codex: KindInfo = {
  id: "codex",
  name: "Codex",
  verified: true,
  available: true,
  switch: true,
  models: [
    { id: "gpt-6.1-sol", label: "GPT-6.1 Sol", description: "" },
    { id: "gpt-6.1-sol-mini", label: "GPT-6.1 Sol mini", description: "" },
  ],
};
const devin: KindInfo = {
  id: "devin",
  name: "Devin",
  verified: true,
  available: true,
  switch: true,
  models: [
    { id: "swe-2", label: "SWE 2", description: "" },
    { id: "claude-opus-5-5", label: "Opus 5.5", description: "" },
  ],
};
const antigravity: KindInfo = {
  id: "antigravity",
  name: "Antigravity CLI",
  verified: true,
  available: true,
  switch: true,
  models: [{ id: "gemini-3.8-pro", label: "Gemini 3.8 Pro", description: "" }],
};
/** A harness none of whose sessions ran here yet: it lists no models. */
const gemini: KindInfo = { id: "gemini", name: "Gemini CLI", verified: false, available: true, switch: true };
const kindsOf = (hs: string[]) => [claude, codex, devin, antigravity, gemini].filter((k) => hs.includes(k.id));

/** The moves asked for, newest last. */
const moves: { id: string; to: string; now: boolean; kind?: string; model?: string }[] = [];

/** Handles the routes above; false when it doesn't know the route. */
export async function handleMachines(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  let mm: RegExpMatchArray | null;
  if (p === "/api/reach/runners" && m === "GET") return (ctx.json(res, 200, { runners: RUNNERS }), true);
  if (p === "/api/harness/kinds" && m === "GET") {
    const all: Record<string, KindInfo[]> = {
      local: [claude, codex, devin, antigravity, gemini],
      ...Object.fromEntries(RUNNERS.filter((r) => r.connected).map((r) => [`agent:${r.name}`, kindsOf(r.harnesses)])),
    };
    // One machine's (`?target=`), or every machine's.
    const target = url.searchParams.get("target");
    return (ctx.json(res, 200, target ? (all[target] ?? []) : all), true);
  }
  if ((mm = p.match(/^\/api\/harness\/sessions\/([^/]+)\/move$/)) && m === "POST") {
    const b = await ctx.body<{ to: string; now?: boolean; kind?: string; model?: string }>(req);
    moves.push({ id: decodeURIComponent(mm[1]!), to: b.to, now: Boolean(b.now), ...(b.kind ? { kind: b.kind } : {}), ...(b.model ? { model: b.model } : {}) });
    return (ctx.json(res, 202, { moving: b.to.replace(/^agent:/, "") }), true);
  }
  if (p === "/api/mock/moves" && m === "GET") return (ctx.json(res, 200, moves), true);
  return false;
}
