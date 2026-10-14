import * as React from "react";
import { Link, useNavigate, useParams } from "react-router";
import { ArrowLeft, ChevronRight, GitBranch, RotateCcw, Square, User, Workflow } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Job, PlannedJob, Status, Step } from "@/api/types";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { ErrorBoundary } from "@/components/ui/error-boundary";
import { EmptyState, ErrorState, Facts, Panel, Skeleton } from "@/components/ui/misc";
import { Chips, StatusBadge, StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { absoluteTime, firstLine, formatBytes, formatDuration, isActive, shortSha, spanSeconds } from "@/lib/format";
import { errorMessage, useDocumentTitle, useNow, useResource } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { EventIcon, LiveDuration, RelTime, upsert } from "../shared";
import { JobDag } from "./dag";

const LogViewer = React.lazy(() => import("./log-viewer"));

function pickDefaultJob(jobs: Job[]): Job | undefined {
  return (
    jobs.find((j) => j.status === "failed" || j.status === "timed_out") ??
    jobs.find((j) => j.status === "running") ??
    jobs[0]
  );
}

export default function RunDetail() {
  const { id = "", jobId } = useParams();
  const navigate = useNavigate();
  const res = useResource((s) => api.run(id, s), [id]);
  const [confirm, setConfirm] = React.useState<null | "cancel" | "retry-all">(null);
  const [busy, setBusy] = React.useState(false);

  const run = res.data?.run;
  const jobs = React.useMemo(() => res.data?.jobs ?? [], [res.data?.jobs]);
  useDocumentTitle(run ? `${run.repo_name} #${run.number}` : "Run");

  useServerEvents((ev) => {
    if (ev.type === "run" && ev.run.id === id) res.setData((d) => (d ? { ...d, run: ev.run } : d));
    if (ev.type === "job" && ev.job.run_id === id)
      res.setData((d) => (d ? { ...d, jobs: upsert(d.jobs, { ...ev.job, spec: d.jobs.find((j) => j.id === ev.job.id)?.spec }, (j) => j.id, false) } : d));
  });

  const selected = jobs.find((j) => j.id === jobId) ?? pickDefaultJob(jobs);
  const select = (jid: string) => navigate(`/runs/${id}/jobs/${jid}`, { replace: true });

  const act = async (fn: () => Promise<void>, ok: string) => {
    setBusy(true);
    try {
      await fn();
      toast({ title: ok, tone: "info" });
      res.reload();
    } catch (e) {
      toast.error("Action failed", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  if (res.error && !res.data) {
    return (
      <div className="container-console page">
        <BackLink />
        <div className="mt-6">
          {(res.error as { status?: number }).status === 404 ? (
            <EmptyState icon={<Workflow size={24} />} title="Run not found." action={<Link to="/runs" className="text-link">Back to runs</Link>}>
              It may have been pruned, or the link is wrong.
            </EmptyState>
          ) : (
            <ErrorState error={res.error} onRetry={res.reload} />
          )}
        </div>
      </div>
    );
  }

  const failedCount = jobs.filter((j) => j.status === "failed" || j.status === "timed_out" || j.status === "cancelled").length;
  const active = run ? isActive(run.status) : false;

  return (
    <div className="container-console page">
      <BackLink repo={run?.repo_name} repoId={run?.repo_id} />

      {/* header */}
      <header className="mt-5 flex flex-col gap-6 pb-7 md:flex-row md:items-start md:justify-between">
        <div className="min-w-0 flex-1">
          {run ? (
            <>
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
            </>
          ) : (
            <>
              <Skeleton className="h-4 w-32" />
              <Skeleton className="mt-4 h-10 w-96 max-w-full" />
              <Skeleton className="mt-4 h-4 w-[28rem] max-w-full" />
            </>
          )}
        </div>
        {run && (
          <div className="flex flex-wrap gap-2">
            {active && (
              <Button variant="danger" disabled={busy} onClick={() => setConfirm("cancel")}>
                <Square /> Cancel
              </Button>
            )}
            {!active && failedCount > 0 && (
              <Button variant="secondary" disabled={busy} onClick={() => act(() => api.retryRun(run.id, true), "Retrying failed jobs")}>
                <RotateCcw /> Retry failed
              </Button>
            )}
            <Button variant={active ? "ghost" : "primary"} disabled={busy} onClick={() => setConfirm("retry-all")}>
              <RotateCcw /> Retry all
            </Button>
          </div>
        )}
      </header>

      {/* DAG */}
      <Panel
        eyebrow={`Jobs · ${jobs.length}`}
        title="Pipeline graph"
        actions={<JobTally jobs={jobs} />}
        bodyClassName="px-0 pb-3"
      >
        {!res.data ? (
          <div className="px-5">
            <Skeleton className="h-40 w-full" />
          </div>
        ) : jobs.length === 0 ? (
          <p className="px-5 py-8 text-center text-[13px] text-[#8f939e]">This run has no jobs (yet).</p>
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
                className={cn(
                  "relative flex min-h-11 w-full items-center gap-3 px-2 py-2 text-left transition-colors hover:bg-[#101114]",
                  selected?.id === j.id && "bg-[#101114]",
                )}
              >
                {selected?.id === j.id && <span className="absolute inset-y-0 left-0 w-px bg-[linear-gradient(180deg,#b8d9d2,#a6c4ed,#b8a6d5,#d6a2aa)]" />}
                <StatusDot status={j.status} />
                <span className="min-w-0 flex-1 truncate text-[12.5px] text-[#d4d6dc]">{j.display}</span>
                <span className="font-mono text-[10px] text-[#8c8f98]">
                  <LiveDuration start={j.started_at} end={j.finished_at} />
                </span>
              </button>
            ))}
            {!res.data && Array.from({ length: 4 }, (_, i) => <Skeleton key={i} className="my-2 h-7" />)}
          </div>
        </nav>
        <div className="min-w-0">
          {selected ? (
            <ErrorBoundary resetKey={selected.id} compact label="Job view crashed">
              <JobPanel key={selected.id} job={selected} onJobPatch={(j) => res.setData((d) => (d ? { ...d, jobs: upsert(d.jobs, j, (x) => x.id, false) } : d))} />
            </ErrorBoundary>
          ) : res.data ? null : (
            <Skeleton className="h-96 w-full" />
          )}
        </div>
      </div>

      {run && (
        <>
          <ConfirmDialog
            open={confirm === "cancel"}
            onOpenChange={(o) => !o && setConfirm(null)}
            title={`Cancel ${run.repo_name} #${run.number}?`}
            description="Running jobs are killed (whole process group) and queued jobs are dropped."
            confirmLabel="Cancel run"
            danger
            onConfirm={() => act(() => api.cancelRun(run.id), "Run cancelled")}
          />
          <ConfirmDialog
            open={confirm === "retry-all"}
            onOpenChange={(o) => !o && setConfirm(null)}
            title={`Retry every job in #${run.number}?`}
            description="All jobs are re-queued from scratch, including those that passed."
            confirmLabel="Retry all"
            onConfirm={() => act(() => api.retryRun(run.id, false), "Run re-queued")}
          />
        </>
      )}
    </div>
  );
}

function BackLink({ repo, repoId }: { repo?: string; repoId?: string }) {
  return (
    <nav aria-label="Breadcrumb" className="flex items-center gap-2 font-mono text-[10px] tracking-[0.08em] text-[#8c8f98] uppercase">
      <Link to="/runs" className="inline-flex min-h-11 items-center gap-2 transition-colors hover:text-white">
        <ArrowLeft size={13} /> Runs
      </Link>
      {repo && (
        <>
          <ChevronRight size={11} className="text-[#494e58]" />
          <Link to={`/runs?repo=${encodeURIComponent(repoId ?? repo)}`} className="inline-flex min-h-11 items-center transition-colors hover:text-white">
            {repo}
          </Link>
        </>
      )}
    </nav>
  );
}

function JobTally({ jobs }: { jobs: Job[] }) {
  const counts = new Map<Status, number>();
  jobs.forEach((j) => counts.set(j.status, (counts.get(j.status) ?? 0) + 1));
  return (
    <div className="hidden flex-wrap items-center gap-3 sm:flex">
      {[...counts.entries()].map(([s, n]) => (
        <span key={s} className="status font-mono text-[10px]">
          <i className="dot" data-status={s} aria-hidden="true" />
          {n} {s.replace("_", " ")}
        </span>
      ))}
    </div>
  );
}

function JobPanel({ job, onJobPatch }: { job: Job; onJobPatch: (j: Job) => void }) {
  const now = useNow();
  const [spec, setSpec] = React.useState<PlannedJob | undefined>(job.spec);
  const [specOpen, setSpecOpen] = React.useState(false);

  React.useEffect(() => {
    if (job.spec) return;
    const ctl = new AbortController();
    api
      .job(job.id, ctl.signal)
      .then((j) => setSpec(j.spec))
      .catch(() => undefined);
    return () => ctl.abort();
  }, [job.id, job.spec]);

  const defaultOpen = React.useMemo(() => {
    const s = job.steps;
    const failed = s.find((x) => x.status === "failed" || x.status === "timed_out");
    const running = s.find((x) => x.status === "running");
    const pick = failed ?? running ?? (job.status === "succeeded" ? undefined : s[s.length - 1]);
    return new Set(pick ? [pick.idx] : []);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [job.id]);
  const [open, setOpen] = React.useState<Set<number>>(defaultOpen);

  // Auto-expand a step when it starts running (follow the job as it progresses).
  const prevRunning = React.useRef<number | null>(null);
  React.useEffect(() => {
    const running = job.steps.find((s) => s.status === "running");
    if (running && running.idx !== prevRunning.current) {
      prevRunning.current = running.idx;
      setOpen((o) => new Set([...o, running.idx]));
    }
  }, [job.steps]);

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
            needs {job.needs.map((n) => <span key={n} className="chip">{n}</span>)}
          </div>
        )}
      </div>

      {/* steps */}
      <div className="border-t border-[#28313d]">
        {job.steps.length === 0 && <p className="px-5 py-6 text-[12.5px] text-[#8f939e]">No steps reported yet.</p>}
        {job.steps.map((s) => (
          <StepRow
            key={s.idx}
            jobId={job.id}
            step={s}
            open={open.has(s.idx)}
            onToggle={() =>
              setOpen((o) => {
                const n = new Set(o);
                if (n.has(s.idx)) n.delete(s.idx);
                else n.add(s.idx);
                return n;
              })
            }
            onStepStatus={(idx, status) => onJobPatch({ ...job, steps: job.steps.map((x) => (x.idx === idx ? { ...x, status } : x)) })}
            command={spec?.steps[s.idx]?.run}
            now={now}
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
            <pre className="mt-3 max-h-96 overflow-auto rounded-md border border-[#2b2e39] bg-[#101217] p-4 text-[11px] leading-relaxed text-[#bcc3d1]">
              {JSON.stringify(spec, null, 2)}
            </pre>
          )}
        </div>
      )}
    </section>
  );
}

