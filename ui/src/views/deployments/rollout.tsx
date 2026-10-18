/**
 * An app's rollout on a target beyond Kubernetes (docs/cd-targets.md §5), as Argo Rollouts' dashboard
 * shows one: where it stands and why, the traffic between blue and green, its steps with the one it's
 * at, its checks with what they measured, approvals waiting for a person, and what happened. Its
 * actions are a person's: promote, pause or resume, abort, retry, approve or deny. Promoting past
 * every step and aborting ask first. A rollout that finished well folds to a line.
 */
import * as React from "react";

import { cdApi } from "@/api/cd";
import type { CdCheckRun, CdRollout, CdRolloutStep } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Confirm } from "@/components/ui/misc";
import { RelTime } from "@/views/shared";
import { StatePair, shortRev } from "./parts";

const TONE: Record<CdRollout["status"]["state"], string> = { running: "running", paused: "timed_out", succeeded: "succeeded", aborted: "failed", superseded: "skipped" };
const WORD: Record<CdRollout["status"]["state"], string> = { running: "Rolling out", paused: "Paused", succeeded: "Rolled out", aborted: "Aborted", superseded: "Superseded" };

/** A duration in the deploy file's words: 250ms, 30s, 5m, 1h 30m. */
export function span(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return s % 60 ? `${m}m ${s % 60}s` : `${m}m`;
  return m % 60 ? `${Math.floor(m / 60)}h ${m % 60}m` : `${Math.floor(m / 60)}h`;
}

/** What a step does, in words. */
export function stepWords(s: CdRolloutStep): string {
  switch (s.step) {
    case "weight":
      return s.weight === 100 ? "All the traffic to green" : `${s.weight}% of the traffic to green`;
    case "pause":
      return s.ms == null ? "Wait for a person to promote" : `Wait ${span(s.ms)}`;
    case "checks":
      return `Check ${s.names.join(", ")} against ${s.against === "preview" ? "green's preview" : "production"}`;
    case "watch":
      return `Watch ${s.names.join(", ")} for ${span(s.window_ms)}`;
    case "approve":
      return "Wait for approval";
  }
}

const version = (v: string | number) => (typeof v === "number" || /^\d+$/.test(v) ? `version ${v}` : shortRev(String(v)));

/** One line of what happened. */
export function logWords(e: CdRollout["log"][number]): string {
  const f = (k: string) => (e[k] == null ? "" : String(e[k]));
  switch (e.event) {
    case "read":
      return `Read the traffic: green has ${f("to")}%`;
    case "ready":
      return "Green is ready";
    case "unready":
      return `Green isn't ready: ${f("why")}`;
    case "moved":
      return `Moved ${f("weight")}% of the traffic to green`;
    case "move-failed":
      return `Traffic couldn't move to ${f("weight")}%: ${f("why")}`;
    case "drifted":
      return e.to == null ? `Traffic moved by something else: ${f("why")}` : `Traffic moved by something else, to ${f("to")}%`;
    case "pulled":
      return `Took the traffic off green: ${f("why")}`;
    case "measured":
      return `${f("check")} #${f("n")}: ${f("verdict")}${e.message ? ` (${f("message")})` : ""}${e.decided ? `, so it ${f("decided") === "pass" ? "passed" : f("decided") === "fail" ? "failed" : f("decided")}` : ""}`;
    case "checked": {
      const c = (e.check ?? {}) as { name?: string; verdict?: string | null };
      return `Check ${c.name ?? ""} ended${c.verdict ? `: ${c.verdict}` : ""}`;
    }
    case "watching":
      return `Watching ${Array.isArray(e.checks) ? e.checks.join(", ") : ""} in the background`;
    case "unclear":
      return f("why");
    case "promoted":
      return e.full ? "Promoted past every step" : "Promoted";
    case "paused":
      return "Paused by a person";
    case "resumed":
      return "Resumed";
    case "aborted":
      return `Aborted: ${f("why")}`;
    case "superseded":
      return `Superseded by ${shortRev(f("by"))}`;
    case "retried":
      return "Retried from the first step";
    case "succeeded":
      return "Green has all the traffic";
    case "reverted":
      return "All the traffic back on blue";
    case "released":
      return `Let ${f("version")} go`;
    case "window-restarted":
      return "The window started again: green has the traffic back";
    case "straight":
      return `All the traffic at once: ${f("why")}`;
    default:
      return e.event;
  }
}

type Ask = null | "full" | "abort";

