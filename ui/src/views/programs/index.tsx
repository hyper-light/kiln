/**
 * Programs (docs/programs.md): the multi-stack programs kiln runs, the critical path first. One row
 * per track: its worker and how it stands (paused at a usage limit or its threshold, and when it
 * goes on), the goal under way (and on which agent), its goals met, its change requests (built,
 * merged or closed), its pass, its slot, its threshold and its usage. The person assigns the
 * workers and sets the slots and the weights here, in the page's flow; the coordinator is told,
 * and follows them at its next goal boundary.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import type { AgentInfo } from "@/api/harness";
import { planHref } from "@/api/plans";
import { programsApi, type Program, type ProgramAgent, type ProgramTrack } from "@/api/programs";
import { Button } from "@/components/ui/button";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, Meter, PageIntro } from "@/components/ui/misc";
import { StatusBadge } from "@/components/ui/status";
import { harnessSessionsRes, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { useClock } from "@/views/harness/dom";
import { programsRes } from "./data";
import { changeWords, edited, goalWords, noticeWords, passWords, pauseWords, slotWords, thresholdWords, usageWords } from "./words";
import "./programs.css";

export default function Programs() {
  return (
    <div className="container-console page programs-page">
      <title>Programs · kiln</title>
      <PageIntro
        index={6}
        label="Agents / Programs"
        title="Programs."
        compact
        lead="Multi-stack programs kiln runs, a row per track: its worker, the goal under way, goals met, change requests, pass, threshold and usage. Assign the workers and set the slots and weights here: the coordinator is told, and follows them at its next goal boundary."
        actions={
          <Button asChild variant="secondary" size="sm">
            <Link to="/agents">Mission control</Link>
          </Button>
        }
      />
      <Loadable label="the programs" fallback={<RowsHold rows={10} rowHeight={64} label="Loading the programs" />}>
        <ProgramsBody />
      </Loadable>
    </div>
  );
}

function ProgramsBody() {
  const res = useRes(programsRes);
  // When pauses end and notices went, said against a clock read every 30 s.
  const now = useClock(30_000);
  const programs = res.data;
  const stale = res.refreshError ? (
    <p className="programs-stale" role="status">
      Couldn&apos;t read the programs again: {errorMessage(res.refreshError)}. This is what was read before.
    </p>
  ) : null;
  if (programs.length === 0) {
    return (
      <>
        {stale}
        <EmptyState title="No programs yet">
          A program is a master plan, tagged <code>master</code>, and the track plans that share its tag, each tagged <code>t1</code>, <code>t2</code>, … Run the master plan as goals on an agent, its coordinator, and its tracks show here.
        </EmptyState>
      </>
    );
  }
  const saved = (p: Program) => res.setData((all) => all.map((x) => (x.id === p.id ? p : x)));
  return (
    <div className="programs-list">
      {stale}
      {programs.map((p) => (
        <ProgramSection key={p.id} program={p} now={now} onSaved={saved} />
      ))}
    </div>
  );
}

function AgentName({ agent }: { agent: ProgramAgent }) {
  return (
    <Link className="programs-agent" to={`/agents/${encodeURIComponent(agent.id)}`}>
      {agent.name}
    </Link>
  );
}

/** How an agent stands: its status, its session's pause (and when it goes on), and its goal stack's state when it has one that's paused or done. */
function Stands({ agent, now }: { agent: ProgramAgent; now: number }) {
  const stack = agent.stack && agent.stack !== "running" ? agent.stack : null;
  const pause = pauseWords(agent, now);
  return (
    <span className="programs-stands">
      <StatusBadge status={agent.status} />
      {pause && <span className="programs-paused">{pause}</span>}
      {stack && (
        <span className="programs-dim" title={agent.reason ?? undefined}>
          goals {stack}
        </span>
      )}
    </span>
  );
}

