/**
 * Insights (docs/ci.md W19-R01, W21-R12): where CI time goes, over a window. The overview holds
 * repositories (their share of runner time against their fair-share weight's) and runners
 * (utilization); a repository, its workflows; a workflow, its durations, success, waiting by
 * reason, jobs, slowest steps, executors and pools, cache and critical path; a runner, its
 * utilization and failures. Each chart carries one measure in one hue (p90 is the darker step of
 * p50's, an ordinal pair validated against the chart surface), with its numbers in a table too.
 */
import * as React from "react";
import { ci, type CiRunnerInsight } from "@/api/ci";
import { Chart } from "@/components/charts/chart";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, EmptyState, Facts, Meter, Panel, Sparkline } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { Tip } from "@/components/ui/tooltip";
import { formatBytes, formatDuration, formatNumber } from "@/lib/format";
import { Link, useSearchParams } from "@/lib/router";
import { useSuspenseResource } from "@/lib/resource";

type Window = "7" | "30" | "90";
const WINDOWS: { value: Window; label: string }[] = [
  { value: "7", label: "7 days" },
  { value: "30", label: "30 days" },
  { value: "90", label: "90 days" },
];

/** A link inside a sentence, as the metrics page's. */
const INLINE = "text-[#d1c6e6] underline decoration-[#635b73] underline-offset-4 hover:text-[#f1e7ff]";

/** One measure's hue, and its darker step for the tail (p90). */
const MEASURE = "#b8a6d5";
const TAIL = "#7f6ca6";

/** W05-R13's wait reasons, as the queue breakdown names them. */
const REASONS: Record<string, string> = {
  capacity: "Runners busy",
  "no-runner": "No runner connected",
  unroutable: "No runner could run it",
  capability: "Runners lack a capability",
  concurrency: "Concurrency group",
  quota: "Quota",
  exclusive: "Exclusive",
  retry: "Retry backoff",
  approval: "Approval",
  "wait-timer": "Wait timer",
  scheduling: "Being scheduled",
};

const pct = (v: number | null | undefined) => (v === null || v === undefined ? "—" : v > 0 && v < 0.1 ? `${(v * 100).toFixed(1)}%` : `${Math.round(v * 100)}%`);
const minutes = (m: number) => formatDuration(m * 60);

function money(v: number | null, currency: string) {
  if (v === null) return "—";
  return new Intl.NumberFormat(undefined, currency ? { style: "currency", currency, maximumFractionDigits: 2 } : { maximumFractionDigits: 2 }).format(v);
}

const insightsTo = (p: Record<string, string>) => {
  const q = new URLSearchParams(Object.entries(p).filter(([k, v]) => v !== "" && !(k === "days" && v === "30")));
  return `/insights${q.size ? `?${q}` : ""}`;
};

