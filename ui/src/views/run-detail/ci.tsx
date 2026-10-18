/** The CI engine's part of the run page (docs/ci.md W21-R04): attempts, reruns, and what a run
 * leaves behind (summaries, annotations, artifacts, the actions it pinned). */
import * as React from "react";
import { RotateCcw, Square } from "lucide-react";
import { ci, reviewOf, waitSentence, type CiAnnotation, type CiRunDetail, type RerunMode } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Confirm, Panel, SelectField, TextField } from "@/components/ui/misc";
import { Link } from "@/lib/router";
import { toast } from "@/components/ui/toast";
import { formatBytes } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";

const Markdown = React.lazy(() => import("@/lib/markdown").then((m) => ({ default: m.MarkdownBody })));
const TerminalSession = React.lazy(() => import("@/views/terminal/session").then((m) => ({ default: m.TerminalSession })));

/** A leg's key, as the graph groups it: a matrix leg's job, without its values. */
export function nodeKey(legKey: string): string {
  const i = legKey.indexOf(" (");
  return i > 0 ? legKey.slice(0, i) : legKey;
}

/** The graph nodes on the run's critical path, by the keys the graph uses. */
export function criticalKeys(detail: CiRunDetail, path: string[]): Set<string> {
  const on = new Set(path);
  return new Set(detail.legs.filter((l) => l.job && on.has(l.job)).map((l) => nodeKey(l.key)));
}

/** The attempt shown, and the run's attempts to choose from. */
export function AttemptSelect({ current, shown, onChange }: { current: number; shown: number; onChange: (n: number) => void }) {
  if (current < 2) return null;
  const options = Array.from({ length: current }, (_, i) => current - i).map((n) => ({ value: String(n), label: n === current ? `Attempt ${n} (latest)` : `Attempt ${n}` }));
  return <SelectField label="Attempt" hideLabel value={String(shown)} onChange={(v) => onChange(Number(v))} options={options} className="w-44" />;
}

/** "Edit workflow": the run's workflow in the editor, at the run's branch (W21-R06). */
export function EditWorkflowLink({ detail }: { detail: CiRunDetail }) {
  const { run } = detail;
  const to = `/workflows?repo=${encodeURIComponent(run.repo)}&path=${encodeURIComponent(run.workflow)}&ref=${encodeURIComponent(run.branch)}`;
  return (
    <Link to={to} className="text-link font-mono text-[11px]">
      Edit workflow
    </Link>
  );
}

/** Rerun (all, failed, one job; with debug logging) and Cancel, where the run's header is. */
export function CiRunActions({ detail, jobs }: { detail: CiRunDetail; jobs: string[] }) {
  const { run } = detail;
  const active = !run.finished_at && run.status !== "succeeded" && run.status !== "failed" && run.status !== "cancelled";
  const failed = detail.legs.some((l) => l.conclusion === "failure" || l.conclusion === "cancelled" || l.conclusion === "timed_out");
  const [mode, setMode] = React.useState<string>(failed ? "failed" : "all");
  const [debug, setDebug] = React.useState(false);
  const [keepFailed, setKeepFailed] = React.useState(false);
  const [confirm, setConfirm] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const debugId = React.useId();
  const keepId = React.useId();

  const go = async (fn: () => Promise<unknown>, ok: string) => {
    setBusy(true);
    try {
      await fn();
      toast({ title: ok, tone: "info" });
      invalidate(`run/${run.id}`);
      invalidate(`ci-run/${run.id}`);
      invalidate("runs/");
    } catch (e) {
      toast.error("Action failed", errorMessage(e));
    } finally {
      setBusy(false);
      setConfirm(false);
    }
  };

  if (active) {
    return (
      <div className="flex flex-col items-start gap-3 md:items-end">
        <DeployReview detail={detail} />
        <Button variant="danger" disabled={busy} aria-expanded={confirm} onClick={() => setConfirm(!confirm)}>
          <Square /> Cancel
        </Button>
        {confirm && (
          <Confirm
            label={`Cancel ${run.repo} #${run.number}?`}
            tone="danger"
            actions={
              <>
                <Button size="xs" variant="danger" disabled={busy} onClick={() => go(() => ci.cancel(run.id), "Run cancelled")}>
                  Cancel run
                </Button>
                <Button size="xs" variant="ghost" onClick={() => setConfirm(false)}>
                  Keep running
                </Button>
              </>
            }
          >
            <p className="font-medium text-[#e4e5e7]">{`Cancel ${run.repo} #${run.number}?`}</p>
            <p>Running steps are interrupted; steps that run always (and post steps) still run.</p>
          </Confirm>
        )}
      </div>
    );
  }

  const options = [...(failed ? [{ value: "failed", label: "Failed jobs" }] : []), { value: "all", label: "All jobs" }, ...jobs.map((j) => ({ value: `job:${j}`, label: `Job: ${j}` }))];
  const [m, job] = mode.startsWith("job:") ? (["job", mode.slice(4)] as const) : ([mode as RerunMode, null] as const);
  return (
    <div className="flex flex-wrap items-center gap-2 md:justify-end">
      <SelectField label="What to rerun" hideLabel value={mode} onChange={setMode} options={options} className="w-48" />
      <label htmlFor={debugId} className="inline-flex min-h-9 items-center gap-2 text-[12.5px] text-[#b0b4be]">
        <input id={debugId} type="checkbox" className="check" checked={debug} onChange={(e) => setDebug(e.target.checked)} />
        Debug logging
      </label>
      <label htmlFor={keepId} className="inline-flex min-h-9 items-center gap-2 text-[12.5px] text-[#b0b4be]" title="A leg that fails is kept for 30 minutes to look into, with a terminal">
        <input id={keepId} type="checkbox" className="check" checked={keepFailed} onChange={(e) => setKeepFailed(e.target.checked)} />
        Keep failed legs
      </label>
      <Button variant="primary" disabled={busy} onClick={() => go(() => ci.rerun(run.id, m, job, debug, keepFailed), `Attempt ${run.attempt + 1} started`)}>
        <RotateCcw /> Rerun
      </Button>
    </div>
  );
}

