/**
 * A plan run as one agent per step (docs/plans.md, "One agent per step"): each step with its
 * agent (or the agent it was given to), where it stands (waiting, and for which steps; working,
 * needs you, met and how, not met), what it changed and its diff in review (each met step's on the
 * one met before it: the run ends as a stack); stop the run, retry a step, take a step over (kiln
 * stops holding its agent to the goal) and say it's met. Everything in the page's flow, kept live.
 */
import * as React from "react";
import { Link, useParams } from "@/lib/router";
import { changesRes, changeWords } from "@/api/changes";
import { ApiError } from "@/api/client";
import { planHref, plansApi, type PlanRunView, type RunStack, type RunStep, type StackStep, type StepStand } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, EmptyState } from "@/components/ui/misc";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { ChangeLink, Quiet } from "@/views/harness/change-bar";
import { PlanGraph, StepBar } from "./graph";
import { StepGlyph } from "./status";
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

/** Steps by number, as words: "step 2", "steps 2 and 3", "steps 1, 2 and 3". */
function stepsWords(n: number[]): string {
  if (n.length <= 1) return n.length ? `step ${n[0]}` : "";
  return `steps ${n.slice(0, -1).join(", ")} and ${n[n.length - 1]}`;
}

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
  const res = useSuspenseResource(`plans/runs/${run}`, (s) => plansApi.runView(run, s).catch(unlessGone), { on: ["harness", "harness_closed", "plan_stack"], pollMs: 4000 });
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
  return <Crumbs index={6} items={[{ label: "Agents", to: "/agents" }, ...(v ? [{ label: v.title, to: planHref(v.plan) }] : [])]} />;
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
      <StepBar stands={v.steps.map((s) => s.stand)} />
      {v.steps.length > 1 && (
        <PlanGraph
          label="Its steps, as they run"
          steps={v.steps.map((s) => ({
            n: s.n,
            title: s.title,
            needs: s.needs ?? [],
            stand: s.stand,
            who: s.agent ? `${s.agent.name} · ${s.agent.kind}` : undefined,
            href: s.agent ? `/agents/${encodeURIComponent(s.agent.id)}` : undefined,
          }))}
        />
      )}
      {v.note && <p className="plan-dim run-note">{v.note}</p>}
      {v.stack && <StackLine stack={v.stack} />}
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
          <Step key={s.n} s={s} stack={v.stack} busy={busy} onAct={(a) => void act(`${s.n}:${a}`, () => plansApi.step(v.id, s.n, a))} />
        ))}
      </ol>
    </>
  );
}

/** Where the run's met steps go up for review, or why they don't. */
function StackLine({ stack }: { stack: RunStack }) {
  if (stack.off) return <p className="plan-dim run-note">Its steps don't go up for review: {stack.off}.</p>;
  const made = stack.steps.filter((e) => e.state === "made");
  if (made.length === 0) return null;
  const where =
    stack.provider === "local"
      ? "commits in the folder's repository"
      : stack.provider === "kiln"
        ? `a stack of ${made.length} diff${made.length === 1 ? "" : "s"}`
        : `${made.length} pull request${made.length === 1 ? "" : "s"}, each on the one before`;
  return (
    <p className="plan-dim run-note">
      In review as {where}, into {stack.trunk}
      {stack.repo ? ` of ${stack.repo}` : ""}.
    </p>
  );
}

/** A met step's work in review: its diff and where it stands, or why it has none. */
function StepReview({ e, agentId, provider }: { e: StackStep; agentId: string | null; provider: string }) {
  const record = useRes(changesRes).data.find((c) => c.agent === agentId);
  if (e.state === "nothing") return <p className="plan-dim run-what">It changed no files: no diff for this step.</p>;
  if (e.state === "failed") return <p className="plan-dim run-what">Its work goes up with the next step's{e.error ? `: ${e.error}` : "."}</p>;
  if (e.state === "elsewhere") return <p className="plan-dim run-what">{e.error}</p>;
  const sha = e.sha?.slice(0, 10) ?? "";
  if (!e.pushed && !e.change)
    return (
      <p className="plan-dim run-what">
        Committed as {sha}
        {provider === "local" ? " in the folder's repository (it has no remote)." : `, not pushed yet${e.error ? `: ${e.error}` : "."}`}
      </p>
    );
  const words = record && record.change === e.change ? changeWords(record) : (e.change ?? `${e.branch ?? sha} pushed: open its pull request`);
  return (
    <p className="run-what run-review">
      <ChangeLink c={{ provider, change: e.change ?? undefined, url: e.url }}>{words}</ChangeLink>
      {e.error && <span className="plan-dim"> · {e.error}</span>}
    </p>
  );
}

function Step({ s, stack, busy, onAct }: { s: RunStep; stack: RunStack | null; busy: string | null; onAct: (a: "retry" | "take" | "met") => void }) {
  const e = stack?.steps.find((x) => x.n === s.n);
  const a = s.agent;
  const live = s.stand === "working" || s.stand === "needs_you";
  const what = a?.needs?.[0] ? `Asks a person: ${a.needs[0].tool_name ? `${a.needs[0].tool_name}: ` : ""}${a.needs[0].title}` : (a?.doing ?? a?.summary ?? "");
  return (
    <li className="run-step" data-stand={s.stand}>
      <div className="run-step-head">
        <span className="run-n">{s.n}</span>
        <span className="run-step-title">{s.title}</span>
        <span className="run-word">
          <StepGlyph stand={s.stand} />
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
            {s.given ? " · on its goal stack" : ""}
            {a.files_changed > 0 ? ` · ${a.files_changed} file${a.files_changed === 1 ? "" : "s"} +${a.lines_added} −${a.lines_removed}` : ""}
            {s.tries > 1 ? ` · try ${s.tries}` : ""}
          </span>
          {what && <span className="run-what">{what}</span>}
        </div>
      ) : (
        s.agent_id && <p className="plan-dim run-what">Its agent has ended{s.tries > 1 ? ` (try ${s.tries})` : ""}.</p>
      )}
      {s.stand === "waiting" && s.needs?.length ? (
        <p className="plan-dim run-what">
          Starts once {stepsWords(s.needs)} {s.needs.length === 1 ? "is" : "are"} met.
        </p>
      ) : null}
      {s.reason && (s.stand === "unmet" || s.stand === "stopped" || s.stand === "yours" || s.stand === "met") && <p className="plan-dim run-what">{s.reason}</p>}
      {e && stack && (
        <Quiet>
          <React.Suspense fallback={null}>
            <StepReview e={e} agentId={s.agent_id} provider={stack.provider} />
          </React.Suspense>
        </Quiet>
      )}
      <p className="plan-dim run-goal" title={s.goal}>
        Held to: {s.goal}
      </p>
      {(live || s.stand === "unmet" || s.stand === "stopped" || s.stand === "yours") && (
        <div className="plan-actions">
          {(s.stand === "unmet" || s.stand === "stopped" || live) && (
            <Button size="xs" variant="secondary" disabled={busy !== null} onClick={() => onAct("retry")}>
              {busy === `${s.n}:retry` ? "Starting…" : s.given ? "Try it again" : "Retry with a new agent"}
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
