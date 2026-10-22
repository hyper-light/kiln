/**
 * A platform's build log, under the Builds section's tiles: the release run and job its build came
 * from (each a link), the job's steps, and the log of the step picked (the one running, else the one
 * that failed, else the last), streamed live while it runs, with the Runs page's own log viewer. A
 * machine's build of itself shows that upgrade's output; a build kiln has no log of says why.
 */
import * as React from "react";
import { api } from "@/api/client";
import type { Job, ReleaseBuild, Run, UpdateJob } from "@/api/types";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { StatusDot } from "@/components/ui/status";
import { formatDuration, spanSeconds } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { Link } from "@/lib/router";
import { buildLogOf, findRun, stepToShow, type BuildLog } from "./logs";
import { UpgradeLog } from "./upgrade-log";
import type { PlatformView } from "./words";

const LogViewer = React.lazy(() => import("@/views/run-detail/log-viewer"));

/** How many runs a page reads, looking for the one that built a platform. */
const PAGE = 200;

export function BuildLogPanel({ id, p, build, updateJobs }: { id: string; p: PlatformView; build: ReleaseBuild | undefined; updateJobs: UpdateJob[] }) {
  const log = buildLogOf(p, build?.last ?? null, updateJobs);
  return (
    <section id={id} className="up-blog" data-kind={log.kind} aria-label={`The ${p.label} build's log`}>
      {log.kind === "none" && <p className="up-blog-none">{log.why}</p>}
      {log.kind === "upgrade" && <UpgradeLog job={log.job} defaultOpen />}
      {log.kind === "job" && <JobLog key={log.job.id} run={log.run} job={log.job} />}
      {log.kind === "run" && (
        <Loadable label="its build's log" fallback={<Hold height={150} className="up-blog-hold" label="Loading its build's log" />}>
          <RunLog log={log} repo={build?.repo ?? ""} />
        </Loadable>
      )}
    </section>
  );
}

/** The release run that built it, found by its number when kiln doesn't have its id, and its job of the platform's family. */
function RunLog({ log, repo }: { log: Extract<BuildLog, { kind: "run" }>; repo: string }) {
  const found = useSuspenseResource(`updates/build-log/${repo}/${log.id ?? log.number}/${log.family}`, async (signal) => {
    const run = log.id ? { id: log.id } : await findRun((before) => api.runs({ repo, limit: PAGE, before }, signal), log.number, log.sha, PAGE);
    if (!run) return null;
    const d = await api.run(run.id, signal);
    return { run: d.run, job: d.jobs.find((j) => j.key === log.family) ?? null };
  }).data;
  if (!found) return <p className="up-blog-none">Release run #{log.number} isn&rsquo;t kept here any more, so its log is gone.</p>;
  if (!found.job)
    return (
      <p className="up-blog-none">
        Release run #{found.run.number} has no {log.family} job to show.
      </p>
    );
  return <JobLog key={found.job.id} run={found.run} job={found.job} />;
}

/** A job of a release run: where it ran, its steps, and the log of the one picked, live while it runs. */
function JobLog({ run, job }: { run: Run; job: Job }) {
  const now = useNow();
  // Following the job: the step running shows, until one is picked.
  const [chosen, setChosen] = React.useState<number | null>(null);
  const shown = chosen ?? stepToShow(job.steps);
  const step = job.steps.find((s) => s.idx === shown) ?? null;
  const took = spanSeconds(job.started_at, job.finished_at, now);
  return (
    <>
      <header className="up-blog-head">
        <StatusDot status={job.status} />
        <span className="up-blog-title">
          <Link to={`/runs/${run.id}`}>Release run #{run.number}</Link>
          <span aria-hidden="true"> · </span>
          <Link to={`/runs/${run.id}/jobs/${job.id}`}>{job.display}</Link>
        </span>
        <span className="up-blog-meta">
          {run.sha.slice(0, 7)}
          {job.agent ? ` · on ${job.agent}` : ""}
          {took !== null ? ` · ${formatDuration(took)}` : ""}
        </span>
      </header>
      {job.steps.length > 0 && (
        <ol className="up-blog-steps" aria-label="Its steps">
          {job.steps.map((s, i) => {
            const t = spanSeconds(s.started_at, s.finished_at, now);
            return (
              <li key={s.idx}>
                <button type="button" aria-pressed={s.idx === shown} onClick={() => setChosen(s.idx)}>
                  <StatusDot status={s.status} />
                  <span className="up-blog-n">{String(i + 1).padStart(2, "0")}</span>
                  <span className="up-blog-step">{s.name}</span>
                  <span className="up-blog-t">{t === null ? "—" : formatDuration(t)}</span>
                </button>
              </li>
            );
          })}
        </ol>
      )}
      {!step ? (
        <p className="up-blog-none">{job.status === "queued" || job.status === "pending" ? "Queued: its log starts once a machine takes the job." : "No steps reported yet."}</p>
      ) : step.status === "pending" || step.status === "queued" ? (
        <p className="up-blog-none">{job.status === "queued" || job.status === "pending" ? "Queued: its log starts once a machine takes the job." : "Waiting to start…"}</p>
      ) : step.status === "skipped" || step.status === "cancelled" ? (
        <p className="up-blog-none">This step didn&rsquo;t run.</p>
      ) : (
        <React.Suspense fallback={<Hold height={125} className="up-blog-hold" />}>
          <div className="up-blog-log">
            <LogViewer key={`${job.id}:${step.idx}`} jobId={job.id} step={step.idx} live={step.status === "running"} />
          </div>
        </React.Suspense>
      )}
    </>
  );
}
