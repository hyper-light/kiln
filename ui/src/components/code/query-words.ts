/**
 * What kiln's monitoring query syntax completes as it's typed (query-input.tsx), each part in its turn:
 * a time aggregation, its window, a space aggregation, a metric or a function, a scope's tag and its
 * values, `by {…}` and its groups, a comparison. A metric query alone (the metrics explorer's) starts at
 * its space aggregation and ends at its groups. Pure: the text before the cursor in, completions out; a
 * metric's own tags looked up through what's passed in.
 */

/** What a query can name: its metrics, and each scope tag with the values it takes. */
export interface QueryWords {
  metrics: readonly string[];
  tags: Readonly<Record<string, readonly { value: string; detail?: string }[]>>;
  /**
   * The query's shape: a monitor's (unless said: a time aggregation over a window first, a comparison
   * and a bound last), or a metric query's alone (`avg:a.metric{scope} by {group}`: a space aggregation
   * first, no comparison).
   */
  kind?: "monitor" | "metric";
}

/** A completion as CodeMirror takes it. */
export interface Completion {
  label: string;
  detail?: string;
  type?: string;
}

/** Completions, where in the text they replace from, and (`validFor`) what typed after it keeps them, unasked again. */
export type Completions = { from: number; options: Completion[]; validFor?: RegExp } | null;

/** A metric's own tags and their values (a metric query's), asked for as the cursor comes into its scope or its groups: read them through lib/resource.ts, which loads a metric's once. */
export type TagsOf = (metric: string, signal: AbortSignal) => Promise<QueryWords["tags"]>;

/** Every metric's name (a metric query's), asked for as the cursor comes to where a metric goes: read through lib/resource.ts, which loads them once. */
export type MetricsOf = (signal: AbortSignal) => Promise<readonly string[]>;

const TIME_AGGS = ["avg", "sum", "min", "max", "last"];
export const SPACE_AGGS = ["avg", "sum", "min", "max"];
/** A monitor's windows: kiln's monitors read a week at most (kiln_obs::monitor::parse refuses a longer one). */
const WINDOWS = ["last_5m", "last_15m", "last_1h", "last_4h", "last_1d", "last_7d"];
const FUNCTIONS = [
  "abs",
  "log2",
  "log10",
  "cumsum",
  "integral",
  "diff",
  "derivative",
  "per_second",
  "per_minute",
  "per_hour",
  "rollup",
  "timeshift",
  "ewma_3",
  "ewma_10",
  "median_3",
  "median_5",
  "top",
  "default_zero",
];
const COMPARISONS: { label: string; detail: string }[] = [
  { label: ">", detail: "greater than" },
  { label: ">=", detail: "greater than or equal" },
  { label: "==", detail: "equal" },
  { label: "!=", detail: "not equal" },
  { label: "<", detail: "less than" },
  { label: "<=", detail: "less than or equal" },
];

/** The cursor is in a group's braces (`by {…`). */
const IN_GROUPS = /\bby\s*\{[^}]*$/;
/** The cursor is where a metric goes: after a space aggregation (`avg:`). */
const AT_METRIC = /(?:^|[\s(:])(?:avg|sum|min|max):\s*[\w.]*$/;
/**
 * A tag (a scope's or a group's) and a tag's value (`10.0.0.1:9100` too), as they're typed: their
 * completions hold till what ends them (a `,`; a tag's `:`) asks again, not at each key.
 */
const TAG_TYPED = /^[^,}:\s]*$/;
const VALUE_TYPED = /^[^,}\s]*$/;

const spaceAggs = (): Completion[] => SPACE_AGGS.map((a) => ({ label: `${a}:`, detail: "across groups", type: "keyword" }));

