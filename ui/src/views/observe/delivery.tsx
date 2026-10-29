/**
 * Delivery: DORA's four keys (docs/observe.md §10.7), Datadog's DORA Metrics. How often a service
 * deploys, how long a change takes to reach production, how often a deployment fails and how soon it
 * is put right; each beside the 2024 report's levels. From kiln CD's deploys and those sent to
 * Datadog's DORA API.
 */
import * as React from "react";
import { observeApi, type DoraDeployment, type DoraFailure, type DoraLevel, type DoraRemedyKind, type DoraSummary } from "@/api/observe";
import { Chart } from "@/components/charts/chart";
import { SERIES } from "@/components/charts/types";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, Panel, SelectField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { absoluteDate, formatDuration, relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { ObserveNav, useParam } from "./parts";

const DAY = 86_400_000;
const RANGES = { "7d": 7 * DAY, "30d": 30 * DAY, "90d": 90 * DAY } as const;
type Span = keyof typeof RANGES;
const isSpan = (s: string): s is Span => s in RANGES;

interface Narrowed {
  span: Span;
  service: string;
  env: string;
  team: string;
}

/** A search of the deployments' fields for what is narrowed to. */
function searchOf(n: Narrowed) {
  const q = (v: string) => (/^[\w.\-/:]+$/.test(v) ? v : `"${v.replace(/["\\]/g, "\\$&")}"`);
  return [n.service ? `service:${q(n.service)}` : "", n.env ? `@env:${q(n.env)}` : "", n.team ? `@team:${q(n.team)}` : ""].filter(Boolean).join(" ");
}

const key = (n: Narrowed) => `observe/dora/${n.span}/${n.service}/${n.env}/${n.team}`;

const summaryRes = (n: Narrowed): Res<DoraSummary> => ({
  key: `${key(n)}/summary`,
  fetch: (s) => {
    const to = Date.now();
    return observeApi.doraSummary({ from: to - RANGES[n.span], to, service: n.service || undefined, env: n.env || undefined, team: n.team || undefined }, s);
  },
  opts: { pollMs: 60_000 },
});

const deploymentsRes = (n: Narrowed): Res<DoraDeployment[]> => ({
  key: `${key(n)}/deployments`,
  fetch: (s) => {
    const to = Date.now();
    return observeApi.doraDeployments({ from: to - RANGES[n.span], to: to + 60_000, query: searchOf(n), limit: 200, sort: "-finished_at" }, s);
  },
  opts: { pollMs: 60_000 },
});

const failuresRes = (n: Narrowed): Res<DoraFailure[]> => ({
  key: `${key(n)}/failures`,
  fetch: (s) => {
    const to = Date.now();
    return observeApi.doraFailures({ from: to - RANGES[n.span], to: to + 60_000, query: searchOf(n), limit: 100, sort: "-started_at" }, s);
  },
  opts: { pollMs: 60_000 },
});

const LEVEL: Record<DoraLevel, string> = { elite: "Elite", high: "High", medium: "Medium", low: "Low" };

/** Each key's bands in the 2024 report: what each cluster's range is. */
const BANDS = {
  frequency: [
    ["elite", "several a day"],
    ["high", "daily to weekly"],
    ["medium", "weekly to monthly"],
    ["low", "monthly or less"],
  ],
  lead: [
    ["elite", "under a day"],
    ["high", "a day to a week"],
    ["medium", "a week to a month"],
    ["low", "a month or more"],
  ],
  recovery: [
    ["elite", "under an hour"],
    ["high", "under a day"],
    ["medium", "under a day"],
    ["low", "a week to a month"],
  ],
} as const satisfies Record<string, readonly (readonly [DoraLevel, string])[]>;

/** A rate of deployments, as read: a day, or a week when fewer. */
function frequencyText(perDay: number) {
  if (perDay >= 1) return `${perDay.toFixed(perDay >= 10 ? 0 : 1)} a day`;
  const perWeek = perDay * 7;
  return perWeek >= 1 ? `${perWeek.toFixed(1)} a week` : `${(perDay * 30).toFixed(1)} a month`;
}

const percent = (r: number | null) => (r === null ? "—" : `${Math.round(r * 1000) / 10}%`);

export default function Delivery() {
  const [span, setSpan] = useParam("range", "30d");
  const [service, setService] = useParam("service", "");
  const [env, setEnv] = useParam("env", "");
  const [team, setTeam] = useParam("team", "");
  const n: Narrowed = { span: isSpan(span) ? span : "30d", service, env, team };
  return (
    <div className="container-console page">
      <title>Delivery · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Delivery"
        lead="DORA's four keys: how often you deploy, how long a change takes to reach production, how often a deployment fails, and how soon a failure is put right. From kiln CD's deploys, and those another CD system sends to Datadog's DORA API."
      />
      <ObserveNav />
      <Loadable key={key(n)} label="the four keys" fallback={<Hold height={420} />}>
        <Keys n={n} onSpan={setSpan} onService={setService} onEnv={setEnv} onTeam={setTeam} />
      </Loadable>
      <Loadable key={`${key(n)}/list`} label="the deployments" fallback={<TableHold head={<DeploymentsHead />} rows={4} />}>
        <Deployments n={n} />
      </Loadable>
    </div>
  );
}

function Keys({ n, onSpan, onService, onEnv, onTeam }: { n: Narrowed; onSpan: (s: string) => void; onService: (s: string) => void; onEnv: (s: string) => void; onTeam: (s: string) => void }) {
  const s = useRes(summaryRes(n)).data;
  const all = (label: string, values: string[], value: string, set: (v: string) => void) =>
    values.length > 1 || value ? (
      <SelectField
        label={label}
        hideLabel
        value={value}
        onChange={set}
        options={[{ value: "", label: `Every ${label.toLowerCase()}` }, ...values.map((v) => ({ value: v, label: v }))]}
        className="min-w-[10rem]"
      />
    ) : null;
  const days = Math.round((s.to - s.from) / DAY);
  return (
    <>
      <div className="mb-6 flex flex-wrap items-center gap-3">
        <Segmented
          label="Range"
          value={n.span}
          onChange={onSpan}
          options={[
            { value: "7d", label: "7 days" },
            { value: "30d", label: "30 days" },
            { value: "90d", label: "90 days" },
          ]}
        />
        {all("Service", s.services, n.service, onService)}
        {all("Environment", s.envs, n.env, onEnv)}
        {all("Team", s.teams, n.team, onTeam)}
      </div>
      <section aria-label="The four keys" className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
        <Key
          title="Deployment frequency"
          value={s.deployments === 0 ? "None" : frequencyText(s.per_day)}
          said={`${s.deployments} ${s.deployments === 1 ? "deployment" : "deployments"} in ${days} days`}
          bands={BANDS.frequency}
          level={s.frequency_level}
        />
        <Key
          title="Change lead time"
          value={s.lead_time_s === null ? "—" : formatDuration(s.lead_time_s)}
          said={s.lead_deployments === 0 ? "no deployment's commits read" : `the median of ${s.lead_deployments} ${s.lead_deployments === 1 ? "deployment's" : "deployments'"}`}
          bands={BANDS.lead}
          level={s.lead_time_level}
        />
        <Key
          title="Change failure rate"
          value={percent(s.failure_rate)}
          said={`${s.change_failures} of ${s.deployments} failed`}
          bands={s.failure_rates.map((r) => [r.level, `${Math.round(r.rate * 100)}%`] as const)}
          level={null}
          note="Each cluster's own rate: the report's medium failed less often than its high, so no rate is given a level."
        />
        <Key
          title="Failed deployment recovery time"
          value={s.recovery_s === null ? "—" : formatDuration(s.recovery_s)}
          said={s.recovered === 0 ? "none put right yet" : `the median of ${s.recovered} put right`}
          bands={BANDS.recovery}
          level={s.recovery_level}
          note={s.recovery_s !== null && s.recovery_level === null ? "A day to a week is no cluster's range." : undefined}
        />
      </section>
      <p className="mt-3 text-[12px] text-[#8c8f98]">Levels from DORA's 2024 Accelerate State of DevOps report: the range each of its four clusters' deployments fell in.</p>
      {s.deployments > 0 && <Weeks s={s} />}
      {(s.failures > 0 || n.service || n.env || n.team) && <Failures n={n} />}
    </>
  );
}

/** One key: its value, what it was taken from, and the report's bands beside it, its own marked. */
function Key({ title, value, said, bands, level, note }: { title: string; value: string; said: string; bands: readonly (readonly [DoraLevel, string])[]; level: DoraLevel | null; note?: string }) {
  return (
    <div className="rounded-[18px] border border-white/10 bg-white/[0.02] p-5" role="group" aria-label={title}>
      <h2 className="field-label">{title}</h2>
      <p className="mt-2 text-[26px] leading-tight tracking-[-0.02em] text-[#f2f2f3]">{value}</p>
      <p className="mt-1 text-[12px] text-[#8c8f98]">
        {said}
        {level && (
          <>
            {" "}
            · <span className="text-[#e4e5e7]">{LEVEL[level]}</span>
          </>
        )}
      </p>
      <ul className="mt-4 space-y-1 text-[12px]">
        {bands.map(([l, range], i) => (
          <li key={`${l}-${i}`} className={l === level ? "text-[#f2f2f3]" : "text-[#8c8f98]"} aria-current={l === level ? "true" : undefined}>
            <span className="inline-block w-16">{LEVEL[l]}</span>
            {range}
          </li>
        ))}
      </ul>
      {note && <p className="mt-3 text-[11.5px] leading-snug text-[#8c8f98]">{note}</p>}
    </div>
  );
}

/** Week by week: deployments and those that failed; the lead time. (Recovery times are few and far
 * apart, and an axis for them would flatten the lead time: each is in its deployment's row.) */
function Weeks({ s }: { s: DoraSummary }) {
  const xs = s.weeks.map((w) => w.start / 1000);
  const hours = (v: number | null) => (v === null ? null : v / 3600);
  const timed = s.weeks.some((w) => w.lead_time_s !== null);
  return (
    <section aria-label="Week by week" className="mt-8 grid grid-cols-1 gap-8 lg:grid-cols-2">
      <Panel eyebrow="Week by week" title="Deployments">
        <Chart
          ariaLabel="Deployments each week, and those that failed"
          data={[xs, s.weeks.map((w) => w.deployments), s.weeks.map((w) => w.change_failures)]}
          height={200}
          timeFormat="day"
          yFormat={(v) => String(Math.round(v))}
          series={[
            { label: "Deployments", color: SERIES.ok, bars: true },
            { label: "Failed", color: SERIES.fail, bars: true },
          ]}
        />
      </Panel>
      {timed && (
        <Panel eyebrow="Week by week" title="Change lead time">
          <Chart
            ariaLabel="Each week's median change lead time, in hours"
            data={[xs, s.weeks.map((w) => hours(w.lead_time_s))]}
            height={200}
            timeFormat="day"
            yFormat={(v) => `${Number(v.toFixed(1))} h`}
            legend={false}
            series={[{ label: "Lead time", color: SERIES.violet, width: 2, points: true }]}
          />
        </Panel>
      )}
    </section>
  );
}

function DeploymentsHead() {
  return (
    <tr>
      <th>Deployment</th>
      <th className="hidden md:table-cell">Lead time</th>
      <th className="hidden sm:table-cell">State</th>
      <th className="hidden sm:table-cell" />
    </tr>
  );
}

const FOUND: Record<string, string> = { rollback: "a rollback", revert: "a revert", hotfix: "a hotfix", manual: "hand" };

/** A deployment's state: failed (how it was found, and what put it right, how soon), or not. */
function Failed({ d, byId, quiet }: { d: DoraDeployment; byId: Map<string, DoraDeployment>; quiet?: boolean }) {
  const cf = d.attributes.change_failure;
  if (!cf) {
    const fixed = d.attributes.remedies;
    if (fixed.length === 0) return quiet ? <span /> : <span className="text-[12px] text-[#8c8f98]">—</span>;
    const one = fixed.length === 1 ? byId.get(fixed[0] ?? "") : undefined;
    return <span className="text-[12px] text-[#8c8f98]">Put {one ? one.attributes.version || one.id : fixed.length === 1 ? "one" : fixed.length} right</span>;
  }
  const r = cf.remediation;
  const by = r ? byId.get(r.id) : undefined;
  return (
    <span className="block text-[12px]">
      <span className="status">
        <i className="dot" data-status="failed" aria-hidden="true" />
        <span className="status-text" data-status="failed">
          Failed
        </span>
      </span>
      <span className="text-[#8c8f98]">
        {" "}
        · by {FOUND[cf.found] ?? cf.found}
        {r ? ` · put right by ${r.type} in ${formatDuration(d.attributes.recovery_s)}${by?.attributes.version ? ` (${by.attributes.version})` : ""}` : ""}
      </span>
    </span>
  );
}

function Deployments({ n }: { n: Narrowed }) {
  const all = useRes(deploymentsRes(n)).data;
  const [marking, setMarking] = React.useState<DoraDeployment | null>(null);
  const byId = new Map(all.map((d) => [d.id, d]));
  const unmark = async (d: DoraDeployment) => {
    try {
      await observeApi.markDeployment(d.id, { change_failure: false });
      invalidate("observe/dora");
    } catch (e) {
      toast({ title: "Couldn't change it", description: errorMessage(e), tone: "failed" });
    }
  };
  if (all.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        No deployments in this range. kiln CD's come here as they finish; another CD system sends its own to Datadog's DORA API (<code>POST /api/v2/dora/deployment</code>) with observe's ingest token.
      </p>
    );
  return (
    <section aria-label="Deployments" className="mt-8">
      <h2 className="field-label mb-3">Deployments</h2>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <DeploymentsHead />
          </thead>
          <tbody>
            {all.map((d) => {
              const a = d.attributes;
              const what = a.version || (a.git ? a.git.commit_sha.slice(0, 12) : "");
              const control = a.change_failure ? (
                <Button variant="ghost" size="xs" onClick={() => void unmark(d)}>
                  Not failed<span className="sr-only"> {d.id}</span>
                </Button>
              ) : (
                <Button variant="ghost" size="xs" onClick={() => setMarking(d)}>
                  Mark failed<span className="sr-only"> {d.id}</span>
                </Button>
              );
              return (
                <tr key={d.id}>
                  <td className="truncate-cell">
                    <div className="truncate text-[13px] text-[#e4e5e7]">
                      {a.service}
                      {what && <span className="ml-2 font-mono text-[11.5px] text-[#a7aab3]">{what}</span>}
                    </div>
                    <p className="truncate text-[11px] text-[#8c8f98]" title={absoluteDate(a.finished_at / 1000)}>
                      {relativeTime(a.finished_at / 1000)}
                      {a.env && ` · ${a.env}`}
                      {a.team && ` · ${a.team}`}
                      {a.source === "kiln.cd" && " · kiln CD"}
                    </p>
                    <p className="truncate text-[11px] text-[#8c8f98] md:hidden">{a.lead ? `lead ${formatDuration(a.lead.mean_s)}` : ""}</p>
                    <div className="mt-1 flex items-center justify-between gap-2 whitespace-normal sm:hidden">
                      <Failed d={d} byId={byId} quiet />
                      <span className="flex items-center">{control}</span>
                    </div>
                  </td>
                  <td className="hidden whitespace-nowrap md:table-cell">
                    {a.lead ? (
                      <>
                        {formatDuration(a.lead.mean_s)}
                        <div className="text-[12px] text-[#8c8f98]">
                          {a.lead.commits} {a.lead.commits === 1 ? "commit" : "commits"}
                        </div>
                      </>
                    ) : (
                      <span className="text-[#8c8f98]">—</span>
                    )}
                  </td>
                  <td className="hidden max-w-[26rem] sm:table-cell sm:min-w-[15rem]">
                    <Failed d={d} byId={byId} />
                  </td>
                  <td className="hidden w-1 whitespace-nowrap text-right sm:table-cell">{control}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {marking && (
        <MarkDialog
          d={marking}
          later={all.filter((x) => x.attributes.service === marking.attributes.service && x.attributes.env === marking.attributes.env && x.attributes.finished_at > marking.attributes.finished_at)}
          onClose={() => setMarking(null)}
        />
      )}
    </section>
  );
}

/** A deployment marked failed by hand, and what put it right (a later one of its service and environment), when one did. */
function MarkDialog({ d, later, onClose }: { d: DoraDeployment; later: DoraDeployment[]; onClose: () => void }) {
  const [by, setBy] = React.useState("");
  const [kind, setKind] = React.useState<DoraRemedyKind>("rollback");
  const [busy, setBusy] = React.useState(false);
  const a = d.attributes;
  const save = async () => {
    setBusy(true);
    try {
      await observeApi.markDeployment(d.id, { change_failure: true, ...(by ? { remediation: { id: by, type: kind } } : {}) });
      invalidate("observe/dora");
      onClose();
    } catch (e) {
      toast({ title: "Couldn't mark it", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const name = (x: DoraDeployment) => `${x.attributes.version || x.attributes.git?.commit_sha.slice(0, 12) || x.id}, ${relativeTime(x.attributes.finished_at / 1000)}`;
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={`Mark ${a.service} ${a.version || ""} failed`} description="It caused a failure in production. What put it right, when one did, gives its recovery time.">
        <div className="space-y-4">
          <SelectField
            label="Put right by"
            value={by}
            onChange={setBy}
            options={[
              { value: "", label: "Nothing yet" },
              ...later
                .slice()
                .reverse()
                .map((x) => ({ value: x.id, label: name(x) })),
            ]}
          />
          {by && (
            <Segmented
              label="How"
              value={kind}
              onChange={(v) => setKind(v === "rollforward" ? "rollforward" : "rollback")}
              options={[
                { value: "rollback", label: "A rollback" },
                { value: "rollforward", label: "A fix rolled forward" },
              ]}
            />
          )}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button disabled={busy} onClick={() => void save()}>
              Mark it failed
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function Failures({ n }: { n: Narrowed }) {
  const all = useRes(failuresRes(n)).data;
  if (all.length === 0) return null;
  return (
    <section aria-label="Failures" className="mt-8">
      <h2 className="field-label mb-3">Failures</h2>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <tr>
              <th>Failure</th>
              <th className="hidden sm:table-cell">Began</th>
              <th>Lasted</th>
            </tr>
          </thead>
          <tbody>
            {all.map((f) => {
              const a = f.attributes;
              return (
                <tr key={f.id}>
                  <td className="truncate-cell">
                    <div className="truncate text-[13px] text-[#e4e5e7]">{a.name || f.id}</div>
                    <p className="truncate text-[11px] text-[#8c8f98]">
                      {a.services.join(", ") || a.team}
                      {a.env && ` · ${a.env}`}
                      {a.severity && ` · ${a.severity}`}
                    </p>
                  </td>
                  <td className="hidden whitespace-nowrap sm:table-cell" title={absoluteDate(a.started_at / 1000)}>
                    {relativeTime(a.started_at / 1000)}
                  </td>
                  <td className="w-1 whitespace-nowrap">
                    {a.finished_at === null ? (
                      <span className="status">
                        <i className="dot" data-status="failed" aria-hidden="true" />
                        <span className="status-text" data-status="failed">
                          Going on
                        </span>
                      </span>
                    ) : (
                      formatDuration((a.finished_at - a.started_at) / 1000)
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
}