export default function Insights() {
  const [search, setSearch] = useSearchParams();
  const repo = search.get("repo") ?? "";
  const workflow = search.get("workflow") ?? "";
  const runner = search.get("runner") ?? "";
  const win = (WINDOWS.find((w) => w.value === search.get("days"))?.value ?? "30") as Window;
  const days = Number(win);
  const setWindow = (w: Window) => setSearch(Object.fromEntries(Object.entries({ repo, workflow, runner, days: w }).filter(([k, v]) => v !== "" && !(k === "days" && v === "30"))), { replace: true });
  const crumbs: { label: React.ReactNode; to?: string }[] = [
    { label: "Runs", to: "/runs" },
    { label: "Insights", to: insightsTo({ days: win }) },
  ];
  if (repo) crumbs.push({ label: repo, to: workflow ? insightsTo({ repo, days: win }) : undefined });
  if (repo && workflow) crumbs.push({ label: workflow.split("/").pop() ?? workflow });
  if (runner) crumbs.push({ label: runner });
  const title = runner || (workflow ? (workflow.split("/").pop() ?? workflow) : repo) || "Insights";
  return (
    <div className="container-console page">
      <title>{`${title === "Insights" ? "" : `${title} · `}Insights · kiln`}</title>
      <Crumbs index={2} items={crumbs} />
      <h1 className="mt-5 text-[clamp(26px,3vw,40px)] leading-[1.1] font-[450] tracking-[-0.04em] break-words">{title}</h1>
      <p className="mt-3 max-w-2xl text-[14px] leading-[1.7] text-[#b0b4be]">
        {runner
          ? "How busy this runner was, and how its legs ended."
          : workflow
            ? "How long this workflow takes, how often it passes, and where its time goes."
            : repo
              ? "This repository's workflows, and its share of runner time."
              : "Where CI time goes: repositories and runners, rolled up by the hour."}
      </p>
      <div className="mt-6">
        <Segmented label="Window" value={win} onChange={setWindow} options={WINDOWS} />
      </div>
      <Loadable label="insights" fallback={<Hold height={480} className="mt-6" />}>
        {runner ? (
          <RunnerInsights runner={runner} days={days} />
        ) : repo && workflow ? (
          <WorkflowInsights repo={repo} workflow={workflow} days={days} />
        ) : repo ? (
          <RepoInsights repo={repo} days={days} />
        ) : (
          <Overview days={days} />
        )}
      </Loadable>
    </div>
  );
}

