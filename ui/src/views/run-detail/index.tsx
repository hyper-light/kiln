import * as React from "react";
import { Link, useNavigate, useParams, useSearchParams } from "@/lib/router";
import { ChevronRight, GitBranch, RotateCcw, Square, User, Workflow } from "lucide-react";
import { api } from "@/api/client";
import { ci, ciRun, waitSentence, type CiRunDetail } from "@/api/ci";
import { useServerEvents } from "@/api/events";
import type { Job, RunDetail as RunData, Status, Step } from "@/api/types";
import { Button } from "@/components/ui/button";
import { ErrorBoundary } from "@/components/ui/error-boundary";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, Crumbs, EmptyState, ErrorState, Facts, Panel } from "@/components/ui/misc";
import { Chips, StatusBadge, StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { upsert } from "@/lib/data";
import { absoluteTime, firstLine, formatBytes, formatDuration, isActive, shortSha, spanSeconds } from "@/lib/format";
import { errorMessage, useNow } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { EventIcon, LiveDuration, RelTime } from "../shared";
import { AttemptSelect, CiLegExtras, CiRunActions, CiRunParts, CompareLink, criticalKeys, EditWorkflowLink, WorkflowInsightsLink } from "./ci";
import { JobDag } from "./dag";

const LogViewer = React.lazy(() => import("./log-viewer"));

function pickDefaultJob(jobs: Job[]): Job | undefined {
  return jobs.find((j) => j.status === "failed" || j.status === "timed_out") ?? jobs.find((j) => j.status === "running") ?? jobs[0];
}

export default function RunDetail() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page">
      <Loadable label="the run" fallback={<RunHold />}>
        <RunBody id={id} />
      </Loadable>
    </div>
  );
}

/** A run, and null when there is no such run (pruned, or a wrong link). */
async function readRun(id: string, signal: AbortSignal, attempt?: number): Promise<RunData | null> {
  try {
    return await api.run(id, signal, attempt);
  } catch (e) {
    if ((e as { status?: number }).status === 404) return null;
    throw e;
  }
}

