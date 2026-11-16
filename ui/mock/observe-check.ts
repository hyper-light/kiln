/**
 * kiln's monitor check (`POST /api/observe/v1/monitors/validate`), as far as an experiment's criteria
 * need it in the mock: a metric monitor's query in Datadog's syntax, its time aggregation and window,
 * its query (an aggregation of a metric, or a formula of several, with its scope and groups), its
 * comparator (kiln-obs monitor.rs `Op`: > >= == != < <=) and its threshold. The server's own check
 * (crates/kiln-obs/src/monitor.rs) reads every kind; this one answers a metric monitor's, or why not.
 */
import type http from "node:http";

type Ctx = { json: (res: http.ServerResponse, status: number, body: unknown) => void; body: (req: http.IncomingMessage) => Promise<unknown> };

const UNIT_MS: Record<string, number> = { s: 1_000, m: 60_000, h: 3_600_000, d: 86_400_000, w: 604_800_000 };

/** A metric monitor's query read, or what's wrong with it. */
export function checkMonitor(query: string): Record<string, unknown> {
  const m = /^\s*(avg|sum|min|max|last)\(\s*last_(\d+)([smhdw])\s*\)\s*:\s*(.+?)\s*(>=|<=|==|!=|>|<)\s*(-?\d+(?:\.\d+)?)\s*$/.exec(query);
  if (!m) {
    if (!/(>=|<=|==|!=|>|<)\s*-?\d/.test(query)) return { ok: false, error: "It ends in a comparison and a number: > 0.9, <= 250" };
    return { ok: false, error: "Datadog's monitor syntax: avg(last_14d):avg:a.metric{scope} by {group} < 250" };
  }
  const [, , n, unit, watched, op, threshold] = m;
  if (!/[a-z_][\w.]*\{[^}]*\}/i.test(watched!)) return { ok: false, error: "It names no metric with its scope: a.metric{scope}" };
  const by =
    /\bby\s*\{([^}]*)\}/
      .exec(watched!)?.[1]
      ?.split(",")
      .map((g) => g.trim())
      .filter(Boolean) ?? [];
  return { ok: true, kind: "metric", op, threshold: Number(threshold), window_ms: Number(n) * (UNIT_MS[unit!] ?? 0), watched: { query: watched, by } };
}

export async function handleObserveCheck(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  if (url.pathname !== "/api/observe/v1/monitors/validate" || req.method !== "POST") return false;
  const ask = (await ctx.body(req)) as { query?: string };
  ctx.json(res, 200, checkMonitor(String(ask.query ?? "")));
  return true;
}
