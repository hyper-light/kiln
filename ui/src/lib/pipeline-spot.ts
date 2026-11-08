/**
 * Where a Pipelines pane's line is in its file (docs/ci.md W29-R04, R05): the lines of the part it
 * names, so the detail tints them and opens there. A line's key says what it is, as the server
 * makes it (`kiln_ci::pipelines`): `on.push`, `jobs.rust`, `uses.kiln/checkout@v1`, `secrets.TOKEN`,
 * `call.inputs.depth`, an action's `inputs.target`, `runs.steps.0`, `runs.image`. A run, a version,
 * a consumer or a setting kiln keeps isn't in the file, and has no place there.
 */
import { isMap, isScalar, isSeq, parseDocument, visit, type Node, type Pair } from "yaml";

/** Lines from 0, both ends in. */
export interface Spot {
  from: number;
  to: number;
}

/** The line of each offset: how many line breaks come before it. */
function lineAt(text: string, offset: number): number {
  let n = 0;
  for (let i = text.indexOf("\n"); i >= 0 && i < offset; i = text.indexOf("\n", i + 1)) n++;
  return n;
}

/** From a pair's key to the end of its value (not the blank lines or comments after it). */
function pairSpot(text: string, p: Pair): Spot | null {
  const key = p.key as Node | null;
  const value = p.value as Node | null;
  const start = key?.range?.[0] ?? value?.range?.[0];
  const end = value?.range?.[1] ?? key?.range?.[1];
  if (start === undefined || end === undefined) return null;
  return { from: lineAt(text, start), to: lineAt(text, Math.max(start, end - 1)) };
}

const keyOf = (p: Pair) => (isScalar(p.key) ? String(p.key.value) : null);

/** The pair at `path` (keys, or a sequence's index), or a sequence item's own lines. */
function spotAt(text: string, root: unknown, path: (string | number)[]): Spot | null {
  let node: unknown = root;
  let spot: Spot | null = null;
  for (const seg of path) {
    if (isMap(node)) {
      const p = node.items.find((x) => keyOf(x) === String(seg));
      if (!p) return null;
      spot = pairSpot(text, p);
      node = p.value;
    } else if (isSeq(node)) {
      // A sequence names its items by index, or (an `on:` list) by their own words.
      const item = typeof seg === "number" ? node.items[seg] : node.items.find((x) => isScalar(x) && String(x.value) === seg);
      const r = (item as Node | undefined)?.range;
      if (!item || !r) return null;
      spot = { from: lineAt(text, r[0]), to: lineAt(text, Math.max(r[0], r[1] - 1)) };
      node = item;
    } else return null;
  }
  return spot;
}

/** The first pair under `key` whose value is `value`, anywhere: a `uses:` of a step or a job. */
function firstPair(text: string, root: unknown, key: string, value: string): Spot | null {
  let found: Spot | null = null;
  visit(root as Node, {
    Pair(_, p) {
      if (keyOf(p) === key && isScalar(p.value) && String(p.value.value) === value) {
        found = pairSpot(text, p);
        return visit.BREAK;
      }
      return undefined;
    },
  });
  return found;
}

/** The lines of a pane's line in its file, or null where it has none. */
export function spotOf(text: string, line: { key: string; kind: string }): Spot | null {
  const { key, kind } = line;
  if (["run", "version", "consumer", "note"].includes(kind)) return null;
  // Names used in expressions: the first line that uses one.
  const named = /^(secrets|vars)\.(.+)$/.exec(key);
  if (named) {
    const at = text.split("\n").findIndex((l) => l.includes(`${named[1]}.${named[2]}`));
    return at >= 0 ? { from: at, to: at } : null;
  }
  const doc = parseDocument(text, { keepSourceTokens: false });
  if (doc.errors.length || !doc.contents) return null;
  if (key.startsWith("uses.")) return firstPair(text, doc.contents, "uses", key.slice("uses.".length));
  if (key === "settings.concurrency") return spotAt(text, doc.contents, ["concurrency"]);
  if (key.startsWith("settings.")) return null;
  const parts: (string | number)[] = key.startsWith("call.") ? ["on", "workflow_call", ...key.slice("call.".length).split(".")] : key.split(".");
  const path = parts.map((s, i) => (typeof s === "string" && /^\d+$/.test(s) && parts[i - 1] === "steps" ? Number(s) : s));
  // `on: push` and `on: [push, …]` name their events without a mapping.
  if (path[0] === "on" && path.length === 2 && isMap(doc.contents)) {
    const on = doc.contents.items.find((p) => keyOf(p) === "on");
    if (on && (isScalar(on.value) || isSeq(on.value))) return isScalar(on.value) ? pairSpot(text, on) : spotAt(text, doc.contents, path);
  }
  // kiln's dialect writes GitHub's `workflow_dispatch` as `manual:` (W02-R06).
  return spotAt(text, doc.contents, path) ?? (key === "on.workflow_dispatch" ? spotAt(text, doc.contents, ["on", "manual"]) : null);
}

/** A manual trigger's input (GitHub's workflow_dispatch, kiln's `manual:`), as Run asks for it (W29-R14). */
export interface ManualInput {
  name: string;
  /** boolean, choice, number, environment or string (GitHub's five). */
  type: string;
  default: string | null;
  options: string[];
  required: boolean;
  description: string | null;
}

/** A workflow's manual inputs, in the file's order: none where it has no manual trigger, or isn't YAML yet. */
export function manualInputs(text: string): ManualInput[] {
  const doc = parseDocument(text);
  if (doc.errors.length) return [];
  const on = doc.toJS() as { on?: unknown } | null;
  const triggers = on?.on;
  if (!triggers || typeof triggers !== "object" || Array.isArray(triggers)) return [];
  const t = triggers as Record<string, unknown>;
  const manual = (t.workflow_dispatch ?? t.manual) as { inputs?: Record<string, Record<string, unknown> | null> } | null | undefined;
  return Object.entries(manual?.inputs ?? {}).map(([name, i]) => ({
    name,
    type: String(i?.type ?? "string"),
    default: i?.default === undefined || i?.default === null ? null : String(i.default),
    options: Array.isArray(i?.options) ? i.options.map(String) : [],
    required: i?.required === true,
    description: typeof i?.description === "string" ? i.description : null,
  }));
}
