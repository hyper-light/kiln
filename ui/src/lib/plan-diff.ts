/**
 * A workflow's plan for an event as Ship's Pipelines says it (docs/ci.md W29-R07): its jobs, each
 * with its legs and the machines that can take them now; and a draft's plan against the published
 * one's for the same event, job by job: added, removed, changed, and how many are as they were.
 * Pure: the page passes in the plans W04-R29's preview made.
 */
import type { CiPreview } from "@/api/ci";

/** A job of a plan: how many legs, the labels they ask for, and the machines that can take one now. */
export interface PlanJob {
  job: string;
  legs: number;
  labels: string[];
  runners: string[];
  /** How many machines the routing weighed. */
  of: number;
}

/** A plan's jobs, in its legs' order. */
export function jobsOf(plan: CiPreview["plan"]): PlanJob[] {
  const out = new Map<string, PlanJob>();
  for (const l of plan?.legs ?? []) {
    const j = out.get(l.job) ?? { job: l.job, legs: 0, labels: [], runners: [], of: 0 };
    j.legs += 1;
    for (const label of l.runs_on.labels) if (!j.labels.includes(label)) j.labels.push(label);
    for (const r of l.routing?.runners ?? []) if (r.eligible && r.connected && !j.runners.includes(r.name)) j.runners.push(r.name);
    j.of = Math.max(j.of, l.routing?.runners.length ?? 0);
    out.set(l.job, j);
  }
  return [...out.values()];
}

/** A draft's plan against the published one's. */
export interface PlanDiff {
  added: PlanJob[];
  removed: PlanJob[];
  /** Jobs in both whose legs or labels differ. */
  changed: { now: PlanJob; was: PlanJob }[];
  /** The rest: as they were. */
  same: { jobs: number; legs: number };
}

const sameLabels = (a: readonly string[], b: readonly string[]) => a.length === b.length && [...a].sort().every((x, i) => x === [...b].sort()[i]);

export function planDiff(published: readonly PlanJob[], draft: readonly PlanJob[]): PlanDiff {
  const was = new Map(published.map((j) => [j.job, j]));
  const now = new Map(draft.map((j) => [j.job, j]));
  const diff: PlanDiff = { added: [], removed: [], changed: [], same: { jobs: 0, legs: 0 } };
  for (const j of draft) {
    const w = was.get(j.job);
    if (!w) diff.added.push(j);
    else if (w.legs !== j.legs || !sameLabels(w.labels, j.labels)) diff.changed.push({ now: j, was: w });
    else {
      diff.same.jobs += 1;
      diff.same.legs += j.legs;
    }
  }
  for (const j of published) if (!now.has(j.job)) diff.removed.push(j);
  return diff;
}

/** One line of what the pane says of a plan: a mark, the words, and a value on the right. */
export interface PlanLine {
  key: string;
  mark: "added" | "removed" | "changed" | "same" | "job";
  text: string;
  value: string;
}

const legs = (n: number) => (n === 1 ? "1 leg" : `${n} legs`);
const jobs = (n: number) => (n === 1 ? "1 job" : `${n} jobs`);
/** Where a job's legs can go now: `on fedora or adacachy (linux)`, or that nothing connected can take them. */
export const whereWords = (j: PlanJob) => {
  // W29-R04: bare metal is the default machine kind, rather than an extra routing label.
  const labels = j.labels.filter((l) => !["baremetal", "bare-metal"].includes(l));
  return j.runners.length
    ? `on ${j.runners.length > 2 ? `${j.runners.slice(0, -1).join(", ")} or ${j.runners.at(-1)}` : j.runners.join(" or ")}${labels.length ? ` (${labels.join(", ")})` : ""}`
    : `no machine connected can take ${labels.length ? labels.join(", ") : "it"} now`;
};

/** A plan's lines: each job, or (with the published plan) what changes and how much is as on `ref`. */
export function planLines(draft: readonly PlanJob[], published: readonly PlanJob[] | null, ref: string): PlanLine[] {
  if (!published) return draft.map((j) => ({ key: j.job, mark: "job", text: `${j.job}: ${legs(j.legs)}, ${whereWords(j)}`, value: legs(j.legs) }));
  const d = planDiff(published, draft);
  return [
    ...d.added.map((j): PlanLine => ({ key: `+${j.job}`, mark: "added", text: `${j.job}, a new job: ${legs(j.legs)}, ${whereWords(j)}`, value: "+1 job" })),
    ...d.removed.map((j): PlanLine => ({ key: `-${j.job}`, mark: "removed", text: `${j.job}, taken out: it ran ${legs(j.legs)}`, value: "−1 job" })),
    ...d.changed.map(({ now, was }): PlanLine => ({ key: `~${now.job}`, mark: "changed", text: `${now.job}: ${legs(now.legs)} (was ${was.legs}), ${whereWords(now)}`, value: "changed" })),
    ...(d.same.jobs
      ? [
          {
            key: "=",
            mark: "same" as const,
            text: `${d.added.length || d.removed.length || d.changed.length ? "the other " : ""}${jobs(d.same.jobs)} and ${legs(d.same.legs)} as on ${ref}`,
            value: "same",
          },
        ]
      : []),
  ];
}