export function RolloutPanel({
  appId,
  appName,
  rollouts,
  busy,
  onAct,
}: {
  appId: string;
  appName: string;
  rollouts: CdRollout[];
  busy: boolean;
  onAct: (done: string, f: () => Promise<unknown>) => Promise<void>;
}) {
  const r = rollouts[0];
  const [open, setOpen] = React.useState(false);
  if (!r) return null;
  const done = r.status.state === "succeeded" || r.status.state === "superseded";
  if (done && !open) {
    return (
      <p className="cd-rollout-line">
        <StatePair tone={TONE[r.status.state]} word={WORD[r.status.state]} />
        <span className="cd-meta">
          {r.kind} {r.name}: {version(r.green)}, <RelTime ts={Math.floor(r.updated_ms / 1000)} />
        </span>
        <button type="button" className="cd-link" onClick={() => setOpen(true)}>
          Details
        </button>
      </p>
    );
  }
  return <Rollout key={r.id} appId={appId} appName={appName} r={r} busy={busy} onAct={onAct} onFold={done ? () => setOpen(false) : undefined} />;
}

function Rollout({
  appId,
  appName,
  r,
  busy,
  onAct,
  onFold,
}: {
  appId: string;
  appName: string;
  r: CdRollout;
  busy: boolean;
  onAct: (done: string, f: () => Promise<unknown>) => Promise<void>;
  onFold?: () => void;
}) {
  const [ask, setAsk] = React.useState<Ask>(null);
  const [reason, setReason] = React.useState("");
  const st = r.status;
  const over = st.state === "succeeded" || st.state === "aborted" || st.state === "superseded";
  const call = (done: string, action: Parameters<typeof cdApi.rollout>[1], body: Parameters<typeof cdApi.rollout>[2] = {}) =>
    onAct(done, () => cdApi.rollout(appId, action, { ...body, rollout: r.id })).then(() => setAsk(null));
  const why = [st.why, st.waiting, !r.ready && r.unready ? `green isn't ready: ${r.unready}` : ""].filter((x, i, all) => x && all.indexOf(x) === i);
  return (
    <section className="cd-rollout" data-state={st.state} aria-label="Rollout">
      <header className="cd-rollout-head">
        <h2 className="cd-rollout-title">Rollout</h2>
        <StatePair tone={TONE[st.state]} word={st.held ? "Paused by a person" : WORD[st.state]} />
        <span className="cd-meta">
          {r.kind} {r.name} · {r.revision ? shortRev(r.revision) : version(r.green)} · started <RelTime ts={Math.floor(r.started_ms / 1000)} />
        </span>
        {onFold && (
          <button type="button" className="cd-link cd-rollout-fold" onClick={onFold}>
            Fold
          </button>
        )}
      </header>
      {why.length > 0 && <p className="cd-rollout-why">{why.join(" · ")}</p>}
      <Traffic r={r} />
      {r.preview_url && (
        <p className="cd-meta cd-rollout-preview">
          Green's preview: <a href={r.preview_url} target="_blank" rel="noreferrer">{r.preview_url}</a>
        </p>
      )}
      {r.waiting.length > 0 && (
        <ul className="cd-rollout-asks" aria-label="Approvals waiting">
          {r.waiting.map((c) => (
            <li key={c} className="cd-rollout-ask">
              <span>
                <strong>{c}</strong> waits for your approval to go on.
              </span>
              <span className="cd-fix-acts">
                <Button size="xs" onClick={() => call(`Approved ${c}.`, "approve", { check: c })} disabled={busy}>
                  Approve
                </Button>
                <Button size="xs" variant="secondary" onClick={() => call(`Denied ${c}: the rollout aborts.`, "deny", { check: c })} disabled={busy}>
                  Deny
                </Button>
              </span>
            </li>
          ))}
        </ul>
      )}
      <Steps r={r} />
      {r.watching.length > 0 && (
        <div className="cd-rollout-bg">
          <h3 className="cd-rollout-sub">In the background</h3>
          <Checks runs={r.watching} waiting={r.waiting} />
        </div>
      )}
      <div className="cd-actions">
        {!over && (
          <>
            <Button size="xs" onClick={() => call("Promoted.", "promote")} disabled={busy} title="Past the step it waits at">
              Promote
            </Button>
            <Button size="xs" variant="secondary" onClick={() => setAsk(ask === "full" ? null : "full")} aria-expanded={ask === "full"} disabled={busy}>
              Promote fully…
            </Button>
            {st.held ? (
              <Button size="xs" variant="secondary" onClick={() => call("Resumed.", "resume")} disabled={busy}>
                Resume
              </Button>
            ) : (
              <Button size="xs" variant="secondary" onClick={() => call("Paused: traffic stays where it is.", "pause")} disabled={busy}>
                Pause
              </Button>
            )}
            <Button size="xs" variant="ghost" onClick={() => setAsk(ask === "abort" ? null : "abort")} aria-expanded={ask === "abort"} disabled={busy}>
              Abort…
            </Button>
          </>
        )}
        {st.state === "aborted" && (
          <Button size="xs" variant="secondary" onClick={() => call("Rolling green out again, from the first step.", "retry")} disabled={busy}>
            Retry
          </Button>
        )}
      </div>
      {ask === "full" && (
        <Confirm
          label="Confirm"
          actions={
            <>
              <Button size="xs" onClick={() => call("Promoted past every step: green takes all the traffic.", "promote", { full: true })} disabled={busy}>
                Promote fully
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setAsk(null)}>
                Cancel
              </Button>
            </>
          }
        >
          <p>Every step and check left is skipped: green takes all the traffic now.</p>
        </Confirm>
      )}
      {ask === "abort" && (
        <Confirm
          label="Confirm"
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" onClick={() => call("Aborted: traffic goes back to blue.", "abort", reason.trim() ? { reason: reason.trim() } : {})} disabled={busy}>
                Abort
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setAsk(null)}>
                Cancel
              </Button>
            </>
          }
        >
          <p>
            All the traffic goes back to blue ({version(r.blue)}).{" "}
            {r.auto_rollback && r.back_to != null
              ? `Then ${appName} goes back to deployment ${r.back_to} and holds there until git changes it.`
              : "The app stays on this revision, degraded, until you retry or git moves on."}
          </p>
          <label className="cd-opt cd-rollout-reason">
            Why <input className="input" value={reason} onChange={(e) => setReason(e.target.value)} placeholder="for the record" />
          </label>
        </Confirm>
      )}
      <details className="cd-rollout-log">
        <summary>What happened ({r.log.length + r.dropped})</summary>
        <ol>
          {r.dropped > 0 && <li className="cd-meta">{r.dropped} earlier entries dropped</li>}
          {[...r.log].reverse().map((e, i) => (
            <li key={`${e.at_ms}-${i}`}>
              <RelTime ts={Math.floor(e.at_ms / 1000)} className="cd-meta" /> <span>{logWords(e)}</span>
            </li>
          ))}
        </ol>
      </details>
    </section>
  );
}