function RunBody({ id }: { id: string }) {
  const { jobId } = useParams();
  const navigate = useNavigate();
  // A CI run's attempt (docs/ci.md W05-R17): the latest unless the URL names one.
  const [search, setSearch] = useSearchParams();
  const asked = Number(search.get("attempt")) || undefined;
  const res = useSuspenseResource(`run/${id}/${asked ?? ""}`, (s) => readRun(id, s, asked));
  // The CI engine's view of the run; null for v1 pipelines.
  const ciRes = useSuspenseResource(`ci-run/${id}/${asked ?? ""}`, (s) => ciRun(id, asked, s));
  const ciDetail = ciRes.data;
  const [confirm, setConfirm] = React.useState<null | "cancel" | "retry-all">(null);
  const [busy, setBusy] = React.useState(false);
  const [firstPick, setFirstPick] = React.useState(() => (res.data ? pickDefaultJob(res.data.jobs)?.id : undefined));
  if (firstPick === undefined && res.data?.jobs.length) setFirstPick(pickDefaultJob(res.data.jobs)?.id);

  useServerEvents((ev) => {
    if (ev.type === "run" && ev.run.id === id) res.setData((d) => (d ? { ...d, run: ev.run } : d));
    if (ev.type === "job" && ev.job.run_id === id) res.setData((d) => (d ? { ...d, jobs: upsert(d.jobs, { ...ev.job, spec: d.jobs.find((j) => j.id === ev.job.id)?.spec }, (j) => j.id, false) } : d));
  });

  if (!res.data) {
    return (
      <>
        <title>Run · kiln</title>
        <BackLink />
        <div className="mt-6">
          <EmptyState
            icon={<Workflow size={24} />}
            title="Run not found."
            action={
              <Link to="/runs" className="text-link">
                Back to runs
              </Link>
            }
          >
            It may have been pruned, or the link is wrong.
          </EmptyState>
        </div>
      </>
    );
  }

  const { run, jobs } = res.data;
  // Without a job in the URL, the one picked first stays picked as statuses change (no jumping).
  const selected = jobs.find((j) => j.id === jobId) ?? jobs.find((j) => j.id === firstPick) ?? pickDefaultJob(jobs);
  const select = (jid: string) => navigate(`/runs/${id}/jobs/${jid}${asked ? `?attempt=${asked}` : ""}`, { replace: true });
  const shownAttempt = asked ?? ciDetail?.run.attempt ?? 1;
  const setAttempt = (n: number) => setSearch(n === ciDetail?.run.attempt ? {} : { attempt: String(n) }, { replace: true });

  const act = async (fn: () => Promise<void>, ok: string) => {
    setBusy(true);
    try {
      await fn();
      toast({ title: ok, tone: "info" });
      invalidate(`run/${id}`);
      invalidate("runs/");
    } catch (e) {
      toast.error("Action failed", errorMessage(e));
    } finally {
      setBusy(false);
      setConfirm(null);
    }
  };

  const failedCount = jobs.filter((j) => j.status === "failed" || j.status === "timed_out" || j.status === "cancelled").length;
  const active = isActive(run.status);

  return (
    <>
      <title>{`${run.repo_name} #${run.number} · kiln`}</title>
      <BackLink repo={run.repo_name} repoId={run.repo_id} />

      {/* header */}
      <header className="mt-5 flex flex-col gap-6 pb-7 md:flex-row md:items-start md:justify-between">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-3">
            <StatusBadge status={run.status} className="text-[12px]" />
            <span className="label">{run.pipeline}</span>
          </div>
          <h1 className="mt-3 text-[clamp(28px,3.2vw,44px)] leading-[1.1] font-[450] tracking-[-0.045em]">
            <span className="text-[#8c8f98]">{run.repo_name.split("/")[0]}/</span>
            {run.repo_name.split("/").slice(1).join("/")} <span className="text-[#8c8f98]">#{run.number}</span>
          </h1>
          <p className="mt-3 max-w-3xl text-[15px] leading-[1.7] text-[#b0b4be]" title={run.message}>
            {firstLine(run.message)}
          </p>
          <div className="mt-4 flex flex-wrap items-center gap-x-5 gap-y-2 font-mono text-[11px] text-[#9a9da6]">
            <span className="inline-flex items-center gap-1.5">
              <GitBranch size={12} className="text-[#7b7e88]" /> {run.branch}
            </span>
            <span className="text-[#a6b2c6]" title={run.sha}>
              {shortSha(run.sha)}
            </span>
            <span className="inline-flex items-center gap-1.5">
              <User size={12} className="text-[#7b7e88]" /> {run.author}
            </span>
            <span className="inline-flex items-center gap-1.5">
              <EventIcon event={run.event} /> {run.event.replace("_", " ")}
            </span>
            <span title={absoluteTime(run.created_at)}>
              created <RelTime ts={run.created_at} />
            </span>
            <span>
              took <LiveDuration start={run.started_at} end={run.finished_at} />
            </span>
            {run.concurrency_group && <span className="chip">group: {run.concurrency_group}</span>}
          </div>
          {run.error && (
            <div className="mt-4 max-w-3xl">
              <ErrorState error={run.error} compact />
            </div>
          )}
        </div>
        {ciDetail ? (
          <div className="flex flex-col items-start gap-3 md:items-end">
            <AttemptSelect current={ciDetail.run.attempt} shown={shownAttempt} onChange={setAttempt} />
            <div className="flex flex-wrap gap-4">
              <EditWorkflowLink detail={ciDetail} />
              <WorkflowInsightsLink detail={ciDetail} />
              <CompareLink detail={ciDetail} />
            </div>
            {shownAttempt === ciDetail.run.attempt && <CiRunActions detail={ciDetail} jobs={[...new Set(ciDetail.legs.map((l) => (l.job ?? "").split("/")[0] ?? "").filter((j) => j !== ""))]} />}
          </div>
        ) : (
          <div className="flex flex-col items-start gap-3 md:items-end">
            <div className="flex flex-wrap gap-2">
              {active && (
                <Button variant="danger" disabled={busy} aria-expanded={confirm === "cancel"} onClick={() => setConfirm(confirm === "cancel" ? null : "cancel")}>
                  <Square /> Cancel
                </Button>
              )}
              {!active && failedCount > 0 && (
                <Button variant="secondary" disabled={busy} onClick={() => act(() => api.retryRun(run.id, true), "Retrying failed jobs")}>
                  <RotateCcw /> Retry failed
                </Button>
              )}
              <Button variant={active ? "ghost" : "primary"} disabled={busy} aria-expanded={confirm === "retry-all"} onClick={() => setConfirm(confirm === "retry-all" ? null : "retry-all")}>
                <RotateCcw /> Retry all
              </Button>
            </div>
            {confirm === "cancel" && (
              <Confirm
                label={`Cancel ${run.repo_name} #${run.number}?`}
                tone="danger"
                actions={
                  <>
                    <Button size="xs" variant="danger" disabled={busy} onClick={() => act(() => api.cancelRun(run.id), "Run cancelled")}>
                      Cancel run
                    </Button>
                    <Button size="xs" variant="ghost" onClick={() => setConfirm(null)}>
                      Cancel
                    </Button>
                  </>
                }
              >
                <p className="font-medium text-[#e4e5e7]">{`Cancel ${run.repo_name} #${run.number}?`}</p>
                <p>Running jobs are killed (whole process group) and queued jobs are dropped.</p>
              </Confirm>
            )}
            {confirm === "retry-all" && (
              <Confirm
                label={`Retry every job in #${run.number}?`}
                actions={
                  <>
                    <Button size="xs" disabled={busy} onClick={() => act(() => api.retryRun(run.id, false), "Run re-queued")}>
                      Retry all
                    </Button>
                    <Button size="xs" variant="ghost" onClick={() => setConfirm(null)}>
                      Cancel
                    </Button>
                  </>
                }
              >
                <p className="font-medium text-[#e4e5e7]">{`Retry every job in #${run.number}?`}</p>
                <p>All jobs are re-queued from scratch, including those that passed.</p>
              </Confirm>
            )}
          </div>
        )}
      </header>

      {/* DAG */}
      <Panel eyebrow={`Jobs · ${jobs.length}`} title="Pipeline graph" actions={<JobTally jobs={jobs} />} bodyClassName="px-0 pb-3">
        {jobs.length === 0 ? (
          <p className="empty-line px-5 py-8 text-center">This run has no jobs (yet).</p>
        ) : ciDetail ? (
          <Loadable label="the graph" fallback={<Hold height={160} />}>
            <CiDag detail={ciDetail} attempt={asked} jobs={jobs} selected={selected?.id ?? null} onSelect={select} />
          </Loadable>
        ) : (
          <JobDag jobs={jobs} selected={selected?.id ?? null} onSelect={select} />
        )}
      </Panel>

      {/* jobs + selected job */}
      <div className="mt-6 grid gap-4 lg:grid-cols-[280px_minmax(0,1fr)]">
        <nav aria-label="Jobs" className="lg:sticky lg:top-[calc(var(--header-h)+16px)] lg:self-start">
          <div className="label mb-2 px-1">Jobs</div>
          <div className="row-list max-h-[70vh] overflow-y-auto">
            {jobs.map((j) => (
              <button
                key={j.id}
                onClick={() => select(j.id)}
                aria-current={selected?.id === j.id ? "true" : undefined}
                className={cn("relative flex min-h-11 w-full items-center gap-3 px-2 py-2 text-left transition-colors hover:bg-[#101114]", selected?.id === j.id && "bg-[#101114]")}
              >
                {selected?.id === j.id && <span className="absolute inset-y-0 left-0 w-px bg-[linear-gradient(180deg,#b8d9d2,#a6c4ed,#b8a6d5,#d6a2aa)]" />}
                <StatusDot status={j.status} />
                <span className="min-w-0 flex-1 truncate text-[12.5px] text-[#d4d6dc]">{j.display}</span>
                <span className="font-mono text-[10px] text-[#8c8f98]">
                  <LiveDuration start={j.started_at} end={j.finished_at} />
                </span>
              </button>
            ))}
          </div>
        </nav>
        <div className="min-w-0">
          {selected && (
            <Loadable label="the job" fallback={<Hold height={420} className="panel" />}>
              <ErrorBoundary resetKey={selected.id} compact label="Job view crashed">
                <JobPanel
                  key={selected.id}
                  job={selected}
                  ciDetail={ciDetail}
                  attempt={asked}
                  onJobPatch={(j) => res.setData((d) => (d ? { ...d, jobs: upsert(d.jobs, j, (x) => x.id, false) } : d))}
                />
              </ErrorBoundary>
            </Loadable>
          )}
        </div>
      </div>

      {ciDetail && (
        <Loadable label="the run's summaries and annotations" fallback={<Hold height={120} className="mt-6" />}>
          <CiRunParts detail={ciDetail} attempt={asked} />
        </Loadable>
      )}
    </>
  );
}