function StepRow({
  jobId,
  step,
  open,
  onToggle,
  onStepStatus,
  command,
  now,
}: {
  jobId: string;
  step: Step;
  open: boolean;
  onToggle: () => void;
  onStepStatus: (idx: number, s: Status) => void;
  command?: string;
  now: number;
}) {
  const dur = spanSeconds(step.started_at, step.finished_at, now);
  const notStarted = step.status === "pending" || step.status === "queued";
  return (
    <div className="border-b border-[#1d2128] last:border-b-0">
      <button
        className="flex min-h-11 w-full items-center gap-3 px-5 py-2 text-left transition-colors hover:bg-[#0e1014]"
        onClick={onToggle}
        aria-expanded={open}
      >
        <ChevronRight size={13} className={cn("shrink-0 text-[#7f8a9a] transition-transform duration-200", open && "rotate-90")} />
        <StatusDot status={step.status} />
        <span className="font-mono text-[10px] text-[#6d7078]">{String(step.idx + 1).padStart(2, "0")}</span>
        <span className="min-w-0 flex-1 truncate text-[13px] text-[#d4dde8]">{step.name}</span>
        {step.exit_code !== null && step.exit_code !== 0 && <span className="font-mono text-[10px] text-[#d1aaad]">exit {step.exit_code}</span>}
        <span className={cn("font-mono text-[11px] tabular-nums", step.status === "running" ? "text-[#d8d0f2]" : "text-[#93a1b4]")}>
          {dur === null ? "—" : formatDuration(dur)}
        </span>
      </button>
      {open && (
        <div>
          {command && (
            <pre className="mx-5 mb-2 overflow-x-auto rounded border border-[#22262d] bg-[#0d0f13] px-3 py-2 font-mono text-[11px] text-[#a6b2c6]">
              <span className="text-[#6d7078] select-none">$ </span>
              {command}
            </pre>
          )}
          {notStarted ? (
            <p className="border-t border-[#1d2128] px-5 py-4 font-mono text-[11px] text-[#7f8a9a]">Waiting to start…</p>
          ) : (
            <React.Suspense fallback={<Skeleton className="h-40 w-full rounded-none" />}>
              <LogViewer jobId={jobId} step={step.idx} live={step.status === "running"} onStepStatus={onStepStatus} />
            </React.Suspense>
          )}
        </div>
      )}
    </div>
  );
}
