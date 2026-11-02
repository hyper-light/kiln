import assert from "node:assert/strict";
import { test } from "node:test";
import type { CiPreview } from "../src/api/ci.ts";
import { jobsOf, planDiff, planLines, whereWords, type PlanJob } from "../src/lib/plan-diff.ts";

const fit = (name: string, ok = true) => ({ runner: name, name, connected: ok, eligible: ok, room: ok, score: 1, checks: [] });
const leg = (job: string, labels: string[], runners: ReturnType<typeof fit>[]) => ({
  key: `${job}#`,
  job,
  name: job,
  matrix: {},
  runs_on: { labels },
  routing: { labels, image: null, executors: [], runners },
});
const plan = (legs: ReturnType<typeof leg>[]): CiPreview["plan"] => ({ name: "ci", legs, decided: [], removed: {}, later: [] });
const job = (name: string, legs: number, labels = ["baremetal"]): PlanJob => ({ job: name, legs, labels, runners: ["fedora"], of: 7 });

test("w29_r07_a_plans_jobs_their_legs_and_who_can_take_them", () => {
  const p = plan([
    leg("rust", ["baremetal", "linux"], [fit("fedora"), fit("adacachy"), fit("mac", false)]),
    leg("release", ["baremetal"], [fit("fedora")]),
    leg("release", ["baremetal"], [fit("fedora"), fit("adacachy")]),
  ]);
  assert.deepEqual(jobsOf(p), [
    { job: "rust", legs: 1, labels: ["baremetal", "linux"], runners: ["fedora", "adacachy"], of: 3 },
    { job: "release", legs: 2, labels: ["baremetal"], runners: ["fedora", "adacachy"], of: 2 },
  ]);
  assert.deepEqual(jobsOf(null), []);
  assert.equal(whereWords({ job: "x", legs: 1, labels: ["linux"], runners: ["a", "b", "c"], of: 3 }), "on a, b or c (linux)");
  assert.equal(whereWords({ job: "x", legs: 1, labels: ["gpu"], runners: [], of: 3 }), "no machine connected can take gpu now");
});

test("w29_r07_a_drafts_plan_against_the_published_one", () => {
  const was = [job("rust", 1), job("ui", 1), job("release", 3), job("gone", 2)];
  const now = [job("rust", 1), job("ui", 1, ["baremetal", "docker"]), job("release", 2), job("helm-lint", 1, ["baremetal", "linux"])];
  const d = planDiff(was, now);
  assert.deepEqual(
    d.added.map((j) => j.job),
    ["helm-lint"],
  );
  assert.deepEqual(
    d.removed.map((j) => j.job),
    ["gone"],
  );
  assert.deepEqual(
    d.changed.map((c) => [c.now.job, c.was.legs, c.now.legs]),
    [
      ["ui", 1, 1],
      ["release", 3, 2],
    ],
  );
  assert.deepEqual(d.same, { jobs: 1, legs: 1 });
  // As pl-3 says it: the new job, then the rest as on main.
  const lines = planLines([...was.slice(0, 3), job("helm-lint", 1, ["linux"])], was.slice(0, 3), "main");
  assert.deepEqual(
    lines.map((l) => [l.mark, l.text, l.value]),
    [
      ["added", "helm-lint, a new job: 1 leg, on fedora (linux)", "+1 job"],
      ["same", "the other 3 jobs and 5 legs as on main", "same"],
    ],
  );
  // Nothing changed: all as on main; no published plan: each job.
  assert.deepEqual(
    planLines(was, was, "main").map((l) => l.text),
    ["4 jobs and 7 legs as on main"],
  );
  assert.deepEqual(
    planLines([job("rust", 2)], null, "main").map((l) => [l.mark, l.text]),
    [["job", "rust: 2 legs, on fedora"]],
  );
});