/** Blue's and green's shares of the traffic. */
function Traffic({ r }: { r: CdRollout }) {
  const w = Math.max(0, Math.min(100, r.status.weight));
  return (
    <div className="cd-traffic" role="img" aria-label={`Blue ${100 - w}%, green ${w}%`}>
      <div className="cd-traffic-bar">
        {w < 100 && <span className="cd-traffic-blue" style={{ flexGrow: 100 - w }} />}
        {w > 0 && <span className="cd-traffic-green" style={{ flexGrow: w }} />}
      </div>
      <div className="cd-traffic-legend">
        <span>
          <i className="cd-traffic-key" data-side="blue" aria-hidden="true" /> Blue · {version(r.blue)} · {100 - w}%
        </span>
        <span>
          <i className="cd-traffic-key" data-side="green" aria-hidden="true" /> Green · {version(r.green)} · {w}%
          {r.status.target !== w && r.status.state === "running" ? ` → ${r.status.target}%` : ""}
        </span>
      </div>
    </div>
  );
}

/** The steps, the one it's at (or ended at) marked, with that step's checks under it. */
function Steps({ r }: { r: CdRollout }) {
  const state = r.status.state;
  const finished = state !== "running" && state !== "paused";
  return (
    <ol className="cd-rollout-steps" aria-label="Steps">
      {r.steps.map((s, i) => {
        const at =
          i < r.step || state === "succeeded" ? "done" : i === r.step && state === "aborted" ? "failed" : i === r.step && !finished ? "now" : "next";
        return (
          <li key={i} className="cd-rollout-step" data-at={at} aria-current={at === "now" ? "step" : undefined}>
            <span className="cd-rollout-n">{i + 1}</span>
            <span className="cd-rollout-step-text">
              {stepWords(s)}
              {at === "now" && r.checks.length > 0 && <Checks runs={r.checks} waiting={r.waiting} />}
            </span>
          </li>
        );
      })}
    </ol>
  );
}

function Checks({ runs, waiting }: { runs: CdCheckRun[]; waiting: string[] }) {
  return (
    <ul className="cd-checks">
      {runs.map((c) => {
        const last = c.measurements[c.measurements.length - 1];
        const t = c.tally;
        const counts = [t.passed && `${t.passed} passed`, t.failed && `${t.failed} failed`, t.inconclusive && `${t.inconclusive} inconclusive`, t.errors && `${t.errors} errors`].filter(Boolean).join(" · ");
        return (
          <li key={c.name} className="cd-check" data-verdict={c.verdict ?? "measuring"}>
            <span className="cd-check-name">{c.name}</span>
            <span className="cd-check-verdict">{c.verdict ?? (waiting.includes(c.name) ? "waiting for approval" : c.asked ? "measuring…" : "waiting")}</span>
            <span className="cd-meta">
              {counts || (waiting.includes(c.name) ? "nobody has answered yet" : "not measured yet")}
              {c.timing.dry_run ? " · dry run" : ""}
              {c.why ? ` · ${c.why}` : last?.message ? ` · ${last.message}` : ""}
            </span>
          </li>
        );
      })}
    </ul>
  );
}