function Overview({ days }: { days: number }) {
  const win = String(days);
  const repos = useSuspenseResource(`insights/repos/${days}`, (s) => ci.insightsRepos(days, s)).data;
  const runners = useSuspenseResource(`insights/runners/${days}`, (s) => ci.insightsRunners(days, s)).data;
  return (
    <div className="mt-6 grid gap-6">
      <Panel title="Repositories" description="Runs, and each repository's share of runner time beside the share its fair-share weight gives it.">
        {repos.repos.length === 0 ? (
          <EmptyState title="No runs in this window." />
        ) : (
          <div className="table-wrap">
            <table className="data-table">
              <thead>
                <tr>
                  <th>Repository</th>
                  <th className="num">Runs</th>
                  <th className="num">Success</th>
                  <th className="num hidden sm:table-cell">p50</th>
                  <th className="num hidden md:table-cell">p90</th>
                  <th className="num hidden md:table-cell">Runner time</th>
                  <th className="num">Share</th>
                  <th className="num hidden lg:table-cell">Fair share</th>
                </tr>
              </thead>
              <tbody>
                {repos.repos.map((r) => (
                  <tr key={r.id}>
                    <td className="max-w-[280px] truncate">
                      <Link to={insightsTo({ repo: r.name ?? r.id, days: win })} className="text-link">
                        {r.name ?? r.id}
                      </Link>
                    </td>
                    <td className="num cell-mono">{formatNumber(r.runs.count)}</td>
                    <td className="num cell-mono">{pct(r.runs.success_rate)}</td>
                    <td className="num cell-mono hidden sm:table-cell">{formatDuration(r.runs.p50)}</td>
                    <td className="num cell-mono hidden md:table-cell">{formatDuration(r.runs.p90)}</td>
                    <td className="num cell-mono hidden md:table-cell">{minutes(r.runner_minutes)}</td>
                    <td className="num cell-mono">{pct(r.share)}</td>
                    <td className="num cell-mono hidden lg:table-cell">
                      {pct(r.weight_share)}
                      <span className="ml-1.5 text-[#7b7e88]">weight {r.weight}</span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
      <Panel title="Runners" description="Utilization is busy slot-time over connected slot-time.">
        <RunnersTable runners={runners.runners} currency={runners.currency} days={days} />
      </Panel>
    </div>
  );
}

function RunnersTable({ runners, currency, days }: { runners: CiRunnerInsight[]; currency: string; days: number }) {
  if (runners.length === 0) return <EmptyState title="No runner time in this window." />;
  const costs = runners.some((r) => r.cost !== null);
  return (
    <div className="table-wrap">
      <table className="data-table">
        <thead>
          <tr>
            <th>Runner</th>
            <th className="hidden md:table-cell">Pool</th>
            <th>Utilization</th>
            <th className="num hidden sm:table-cell">Busy</th>
            <th className="num">Legs</th>
            <th className="num hidden md:table-cell">Success</th>
            <th className="hidden lg:table-cell">Failures</th>
            {costs && <th className="num hidden lg:table-cell">Cost</th>}
          </tr>
        </thead>
        <tbody>
          {runners.map((r) => (
            <tr key={r.runner}>
              <td className="max-w-[220px] truncate">
                <Link to={insightsTo({ runner: r.runner, days: String(days) })} className="text-link">
                  {r.runner}
                </Link>
              </td>
              <td className="cell-mono hidden md:table-cell">{r.pool}</td>
              <td className="min-w-[110px]">
                <div className="font-mono text-[11px] text-[#c8cad0] tabular-nums">{pct(r.utilization)}</div>
                {r.utilization !== null && (
                  <div className="mt-1 w-20">
                    <Meter value={r.utilization * 100} label={`${r.runner} utilization`} />
                  </div>
                )}
              </td>
              <td className="num cell-mono hidden sm:table-cell">{minutes(r.busy_minutes)}</td>
              <td className="num cell-mono">{formatNumber(r.legs.count)}</td>
              <td className="num cell-mono hidden md:table-cell">{pct(r.legs.success_rate)}</td>
              <td className="cell-mono hidden lg:table-cell">{classes(r.legs.failures)}</td>
              {costs && <td className="num cell-mono hidden lg:table-cell">{money(r.cost, currency)}</td>}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** Failure classes, most first: "failure 12 · runner-lost 2". */
function classes(f: Record<string, number>) {
  const list = Object.entries(f).sort((a, b) => b[1] - a[1]);
  return list.length ? list.map(([k, n]) => `${k} ${n}`).join(" · ") : "—";
}

function RepoInsights({ repo, days }: { repo: string; days: number }) {
  const d = useSuspenseResource(`insights/repo/${repo}/${days}`, (s) => ci.insightsRepo(repo, days, s)).data;
  const win = String(days);
  const share = d.share;
  return (
    <div className="mt-6 grid gap-6">
      {share && (
        <Facts
          items={[
            { label: "Runs", value: formatNumber(share.runs.count) },
            { label: "Success", value: pct(share.runs.success_rate) },
            { label: "Runner time", value: minutes(share.runner_minutes) },
            { label: "Share of runner time", value: pct(share.share) },
            { label: `Fair share (weight ${share.weight})`, value: pct(share.weight_share) },
          ]}
        />
      )}
      <Panel title="Workflows" description="Each workflow's runs; the trend is its median duration by day.">
        {d.workflows.length === 0 ? (
          <EmptyState title="No runs in this window." />
        ) : (
          <div className="table-wrap">
            <table className="data-table">
              <thead>
                <tr>
                  <th>Workflow</th>
                  <th className="num">Runs</th>
                  <th className="num">Success</th>
                  <th className="num hidden md:table-cell">Flaky</th>
                  <th className="num hidden sm:table-cell">p50</th>
                  <th className="num hidden md:table-cell">p90</th>
                  <th className="hidden lg:table-cell">Trend</th>
                </tr>
              </thead>
              <tbody>
                {[...d.workflows]
                  .sort((a, b) => b.count - a.count)
                  .map((w) => (
                    <tr key={w.workflow}>
                      <td className="max-w-[320px]">
                        <Link to={insightsTo({ repo, workflow: w.workflow, days: win })} className="text-link block truncate">
                          {w.name || w.workflow}
                        </Link>
                        {w.name && <div className="truncate font-mono text-[10px] text-[#7b7e88]">{w.workflow}</div>}
                      </td>
                      <td className="num cell-mono">{formatNumber(w.count)}</td>
                      <td className="num cell-mono">{pct(w.success_rate)}</td>
                      <td className="num cell-mono hidden md:table-cell">{pct(w.flaky_rate)}</td>
                      <td className="num cell-mono hidden sm:table-cell">{formatDuration(w.p50)}</td>
                      <td className="num cell-mono hidden md:table-cell">{formatDuration(w.p90)}</td>
                      <td className="hidden lg:table-cell">
                        <Sparkline values={w.series.map((p) => p.p50 ?? 0)} width={96} height={22} stroke={MEASURE} />
                      </td>
                    </tr>
                  ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </div>
  );
}

/** Every bucket of the window, oldest first, so a chart spans all of it (buckets without data are gaps), and its time labels. */
function axis(since: number, grain: "hour" | "day") {
  const step = grain === "hour" ? 3600 : 86400;
  const xs: number[] = [];
  for (let b = since - (since % step); b <= Date.now() / 1000; b += step) xs.push(b);
  return { xs, timeFormat: grain === "hour" ? ("hour" as const) : ("day" as const) };
}

/** `f` of the point in each bucket of `xs`, null where there's none. */
function along<T extends { bucket: number }>(xs: number[], points: T[], f: (p: T) => number | null): (number | null)[] {
  const at = new Map(points.map((p) => [p.bucket, p]));
  return xs.map((x) => {
    const p = at.get(x);
    return p ? f(p) : null;
  });
}

function WorkflowInsights({ repo, workflow, days }: { repo: string; workflow: string; days: number }) {
  const d = useSuspenseResource(`insights/workflow/${repo}/${workflow}/${days}`, (s) => ci.insightsWorkflow(repo, workflow, days, s)).data;
  if (d.count === 0 && d.jobs.length === 0) {
    return (
      <div className="mt-8">
        <EmptyState title="No runs of this workflow in this window." />
      </div>
    );
  }
  const { xs, timeFormat } = axis(d.since, d.grain);
  // Legs' time: the jobs' together (runs' own queue is only their start).
  const legs = d.jobs.reduce<{ queue: number; run: number; waits: Record<string, number> }>(
    (a, j) => {
      for (const [k, v] of Object.entries(j.waits)) a.waits[k] = (a.waits[k] ?? 0) + v;
      return { queue: a.queue + j.queue_seconds, run: a.run + j.run_seconds, waits: a.waits };
    },
    { queue: 0, run: 0, waits: {} },
  );
  const reasons = Object.entries(legs.waits)
    .filter(([, v]) => v > 0)
    .sort((a, b) => b[1] - a[1]);
  // Legs' queue and run time by bucket, for the share they waited.
  const byBucket = new Map<number, { queue: number; run: number }>();
  for (const j of d.jobs) for (const p of j.series) byBucket.set(p.bucket, { queue: (byBucket.get(p.bucket)?.queue ?? 0) + p.queue_seconds, run: (byBucket.get(p.bucket)?.run ?? 0) + p.run_seconds });
  const waits = [...byBucket.entries()].sort((a, b) => a[0] - b[0]).map(([bucket, t]) => ({ bucket, ...t, share: t.queue + t.run > 0 ? t.queue / (t.queue + t.run) : null }));
  const costs = d.executors.some((e) => e.cost !== null);
  return (
    <div className="mt-6 grid gap-6">
      <Facts
        items={[
          { label: "Runs", value: formatNumber(d.count) },
          { label: "Success", value: pct(d.success_rate) },
          { label: "Flaky", value: pct(d.flaky_rate) },
          { label: "p50", value: formatDuration(d.p50) },
          { label: "p90", value: formatDuration(d.p90) },
          { label: "Legs waited", value: pct(legs.queue + legs.run > 0 ? legs.queue / (legs.queue + legs.run) : null) },
        ]}
      />
      {d.critical_path && d.critical_path.jobs.length > 0 && (
        <p className="text-[13px] leading-[1.7] text-[#b0b4be]">
          The longest chain of the latest run,{" "}
          <Link to={`/runs/${encodeURIComponent(d.critical_path.run)}`} className={INLINE}>
            #{d.critical_path.number}
          </Link>
          : <span className="font-mono text-[12px] text-[#e4e5e7]">{d.critical_path.jobs.join(" → ")}</span>, {formatDuration(d.critical_path.seconds)}.
        </p>
      )}
      <div className="grid gap-6 lg:grid-cols-2">
        <Panel title="Duration" description="Runs' median and 90th percentile.">
          <Chart
            data={[xs, along(xs, d.series, (p) => p.p50), along(xs, d.series, (p) => p.p90)]}
            series={[
              { label: "p50", color: MEASURE, width: 2, points: true },
              { label: "p90", color: TAIL, width: 2, dash: [5, 4], points: true },
            ]}
            height={180}
            timeFormat={timeFormat}
            yFormat={(v) => formatDuration(v)}
            ariaLabel="Run duration, median and 90th percentile"
          />
          <SeriesTable
            points={d.series}
            grain={d.grain}
            cols={[
              ["Runs", (p) => formatNumber(p.count)],
              ["p50", (p) => formatDuration(p.p50)],
              ["p90", (p) => formatDuration(p.p90)],
            ]}
          />
        </Panel>
        <Panel title="Success" description="Of the runs that passed or failed, the share that passed.">
          <Chart
            data={[xs, along(xs, d.series, (p) => p.success_rate)]}
            series={[{ label: "Success", color: MEASURE, width: 2, points: true }]}
            height={180}
            timeFormat={timeFormat}
            yRange={[0, 1]}
            yFormat={(v) => pct(v)}
            ariaLabel="Run success rate"
          />
          <SeriesTable
            points={d.series}
            grain={d.grain}
            cols={[
              ["Runs", (p) => formatNumber(p.count)],
              ["Passed", (p) => formatNumber(p.success)],
              ["Failed", (p) => formatNumber(p.failure)],
              ["Success", (p) => pct(p.success_rate)],
            ]}
          />
        </Panel>
        <Panel title="Waiting" description="Of legs' time, the share they spent queued rather than running.">
          <Chart
            data={[xs, along(xs, waits, (w) => w.share)]}
            series={[{ label: "Waiting", color: MEASURE, width: 2, points: true }]}
            height={180}
            timeFormat={timeFormat}
            yRange={[0, 1]}
            yFormat={(v) => pct(v)}
            ariaLabel="Share of legs' time spent waiting"
          />
          <SeriesTable
            points={waits}
            grain={d.grain}
            cols={[
              ["Queued", (w) => formatDuration(w.queue)],
              ["Running", (w) => formatDuration(w.run)],
              ["Waiting", (w) => pct(w.share)],
            ]}
          />
        </Panel>
        <Panel title="Why legs waited" description="Queue time by wait reason, over the window.">
          {reasons.length === 0 ? <p className="text-[13px] text-[#7b7e88]">No waiting recorded.</p> : <Bars items={reasons} total={legs.queue} />}
        </Panel>
      </div>
      <Panel title="Jobs">
        <div className="table-wrap">
          <table className="data-table">
            <thead>
              <tr>
                <th>Job</th>
                <th className="num">Legs</th>
                <th className="num">Success</th>
                <th className="num hidden sm:table-cell">Flaky</th>
                <th className="num hidden lg:table-cell">Retried</th>
                <th className="num">p50</th>
                <th className="num hidden md:table-cell">p90</th>
                <th className="num hidden md:table-cell">Queued (p50)</th>
                <th className="hidden xl:table-cell">Waited most on</th>
                <th className="num hidden xl:table-cell">CPU</th>
                <th className="num hidden xl:table-cell">Peak memory</th>
                <th className="num hidden lg:table-cell">Runner time</th>
              </tr>
            </thead>
            <tbody>
              {d.jobs.map((j) => {
                const top = Object.entries(j.waits).sort((a, b) => b[1] - a[1])[0];
                return (
                  <tr key={j.job}>
                    <td className="max-w-[220px] truncate font-mono text-[12px] text-[#e4e5e7]">{j.job}</td>
                    <td className="num cell-mono">{formatNumber(j.count)}</td>
                    <td className="num cell-mono">{pct(j.success_rate)}</td>
                    <td className="num cell-mono hidden sm:table-cell">{pct(j.flaky_rate)}</td>
                    <td className="num cell-mono hidden lg:table-cell">{formatNumber(j.retried)}</td>
                    <td className="num cell-mono">{formatDuration(j.p50)}</td>
                    <td className="num cell-mono hidden md:table-cell">{formatDuration(j.p90)}</td>
                    <td className="num cell-mono hidden md:table-cell">{formatDuration(j.queue_p50)}</td>
                    <td className="cell-mono hidden xl:table-cell">{top && top[1] > 0 ? `${REASONS[top[0]] ?? top[0]} · ${formatDuration(top[1])}` : "—"}</td>
                    <td className="num cell-mono hidden xl:table-cell">{j.cpu_seconds > 0 ? formatDuration(j.cpu_seconds) : "—"}</td>
                    <td className="num cell-mono hidden xl:table-cell">{j.peak_rss > 0 ? formatBytes(j.peak_rss) : "—"}</td>
                    <td className="num cell-mono hidden lg:table-cell">{minutes(j.runner_minutes)}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </Panel>
      {d.steps.length > 0 && (
        <Panel title="Slowest steps" description="By median duration.">
          <div className="table-wrap">
            <table className="data-table">
              <thead>
                <tr>
                  <th className="hidden sm:table-cell">Job</th>
                  <th>Step</th>
                  <th className="num hidden md:table-cell">Runs</th>
                  <th className="num">p50</th>
                  <th className="num hidden sm:table-cell">p90</th>
                  <th className="num hidden md:table-cell">Total</th>
                </tr>
              </thead>
              <tbody>
                {d.steps.map((s) => (
                  <tr key={`${s.job}\u0000${s.step}`}>
                    <td className="max-w-[160px] truncate font-mono text-[12px] hidden sm:table-cell">{s.job}</td>
                    <td className="max-w-[360px] truncate max-sm:max-w-[190px]">{s.step}</td>
                    <td className="num cell-mono hidden md:table-cell">{formatNumber(s.count)}</td>
                    <td className="num cell-mono">{formatDuration(s.p50)}</td>
                    <td className="num cell-mono hidden sm:table-cell">{formatDuration(s.p90)}</td>
                    <td className="num cell-mono hidden md:table-cell">{formatDuration(s.total)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </Panel>
      )}
      <div className="grid gap-6 lg:grid-cols-2">
        {d.executors.length > 0 && (
          <Panel title="Executors and pools" description="Where its legs ran, and what they used.">
            <div className="table-wrap">
              <table className="data-table">
                <thead>
                  <tr>
                    <th>Executor</th>
                    <th>Pool</th>
                    <th className="num">Legs</th>
                    <th className="num">Runner time</th>
                    <th className="num hidden sm:table-cell">CPU</th>
                    <th className="num hidden md:table-cell">Peak memory</th>
                    {costs && <th className="num">Cost</th>}
                  </tr>
                </thead>
                <tbody>
                  {d.executors.map((e) => (
                    <tr key={`${e.executor}\u0000${e.pool}`}>
                      <td className="cell-mono">{e.executor}</td>
                      <td className="cell-mono">{e.pool}</td>
                      <td className="num cell-mono">{formatNumber(e.legs)}</td>
                      <td className="num cell-mono">{minutes(e.runner_minutes)}</td>
                      <td className="num cell-mono hidden sm:table-cell">{e.cpu_seconds > 0 ? formatDuration(e.cpu_seconds) : "—"}</td>
                      <td className="num cell-mono hidden md:table-cell">{e.peak_rss > 0 ? formatBytes(e.peak_rss) : "—"}</td>
                      {costs && <td className="num cell-mono">{money(e.cost, d.currency)}</td>}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </Panel>
        )}
        {d.cache.length > 0 && (
          <Panel title="Cache" description="Lookups by key prefix. Time saved compares a job's legs that hit with those that missed.">
            <div className="table-wrap">
              <table className="data-table">
                <thead>
                  <tr>
                    <th>Key prefix</th>
                    <th className="num">Hit rate</th>
                    <th className="num hidden sm:table-cell">Hits</th>
                    <th className="num hidden sm:table-cell">Misses</th>
                    <th className="num hidden md:table-cell">Restored</th>
                    <th className="num hidden md:table-cell">Saved</th>
                    <th className="num">Time saved</th>
                  </tr>
                </thead>
                <tbody>
                  {d.cache.map((c) => (
                    <tr key={c.prefix}>
                      <td className="max-w-[220px] truncate font-mono text-[12px]">{c.prefix}</td>
                      <td className="num cell-mono">{pct(c.hit_rate)}</td>
                      <td className="num cell-mono hidden sm:table-cell">{formatNumber(c.hits)}</td>
                      <td className="num cell-mono hidden sm:table-cell">{formatNumber(c.misses)}</td>
                      <td className="num cell-mono hidden md:table-cell">{formatBytes(c.bytes_restored)}</td>
                      <td className="num cell-mono hidden md:table-cell">{formatBytes(c.bytes_saved)}</td>
                      <td className="num cell-mono">{c.seconds_saved === null ? "—" : formatDuration(c.seconds_saved)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </Panel>
        )}
      </div>
    </div>
  );
}

/** Wait reasons as bars of one hue, longest first, each with its time and share. */
function Bars({ items, total }: { items: [string, number][]; total: number }) {
  const max = Math.max(1, ...items.map(([, v]) => v));
  return (
    <ul className="grid gap-2.5">
      {items.map(([k, v]) => (
        <li key={k}>
          <Tip content={`${REASONS[k] ?? k}: ${formatDuration(v)}, ${pct(total > 0 ? v / total : null)} of the waiting`}>
            <div tabIndex={0} className="grid grid-cols-[minmax(8rem,11rem)_1fr_auto] items-center gap-3 rounded-[4px] py-0.5 outline-none focus-visible:ring-2 focus-visible:ring-[#bab0e3]">
              <span className="truncate text-[12.5px] text-[#c8cad0]">{REASONS[k] ?? k}</span>
              <span className="h-2.5">
                <span className="block h-full rounded-r-[4px]" style={{ width: `${Math.max(1, (v / max) * 100)}%`, background: MEASURE }} />
              </span>
              <span className="font-mono text-[11px] text-[#a7aab3] tabular-nums">{formatDuration(v)}</span>
            </div>
          </Tip>
        </li>
      ))}
    </ul>
  );
}

/** A chart's numbers, as a table, behind a disclosure. */
function SeriesTable<T extends { bucket: number }>({ points, grain, cols }: { points: T[]; grain: "hour" | "day"; cols: [string, (p: T) => string][] }) {
  if (points.length === 0) return null;
  const when = (b: number) =>
    new Date(b * 1000).toLocaleString(undefined, grain === "hour" ? { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" } : { month: "short", day: "numeric" });
  return (
    <details className="mt-3">
      <summary className="cursor-pointer font-mono text-[11px] text-[#9499a5]">As a table</summary>
      <div className="table-wrap mt-2">
        <table className="data-table">
          <thead>
            <tr>
              <th>{grain === "hour" ? "Hour" : "Day"}</th>
              {cols.map(([c]) => (
                <th key={c} className="num">
                  {c}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {points.map((p) => (
              <tr key={p.bucket}>
                <td className="cell-mono">{when(p.bucket)}</td>
                {cols.map(([c, f]) => (
                  <td key={c} className="num cell-mono">
                    {f(p)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </details>
  );
}

function RunnerInsights({ runner, days }: { runner: string; days: number }) {
  const d = useSuspenseResource(`insights/runner/${runner}/${days}`, (s) => ci.insightsRunner(runner, days, s)).data;
  const { xs, timeFormat } = axis(d.since, d.grain);
  const fails = Object.entries(d.legs.failures).sort((a, b) => b[1] - a[1]);
  return (
    <div className="mt-6 grid gap-6">
      <Facts
        items={[
          { label: "Utilization", value: pct(d.utilization) },
          { label: "Busy", value: minutes(d.busy_minutes) },
          { label: "Connected", value: minutes(d.connected_minutes) },
          { label: "Legs", value: formatNumber(d.legs.count) },
          { label: "Success", value: pct(d.legs.success_rate) },
          ...(d.cost !== null ? [{ label: "Cost", value: money(d.cost, d.currency) }] : []),
        ]}
      />
      <div className="grid gap-6 lg:grid-cols-2">
        <Panel title="Utilization" description="Busy slot-time over connected slot-time.">
          <Chart
            data={[xs, along(xs, d.series, (p) => p.utilization)]}
            series={[{ label: "Utilization", color: MEASURE, width: 2, points: true }]}
            height={180}
            timeFormat={timeFormat}
            yRange={[0, 1]}
            yFormat={(v) => pct(v)}
            ariaLabel={`${runner} utilization`}
          />
          <SeriesTable
            points={d.series}
            grain={d.grain}
            cols={[
              ["Utilization", (p) => pct(p.utilization)],
              ["Busy", (p) => minutes(p.busy_minutes)],
              ["Legs", (p) => formatNumber(p.count)],
              ["Failed", (p) => formatNumber(p.failure)],
            ]}
          />
        </Panel>
        <Panel title="How its legs failed" description="By failure class.">
          {fails.length === 0 ? (
            <p className="text-[13px] text-[#7b7e88]">No failures in this window.</p>
          ) : (
            <ul className="grid gap-2">
              {fails.map(([k, n]) => (
                <li key={k} className="flex items-baseline justify-between gap-3 border-b border-[#1d1f23] pb-2 text-[12.5px]">
                  <span className="font-mono text-[#c8cad0]">{k}</span>
                  <span className="font-mono text-[11px] text-[#a7aab3] tabular-nums">{formatNumber(n)}</span>
                </li>
              ))}
            </ul>
          )}
        </Panel>
      </div>
      <Panel title="Repositories" description="Whose legs it ran.">
        {d.repos.length === 0 ? (
          <EmptyState title="No legs in this window." />
        ) : (
          <div className="table-wrap">
            <table className="data-table">
              <thead>
                <tr>
                  <th>Repository</th>
                  <th className="num">Legs</th>
                  <th className="num">Runner time</th>
                  <th className="num">Failed</th>
                </tr>
              </thead>
              <tbody>
                {[...d.repos]
                  .sort((a, b) => b.runner_minutes - a.runner_minutes)
                  .map((r) => (
                    <tr key={r.id}>
                      <td className="max-w-[280px] truncate">
                        <Link to={insightsTo({ repo: r.name ?? r.id, days: String(days) })} className="text-link">
                          {r.name ?? r.id}
                        </Link>
                      </td>
                      <td className="num cell-mono">{formatNumber(r.legs)}</td>
                      <td className="num cell-mono">{minutes(r.runner_minutes)}</td>
                      <td className="num cell-mono">{formatNumber(r.failure)}</td>
                    </tr>
                  ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </div>
  );
}
