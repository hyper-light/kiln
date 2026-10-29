/**
 * The reaper's policies (GET and PUT /api/ci/work/reap/policies, docs/ci.md W24-R31), kept in memory:
 * each scope's policy as set, and the policy in force for the scopes a view names, each value with
 * the scope it came from, on a 1 TiB volume.
 */
import type http from "node:http";

type Ctx = { json: (res: http.ServerResponse, status: number, body: unknown) => void; body: (req: http.IncomingMessage) => Promise<unknown> };
type Scope = "system" | "runner" | "repo" | "session";
type Policy = Record<string, string | number>;

const VOLUME = 2 ** 40;
const GIB = 2 ** 30;
const now = () => Math.floor(Date.now() / 1000);
const stored = new Map<string, { scope: Scope; target: string; policy: Policy; updated_at: number; updated_by: string }>([
  ["system:", { scope: "system", target: "", policy: { floor: "10%", keep_last: 2, max_age: "30d", schedule: "30 3 * * *" }, updated_at: now() - 86_400, updated_by: "user:ada" }],
  ["runner:metal-ryzen-01", { scope: "runner", target: "metal-ryzen-01", policy: { floor: "200GiB", ceiling: "20%" }, updated_at: now() - 3600, updated_by: "user:ada" }],
]);

function bytes(v: string | number | undefined): number | undefined {
  if (v === undefined) return undefined;
  const s = String(v).trim();
  const pct = s.match(/^([\d.]+)%$/);
  if (pct) return Math.round((VOLUME * Number(pct[1])) / 100);
  const m = s.match(/^([\d.]+)\s*(B|KB|MB|GB|TB|KiB|MiB|GiB|TiB)?$/i);
  if (!m) return undefined;
  const mult: Record<string, number> = { b: 1, kb: 1e3, mb: 1e6, gb: 1e9, tb: 1e12, kib: 1024, mib: 1024 ** 2, gib: GIB, tib: GIB * 1024 };
  return Math.round(Number(m[1]) * mult[(m[2] ?? "b").toLowerCase()]);
}

function view(at: { runner?: string; repo?: string; session?: string }) {
  const chain: [Scope, string][] = [["system", ""]];
  if (at.runner) chain.push(["runner", at.runner]);
  if (at.repo) chain.push(["repo", at.repo.toLowerCase()]);
  if (at.session) chain.push(["session", at.session]);
  const policies = chain.map(([s, t]) => stored.get(`${s}:${t}`)).filter((p) => p !== undefined);
  const hw = (key: string, stricter: (a: number, b: number) => boolean, fallback: number) => {
    let best: { bytes: number; from: Scope | null } = { bytes: fallback, from: null };
    for (const p of policies.filter((p) => p.scope === "system" || p.scope === "runner")) {
      const b = bytes(p.policy[key]);
      if (b !== undefined && (best.from === null || stricter(b, best.bytes))) best = { bytes: b, from: p.scope };
    }
    return best;
  };
  const specific = (key: string, fallback: string | number) => {
    const p = [...policies].reverse().find((p) => p.policy[key] !== undefined);
    return { value: p ? p.policy[key] : fallback, from: p ? p.scope : null };
  };
  const floor = hw("floor", (a, b) => a > b, Math.min(VOLUME / 10, 100 * GIB));
  const target = hw("target", (a, b) => a > b, floor.bytes + VOLUME / 20);
  return {
    policies,
    effective: {
      volume: VOLUME,
      floor,
      ceiling: hw("ceiling", (a, b) => a < b, Math.min(VOLUME / 4, 300 * GIB)),
      target: { ...target, bytes: Math.max(target.bytes, floor.bytes) },
      keep_last: specific("keep_last", 2),
      keep_within: specific("keep_within", "1d"),
      max_age: specific("max_age", "30d"),
      schedule: specific("schedule", "1d"),
    },
  };
}

/** Handles the routes above; false when it doesn't know the route. */
export async function handleReap(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  if (url.pathname !== "/api/ci/work/reap/policies") return false;
  const q = url.searchParams;
  if (req.method === "GET") {
    ctx.json(res, 200, view({ runner: q.get("runner") ?? undefined, repo: q.get("repo") ?? undefined, session: q.get("session") ?? undefined }));
    return true;
  }
  if (req.method === "PUT") {
    const raw = (q.get("scope") ?? "server").trim();
    const [kind, ...rest] = raw.split(":");
    const scope: Scope | undefined = raw === "server" ? "system" : kind === "runner" || kind === "repo" || kind === "session" ? kind : undefined;
    const target = scope === "system" ? "" : rest.join(":");
    if (!scope || (scope !== "system" && !target)) return (ctx.json(res, 400, { error: `\`${raw}\` isn't a scope: server, runner:<name>, repo:<owner/name> or session:<id>` }), true);
    const body = (await ctx.body(req)) as { policy?: Policy } | null;
    const policy = body?.policy ?? {};
    if ((scope === "repo" || scope === "session") && ["floor", "ceiling", "target"].some((k) => policy[k] !== undefined))
      return (ctx.json(res, 400, { error: "floor, ceiling and target are set for the system or a runner, not a repository or a session" }), true);
    const key = `${scope}:${scope === "repo" ? target.toLowerCase() : target}`;
    if (Object.keys(policy).length === 0) stored.delete(key);
    else stored.set(key, { scope, target, policy, updated_at: now(), updated_by: "user:ada" });
    ctx.json(res, 200, { scope: raw, policy, effective: view({ [scope]: target }).effective });
    return true;
  }
  return false;
}
