// kiln's monitoring query syntax completed as it's typed (components/code/query-words.ts): each part
// in its turn, from the text before the cursor (an experiment's criterion, the metrics page).
import assert from "node:assert/strict";
import { test } from "node:test";
import { completeQuery, type QueryWords } from "../src/components/code/query-words";

const words: QueryWords = {
  metrics: ["kiln.session.warm_ms", "kiln.session.failed_pct"],
  tags: { experiment: [{ value: "reviewer-size", detail: "Right-size its microVM" }], variant: [{ value: "5e1c", detail: "Small" }] },
};
const labels = (before: string) => completeQuery(words, before, before.length)?.options.map((o) => o.label) ?? [];

test("a query starts with a time aggregation", () => {
  assert.deepEqual(labels(""), ["avg(", "sum(", "min(", "max(", "last("]);
  assert.equal(completeQuery(words, "av", 2)?.from, 0);
});

test("a time aggregation takes a window", () => {
  assert.ok(labels("avg(").includes("last_14d"));
  assert.ok(labels("avg(last_1").every((l) => l.startsWith("last_")));
});

test("after its window, a space aggregation; after that, a metric or a function", () => {
  assert.deepEqual(labels("avg(last_14d):"), ["avg:", "sum:", "min:", "max:"]);
  const m = labels("avg(last_14d):avg:kiln.");
  assert.ok(m.includes("kiln.session.warm_ms") && m.includes("abs("));
});

test("a scope offers its tags, then a tag's values; a group's braces its tags alone", () => {
  assert.deepEqual(labels("avg(last_14d):avg:kiln.session.warm_ms{"), ["*", "experiment:", "variant:"]);
  assert.deepEqual(labels("avg(last_14d):avg:kiln.session.warm_ms{experiment:"), ["reviewer-size"]);
  assert.deepEqual(labels("avg(last_14d):avg:kiln.session.warm_ms{experiment:reviewer-size,var"), ["*", "experiment:", "variant:"]);
  assert.deepEqual(labels("avg(last_14d):avg:kiln.session.warm_ms{experiment:reviewer-size} by {"), ["experiment", "variant"]);
});

test("after a scope, by {…} or a comparison; after the groups, a comparison", () => {
  const after = labels("avg(last_14d):avg:kiln.session.warm_ms{experiment:reviewer-size} ");
  assert.ok(after.includes("by {") && after.includes("<="));
  assert.deepEqual(labels("avg(last_14d):avg:kiln.session.warm_ms{experiment:reviewer-size} by {variant} "), [">", ">=", "==", "!=", "<", "<="]);
});
