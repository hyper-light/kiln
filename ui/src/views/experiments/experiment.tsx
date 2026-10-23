/**
 * An experiment (docs/ci.md W20-R11): what it asks, its criteria, and each variant: its options,
 * whether it was built, its required checks, and each metric against the baseline with its 95%
 * interval, on one scale. Then the options' main effects, the pairs' interactions, its runs, and
 * the recommendation; "Continue with this" on each variant once it's done.
 */
import * as React from "react";
import { experimentRes, experimentsApi, type ExBuilt, type Experiment, type ExRatio, type ExResults, type ExVariantResult } from "@/api/experiments";
import { Button } from "@/components/ui/button";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, PageIntro, Panel } from "@/components/ui/misc";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useParams } from "@/lib/router";
import { standing } from "./index";

export default function ExperimentPage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page">
      <title>{`${id} · Experiments · kiln`}</title>
      <Loadable label="the experiment" fallback={<RowsHold rows={4} rowHeight={72} label="Loading the experiment" />}>
        <Body id={id} />
      </Loadable>
    </div>
  );
}

const STRATEGY: Record<string, string> = { each: "each alone", all: "every combination", pairs: "each alone and every pair", greedy: "greedy stacking" };

function strategyWords(s: Experiment["strategy"]): string {
  if (!s) return "";
  return typeof s === "string" ? (STRATEGY[s] ?? s) : "the variants given";
}

function Body({ id }: { id: string }) {
  const e = useRes(experimentRes(id)).data;
  const [cancelling, setCancelling] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const running = e.state === "implementing" || e.state === "building" || e.state === "measuring";
  const measured = e.runs.filter((r) => r.conclusion).length;
  const planned = e.variants.filter((v) => v.state !== "conflict").length * e.criteria.filter((c) => c.kind !== "metric").length * e.repetitions;
  const cancel = async () => {
    setError(null);
    try {
      await experimentsApi.cancel(id);
      invalidate(`experiments/${id}`);
      invalidate("experiments");
      setCancelling(false);
    } catch (err) {
      setError(errorMessage(err));
    }
  };
  return (
    <>
      <PageIntro
        index={6}
        label="Experiments"
        title={`${e.id}.`}
        lead={
          <>
            {e.question ? <span className="block">{e.question}</span> : null}
            <span className="block">
              {e.options.map((o) => o.name).join(", ")} on <Link to={`/runs?repo=${encodeURIComponent(e.repo)}`}>{e.repo}</Link>, {strategyWords(e.strategy)}, {e.repetitions} times each.{" "}
              {e.state === "done" ? (e.results?.why ?? "") : e.state === "measuring" ? `${measured} of ${planned} runs measured.` : (e.why ?? standing({ ...e, measured, runs: planned }))}
            </span>
          </>
        }
        actions={
          running && !cancelling ? (
            <Button size="xs" variant="ghost" onClick={() => setCancelling(true)}>
              Cancel
            </Button>
          ) : null
        }
      />
      {cancelling && (
        <Confirm
          label={`Cancel ${e.id}`}
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" onClick={() => void cancel()}>
                Cancel it
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setCancelling(false)}>
                Keep it running
              </Button>
            </>
          }
        >
          No more of its runs start, and those running are cancelled; what was measured stays.
        </Confirm>
      )}
      {error && <p className="text-[12.5px] text-[#d1aaad]">{error}</p>}
      <div className="flex flex-col gap-4">
        <Variants e={e} />
        {e.results && <Effects r={e.results} />}
        <Runs e={e} />
        <Criteria e={e} />
      </div>
    </>
  );
}

/** One scale for every interval in the table: the baseline (1) and every bound, padded. */
function scaleOf(results: ExVariantResult[], metric: string): [number, number] {
  const bounds = results.flatMap((v) => v.metrics.filter((m) => m.name === metric && m.vs_baseline).flatMap((m) => [m.vs_baseline?.low ?? 1, m.vs_baseline?.high ?? 1]));
  const lo = Math.min(1, ...bounds);
  const hi = Math.max(1, ...bounds);
  const pad = (hi - lo) * 0.06 || 0.05;
  return [lo - pad, hi + pad];
}

const W = 168;
const H = 18;

/** A ratio's 95% interval against the baseline, on the table's scale: whiskers from low to high,
 * a dot at the estimate, and the baseline a hairline. Better than it in green, worse in red,
 * neither in grey, and always said in words beside it. */
function Beam({ r, scale, better }: { r: ExRatio; scale: [number, number]; better: "lower" | "higher" }) {
  const [lo, hi] = scale;
  const x = (v: number) => 4 + ((v - lo) / (hi - lo)) * (W - 8);
  const good = better === "lower" ? r.high < 1 : r.low > 1;
  const bad = better === "lower" ? r.low > 1 : r.high < 1;
  const tone = good ? "#a5c8b3" : bad ? "#d1aaad" : "#a6b2c6";
  return (
    <svg width={W} height={H} viewBox={`0 0 ${W} ${H}`} className="shrink-0" aria-hidden="true">
      <line x1={x(1)} x2={x(1)} y1={1} y2={H - 1} stroke="#3a3f48" strokeWidth={1} />
      <line x1={x(r.low)} x2={x(r.high)} y1={H / 2} y2={H / 2} stroke={tone} strokeWidth={2} strokeLinecap="round" />
      <circle cx={x(r.mid)} cy={H / 2} r={4} fill={tone} stroke="#0a0c0f" strokeWidth={2} />
    </svg>
  );
}

