import assert from "node:assert/strict";
import { test } from "node:test";
import type { PlanBrief } from "../src/api/plans.ts";
import type { ResearchBrief } from "../src/api/research.ts";
import type { Chosen } from "../src/components/ui/facet-box.tsx";
import { facetsOf, hasChosen, planDimensions, planMeta, planRow, researchDimensions } from "../src/views/plans/browse.ts";

const plan = (rest: Partial<PlanBrief> = {}): PlanBrief => ({
  id: "p",
  title: "T",
  tags: [],
  source: null,
  share: null,
  version: 1,
  created_at: 0,
  updated_at: 1_000_000,
  step_count: 3,
  runs: 0,
  phase: "ready",
  research: null,
  ...rest,
});

const research = (rest: Partial<ResearchBrief> = {}): ResearchBrief => ({
  id: "r",
  question: "q",
  agent: null,
  by: "ada",
  plan: null,
  summary: { findings: 0, grades: {}, sources: 0, internal: 0, external: 0, checking: 0 },
  grades: [],
  best: null,
  created_at: 0,
  updated_at: 0,
  ...rest,
});

test("a kept plan is this agent's, another agent's or no agent's, and is being planned, new, or run before", () => {
  const whose = [plan({ source: { session: "me" } }), plan({ source: { session: "other", agent: "Replication lead" } }), plan({ source: { agent: "claude-7" } }), plan()].map(
    (p) => planRow(p, "me").whose,
  );
  assert.deepEqual(whose, ["here", "agent", "agent", "none"]);
  assert.deepEqual(
    [plan({ phase: "planning", runs: 2 }), plan({ runs: 2 }), plan()].map((p) => planRow(p, "me").stand),
    ["planning", "ran", "new"],
  );
});

test("plans narrow by facets, any option within one and all across them, each option counting what it would show", () => {
  const rows = [
    planRow(plan({ id: "a", source: { session: "me" }, tags: ["release"] }), "me"),
    planRow(plan({ id: "b", source: { session: "me" }, phase: "planning" }), "me"),
    planRow(plan({ id: "c", runs: 1, tags: ["release", "docs"] }), "me"),
  ];
  const dims = planDimensions(rows);
  const shown = (chosen: Chosen) => rows.filter((r) => hasChosen(dims, chosen, r)).map((r) => r.p.id);
  assert.deepEqual(shown({}), ["a", "b", "c"]);
  assert.deepEqual(shown({ whose: ["here"] }), ["a", "b"]);
  assert.deepEqual(shown({ whose: ["here"], tags: ["release"] }), ["a"]);
  assert.deepEqual(shown({ stand: ["planning", "ran"] }), ["b", "c"]);
  const counts = Object.fromEntries(facetsOf(dims, rows, { whose: ["here"] }).map((f) => [f.id, Object.fromEntries(f.options.map((o) => [o.id, o.count]))]));
  // Its own facet counts as though nothing in it were chosen; the others count among this agent's.
  // An option no plan has ("Other agents") isn't offered.
  assert.deepEqual(counts, { whose: { here: 2, none: 1 }, stand: { planning: 1, new: 1, ran: 0 }, tags: { docs: 0, release: 1 } });
});

test("a plan's row says its steps, when it changed, whose it is and where it stands", () => {
  const now = (1_000_000 + 3600) * 1000;
  const meta = (p: PlanBrief) => planMeta(planRow(p, "me"), now);
  assert.deepEqual(meta(plan({ step_count: 5, source: { session: "me", proposed: true } })), ["5 steps", "1h ago", "proposed here", "not run"]);
  assert.deepEqual(meta(plan({ step_count: 1, runs: 2, source: { session: "x", agent: "Replication lead" } })), ["1 step", "1h ago", "Replication lead", "run 2×"]);
  assert.deepEqual(meta(plan({ step_count: 0, phase: "planning", source: { session: "me", goal: { agent: "me", id: "g", text: "t" } } })), ["1h ago", "from its goal", "being planned"]);
  assert.deepEqual(meta(plan()), ["3 steps", "1h ago", "not run"]);
});

test("research narrows by whose it is, the grades of its findings, and its plan", () => {
  const all = [research({ id: "mine", agent: "me", grades: ["A", "F"], plan: "p" }), research({ id: "theirs", agent: "other", grades: ["B"] }), research({ id: "ada's" })];
  const dims = researchDimensions("me");
  const shown = (chosen: Chosen) => all.filter((r) => hasChosen(dims, chosen, r)).map((r) => r.id);
  assert.deepEqual(shown({ grade: ["F"] }), ["mine"]);
  assert.deepEqual(shown({ grade: ["A", "B"] }), ["mine", "theirs"]);
  assert.deepEqual(shown({ whose: ["person"] }), ["ada's"]);
  assert.deepEqual(shown({ plan: ["none"], whose: ["agent", "person"] }), ["theirs", "ada's"]);
  // Only grades some research has are offered.
  assert.deepEqual(
    facetsOf(dims, all, {})
      .find((f) => f.id === "grade")
      ?.options.map((o) => o.id),
    ["A", "B", "F"],
  );
});