/** A CI run's graph: the critical path drawn heavier, wait reasons and kept legs on the nodes. */
function CiDag({ detail, attempt, jobs, selected, onSelect }: { detail: CiRunDetail; attempt: number | undefined; jobs: Job[]; selected: string | null; onSelect: (id: string) => void }) {
  const graph = useSuspenseResource(`ci-run/${detail.run.id}/graph/${attempt ?? ""}`, (s) => ci.graph(detail.run.id, attempt, s)).data;
  const critical = React.useMemo(() => criticalKeys(detail, graph.critical_path), [detail, graph]);
  const notes = React.useMemo(() => new Map(detail.legs.map((l) => [l.id, { wait: l.status === "queued" ? waitSentence(l.wait_reason) : null, reused: !!l.reused_from }])), [detail]);
  return <JobDag jobs={jobs} selected={selected} onSelect={onSelect} critical={critical} notes={notes} />;
}

function BackLink({ repo, repoId }: { repo?: string; repoId?: string }) {
  return <Crumbs index={2} items={[{ label: "Runs", to: "/runs" }, ...(repo ? [{ label: repo, to: `/runs?repo=${encodeURIComponent(repoId ?? repo)}` }] : [])]} />;
}

/** The run while it loads: the way back, the header's and the graph's space, the jobs' frame. */
function RunHold() {
  return (
    <>
      <title>Run · kiln</title>
      <BackLink />
      <Hold height={206} className="mt-5 mb-7" label="Loading the run" />
      <Panel eyebrow="Jobs" title="Pipeline graph" bodyClassName="px-0 pb-3">
        <Hold height={160} />
      </Panel>
      <div className="mt-6 grid gap-4 lg:grid-cols-[280px_minmax(0,1fr)]">
        <div>
          <div className="label mb-2 px-1">Jobs</div>
          <RowsHold rows={4} rowHeight={44} />
        </div>
        <Hold height={420} className="panel" />
      </div>
    </>
  );
}