const ratioWords = (r: ExRatio) => `×${r.mid.toFixed(3)} (95% ${r.low.toFixed(3)} to ${r.high.toFixed(3)})`;

function Variants({ e }: { e: Experiment }) {
  const results = e.results?.variants ?? [];
  const metrics = [...new Set(e.criteria.filter((c) => c.kind === "metric").map((c) => c.metric ?? c.name))];
  const scales = Object.fromEntries(metrics.map((m) => [m, scaleOf(results, m)]));
  const recommended = e.results?.recommendation;
  return (
    <Panel
      eyebrow={`Variants · ${e.variants.length}`}
      title="What each measured"
      description="Each variant's required checks, and each metric against the baseline: its median, and the ratio's 95% interval."
      bodyClassName="px-0 pb-1"
    >
      <ul className="row-list" aria-label={`${e.id}'s variants`}>
        {e.variants.map((v) => (
          <VariantRow key={v.name} e={e} v={v} r={results.find((x) => x.name === v.name)} scales={scales} recommended={recommended === v.name} />
        ))}
      </ul>
    </Panel>
  );
}

function VariantRow({ e, v, r, scales, recommended }: { e: Experiment; v: ExBuilt; r?: ExVariantResult; scales: Record<string, [number, number]>; recommended: boolean }) {
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const chosen = e.decision?.variant === v.name;
  const decide = async () => {
    setBusy(true);
    setError(null);
    try {
      await experimentsApi.decide(e.id, v.name);
      invalidate(`experiments/${e.id}`);
      invalidate("experiments");
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };
  const checks = r ? Object.entries(r.checks) : [];
  return (
    <li className="grid gap-x-6 gap-y-2 px-5 py-3 md:grid-cols-[minmax(0,0.9fr)_minmax(0,0.8fr)_minmax(0,1.8fr)_auto]">
      <div className="flex min-w-0 flex-col gap-0.5">
        <span className="flex min-w-0 flex-wrap items-baseline gap-x-2">
          <code className="font-mono text-[12.5px] [overflow-wrap:anywhere] text-[#e4e5e7]">{v.name}</code>
          {recommended && <span className="font-mono text-[10px] tracking-[0.06em] text-[#a5c8b3] uppercase">recommended</span>}
          {chosen && <span className="font-mono text-[10px] tracking-[0.06em] text-[#e4e5e7] uppercase">chosen</span>}
        </span>
        <span className="font-mono text-[10.5px] [overflow-wrap:anywhere] text-[#8c8f98]">
          {v.state === "conflict" || v.state === "gluing"
            ? v.why
            : v.commit
              ? `${v.commit.slice(0, 12)}${v.glue ? ` · glued in ${v.glue}` : ""}${v.minutes ? ` · ${v.minutes.toFixed(1)} runner min` : ""}`
              : v.state}
        </span>
      </div>
      <div className="flex min-w-0 flex-col gap-0.5 text-[12px]">
        {v.state === "conflict" ? (
          <span className="text-[#8c8f98]">not measured</span>
        ) : checks.length ? (
          checks.map(([name, [passed, tried]]) => (
            <span key={name} className={passed === tried && tried > 0 ? "text-[#c5cdd8]" : "text-[#d1aaad]"}>
              {name}: {passed} of {tried} passed
            </span>
          ))
        ) : (
          <span className="text-[#8c8f98]">{r ? "no required checks" : "not measured yet"}</span>
        )}
      </div>
      <div className="flex min-w-0 flex-col gap-1">
        {(r?.metrics ?? []).map((m) => (
          <div key={m.name} className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-0.5">
            <span className="font-mono text-[11px] text-[#a6b2c6]">
              {m.name} {m.median != null ? m.median.toPrecision(4) : "—"}
            </span>
            {m.vs_baseline ? (
              <>
                <Beam r={m.vs_baseline} scale={scales[m.name] ?? [0.5, 1.5]} better={m.better} />
                <span className="font-mono text-[10.5px] text-[#8c8f98]">
                  {ratioWords(m.vs_baseline)} · {m.verdict}
                </span>
              </>
            ) : (
              <span className="font-mono text-[10.5px] text-[#8c8f98]">{v.options.length ? "no baseline to compare with" : "the baseline"}</span>
            )}
          </div>
        ))}
      </div>
      <div className="flex min-w-0 flex-col items-start gap-1">
        {e.state === "done" && v.commit && !chosen && (
          <Button size="xs" variant={recommended ? "secondary" : "ghost"} disabled={busy} onClick={() => void decide()}>
            Continue with this
          </Button>
        )}
        {error && <span className="text-[11.5px] text-[#d1aaad]">{error}</span>}
      </div>
    </li>
  );
}

/** Each option's main effect, and each pair's interaction: better together than apart, or not. */
function Effects({ r }: { r: ExResults }) {
  if (!r.effects.length && !r.interactions.length) return null;
  const pct = (g: number) => `${g >= 0 ? "" : "−"}${Math.abs(g * 100).toFixed(1)}%`;
  return (
    <Panel
      eyebrow="Effects"
      title="What each option does, and what pairs do together"
      description="A main effect is how much better the variants with an option are than those without; an interaction, a pair's gain less each one's."
      bodyClassName="px-5 pb-4"
    >
      <div className="grid gap-6 md:grid-cols-2">
        <ul className="flex flex-col gap-1.5" aria-label="Main effects">
          {r.effects.map((f) => (
            <li key={`${f.option}-${f.metric}`} className="flex min-w-0 flex-wrap gap-x-2 text-[12.5px] text-[#c5cdd8]">
              <code className="font-mono text-[#e4e5e7]">{f.option}</code>
              <span>
                {pct(f.gain)} {f.gain >= 0 ? "better" : "worse"} on {f.metric}
              </span>
            </li>
          ))}
        </ul>
        <ul className="flex flex-col gap-1.5" aria-label="Interactions">
          {r.interactions.map((i) => (
            <li key={`${i.a}-${i.b}-${i.metric}`} className="flex min-w-0 flex-wrap gap-x-2 text-[12.5px] text-[#c5cdd8]">
              <code className="font-mono text-[#e4e5e7]">
                {i.a} × {i.b}
              </code>
              <span className={i.kind === "synergistic" ? "text-[#a5c8b3]" : i.kind === "antagonistic" ? "text-[#d1aaad]" : undefined}>{i.kind}</span>
              <span className="text-[#8c8f98]">
                {pct(i.interaction)} on {i.metric}
              </span>
            </li>
          ))}
        </ul>
      </div>
    </Panel>
  );
}

const CONCLUSION_DOT: Record<string, string> = { success: "succeeded", failure: "failed", timed_out: "timed_out", cancelled: "cancelled", untestable: "skipped" };

/** Its runs, by variant, each in its round. */
function Runs({ e }: { e: Experiment }) {
  if (!e.runs.length) return null;
  return (
    <Panel eyebrow={`Runs · ${e.runs.length}`} title="Its runs" description="Every variant once a round, in a shuffled order each round." bodyClassName="px-5 pb-4">
      <ul className="flex flex-col gap-1.5" aria-label={`${e.id}'s runs`}>
        {e.variants
          .filter((v) => e.runs.some((r) => r.variant === v.name))
          .map((v) => (
            <li key={v.name} className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
              <code className="w-28 shrink-0 font-mono text-[11.5px] [overflow-wrap:anywhere] text-[#c5cdd8]">{v.name}</code>
              {e.runs
                .filter((r) => r.variant === v.name)
                .map((r, i) => (
                  <span key={i} className="flex items-center gap-1.5 font-mono text-[10.5px] text-[#8c8f98]" title={r.why ?? `${r.criterion}, round ${r.round + 1}: ${r.conclusion ?? "running"}`}>
                    <i className="dot" data-status={r.conclusion ? (CONCLUSION_DOT[r.conclusion] ?? "pending") : "running"} aria-hidden="true" />
                    {r.run ? (
                      <Link to={`/runs/${r.run}`} className="text-link">
                        {r.criterion} {r.round + 1}
                      </Link>
                    ) : (
                      <span>
                        {r.criterion} {r.round + 1}: {r.why ?? r.conclusion}
                      </span>
                    )}
                  </span>
                ))}
            </li>
          ))}
      </ul>
    </Panel>
  );
}

function Criteria({ e }: { e: Experiment }) {
  const threshold = (t: (typeof e.criteria)[number]["threshold"]) =>
    t?.better != null ? `, ${(t.better * 100).toFixed(0)}% better than the baseline` : t?.at_most != null ? `, at most ${t.at_most}` : t?.at_least != null ? `, at least ${t.at_least}` : "";
  return (
    <Panel eyebrow="Criteria" title="How it measures" bodyClassName="px-5 pb-4">
      <ul className="flex flex-col gap-1 text-[12.5px] text-[#c5cdd8]">
        {e.criteria.map((c) => (
          <li key={c.name}>
            <code className="font-mono text-[#e4e5e7]">{c.name}</code>:{" "}
            {c.kind === "pipeline" ? `${c.workflow} succeeds` : c.kind === "command" ? `\`${c.command}\` exits 0` : `${c.metric}, ${c.better ?? "lower"} is better${threshold(c.threshold)}`}
            {c.required ? " (required)" : " (an objective)"}
          </li>
        ))}
      </ul>
      <p className="mt-2 font-mono text-[10.5px] text-[#8c8f98]">
        From {e.base.slice(0, 12)} on {e.branch} · {e.options.map((o) => `${o.name}: ${o.given}`).join(" · ")} · started by {e.by} {relativeTime(e.created_at)}
        {e.decision ? ` · ${e.decision.variant} chosen by ${e.decision.by}` : ""}
      </p>
    </Panel>
  );
}