/** The deployments a run waits on a review for, decided in place in one step (W15-R04). */
function DeployReview({ detail }: { detail: CiRunDetail }) {
  const { run } = detail;
  const envs = [...new Set(detail.legs.map((l) => reviewOf(l.wait_reason)).filter((e): e is string => !!e))];
  const [comment, setComment] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  if (envs.length === 0) return null;
  const decide = async (state: "approved" | "rejected") => {
    setBusy(true);
    try {
      await ci.approve(run.id, envs, state, comment);
      toast({ title: state === "approved" ? `Approved: deploying to ${envs.join(", ")}` : "Deployment rejected", tone: "info" });
      invalidate(`run/${run.id}`);
      invalidate(`ci-run/${run.id}`);
    } catch (e) {
      toast.error("Review failed", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const label = `Review the deployment to ${envs.join(", ")}`;
  return (
    <Confirm
      label={label}
      className="w-full md:w-96"
      actions={
        <>
          <Button size="xs" variant="primary" disabled={busy} onClick={() => decide("approved")}>
            Approve and deploy
          </Button>
          <Button size="xs" variant="ghost" disabled={busy} onClick={() => decide("rejected")}>
            Reject
          </Button>
        </>
      }
    >
      <p className="font-medium text-[#e4e5e7]">{`Waiting for a review to deploy to ${envs.join(", ")}`}</p>
      <TextField label="Comment" placeholder="Optional" value={comment} onChange={(e) => setComment(e.target.value)} />
    </Confirm>
  );
}

/** What the run left: summaries, annotations, artifacts, and the actions it pinned. */
export function CiRunParts({ detail, attempt }: { detail: CiRunDetail; attempt: number | undefined }) {
  const id = detail.run.id;
  const summaries = useSuspenseResource(`ci-run/${id}/summary/${attempt ?? ""}`, (s) => ci.summaries(id, attempt, s)).data.summaries;
  const annotations = useSuspenseResource(`ci-run/${id}/annotations/${attempt ?? ""}`, (s) => ci.annotations(id, attempt, s)).data.annotations;
  const artifacts = useSuspenseResource(`ci-run/${id}/artifacts`, (s) => ci.artifacts(id, s)).data.artifacts;
  return (
    <div className="mt-6 flex flex-col gap-4">
      {summaries.length > 0 && (
        <Panel eyebrow={`Summaries · ${summaries.length}`} title="Job summaries">
          <div className="flex flex-col gap-6">
            {summaries.map((s) => (
              <section key={`${s.leg}-${s.step}`} aria-label={`Summary of ${s.leg_key}`}>
                <div className="label mb-2">{s.leg_key}</div>
                <React.Suspense fallback={<p className="empty-line">Rendering…</p>}>
                  <Markdown text={s.markdown} />
                </React.Suspense>
              </section>
            ))}
          </div>
        </Panel>
      )}
      {annotations.length > 0 && <Annotations list={annotations} />}
      {artifacts.length > 0 && (
        <Panel eyebrow={`Artifacts · ${artifacts.length}`} title="Artifacts" bodyClassName="px-0 pb-1">
          <ul className="row-list">
            {artifacts.map((a) => (
              <li key={a.id} className="flex min-h-11 flex-wrap items-center gap-x-4 gap-y-1 px-5 py-2">
                <span className="min-w-0 flex-1 truncate text-[13px] text-[#d4dde8]">{a.name}</span>
                <span className="font-mono text-[11px] text-[#93a1b4] tabular-nums">{formatBytes(a.size)}</span>
                <span className={cn("font-mono text-[10px]", a.expired ? "text-[#d1aaad]" : "text-[#7f8a9a]")}>
                  {a.expired ? "expired" : a.expires_at ? `kept until ${new Date(a.expires_at * 1000).toLocaleDateString()}` : "kept"}
                </span>
              </li>
            ))}
          </ul>
        </Panel>
      )}
      {detail.actions.length > 0 && (
        <Panel
          eyebrow={`Actions · ${detail.actions.length}`}
          title="Pinned actions"
          description="Resolved when the run started; every leg and rerun of it uses these commits."
          bodyClassName="px-0 pb-1"
        >
          <ul className="row-list">
            {detail.actions.map((a) => (
              <li key={a.uses} className="flex min-h-10 flex-wrap items-center gap-x-4 gap-y-1 px-5 py-2 font-mono text-[11.5px]">
                <span className="min-w-0 flex-1 [overflow-wrap:anywhere] text-[#d4dde8]">{a.uses}</span>
                <span className="text-[#a6b2c6]" title={a.sha ?? ""}>
                  {a.sha?.slice(0, 12)}
                </span>
              </li>
            ))}
          </ul>
        </Panel>
      )}
    </div>
  );
}

const LEVEL_RANK: Record<string, number> = { error: 0, warning: 1, notice: 2 };

/** An annotation's level as a status dot: errors red, warnings amber, notices hollow. */
function levelDot(level: string): string {
  return level === "error" ? "failed" : level === "warning" ? "timed_out" : "pending";
}

/** Annotations, errors first, each with where it points. */
export function Annotations({ list, title = "Annotations" }: { list: CiAnnotation[]; title?: string }) {
  const sorted = [...list].sort((a, b) => (LEVEL_RANK[a.level] ?? 3) - (LEVEL_RANK[b.level] ?? 3));
  const counts = sorted.reduce<Record<string, number>>((m, a) => ((m[a.level] = (m[a.level] ?? 0) + 1), m), {});
  const summary = Object.entries(counts)
    .map(([l, n]) => `${n} ${l}${n === 1 ? "" : "s"}`)
    .join(" · ");
  return (
    <Panel eyebrow={summary} title={title} bodyClassName="px-0 pb-1">
      <ul className="row-list">
        {sorted.map((a, i) => (
          <li key={i} className="flex flex-col gap-1 px-5 py-2.5">
            <span className="flex min-w-0 items-center gap-2.5">
              <i className="dot" data-status={levelDot(a.level)} aria-hidden="true" />
              <span className="font-mono text-[10px] tracking-[0.06em] text-[#8c8f98] uppercase">{a.level}</span>
              <span className="min-w-0 truncate font-mono text-[10.5px] text-[#93a1b4]">{a.leg_key}</span>
            </span>
            <span className="text-[13px] leading-[1.55] [overflow-wrap:anywhere] whitespace-pre-wrap text-[#d4dde8]">
              {a.title ? <strong className="font-medium">{a.title}: </strong> : null}
              {a.message}
            </span>
            {a.file && (
              <span className="font-mono text-[11px] text-[#a6b2c6]">
                {a.file}
                {a.line ? `:${a.line}` : ""}
                {a.col ? `:${a.col}` : ""}
              </span>
            )}
          </li>
        ))}
      </ul>
    </Panel>
  );
}

/** A CI leg's own notes on its panel: a kept result, its annotations, and why it waits or where it
 * would run (W21-R05, W05-R20). */
export function CiLegExtras({
  runId,
  attempt,
  legId,
  status,
  reused,
  keptUntil,
}: {
  runId: string;
  attempt: number | undefined;
  legId: string;
  status: string;
  reused: boolean;
  keptUntil?: number | null;
}) {
  const annotations = useSuspenseResource(`ci-run/${runId}/annotations/${attempt ?? ""}`, (s) => ci.annotations(runId, attempt, s)).data.annotations.filter((a) => a.leg === legId);
  return (
    <div className="border-t border-[#28313d]">
      <LegDebug runId={runId} legId={legId} running={status === "running"} keptUntil={keptUntil ?? null} />
      {reused && <p className="px-5 py-3 font-mono text-[11px] text-[#a6b2c6]">Kept from the last attempt: this rerun didn't run it again.</p>}
      {status === "queued" && (
        <React.Suspense fallback={<p className="px-5 py-3 font-mono text-[11px] text-[#7f8a9a]">Asking the scheduler why it waits…</p>}>
          <WhyWaiting legId={legId} />
        </React.Suspense>
      )}
      {annotations.length > 0 && (
        <ul className="row-list">
          {annotations.map((a, i) => (
            <li key={i} className="flex flex-col gap-1 px-5 py-2.5">
              <span className="flex items-center gap-2.5">
                <i className="dot" data-status={levelDot(a.level)} aria-hidden="true" />
                <span className="font-mono text-[10px] tracking-[0.06em] text-[#8c8f98] uppercase">{a.level}</span>
                {a.step !== null && <span className="font-mono text-[10px] text-[#6d7078]">step {String(a.step + 1).padStart(2, "0")}</span>}
              </span>
              <span className="text-[13px] leading-[1.55] [overflow-wrap:anywhere] whitespace-pre-wrap text-[#d4dde8]">{a.message}</span>
              {a.file && (
                <span className="font-mono text-[11px] text-[#a6b2c6]">
                  {a.file}
                  {a.line ? `:${a.line}` : ""}
                  {a.col ? `:${a.col}` : ""}
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Why a queued leg waits, and each runner's checks against it. */
const noop = () => {};

/** A running or kept leg's terminal, and a kept one's time left with Extend and Release (W17-R01, R02). */
function LegDebug({ runId, legId, running, keptUntil }: { runId: string; legId: string; running: boolean; keptUntil: number | null }) {
  const [open, setOpen] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [state, setState] = React.useState<string | null>(null);
  const kept = keptUntil !== null && keptUntil * 1000 > Date.now();
  if (!running && !kept) return null;
  const minutes = kept ? Math.max(1, Math.round((keptUntil * 1000 - Date.now()) / 60000)) : 0;
  const act = async (fn: () => Promise<unknown>, ok: string) => {
    setBusy(true);
    try {
      await fn();
      toast({ title: ok, tone: "info" });
      invalidate(`run/${runId}`);
      invalidate(`ci-run/${runId}`);
    } catch (e) {
      toast.error("Action failed", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="border-b border-[#28313d]">
      <div className="flex flex-wrap items-center gap-2 px-5 py-3">
        <span className="mr-auto font-mono text-[11px] text-[#a6b2c6]">{kept ? `Kept to look into for ${minutes} more minute${minutes === 1 ? "" : "s"}` : "Running"}</span>
        <Button size="xs" variant={open ? "ghost" : "primary"} aria-expanded={open} onClick={() => setOpen(!open)}>
          {open ? "Close terminal" : "Open terminal"}
        </Button>
        {kept && (
          <>
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => act(() => ci.keep(legId, 30), "Kept 30 more minutes")}>
              Keep 30 more minutes
            </Button>
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => act(() => ci.release(legId), "Released")}>
              Release
            </Button>
          </>
        )}
      </div>
      {open && (
        <div className="leg-terminal">
          <React.Suspense fallback={<p className="px-5 py-3 font-mono text-[11px] text-[#7f8a9a]">Opening the terminal…</p>}>
            <TerminalSession
              tabId={legId}
              sessionId={null}
              target=""
              endpoint={`/api/ci/legs/${encodeURIComponent(legId)}/terminal`}
              active
              fontSize={13}
              copyOnSelect={false}
              deviceKeyboard
              onState={(_, s) => setState(s.kind === "error" ? s.message : s.kind === "exited" ? "The shell exited." : null)}
              onTitle={noop}
              onSession={noop}
              onClosed={() => setOpen(false)}
            />
          </React.Suspense>
        </div>
      )}
      {open && state && <p className="px-5 py-2 font-mono text-[11px] text-[#d1aaad]">{state}</p>}
    </div>
  );
}

function WhyWaiting({ legId }: { legId: string }) {
  const r = useSuspenseResource(`ci-routing/${legId}`, (s) => ci.routing(legId, s), { on: ["job"] }).data;
  const why = waitSentence(r.wait_reason);
  return (
    <div className="px-5 py-3">
      {why && <p className="text-[13px] text-[#d4dde8]">{why}</p>}
      {r.runners.length > 0 && (
        <ul className="mt-2 flex flex-col gap-1.5">
          {r.runners.map((x) => {
            const failing = x.checks.filter((c) => !c.pass);
            return (
              <li key={x.runner} className="font-mono text-[11px] leading-[1.6] text-[#a6b2c6]">
                <span className={x.eligible ? "text-[#b8d9d2]" : "text-[#d1aaad]"}>{x.name}</span>
                {x.eligible ? (x.room ? " — can take it" : " — eligible, busy") : ` — ${failing.map((c) => `needs ${c.requirement}, has ${c.value || "nothing"}`).join("; ")}`}
                {!x.connected && " (offline)"}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
