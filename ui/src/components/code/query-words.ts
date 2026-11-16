/**
 * What kiln's monitoring query syntax completes as it's typed (query-input.tsx), each part in its turn:
 * a time aggregation, its window, a space aggregation, a metric or a function, a scope's tag and its
 * values, `by {…}` and its groups, a comparison. Pure: the text before the cursor in, completions out.
 */

/** What a query can name: its metrics, and each scope tag with the values it takes. */
export interface QueryWords {
  metrics: readonly string[];
  tags: Readonly<Record<string, readonly { value: string; detail?: string }[]>>;
}

/** A completion as CodeMirror takes it. */
export interface Completion {
  label: string;
  detail?: string;
  type?: string;
}

const TIME_AGGS = ["avg", "sum", "min", "max", "last"];
export const SPACE_AGGS = ["avg", "sum", "min", "max"];
const WINDOWS = ["last_5m", "last_15m", "last_1h", "last_4h", "last_1d", "last_7d", "last_14d", "last_30d"];
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

/** Each part's completions, from the text before the cursor. */
export function completeQuery(words: QueryWords, before: string, pos: number): { from: number; options: Completion[] } | null {
  const word = /[\w.!-]*$/.exec(before)?.[0] ?? "";
  const from = pos - word.length;
  const open = before.lastIndexOf("{");
  if (open > before.lastIndexOf("}")) {
    const inside = before.slice(open + 1);
    const part = (inside.split(",").pop() ?? "").trimStart();
    const at = pos - part.length;
    // A group's braces (`by {…}`): its tags alone.
    if (/\bby\s*\{[^}]*$/.test(before)) return { from: at, options: Object.keys(words.tags).map((k) => ({ label: k, type: "property" })) };
    const colon = part.indexOf(":");
    if (colon >= 0) {
      const key = part.slice(0, colon).replace(/^!/, "");
      return { from: at + colon + 1, options: (words.tags[key] ?? []).map((v) => ({ label: v.value, detail: v.detail, type: "text" })) };
    }
    return { from: at, options: [{ label: "*", detail: "every", type: "constant" }, ...Object.keys(words.tags).map((k) => ({ label: `${k}:`, type: "property" }))] };
  }
  if (/\b(?:avg|sum|min|max|last)\(\s*[\w]*$/.test(before)) return { from, options: WINDOWS.map((w) => ({ label: w, type: "constant" })) };
  if (/\):\s*[\w]*$/.test(before)) return { from, options: SPACE_AGGS.map((a) => ({ label: `${a}:`, detail: "across groups", type: "keyword" })) };
  if (/(?:^|[\s(:])(?:avg|sum|min|max):\s*[\w.]*$/.test(before))
    return { from, options: [...words.metrics.map((m) => ({ label: m, type: "variable" })), ...FUNCTIONS.map((f) => ({ label: `${f}(`, type: "function" }))] };
  if (/\}\s*[\w]*$/.test(before) && !/\bby\s*\{[^}]*\}\s*[\w]*$/.test(before))
    return { from, options: [{ label: "by {", detail: "grouped", type: "keyword" }, ...COMPARISONS.map((c) => ({ ...c, type: "operator" }))] };
  if (/\}\s*$/.test(before)) return { from: pos, options: COMPARISONS.map((c) => ({ ...c, type: "operator" })) };
  if (/^\s*[\w]*$/.test(before)) return { from, options: TIME_AGGS.map((a) => ({ label: `${a}(`, detail: "over a window", type: "keyword" })) };
  return null;
}
