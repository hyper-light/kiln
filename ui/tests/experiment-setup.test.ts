/**
 * An experiment's setup (docs/ci.md W20-R11): it judges variants by checks, each a command run in
 * every variant's own worktree (no CI needed), and by metrics those checks report, each with its
 * threshold; it starts from what the agent that asks suggested, says each part in a line of its
 * summary (a problem on the part it's in), takes a time limit in any unit, and what's sent is what
 * the server takes.
 */
import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import type { ExCriterion } from "../src/api/experiments.ts";

// The views import their CSS, which Node needn't read.
const loader = registerHooks({
  load(url, context, nextLoad) {
    return url.endsWith(".css") ? { format: "module", source: "export {};", shortCircuit: true } : nextLoad(url, context);
  },
});
const { experimentProblem, newSetup, problemOf, specOf, suggestionFor, summaryOf } = await import("../src/views/harness/choices.tsx");
const { criteriaOf, criterionFrom } = await import("../src/views/harness/approval.tsx");
loader.deregister();

type Spec = Parameters<typeof experimentProblem>[0];

const question = { question: "Which cache?", header: "Cache", options: [{ label: "Redis" }, { label: "In memory" }] };

const spec = (criteria: Spec["criteria"]): Spec => ({
  question: question.question,
  options: ["Redis", "In memory"],
  strategy: "each",
  variants: [["Redis"], ["In memory"]],
  baseline: true,
  isolation: "worktree",
  criteria,
  repetitions: 3,
  maxParallel: 4,
  decide: "ask",
});

test("w20_r11 an experiment needs a check, metrics named once each, and thresholds that are numbers", () => {
  assert.equal(experimentProblem(spec([{ kind: "metric", metric: "p95_ms", better: "lower" }])), "Add a check: what each variant runs.");
  assert.equal(experimentProblem(spec([{ kind: "command", command: "cargo bench", required: true }])), null, "a command alone is enough: no CI, no metric");
  const twice = spec([
    { kind: "command", command: "cargo bench", required: true },
    { kind: "metric", metric: "p95_ms", better: "lower" },
    { kind: "metric", metric: "p95_ms", better: "higher" },
  ]);
  assert.equal(experimentProblem(twice), "Two metrics are named p95_ms.");
  const unset = spec([
    { kind: "command", command: "cargo bench", required: true },
    { kind: "metric", metric: "p95_ms", better: "lower", threshold: { type: "absolute", value: NaN } },
  ]);
  assert.equal(experimentProblem(unset), "A threshold has to be a number.");
});

test("w20_r11 the setup starts from the agent's suggestion for its question", () => {
  const suggested = [
    { question: "Something else?", criteria: [{ kind: "command" as const, command: "make other", required: true }] },
    { criteria: [{ kind: "command" as const, command: "make next", required: true }] },
    {
      question: question.question,
      criteria: [
        { kind: "pipeline" as const, pipeline: ".kiln/bench.yaml", required: true },
        { kind: "command" as const, command: "cargo bench --bench cache", required: true },
        { kind: "metric" as const, metric: "p95_ms", better: "lower" as const, threshold: { type: "relative" as const, value: 5 }, required: true },
      ],
    },
  ];
  assert.equal(suggestionFor(question, suggested)?.[1]?.command, "cargo bench --bench cache", "its own question's, wherever it is in the list");
  assert.equal(suggestionFor({ ...question, question: "Unasked?" }, suggested)?.[0]?.command, "make next", "else the newest naming no question");
  assert.equal(suggestionFor(question, []), undefined);

  // CI's engine off: no pipelines to offer, and the suggested pipeline goes.
  const s = newSetup(0, question, "worktree", [], suggestionFor(question, suggested));
  assert.deepEqual(
    s.criteria.map((c) => [c.kind, c.command || c.metric, c.thresholdType, c.threshold, c.required]),
    [
      ["command", "cargo bench --bench cache", "none", "", true],
      ["metric", "p95_ms", "relative", "5", true],
    ],
  );
  // With pipelines to run, it stays.
  assert.equal(newSetup(0, question, "worktree", [".kiln/bench.yaml"], suggestionFor(question, suggested)).criteria[0]?.kind, "pipeline");
  // Metrics alone: a command to fill in comes first, as without a suggestion.
  const metricsOnly = newSetup(0, question, "worktree", [], [{ kind: "metric", metric: "rss_mb", better: "lower" }]);
  assert.deepEqual(
    metricsOnly.criteria.map((c) => [c.kind, c.command || c.metric]),
    [
      ["command", ""],
      ["metric", "rss_mb"],
    ],
  );
  const plain = newSetup(0, question, "worktree", [".kiln/bench.yaml"]);
  assert.deepEqual(
    plain.criteria.map((c) => c.kind),
    ["command"],
    "a check is a command by default, which needs no CI",
  );
});

