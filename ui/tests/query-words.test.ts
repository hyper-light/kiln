// kiln's monitoring query syntax completed as it's typed (components/code/query-words.ts): each part
// in its turn, from the text before the cursor (an experiment's criterion, the metrics page).
import assert from "node:assert/strict";
import { test } from "node:test";
import { completeQuery, completeQueryWith, metricOfBraces, metricTags, type MetricsOf, type QueryWords, type TagsOf } from "../src/components/code/query-words";

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
  assert.ok(labels("avg(").includes("last_7d"));
  // kiln's monitors read a week at most: nothing longer is offered.
  assert.ok(!labels("avg(").some((w) => w === "last_14d" || w === "last_30d"));
  assert.ok(labels("avg(last_1").every((l) => l.startsWith("last_")));
});

test("after its window, a space aggregation; after that, a metric or a function", () => {
  assert.deepEqual(labels("avg(last_7d):"), ["avg:", "sum:", "min:", "max:"]);
  const m = labels("avg(last_7d):avg:kiln.");
  assert.ok(m.includes("kiln.session.warm_ms") && m.includes("abs("));
});

test("a scope offers its tags, then a tag's values; a group's braces its tags alone", () => {
  assert.deepEqual(labels("avg(last_7d):avg:kiln.session.warm_ms{"), ["*", "experiment:", "variant:"]);
  assert.deepEqual(labels("avg(last_7d):avg:kiln.session.warm_ms{experiment:"), ["reviewer-size"]);
  assert.deepEqual(labels("avg(last_7d):avg:kiln.session.warm_ms{experiment:reviewer-size,var"), ["*", "experiment:", "variant:"]);
  assert.deepEqual(labels("avg(last_7d):avg:kiln.session.warm_ms{experiment:reviewer-size} by {"), ["experiment", "variant"]);
});

test("after a scope, by {…} or a comparison; after the groups, a comparison", () => {
  const after = labels("avg(last_7d):avg:kiln.session.warm_ms{experiment:reviewer-size} ");
  assert.ok(after.includes("by {") && after.includes("<="));
  assert.deepEqual(labels("avg(last_7d):avg:kiln.session.warm_ms{experiment:reviewer-size} by {variant} "), [">", ">=", "==", "!=", "<", "<="]);
});

test("a brace's completions hold as its tag or value is typed, asked again at what ends it: a `,`, a tag's `:`", () => {
  const held = (before: string) => completeQuery(words, before, before.length)?.validFor;
  const tag = held("avg(last_7d):avg:kiln.session.warm_ms{");
  assert.ok(tag);
  assert.ok(tag.test("experi") && !tag.test("experiment:") && !tag.test("experiment:x,"));
  const value = held("avg(last_7d):avg:kiln.session.warm_ms{instance:");
  assert.ok(value);
  assert.ok(value.test("10.0.0.1:9100") && !value.test("web-1,") && !value.test("web-1}"));
  const group = held("avg(last_7d):avg:kiln.session.warm_ms{*} by {");
  assert.ok(group);
  assert.ok(group.test("vari") && !group.test("variant,"));
});

// A metric query alone (the metrics explorer's): `avg:a.metric{scope} by {group}`.
const metricWords: QueryWords = { metrics: words.metrics, tags: {}, kind: "metric" };
const metricLabels = (before: string) => completeQuery(metricWords, before, before.length)?.options.map((o) => o.label) ?? [];

test("a metric query starts with a space aggregation; after it, a metric or a function", () => {
  assert.deepEqual(metricLabels(""), ["avg:", "sum:", "min:", "max:"]);
  assert.equal(completeQuery(metricWords, "su", 2)?.from, 0);
  const m = metricLabels("avg:kiln.");
  assert.ok(m.includes("kiln.session.warm_ms") && m.includes("abs("));
});

test("a metric query offers no window and no comparison: after its scope, its groups; after them, nothing", () => {
  // Its `max(` is a function of queries, not a time aggregation over a window.
  assert.deepEqual(metricLabels("max("), []);
  assert.deepEqual(metricLabels("avg:kiln.session.warm_ms{*} "), ["by {"]);
  assert.deepEqual(metricLabels("avg:kiln.session.warm_ms{*} by {variant} "), []);
  assert.deepEqual(metricLabels("avg:kiln.session.warm_ms{*} by {variant}"), []);
  // A monitor's query, the default, still ends in one.
  assert.ok(labels("avg(last_7d):avg:kiln.session.warm_ms{*} ").includes("<="));
});

/** A metric's summary's tags (api/observe.ts MetricSummary): each with its values of most series. */
const SUMMARY = [
  {
    key: "host",
    values: 2,
    series: 3,
    top: [
      { value: "web-1", series: 2 },
      { value: "web-2", series: 1 },
    ],
  },
  { key: "env", values: 1, series: 3, top: [{ value: "prod", series: 3 }] },
];