function JobTally({ jobs }: { jobs: Job[] }) {
  const counts = new Map<Status, number>();
  jobs.forEach((j) => counts.set(j.status, (counts.get(j.status) ?? 0) + 1));
  return (
    <div className="hidden flex-wrap items-center gap-3 sm:flex">
      {[...counts.entries()].map(([s, n]) => (
        <span key={s} className="status font-mono text-[10px] tabular-nums">
          <i className="dot" data-status={s} aria-hidden="true" />
          {n} {s.replace("_", " ")}
        </span>
      ))}
    </div>
  );
}

/** The step to open first: the one that failed, else the one running, else the last of a job not passed. */
function defaultOpen(job: Job): Set<number> {
  const s = job.steps;
  const failed = s.find((x) => x.status === "failed" || x.status === "timed_out");
  const running = s.find((x) => x.status === "running");
  const pick = failed ?? running ?? (job.status === "succeeded" ? undefined : s[s.length - 1]);
  return new Set(pick ? [pick.idx] : []);
}

function JobPanel({ job, ciDetail, attempt, onJobPatch }: { job: Job; ciDetail: CiRunDetail | null; attempt: number | undefined; onJobPatch: (j: Job) => void }) {
  const now = useNow();
  // The job as planned (its steps' commands), read once.
  const planned = useSuspenseResource(`job/${job.id}`, (s) => api.job(job.id, s)).data;
  const spec = job.spec ?? planned.spec;
  const [specOpen, setSpecOpen] = React.useState(false);
  // `?step=` (a log search's hit) opens that step too.
  const [search] = useSearchParams();
  const asked = search.get("step");
  const [open, setOpen] = React.useState<Set<number>>(() => (asked !== null && Number.isInteger(Number(asked)) ? new Set([...defaultOpen(job), Number(asked)]) : defaultOpen(job)));

  // A step that starts running opens (the job is followed as it goes).
  const running = job.steps.find((s) => s.status === "running")?.idx ?? null;
  const [runningSeen, setRunningSeen] = React.useState(running);
  if (running !== runningSeen) {
    setRunningSeen(running);
    if (running !== null && !open.has(running)) setOpen(new Set([...open, running]));
  }

  const wait = job.queued_at && job.started_at ? job.started_at - job.queued_at : job.queued_at && !job.started_at ? now / 1000 - job.queued_at : null;

  return (
    <section className="panel" aria-label={`Job ${job.display}`}>
      <div className="panel-header">
        <div className="min-w-0">
          <span className="panel-eyebrow">Job · {job.key}</span>
          <h2 className="panel-title flex items-center gap-2.5">
            <StatusDot status={job.status} />
            <span className="truncate">{job.display}</span>
          </h2>
          {job.error && <p className="mt-2 font-mono text-[11px] text-[#d1aaad]">{job.error}</p>}
        </div>
        <div className="text-right">
          <div className="font-mono text-[18px] tracking-[-0.04em] text-[#cfdae7] tabular-nums">
            <LiveDuration start={job.started_at} end={job.finished_at} />
          </div>
          <div className="label">duration</div>
        </div>
      </div>
      <div className="panel-body">
        <Facts
          items={[
            { label: "Agent", value: job.agent ?? "—" },
            { label: "Executor", value: job.executor ?? "—" },
            { label: "Attempt", value: job.attempt, tone: job.attempt > 1 ? "error" : undefined },
            { label: "Queue wait", value: wait === null ? "—" : formatDuration(wait), tone: job.status === "queued" ? "pending" : undefined },
            { label: "Peak RSS", value: formatBytes(job.peak_rss) },
            { label: "CPU time", value: job.cpu_seconds === null ? "—" : `${job.cpu_seconds.toFixed(1)}s` },
            {
              label: "Exit code",
              value: job.exit_code ?? "—",
              tone: job.exit_code === null ? undefined : job.exit_code === 0 ? "pass" : "fail",
            },
            { label: "Labels", value: job.labels.length ? <Chips labels={job.labels} /> : "—" },
          ]}
        />
        {job.needs.length > 0 && (
          <div className="mt-4 flex flex-wrap items-center gap-2 font-mono text-[10px] text-[#93a1b4]">
            needs{" "}
            {collapseNeeds(job.needs).map((n) => (
              <span key={n} className="chip">
                {n}
              </span>
            ))}
          </div>
        )}
      </div>

      {ciDetail && (
        <Loadable label="the leg's notes" fallback={null}>
          <CiLegExtras
            runId={ciDetail.run.id}
            attempt={attempt}
            legId={job.id}
            status={job.status}
            reused={(() => {
              const l = ciDetail.legs.find((x) => x.id === job.id);
              return l?.reused_from ? { leg: l.reused_from, run: l.reused_run ?? null, number: l.reused_number ?? null } : null;
            })()}
            keptUntil={ciDetail.legs.find((l) => l.id === job.id)?.kept_until}
          />
        </Loadable>
      )}

      {/* steps */}
      <div className="border-t border-[#28313d]">
        {job.steps.length === 0 && <p className="empty-line px-5 py-6">No steps reported yet.</p>}
        {inRunOrder(job.steps).map((s, i) => (
          <StepRow
            key={s.idx}
            position={i + 1}
            jobId={job.id}
            step={s}
            open={open.has(s.idx)}
            onToggle={() => {
              const n = new Set(open);
              if (n.has(s.idx)) n.delete(s.idx);
              else n.add(s.idx);
              setOpen(n);
            }}
            onStepStatus={(idx, status) => onJobPatch({ ...job, steps: job.steps.map((x) => (x.idx === idx ? { ...x, status } : x)) })}
            command={spec?.steps[s.idx]?.run}
            now={now}
            onRerun={keptLeg(ciDetail, job.id) && s.status === "failed" ? () => rerunInPlace(job.id, s.idx, i + 1) : undefined}
          />
        ))}
      </div>

      {spec && (
        <div className="panel-caption">
          <button className="inline-flex min-h-8 items-center gap-2 hover:text-white" onClick={() => setSpecOpen((o) => !o)} aria-expanded={specOpen}>
            <ChevronRight size={12} className={cn("transition-transform", specOpen && "rotate-90")} /> Planned spec
            <span className="font-mono text-[10px] text-[#7f8a9a]">
              {spec.toolchain ?? spec.image ?? "shell"} · timeout {spec.timeout_minutes}m · retries {spec.retries}
            </span>
          </button>
          {specOpen && (
            <pre className="mt-3 max-h-96 overflow-y-auto rounded-md border border-[#2b2e39] bg-[#101217] p-4 text-[11px] leading-relaxed [overflow-wrap:anywhere] whitespace-pre-wrap text-[#bcc3d1]">
              {JSON.stringify(spec, null, 2)}
            </pre>
          )}
        </div>
      )}
    </section>
  );
}