test("w20_r11 criteria go to the server as it takes them, and come back from an agent's suggestion", () => {
  const sent = criteriaOf(
    spec([
      { kind: "command", command: "cargo bench", required: true },
      { kind: "metric", metric: "p95_ms", better: "lower", threshold: { type: "absolute", value: 10 }, required: true },
      { kind: "metric", metric: "ops", better: "higher", threshold: { type: "absolute", value: 900 } },
      { kind: "metric", metric: "size", better: "lower", threshold: { type: "relative", value: 5 } },
    ]),
  );
  assert.deepEqual(sent, [
    { name: "command-1", kind: "command", command: "cargo bench", required: true },
    { name: "p95_ms", kind: "metric", metric: "p95_ms", better: "lower", required: true, threshold: { at_most: 10 } },
    { name: "ops", kind: "metric", metric: "ops", better: "higher", required: false, threshold: { at_least: 900 } },
    { name: "size", kind: "metric", metric: "size", better: "lower", required: false, threshold: { better: 0.05 } },
  ]);
  const suggested: ExCriterion[] = [
    { name: "bench", kind: "command", command: "cargo bench", required: true },
    { name: "p95", kind: "metric", metric: "p95_ms", better: "lower", required: true, threshold: { at_most: 10 } },
    { name: "size", kind: "metric", metric: "size", better: "lower", required: false, threshold: { better: 0.123 } },
    { name: "rss_mb", kind: "metric", better: "lower", required: false },
  ];
  assert.deepEqual(suggested.map(criterionFrom), [
    { kind: "command", command: "cargo bench", required: true },
    { kind: "metric", metric: "p95_ms", better: "lower", threshold: { type: "absolute", value: 10 }, required: true },
    { kind: "metric", metric: "size", better: "lower", threshold: { type: "relative", value: 12.3 }, required: false },
    { kind: "metric", metric: "rss_mb", better: "lower", required: false },
  ]);
});

test("w20_r11 a metric is required only with a threshold to meet, as its switch shows", () => {
  const suggested = [
    { kind: "command" as const, command: "cargo bench", required: true },
    { kind: "metric" as const, metric: "p95_ms", better: "lower" as const, required: true },
    { kind: "metric" as const, metric: "rss_mb", better: "lower" as const, threshold: { type: "absolute" as const, value: 512 }, required: true },
  ];
  const spec = specOf(question, newSetup(0, question, "worktree", [], suggested), "worktree");
  assert.deepEqual(
    spec.criteria.map((c) => [c.kind, c.command ?? c.metric, c.required]),
    [
      ["command", "cargo bench", true],
      ["metric", "p95_ms", false],
      ["metric", "rss_mb", true],
    ],
  );
});

test("w20_r11 the summary says each part in a few words, and a problem is on its part", () => {
  const suggested = [
    { kind: "command" as const, command: "ci/gate.sh --quick", required: true },
    { kind: "command" as const, command: "cargo test", required: false },
    { kind: "metric" as const, metric: "wall_s", better: "lower" as const },
    { kind: "metric" as const, metric: "rss_mb", better: "lower" as const },
  ];
  const three = { ...question, options: [...question.options, { label: "On disk" }] };
  const s = newSetup(0, three, "worktree", [], suggested);
  assert.deepEqual(summaryOf(s, 3), {
    variants: "Each alone + baseline",
    checks: "ci/gate.sh --quick +1",
    metrics: "wall_s, rss_mb",
    runs: "3 per variant · 4 at once",
    end: "Bring the results back",
  });
  const changed = { ...s, on: [true, true, false], strategy: "pairs" as const, baseline: false, budget: "1", budgetUnit: "days" as const, decide: "auto" as const, notes: "keep it small" };
  assert.deepEqual(summaryOf(changed, 3), {
    variants: "2 of 3 · Alone and in pairs",
    checks: "ci/gate.sh --quick +1",
    metrics: "wall_s, rss_mb",
    runs: "3 per variant · 4 at once · 1 day",
    end: "Pick the clear winner · with a note",
  });
  const bare = newSetup(0, question, "worktree", []);
  assert.deepEqual([summaryOf(bare, 2).checks, summaryOf(bare, 2).metrics], ["None", "None"]);
  assert.deepEqual(problemOf(specOf(question, bare, "worktree")), { part: "checks", text: "Give the command to run." });
  assert.deepEqual(problemOf(specOf(question, { ...s, repetitions: "0" }, "worktree")), { part: "runs", text: "Runs per variant: a whole number, at least 1." });
  assert.deepEqual(problemOf(specOf(question, { ...s, on: [true, false] }, "worktree")), { part: "variants", text: "Keep at least two options in play." });
});

test("w20_r11 a time limit is in any unit, sent in seconds", () => {
  const s = newSetup(0, question, "worktree", [], [{ kind: "command", command: "cargo bench", required: true }]);
  assert.equal(specOf(question, s, "worktree").budgetSeconds, undefined, "none unless set");
  const sent = (budget: string, budgetUnit: Parameters<typeof summaryOf>[0]["budgetUnit"]) => specOf(question, { ...s, budget, budgetUnit }, "worktree").budgetSeconds;
  assert.equal(sent("90", "seconds"), 90);
  assert.equal(sent("2", "hours"), 7_200);
  assert.equal(sent("1.5", "days"), 129_600);
  assert.equal(sent("1", "weeks"), 604_800);
  assert.equal(sent("1", "months"), 2_592_000, "a month as 30 days");
  assert.equal(sent("1", "years"), 31_536_000, "a year as 365");
  assert.equal(problemOf(specOf(question, { ...s, budget: "-1", budgetUnit: "hours" }, "worktree"))?.part, "runs");
});
