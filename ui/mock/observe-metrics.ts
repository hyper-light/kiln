/**
 * Observe's metric names and a metric's summary, as far as the metrics explorer's query field needs
 * them in the mock (views/observe/metrics.tsx: its completions):
 *
 *   GET /api/observe/v1/metrics[?match=&limit=]   {metrics: string[]}, names with `match` in them (`*` any run)
 *   GET /api/observe/v1/metrics/summary/:name     a metric's series and each tag's values of most series (api/observe.ts MetricSummary)
 *
 * The metrics are the ones the page's examples and kiln's monitors name; the series counts are made up.
 */
import type http from "node:http";

type Ctx = { json: (res: http.ServerResponse, status: number, body: unknown) => void };

/** Each metric's tags, each with its values and how many series have it. */
const METRICS: Record<string, Record<string, [string, number][]>> = {
  "app.latency.p95": {
    service: [
      ["kiln-server", 4],
      ["kiln-runner", 9],
      ["hex-node", 6],
    ],
    env: [
      ["prod", 12],
      ["staging", 7],
    ],
  },
  "kiln.ci.job.duration": {
    repo: [
      ["hyper-light/kiln", 412],
      ["hyper-light/shards", 233],
      ["hyper-light/hyper-raft", 87],
    ],
    status: [
      ["succeeded", 590],
      ["failed", 55],
      ["cancelled", 87],
    ],
  },
  "kiln.ci.jobs.finished": {
    status: [
      ["succeeded", 590],
      ["failed", 55],
      ["cancelled", 87],
    ],
    runner: [
      ["metal-ryzen-01", 220],
      ["metal-epyc-01", 341],
      ["adacachy", 171],
    ],
  },
  "kiln.session.failed_pct": {
    experiment: [
      ["exp-reviewer-size", 3],
      ["exp-pr-triage", 3],
    ],
    variant: [
      ["5e1c", 2],
      ["a37d", 2],
      ["0b9e", 1],
    ],
  },
  "kiln.session.peak_memory_gib": {
    experiment: [
      ["exp-reviewer-size", 3],
      ["exp-pr-triage", 3],
    ],
    variant: [
      ["5e1c", 2],
      ["a37d", 2],
      ["0b9e", 1],
    ],
  },
  "kiln.session.warm_ms": {
    experiment: [
      ["exp-reviewer-size", 3],
      ["exp-pr-triage", 3],
    ],
    variant: [
      ["5e1c", 2],
      ["a37d", 2],
      ["0b9e", 1],
    ],
  },
  "system.cpu.user": {
    host: [
      ["metal-ryzen-01", 1],
      ["metal-epyc-01", 1],
      ["adacachy", 1],
      ["adas-macbook-pro-2", 1],
    ],
  },
  "system.disk.in_use": {
    host: [
      ["metal-ryzen-01", 2],
      ["metal-epyc-01", 3],
      ["adacachy", 2],
    ],
    mount: [
      ["/", 4],
      ["/data", 3],
    ],
  },
  "system.mem.used": {
    host: [
      ["metal-ryzen-01", 1],
      ["metal-epyc-01", 1],
      ["adacachy", 1],
      ["adas-macbook-pro-2", 1],
    ],
  },
};

/** A metric's summary: its series (the most any of its tags has), one kind, each tag's values of most series first. */
function summary(name: string, tags: Record<string, [string, number][]>) {
  const keys = Object.entries(tags).map(([key, values]) => ({
    key,
    values: values.length,
    series: values.reduce((n, [, s]) => n + s, 0),
    top: [...values].sort((a, b) => b[1] - a[1]).map(([value, series]) => ({ value, series })),
  }));
  const series = Math.max(1, ...keys.map((k) => k.series));
  return { name, series, kinds: [{ kind: "gauge", series }], tags: keys.sort((a, b) => b.values - a.values), samples: series * 1440, samples_per_s: null, last_ms: Date.now() - 10_000 };
}

export function handleObserveMetrics(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): boolean {
  if (req.method !== "GET") return false;
  if (url.pathname === "/api/observe/v1/metrics") {
    const match = (url.searchParams.get("match") ?? "").toLowerCase();
    const parts = match.split("*").filter(Boolean);
    const limit = Number(url.searchParams.get("limit") ?? 200) || 200;
    const metrics = Object.keys(METRICS)
      .filter((m) => parts.every((p) => m.toLowerCase().includes(p)))
      .slice(0, limit);
    ctx.json(res, 200, { metrics });
    return true;
  }
  const one = /^\/api\/observe\/v1\/metrics\/summary\/(.+)$/.exec(url.pathname);
  if (one) {
    const name = decodeURIComponent(one[1]!);
    const tags = METRICS[name];
    if (!tags) ctx.json(res, 404, { error: `no metric ${name}` });
    else ctx.json(res, 200, summary(name, tags));
    return true;
  }
  return false;
}