function ProgramSection({ program: p, now, onSaved }: { program: Program; now: number; onSaved: (p: Program) => void }) {
  const usage = usageWords(p);
  return (
    <section className="programs-program" data-program={p.id} aria-labelledby={`program-${p.id}`}>
      <header className="programs-head">
        <h2 id={`program-${p.id}`} className="programs-title">
          <Link to={planHref(p.id)}>{p.title}</Link>
        </h2>
        <div className="programs-facts">
          <span>
            {p.coordinator ? (
              <>
                Coordinator <AgentName agent={p.coordinator} /> <Stands agent={p.coordinator} now={now} />
              </>
            ) : p.coordinator_id ? (
              `Coordinator agent ${p.coordinator_id.slice(0, 8)}, not known to kiln now`
            ) : (
              "No coordinator: its master plan hasn't run as goals"
            )}
          </span>
          <span>
            {p.running} of {p.slots} slots at work · {p.met} of {p.total} goals met
          </span>
          {p.goal && <span className="programs-dim programs-clip">Under way: {p.goal.text}</span>}
        </div>
        {usage && (
          <p className="programs-usage" data-tone={usage.tone} role={usage.tone === "ok" ? undefined : "status"}>
            <Meter value={p.usage?.pct ?? 0} label="Usage of the weekly window" />
            <span>
              {usage.text}
              {p.usage?.account ? ` (${p.usage.account})` : ""}
            </span>
          </p>
        )}
      </header>
      <Settings program={p} now={now} onSaved={onSaved} />
    </section>
  );
}

/** The program's tracks and its settings: each track's worker and weight in its row, and the slots. Only what's edited is kept here; the rest shows the program as it is now. */
function Settings({ program: p, now, onSaved }: { program: Program; now: number; onSaved: (p: Program) => void }) {
  const [slots, setSlots] = React.useState<string | null>(null);
  const [weights, setWeights] = React.useState<Record<string, string>>({});
  const [workers, setWorkers] = React.useState<Record<string, string>>({});
  const [saving, setSaving] = React.useState(false);
  const [failed, setFailed] = React.useState<string | null>(null);
  const change = edited(p, slots ?? String(p.slots), weights, workers);
  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!change.changed || change.error) return;
    setSaving(true);
    setFailed(null);
    try {
      let latest: Program | null = null;
      for (const [track, agent] of Object.entries(change.assign)) latest = await programsApi.assign(p.id, track, agent);
      if (change.body.slots !== undefined || change.body.weights !== undefined) latest = await programsApi.set(p.id, change.body);
      if (latest) onSaved(latest);
      setSlots(null);
      setWeights({});
      setWorkers({});
    } catch (err) {
      setFailed(errorMessage(err));
    } finally {
      setSaving(false);
    }
  };
  const notice = noticeWords(p.settings, now);
  return (
    <form className="programs-form" onSubmit={save} aria-label={`${p.title}: workers, slots and weights`}>
      <ol className="programs-tracks" aria-label="Tracks">
        {p.tracks.map((t) => (
          <TrackRow
            key={t.id}
            track={t}
            now={now}
            weight={weights[t.id] ?? String(t.weight)}
            onWeight={(v) => setWeights((w) => ({ ...w, [t.id]: v }))}
            worker={workers[t.id] ?? t.assigned ?? ""}
            onWorker={(v) => setWorkers((w) => ({ ...w, [t.id]: v }))}
          />
        ))}
      </ol>
      <div className="programs-save">
        <label className="programs-field">
          <span className="field-label">Slots</span>
          <span className="field">
            <input type="number" inputMode="numeric" min={1} max={32} value={slots ?? String(p.slots)} onChange={(e) => setSlots(e.target.value)} aria-describedby={`slots-${p.id}`} />
          </span>
        </label>
        <Button type="submit" size="sm" disabled={!change.changed || !!change.error || saving}>
          {saving ? "Saving…" : "Save settings"}
        </Button>
        <span id={`slots-${p.id}`} className="programs-dim">
          {change.error ??
            failed ??
            (p.settings.by
              ? `Kept in kiln: set by ${p.settings.by}.`
              : "Tracks at work at once, each track's worker and weight: kept in kiln, and the coordinator follows them at its next goal boundary.")}
        </span>
      </div>
      {notice && (
        <p className="programs-notice" data-tone={notice.tone} role={notice.tone === "ok" ? undefined : "status"}>
          {notice.text}
        </p>
      )}
    </form>
  );
}

/** A track's worker, chosen from the agents kiln knows (the person's own: kiln refuses another's). */
function WorkerSelect({ track: t, value, onChange }: { track: ProgramTrack; value: string; onChange: (v: string) => void }) {
  return (
    <React.Suspense fallback={<WorkerOptions track={t} value={value} onChange={onChange} agents={null} />}>
      <WorkerList track={t} value={value} onChange={onChange} />
    </React.Suspense>
  );
}

