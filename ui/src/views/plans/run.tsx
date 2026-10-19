/**
 * A plan run as one agent per step (docs/plans.md, "One agent per step"): each step with its
 * agent, where it stands (waiting, working, needs you, met, not met) and what it changed; stop the
 * run, retry a step with a new agent, take a step over (kiln stops holding its agent to the goal)
 * and say it's met. Everything in the page's flow, kept live.
 */
import * as React from "react";
import { Link, useParams } from "@/lib/router";
import { ApiError } from "@/api/client";
import { planHref, plansApi, type PlanRunView, type RunStep, type StepStand } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, EmptyState } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import "./plans.css";

const WORD: Record<StepStand, string> = {
  waiting: "Waiting",
  working: "Working",
  needs_you: "Needs you",
  yours: "Taken over",
  met: "Met",
  unmet: "Not met",
  stopped: "Stopped",
};

const STATE: Record<PlanRunView["state"], string> = { running: "running", stopped: "stopped", done: "done" };

/** "gone" for a run that isn't there: trying again won't bring it. */
const unlessGone = (e: unknown): "gone" => {
  if (e instanceof ApiError && (e.status === 404 || e.status === 400)) return "gone";
  throw e;
};

export default function RunPage() {
  const { run = "" } = useParams();
  return (
    <div className="container-console page plan-page">
      <Loadable label="the run" fallback={<RowsHold rows={3} rowHeight={72} />}>
        <RunOrNone key={run} run={run} />
      </Loadable>
    </div>
  );
}

function RunOrNone({ run }: { run: string }) {
  const res = useSuspenseResource(`plans/runs/${run}`, (s) => plansApi.runView(run, s).catch(unlessGone), { on: ["harness", "harness_closed"], pollMs: 4000 });
  const v = res.data;
  if (v === "gone") {
    return (
      <>
        <RunCrumbs />
        <EmptyState title="No such run">It may belong to a plan that was deleted.</EmptyState>
      </>
    );
  }
  return <Run v={v} onChange={(n) => res.setData(() => n)} />;
}

function RunCrumbs({ v }: { v?: PlanRunView }) {
  return <Crumbs index={6} items={[{ label: "Agents", to: "/agents" }, { label: "Plans", to: "/agents/plans" }, ...(v ? [{ label: v.title, to: planHref(v.plan) }] : [])]} />;
}

function Run({ v, onChange }: { v: PlanRunView; onChange: (v: PlanRunView) => void }) {
  const [busy, setBusy] = React.useState<string | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const act = async (key: string, f: () => Promise<PlanRunView>) => {
    setBusy(key);
    setError(null);
    try {
      onChange(await f());
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  return (
    <>
      <title>{`${v.title}: one agent per step · kiln`}</title>
      <RunCrumbs v={v} />
      <h1 className="run-title">{v.title}</h1>
      <p className="plan-dim plan-meta">
        One agent per step · v{v.version} · {STATE[v.state]} · {v.met} of {v.total} met · started {relativeTime(v.at)} by {v.by}
      </p>
      {v.note && <p className="plan-dim run-note">{v.note}</p>}
      {v.state === "stopped" && v.reason && <p className="run-reason">Stopped: {v.reason}.</p>}
      {v.state === "running" && (
        <div className="plan-actions">
          <Button size="sm" variant="secondary" disabled={busy !== null} onClick={() => void act("stop", () => plansApi.stopRun(v.id))}>
            {busy === "stop" ? "Stopping…" : "Stop the run"}
          </Button>
          {error && <span className="plan-error">{error}</span>}
        </div>
      )}
      {v.state !== "running" && error && <span className="plan-error">{error}</span>}
      <ol className="run-steps">
        {v.steps.map((s) => (
          <Step key={s.n} s={s} busy={busy} onAct={(a) => void act(`${s.n}:${a}`, () => plansApi.step(v.id, s.n, a))} />
        ))}
      </ol>
    </>
  );
}

function Step({ s, busy, onAct }: { s: RunStep; busy: string | null; onAct: (a: "retry" | "take" | "met") => void }) {
  const a = s.agent;
  const live = s.stand === "working" || s.stand === "needs_you";
  const what = a?.needs?.[0] ? `Asks a person: ${a.needs[0].tool_name ? `${a.needs[0].tool_name}: ` : ""}${a.needs[0].title}` : (a?.doing ?? a?.summary ?? "");
  return (
    <li className="run-step" data-stand={s.stand}>
      <div className="run-step-head">
        <span className="run-n">{s.n}</span>
        <span className="run-step-title">{s.title}</span>
        <span className="run-word">
          <i aria-hidden="true" />
          {WORD[s.stand]}
        </span>
      </div>
      {a ? (
        <div className="run-agent">
          <Link to={`/agents/${encodeURIComponent(a.id)}`} className="run-agent-name">
            {a.name}
          </Link>
          <span className="plan-dim">
            {a.machine === "local" ? "server" : a.machine} · {a.kind}
            {a.files_changed > 0 ? ` · ${a.files_changed} file${a.files_changed === 1 ? "" : "s"} +${a.lines_added} −${a.lines_removed}` : ""}
            {s.tries > 1 ? ` · try ${s.tries}` : ""}
          </span>
          {what && <span className="run-what">{what}</span>}
        </div>
      ) : (
        s.agent_id && <p className="plan-dim run-what">Its agent has ended{s.tries > 1 ? ` (try ${s.tries})` : ""}.</p>
      )}
      {s.reason && (s.stand === "unmet" || s.stand === "stopped" || s.stand === "yours") && <p className="plan-dim run-what">{s.reason}</p>}
      <p className="plan-dim run-goal" title={s.goal}>
        Held to: {s.goal}
      </p>
      {(live || s.stand === "unmet" || s.stand === "stopped" || s.stand === "yours") && (
        <div className="plan-actions">
          {(s.stand === "unmet" || s.stand === "stopped" || live) && (
            <Button size="xs" variant="secondary" disabled={busy !== null} onClick={() => onAct("retry")}>
              {busy === `${s.n}:retry` ? "Starting…" : "Retry with a new agent"}
            </Button>
          )}
          {(live || s.stand === "unmet" || s.stand === "stopped") && (
            <Button size="xs" variant="ghost" disabled={busy !== null} onClick={() => onAct("take")}>
              Take it over
            </Button>
          )}
          {s.stand === "yours" && (
            <Button size="xs" disabled={busy !== null} onClick={() => onAct("met")}>
              {busy === `${s.n}:met` ? "Going on…" : "It's met: go on"}
            </Button>
          )}
        </div>
      )}
    </li>
  );
}