test("the metric whose scope or groups the cursor is in", () => {
  assert.equal(metricOfBraces("avg:kiln.session.warm_ms{"), "kiln.session.warm_ms");
  assert.equal(metricOfBraces("avg:kiln.session.warm_ms{experiment:x,variant:"), "kiln.session.warm_ms");
  assert.equal(metricOfBraces("avg:kiln.session.warm_ms{experiment:x} by {"), "kiln.session.warm_ms");
  assert.equal(metricOfBraces("avg:kiln.session.warm_ms by {variant,"), "kiln.session.warm_ms");
  assert.equal(metricOfBraces("sum:kiln.ci.jobs{*}.as_count() / sum:kiln.session.failed_pct{"), "kiln.session.failed_pct");
  assert.equal(metricOfBraces("avg:kiln.session.warm_ms"), null);
  assert.equal(metricOfBraces("avg:kiln.session.warm_ms{experiment:x} "), null);
});

test("a named metric's scope and groups take its own tags, and a tag its values of most series", async () => {
  const asked: string[] = [];
  const tagsOf: TagsOf = async (metric) => {
    asked.push(metric);
    return metric === "kiln.session.warm_ms" ? metricTags(SUMMARY) : {};
  };
  const at = async (before: string) => (await completeQueryWith(metricWords, tagsOf, before, before.length, new AbortController().signal))?.options ?? [];
  // Its name, typed, asks for no tags.
  assert.ok((await at("avg:kiln.session.wa")).some((o) => o.label === "kiln.session.warm_ms"));
  assert.deepEqual(asked, []);
  assert.deepEqual(
    (await at("avg:kiln.session.warm_ms{")).map((o) => o.label),
    ["*", "host:", "env:"],
  );
  assert.deepEqual(await at("avg:kiln.session.warm_ms{env:prod,host:"), [
    { label: "web-1", detail: "2 series", type: "text" },
    { label: "web-2", detail: "1 series", type: "text" },
  ]);
  assert.deepEqual(
    (await at("avg:kiln.session.warm_ms{host:web-1} by {")).map((o) => o.label),
    ["host", "env"],
  );
  assert.deepEqual(asked, ["kiln.session.warm_ms", "kiln.session.warm_ms", "kiln.session.warm_ms"]);
  // Another metric's braces take its own (none here); a lookup that fails, the words' own.
  assert.deepEqual(
    (await at("avg:kiln.session.failed_pct{")).map((o) => o.label),
    ["*"],
  );
  const failing: TagsOf = () => Promise.reject(new Error("no such metric"));
  const unknown = "avg:no.such.metric{";
  assert.deepEqual(
    (await completeQueryWith(metricWords, failing, unknown, unknown.length, new AbortController().signal))?.options.map((o) => o.label),
    ["*"],
  );
  // With no lookup (an experiment's criterion), the words' own tags, at once.
  const criterion = "avg(last_7d):avg:kiln.session.warm_ms{";
  assert.deepEqual(completeQueryWith(words, undefined, criterion, criterion.length, new AbortController().signal), completeQuery(words, criterion, criterion.length));
});

test("a metric's tags load once through the resource layer as its braces are typed in, not once a key (metrics.tsx tagsOf)", async () => {
  Object.assign(globalThis, { window: Object.assign(new EventTarget(), { setTimeout, clearTimeout, setInterval, clearInterval }), document: new EventTarget() });
  const { acquireResource } = await import("../src/lib/resource.ts");
  let loads = 0;
  const tagsOf: TagsOf = (metric, signal) =>
    acquireResource(
      {
        key: `test/metric-summary/${metric}`,
        fetch: async () => {
          loads++;
          return { tags: SUMMARY };
        },
      },
      signal,
    ).then((m) => metricTags(m.tags));
  for (const before of ["avg:kiln.session.warm_ms{", "avg:kiln.session.warm_ms{h", "avg:kiln.session.warm_ms{host:", "avg:kiln.session.warm_ms{host:w", "avg:kiln.session.warm_ms{host:web-1} by {"])
    await completeQueryWith(metricWords, tagsOf, before, before.length, new AbortController().signal);
  assert.equal(loads, 1);
});

test("a metric query's field asks for every metric's name only where a metric goes, and offers what it has where that fails (metrics.tsx metricsOf)", async () => {
  let asks = 0;
  const metricsOf: MetricsOf = async () => {
    asks++;
    return ["kiln.ci.job.duration", "system.cpu.user"];
  };
  const none = { ...metricWords, metrics: [] };
  const at = async (before: string, of: MetricsOf = metricsOf) =>
    (await completeQueryWith(none, undefined, before, before.length, new AbortController().signal, of))?.options.map((o) => o.label) ?? [];
  // Where a metric goes: the names looked up, then the functions.
  const named = await at("avg:");
  assert.deepEqual(named.slice(0, 2), ["kiln.ci.job.duration", "system.cpu.user"]);
  assert.ok(named.some((l) => l.endsWith("(")));
  assert.equal(asks, 1);
  // Anywhere else nothing is asked.
  await at("");
  await at("avg:system.cpu.user{*} ");
  assert.equal(asks, 1);
  // Names that fail to load: the functions alone, never an error.
  const failing: MetricsOf = () => Promise.reject(new Error("no route"));
  const fallback = await at("avg:", failing);
  assert.ok(fallback.length > 0 && fallback.every((l) => l.endsWith("(")));
});