function WorkerList(props: { track: ProgramTrack; value: string; onChange: (v: string) => void }) {
  const agents = useRes(harnessSessionsRes).data;
  return <WorkerOptions {...props} agents={agents} />;
}

function WorkerOptions({ track: t, value, onChange, agents }: { track: ProgramTrack; value: string; onChange: (v: string) => void; agents: AgentInfo[] | null }) {
  const known = (agents ?? []).slice().sort((a, b) => a.name.localeCompare(b.name));
  // The worker as assigned, and the one chosen, are always among the choices.
  const extra = [t.assigned, value].filter((id): id is string => !!id && !known.some((a) => a.id === id));
  const label = (id: string) => (t.worker?.id === id ? t.worker.name : `agent ${id.slice(0, 8)}`);
  return (
    <select className="programs-worker-select" value={value} onChange={(e) => onChange(e.target.value)} aria-label={`${t.id}'s worker`} disabled={agents === null}>
      <option value="">no worker</option>
      {[...new Set(extra)].map((id) => (
        <option key={id} value={id}>
          {label(id)}
        </option>
      ))}
      {known.map((a) => (
        <option key={a.id} value={a.id}>
          {a.name} ({a.id.slice(0, 8)})
        </option>
      ))}
    </select>
  );
}

function TrackRow({
  track: t,
  now,
  weight,
  onWeight,
  worker,
  onWorker,
}: {
  track: ProgramTrack;
  now: number;
  weight: string;
  onWeight: (v: string) => void;
  worker: string;
  onWorker: (v: string) => void;
}) {
  const slot = slotWords(t.slot);
  const usage = t.worker?.usage;
  const threshold = thresholdWords(t);
  return (
    <li className="programs-row" data-track-row={t.id} data-slot={t.slot}>
      <div className="programs-track">
        <Link to={planHref(t.plan)}>
          <span className="programs-id">{t.id}</span> {t.name}
        </Link>
        <span className="programs-slot" data-tone={slot.tone}>
          {slot.text}
        </span>
      </div>
      <div className="programs-worker">
        {t.worker ? (
          <span className="programs-who">
            <AgentName agent={t.worker} /> <Stands agent={t.worker} now={now} />
          </span>
        ) : t.assigned ? (
          <span className="programs-dim">agent {t.assigned.slice(0, 8)}, not known to kiln now</span>
        ) : (
          <span className="programs-dim">no worker yet</span>
        )}
        {threshold && <span className="programs-dim">{threshold}</span>}
        <WorkerSelect track={t} value={worker} onChange={onWorker} />
      </div>
      <div className="programs-goal programs-clip" title={t.goal?.text}>
        {t.goal ? (
          <>
            <span className="programs-dim">{goalWords(t.goal, t.slot)}:</span> {t.goal.text}
          </>
        ) : (
          <span className="programs-dim">{t.total > 0 && t.met === t.total ? "every goal met" : "no goal yet"}</span>
        )}
      </div>
      <div className="programs-met">
        <span className="programs-bar" role="meter" aria-valuenow={t.met} aria-valuemin={0} aria-valuemax={t.total} aria-label={`${t.id}: goals met`}>
          <span style={{ width: `${t.total > 0 ? (100 * t.met) / t.total : 0}%` }} />
        </span>
        <span>
          {t.met} of {t.total} met
        </span>
      </div>
      <div className="programs-changes">
        {t.changes.length === 0 ? (
          <span className="programs-dim">no change request</span>
        ) : (
          t.changes.map((c) => (
            <a key={c.url || c.branch} className="programs-change" data-state={c.state} href={c.url} target="_blank" rel="noreferrer" title={c.title}>
              {changeWords(c)}
            </a>
          ))
        )}
      </div>
      <div className="programs-numbers">
        <span title="Each goal met adds 1/weight: a free slot goes to the lowest pass">pass {passWords(t.pass)}</span>
        <label className="programs-weight">
          <span>weight</span>
          <input type="number" inputMode="numeric" min={1} max={100} value={weight} onChange={(e) => onWeight(e.target.value)} aria-label={`${t.id}'s weight`} />
        </label>
        <span title={usage?.account}>{usage ? `${Math.round(usage.pct)}% weekly` : "usage not known"}</span>
      </div>
    </li>
  );
}