/** Each part's completions, from the text before the cursor. */
export function completeQuery(words: QueryWords, before: string, pos: number): Completions {
  const metric = words.kind === "metric";
  const word = /[\w.!-]*$/.exec(before)?.[0] ?? "";
  const from = pos - word.length;
  const open = before.lastIndexOf("{");
  if (open > before.lastIndexOf("}")) {
    const inside = before.slice(open + 1);
    const part = (inside.split(",").pop() ?? "").trimStart();
    const at = pos - part.length;
    // A group's braces (`by {…}`): its tags alone.
    if (IN_GROUPS.test(before)) return { from: at, options: Object.keys(words.tags).map((k) => ({ label: k, type: "property" })), validFor: TAG_TYPED };
    const colon = part.indexOf(":");
    if (colon >= 0) {
      const key = part.slice(0, colon).replace(/^!/, "");
      return { from: at + colon + 1, options: (words.tags[key] ?? []).map((v) => ({ label: v.value, detail: v.detail, type: "text" })), validFor: VALUE_TYPED };
    }
    return { from: at, options: [{ label: "*", detail: "every", type: "constant" }, ...Object.keys(words.tags).map((k) => ({ label: `${k}:`, type: "property" }))], validFor: TAG_TYPED };
  }
  // A monitor's time aggregation, its window, then its space aggregation. A metric query has none of
  // these: its `max(` and `min(` are functions of queries.
  if (!metric && /\b(?:avg|sum|min|max|last)\(\s*[\w]*$/.test(before)) return { from, options: WINDOWS.map((w) => ({ label: w, type: "constant" })) };
  if (!metric && /\):\s*[\w]*$/.test(before)) return { from, options: spaceAggs() };
  if (AT_METRIC.test(before)) return { from, options: [...words.metrics.map((m) => ({ label: m, type: "variable" })), ...FUNCTIONS.map((f) => ({ label: `${f}(`, type: "function" }))] };
  // After a scope, its groups; and a monitor's comparison, after either.
  if (/\}\s*[\w]*$/.test(before) && !/\bby\s*\{[^}]*\}\s*[\w]*$/.test(before))
    return { from, options: [{ label: "by {", detail: "grouped", type: "keyword" }, ...(metric ? [] : COMPARISONS.map((c) => ({ ...c, type: "operator" })))] };
  if (!metric && /\}\s*$/.test(before)) return { from: pos, options: COMPARISONS.map((c) => ({ ...c, type: "operator" })) };
  if (/^\s*[\w]*$/.test(before)) return { from, options: metric ? spaceAggs() : TIME_AGGS.map((a) => ({ label: `${a}(`, detail: "over a window", type: "keyword" })) };
  return null;
}

/**
 * The metric whose braces the cursor is in: its scope's (`avg:a.metric{…`) or its groups' (`a.metric{…}
 * by {…`, `a.metric by {…`), the tags they take being that metric's. Null outside braces.
 */
export function metricOfBraces(before: string): string | null {
  const open = before.lastIndexOf("{");
  if (open <= before.lastIndexOf("}")) return null;
  let head = before.slice(0, open);
  if (IN_GROUPS.test(before)) head = head.replace(/\bby\s*$/, "").replace(/\{[^}]*\}\s*$/, "");
  return /([a-zA-Z_][\w.\-/]*)\s*$/.exec(head)?.[1] ?? null;
}

/** A metric's tags as its summary has them (api/observe.ts MetricSummary), each with its values of most series, as a query names them. */
export function metricTags(tags: readonly { key: string; top?: readonly { value: string; series: number }[] }[]): QueryWords["tags"] {
  return Object.fromEntries(tags.map((t) => [t.key, (t.top ?? []).map((v) => ({ value: v.value, detail: `${v.series.toLocaleString()} series` }))]));
}

/**
 * `completeQuery`, with what's looked up in place of the words' own: the tags of the metric whose braces
 * the cursor is in (`tagsOf`), every metric's name where a metric goes (`metricsOf`). A promise then, as
 * CodeMirror takes one, never rejected (the words' own where a lookup fails). Elsewhere, and with no
 * lookups, `completeQuery` as it is.
 */
export function completeQueryWith(words: QueryWords, tagsOf: TagsOf | undefined, before: string, pos: number, signal: AbortSignal, metricsOf?: MetricsOf): Completions | Promise<Completions> {
  const metric = tagsOf ? metricOfBraces(before) : null;
  if (tagsOf && metric !== null)
    return tagsOf(metric, signal).then(
      (tags) => completeQuery({ ...words, tags }, before, pos),
      () => completeQuery(words, before, pos),
    );
  if (metricsOf && AT_METRIC.test(before))
    return metricsOf(signal).then(
      (metrics) => completeQuery({ ...words, metrics }, before, pos),
      () => completeQuery(words, before, pos),
    );
  return completeQuery(words, before, pos);
}
