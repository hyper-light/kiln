/**
 * Which log a platform's build has, as the Builds section opens it under its tiles: the job of the
 * release run under way that builds it (followed live), the job of the release run that built it,
 * a machine's build of itself (its upgrade's output), or nothing kiln kept, said in a line. Pure:
 * which log and which step, and finding a run by its number; the page reads them.
 */
import type { Job, Run, Step, UpdateJob } from "@/api/types";
import { platformJob, type PlatformView } from "./words";

export type BuildLog =
  /** Its job of the release run under way, followed as it goes. */
  | { kind: "job"; run: Run; job: Job }
  /** A release run kiln has (`id`), or the one that built it, by its number and commit: its job of the platform's family. */
  | { kind: "run"; id: string | null; number: number; sha: string | null; family: string }
  /** A machine built it of itself: that upgrade's output. */
  | { kind: "upgrade"; job: UpdateJob }
  /** No log here, and why, in a line. */
  | { kind: "none"; why: string };

/** The log of `p`'s build: `last` the latest release run, `jobs` the upgrades kiln knows. */
export function buildLogOf(p: PlatformView, last: Run | null, jobs: UpdateJob[]): BuildLog {
  const family = platformJob(p.target);
  if (p.job && last) return { kind: "job", run: last, job: p.job };
  // A platform the last run failed to build: that run's job says why.
  if (p.state === "failed" && last) return { kind: "run", id: last.id, number: last.number, sha: last.sha, family };
  const b = p.binary;
  if (!b) return { kind: "none", why: "No build of it here yet, so there's no log to show." };
  switch (b.origin) {
    case "built":
      return b.run != null ? { kind: "run", id: null, number: b.run, sha: b.sha ?? null, family } : { kind: "none", why: "Built by kiln's release pipeline, but it didn't say which run." };
    case "machine": {
      const by = jobs
        .filter((j) => j.kind === "runner" && j.method === "build" && j.target === b.by && j.state !== "failed" && (!b.sha || !j.commit || j.commit.toLowerCase() === b.sha.toLowerCase()))
        .sort((x, y) => y.started_at - x.started_at)[0];
      return by ? { kind: "upgrade", job: by } : { kind: "none", why: `${b.by ?? "A machine"} built it of itself; kiln no longer keeps that build's output.` };
    }
    case "server":
      return { kind: "none", why: "The server's own binary: kiln didn't build it here, so there's no build log." };
    case "upload":
      return { kind: "none", why: "Uploaded here: it was built elsewhere, so there's no build log." };
    default:
      return { kind: "none", why: "Fetched from kiln's GitHub release: it was built there, so there's no build log here." };
  }
}

/** The step whose log shows first: the one running, else the one that failed, else the last that ran. */
export function stepToShow(steps: Step[]): number | null {
  const running = steps.find((s) => s.status === "running");
  if (running) return running.idx;
  const failed = steps.find((s) => s.status === "failed" || s.status === "timed_out");
  if (failed) return failed.idx;
  const ran = steps.filter((s) => s.started_at !== null);
  return ran[ran.length - 1]?.idx ?? steps[0]?.idx ?? null;
}

/** Runs a page at a time, newest first (`before`: older than that). */
export type RunsPage = (before: number | undefined) => Promise<Run[]>;

/**
 * Run `number` of a repository (its commit `sha`, when known, to be sure), paging back from the
 * newest: numbers only grow, so it stops once the runs read are older than it. Null: not kept.
 */
export async function findRun(page: RunsPage, number: number, sha: string | null, size: number, pages = 8): Promise<Run | null> {
  const want = sha?.toLowerCase() ?? "";
  // One commit, however much of it each names.
  const same = (s: string) => !want || s.toLowerCase().startsWith(want) || want.startsWith(s.toLowerCase());
  let before: number | undefined;
  for (let i = 0; i < pages; i++) {
    const runs = await page(before);
    const hit = runs.find((r) => r.number === number && same(r.sha));
    if (hit) return hit;
    const oldest = runs[runs.length - 1];
    if (!oldest || runs.length < size || oldest.number < number) return null;
    before = oldest.created_at;
  }
  return null;
}