/** Steps as they ran: by when each started, those yet to start after, in their order. Steps a run
 * adds as it goes (an implicit checkout, pre and post steps) come after the job's own by number. */
function inRunOrder(steps: Step[]): Step[] {
  return [...steps].sort((a, b) => (a.started_at ?? Infinity) - (b.started_at ?? Infinity) || (a.seq ?? Infinity) - (b.seq ?? Infinity) || a.idx - b.idx);
}

/** Whether a leg is kept to look into now (W17-R02). */
function keptLeg(detail: CiRunDetail | null | undefined, legId: string): boolean {
  const until = detail?.legs.find((l) => l.id === legId)?.kept_until;
  return !!until && until * 1000 > Date.now();
}

/** A kept leg's failed step, run again in place (W17-R04). */
async function rerunInPlace(legId: string, idx: number, position: number) {
  try {
    const r = await ci.rerunStep(legId, idx);
    toast({ title: `Step ${position} runs again, as step ${r.idx + 1}`, tone: "info" });
  } catch (e) {
    toast.error("Rerun failed", errorMessage(e));
  }
}

function StepRow({
  position,
  jobId,
  step,
  open,
  onToggle,
  onStepStatus,
  command,
  now,
  onRerun,
}: {
  position: number;
  jobId: string;
  step: Step;
  open: boolean;
  onToggle: () => void;
  onStepStatus: (idx: number, s: Status) => void;
  command?: string;
  now: number;
  /** A kept leg's failed step can run again in place. */
  onRerun?: () => void;
}) {
  const dur = spanSeconds(step.started_at, step.finished_at, now);
  const notStarted = step.status === "pending" || step.status === "queued";
  return (
    <div className="border-b border-[#1d2128] last:border-b-0">
      <button className="flex min-h-11 w-full items-center gap-3 px-5 py-2 text-left transition-colors hover:bg-[#0e1014]" onClick={onToggle} aria-expanded={open}>
        <ChevronRight size={13} className={cn("shrink-0 text-[#7f8a9a] transition-transform duration-200", open && "rotate-90")} />
        <StatusDot status={step.status} />
        <span className="font-mono text-[10px] text-[#6d7078]">{String(position).padStart(2, "0")}</span>
        <span className="min-w-0 flex-1 truncate text-[13px] text-[#d4dde8]">{step.name}</span>
        {step.exit_code !== null && step.exit_code !== 0 && <span className="font-mono text-[10px] text-[#d1aaad]">exit {step.exit_code}</span>}
        <span className={cn("font-mono text-[11px] tabular-nums", step.status === "running" ? "text-[#d8d0f2]" : "text-[#93a1b4]")}>{dur === null ? "—" : formatDuration(dur)}</span>
      </button>
      {open && (
        <div>
          {onRerun && (
            <div className="flex items-center gap-3 px-5 pb-2">
              <span className="font-mono text-[11px] text-[#7f8a9a]">Kept: fix what failed in the leg&rsquo;s terminal, then</span>
              <Button size="xs" variant="ghost" onClick={onRerun}>
                Rerun this step in place
              </Button>
            </div>
          )}
          {command && (
            <pre className="mx-5 mb-2 rounded border border-[#22262d] bg-[#0d0f13] px-3 py-2 font-mono text-[11px] [overflow-wrap:anywhere] whitespace-pre-wrap text-[#a6b2c6]">
              <span className="text-[#6d7078] select-none">$ </span>
              {command}
            </pre>
          )}
          {notStarted ? (
            <p className="border-t border-[#1d2128] px-5 py-4 font-mono text-[11px] text-[#7f8a9a]">Waiting to start…</p>
          ) : (
            <React.Suspense fallback={<Hold height={125} className="border-t border-[#1d2128]" />}>
              <LogViewer jobId={jobId} step={step.idx} live={step.status === "running"} onStepStatus={onStepStatus} />
            </React.Suspense>
          )}
        </div>
      )}
    </div>
  );
}

/** `pods (a=0, b=1)` … `pods (a=7, b=5)` → `pods × 48`. */
function collapseNeeds(needs: string[]): string[] {
  const counts = new Map<string, number>();
  for (const n of needs) {
    const i = n.indexOf(" (");
    const id = i > 0 ? n.slice(0, i) : n;
    counts.set(id, (counts.get(id) ?? 0) + 1);
  }
  return [...counts].map(([id, c]) => (c > 1 ? `${id} × ${c}` : (needs.find((n) => n.startsWith(id)) ?? id)));
}
