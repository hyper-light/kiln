/**
 * A workflow as the flow view reads and edits it (docs/ci.md W21-R06). The YAML is the source: each
 * function takes the text and gives the new text. The `yaml` package's document model finds what an
 * edit touches and where it is; only that is rewritten, in the file's own style (its indentation, how
 * it indents a list under its key, its brackets' spacing), so comments and the formatting of the rest
 * survive byte for byte. Every edit is checked: the new text read back must be the old one's data
 * with just that change made, or the edit is refused (`EditRefused`) and nothing is written.
 */
import { Document, isMap, isScalar, isSeq, parseDocument, stringify, visit, type Node, type Pair, type YAMLMap, type YAMLSeq } from "yaml";

export interface FlowStep {
  name: string | null;
  uses: string | null;
  run: string | null;
}

export interface FlowMatrix {
  /** Each axis and its values, each value as written. */
  axes: { name: string; values: string[] }[];
  include: string[];
  exclude: string[];
  /** kiln's `where` (W04-R16). */
  where: string | null;
  /** The matrix is an expression (`${{ … }}`): shown, edited in the YAML. */
  expr: string | null;
}

export interface FlowJob {
  id: string;
  name: string | null;
  needs: string[];
  /** Its labels; `null` when `runs-on` is an expression or a group, shown as `runsOnText`. */
  runsOn: string[] | null;
  runsOnText: string | null;
  steps: FlowStep[];
  /** The reusable workflow it calls instead of steps. */
  uses: string | null;
  matrix: FlowMatrix | null;
}

export interface Flow {
  jobs: FlowJob[];
  /** Why the text can't be read as a flow (it isn't YAML), if it can't. */
  error: string | null;
}

/** An edit that can't be made here, and why: it's the YAML's to make. */
export class EditRefused extends Error {}

type Path = (string | number)[];

const text = (n: unknown): string | null => (isScalar(n) ? (n.value === null || n.value === undefined ? null : String(n.value)) : null);
/** A value as written: a scalar's text, a collection's YAML on one line. */
const written = (n: unknown): string =>
  isScalar(n) ? String(n.value ?? "") : n && typeof n === "object" && "toJSON" in n ? stringify((n as Node).toJSON(), { collectionStyle: "flow", lineWidth: 0 }).trim() : "";

function readJob(id: string, n: unknown): FlowJob {
  const m = isMap(n) ? n : null;
  const get = (k: string) => m?.get(k, true);
  const needsNode = get("needs");
  const needs = isSeq(needsNode) ? needsNode.items.map(text).filter((x): x is string => x !== null) : text(needsNode) !== null ? [text(needsNode) as string] : [];
  const ro = get("runs-on");
  let runsOn: string[] | null = null;
  let runsOnText: string | null = null;
  const labels = isSeq(ro) ? ro.items.map(text) : isScalar(ro) ? [text(ro)] : null;
  if (labels && labels.every((l): l is string => l !== null && !l.includes("${{"))) runsOn = labels;
  else if (ro) runsOnText = written(ro);
  const stepsNode = get("steps");
  const steps = isSeq(stepsNode)
    ? stepsNode.items.map((s) => (isMap(s) ? { name: text(s.get("name", true)), uses: text(s.get("uses", true)), run: text(s.get("run", true)) } : { name: null, uses: null, run: null }))
    : [];
  const strategy = get("strategy");
  const mx = isMap(strategy) ? strategy.get("matrix", true) : undefined;
  let matrix: FlowMatrix | null = null;
  if (isMap(mx)) {
    const rows = (k: string) => {
      const s = mx.get(k, true);
      return isSeq(s) ? s.items.map(written) : [];
    };
    matrix = {
      axes: mx.items
        .filter((p) => !["include", "exclude", "where", "platforms"].includes(String(text(p.key))))
        .map((p) => ({ name: String(text(p.key)), values: isSeq(p.value) ? p.value.items.map(written) : [written(p.value)] })),
      include: rows("include"),
      exclude: rows("exclude"),
      where: text(mx.get("where", true)),
      expr: null,
    };
  } else if (mx) matrix = { axes: [], include: [], exclude: [], where: null, expr: written(mx) };
  return { id, name: text(get("name")), needs, runsOn, runsOnText, steps, uses: text(get("uses")), matrix };
}

export function readFlow(src: string): Flow {
  const d = parseDocument(src);
  if (d.errors.length) return { jobs: [], error: d.errors[0]?.message ?? "not YAML" };
  const jobs = d.get("jobs", true);
  if (!isMap(jobs)) return { jobs: [], error: null };
  return { jobs: jobs.items.map((p) => readJob(String(text(p.key)), p.value)), error: null };
}

// ---------------------------------------------------------------- the file's style

interface Style {
  unit: number;
  /** A block list indented under its key (`steps:\n  - run`), or level with it (`steps:\n- run`):
   *  as most of the file's are. */
  indentSeq: boolean;
  /** `[ a, b ]` rather than `[a, b]`. */
  padding: boolean;
}

function styleOf(src: string): Style {
  const lines = src.split("\n");
  const indents = lines.filter((l) => /^\s+\S/.test(l) && !/^\s*#/.test(l)).map((l) => l.length - l.trimStart().length);
  const unit = indents.length ? Math.min(...indents.filter((i) => i > 0)) : 2;
  // Each block list under a key: its dash's column against the key's.
  let [under, level] = [0, 0];
  for (let i = 1; i < lines.length; i++) {
    const m = /^(\s*)-\s/.exec(lines[i] ?? "");
    const prev = /^(\s*)[^\s#-][^:]*:\s*(#.*)?$/.exec(lines[i - 1] ?? "");
    if (m && prev) {
      if ((m[1] ?? "").length > (prev[1] ?? "").length) under++;
      else level++;
    }
  }
  const indentSeq = under >= level;
  const flow = /[[{]( ?)[^\]}\s]/.exec(src.replace(/\$\{\{[^}]*\}\}/g, ""));
  return { unit: Number.isFinite(unit) && unit > 0 ? unit : 2, indentSeq, padding: flow?.[1] === " " };
}

/** New content in the file's style, a list of plain values on one line (`needs: [a, b]`). */
function render(value: unknown, s: Style): string {
  const doc = new Document(value);
  visit(doc, {
    Seq(_, n) {
      if (n.items.every(isScalar)) n.flow = true;
    },
  });
  return doc.toString({ indent: s.unit, indentSeq: s.indentSeq, lineWidth: 0, flowCollectionPadding: s.padding });
}

/** `value` on one line, as a flow collection or a scalar. */
function renderFlow(value: unknown, s: Style): string {
  return stringify(value, { collectionStyle: "flow", lineWidth: 0, flowCollectionPadding: s.padding }).trimEnd();
}

/** Text whose first line goes where it's put and whose others are indented to `col`. */
function at(rendered: string, col: number): string {
  const lines = rendered.replace(/\n$/, "").split("\n");
  return lines.map((l, i) => (i === 0 || l === "" ? l : " ".repeat(col) + l)).join("\n");
}

// ---------------------------------------------------------------- positions

const lineStart = (src: string, pos: number) => src.lastIndexOf("\n", pos - 1) + 1;
const lineEnd = (src: string, pos: number) => {
  const i = src.indexOf("\n", pos);
  return i < 0 ? src.length : i;
};
const col = (src: string, pos: number) => pos - lineStart(src, pos);
/** Where a node's text ends. */
const end = (n: Node) => (n.range ? n.range[1] : 0);

/** The end of the last line with text on it, of what spans `from` to `to` (a block scalar's or
 *  collection's range takes in the blank lines after it), before its line break. */
function lastLine(src: string, from: number, to: number): number {
  let i = Math.min(to, src.length) - 1;
  while (i > from && /\s/.test(src[i] ?? "")) i--;
  return lineEnd(src, i);
}

/** The end of the last line of `pair`'s value (after its comment), before its line break. */
function pairEnd(src: string, pair: Pair): number {
  const k = pair.key as Node;
  const v = pair.value as Node | null;
  return lastLine(src, k.range?.[0] ?? 0, v ? end(v) : end(k));
}

function itemEnd(src: string, item: Node): number {
  return lastLine(src, item.range?.[0] ?? 0, end(item));
}

/** `src` without `[from, to)`, the lines it was; a blank line it leaves first in its collection
 *  (under the key above) goes too. */
function cut(src: string, from: number, to: number): string {
  const out = src.slice(0, from) + src.slice(to);
  const above = src.slice(lineStart(src, Math.max(0, from - 1)), Math.max(0, from - 1)).trimEnd();
  return out.startsWith("\n", from) && above.endsWith(":") ? out.slice(0, from) + out.slice(from + 1) : out;
}

// ---------------------------------------------------------------- edits

interface Ctx {
  src: string;
  d: Document.Parsed;
  s: Style;
}

function mapAt(c: Ctx, path: Path): YAMLMap | null {
  const n = path.length ? c.d.getIn(path, true) : c.d.contents;
  return isMap(n) ? n : null;
}

function seqAt(c: Ctx, path: Path): YAMLSeq | null {
  const n = c.d.getIn(path, true);
  return isSeq(n) ? n : null;
}

const splice = (src: string, from: number, to: number, put: string) => src.slice(0, from) + put + src.slice(to);

/** `key: value` set in the mapping at `path` (made, with its parents, where they're missing):
 *  in place when it's there, else before the first of `before` it has, else at its end. */
function setPair(c: Ctx, path: Path, key: string, value: unknown, before: string[] = []): string {
  const m = mapAt(c, path);
  if (!m) {
    // The parent made with this pair in it, one level up.
    if (!path.length) throw new EditRefused("the file isn't a mapping");
    const parent = path.slice(0, -1);
    const name = String(path[path.length - 1]);
    if (c.d.getIn(path, true) !== undefined) throw new EditRefused(`${path.join(".")} isn't a mapping here`);
    // Into a job, before its steps.
    return setPair(c, parent, name, { [key]: value }, parent.length === 2 && parent[0] === "jobs" ? ["steps"] : []);
  }
  if (m.flow) throw new EditRefused(`${path.join(".") || "the file"} is written as a flow mapping: edit it in the YAML`);
  const pair = m.items.find((p) => text(p.key) === key);
  if (pair) {
    const v = pair.value as Node | null;
    const one = typeof value !== "object" || value === null ? renderFlow(value, c.s) : null;
    // A scalar for a scalar: just its text, its comment kept (a block scalar's blank lines after
    // it are the file's).
    if (v && isScalar(v) && one !== null && !one.includes("\n")) {
      const from = v.range?.[0] ?? 0;
      return splice(c.src, from, String(v.type).startsWith("BLOCK") ? lastLine(c.src, from, v.range?.[1] ?? 0) : (v.range?.[1] ?? 0), one);
    }
    const from = (pair.key as Node).range?.[0] ?? 0;
    return splice(c.src, from, pairEnd(c.src, pair), at(render({ [key]: value }, c.s), col(c.src, from)));
  }
  const first = m.items[0];
  const kcol = first ? col(c.src, (first.key as Node).range?.[0] ?? 0) : 0;
  const next = m.items.find((p) => before.includes(String(text(p.key))));
  const put = " ".repeat(kcol) + at(render({ [key]: value }, c.s), kcol);
  if (next) {
    const ls = lineStart(c.src, (next.key as Node).range?.[0] ?? 0);
    if (c.src.slice(ls, (next.key as Node).range?.[0]).trim() !== "") throw new EditRefused(`${key} can't be put before ${String(text(next.key))} here`);
    return splice(c.src, ls, ls, put + "\n");
  }
  const last = m.items[m.items.length - 1];
  if (!last) {
    if (path.length) throw new EditRefused(`${path.join(".")} is empty: edit it in the YAML`);
    return c.src + (c.src.endsWith("\n") || c.src === "" ? "" : "\n") + put + "\n";
  }
  const e = pairEnd(c.src, last);
  return splice(c.src, e, e, "\n" + put);
}

function removePair(c: Ctx, path: Path, key: string): string {
  const m = mapAt(c, path);
  const pair = m?.items.find((p) => text(p.key) === key);
  if (!m || !pair) return c.src;
  if (m.flow) throw new EditRefused(`${path.join(".")} is written as a flow mapping: edit it in the YAML`);
  const from = (pair.key as Node).range?.[0] ?? 0;
  const ls = lineStart(c.src, from);
  if (c.src.slice(ls, from).trim() !== "") {
    // The first key of a list item (`- name: x`): the item rewritten without it.
    const rest = m.items.filter((p) => p !== pair);
    if (!rest.length) throw new EditRefused(`${key} is all there is of it`);
    const item = (m.toJSON() ?? {}) as Record<string, unknown>;
    delete item[key];
    const start = m.range?.[0] ?? 0;
    return splice(c.src, start, itemEnd(c.src, m), at(render(item, c.s), col(c.src, start)));
  }
  return cut(c.src, ls, Math.min(c.src.length, pairEnd(c.src, pair) + 1));
}

/** `value` added to the list at `path` (a block list gets a line of its own; a flow one is
 *  rewritten on its line). */
function addItem(c: Ctx, path: Path, value: unknown): string {
  const q = seqAt(c, path);
  if (!q) throw new EditRefused(`${path.join(".")} isn't a list here`);
  if (q.flow) {
    const items = [...((q.toJSON() as unknown[]) ?? []), value];
    return splice(c.src, q.range?.[0] ?? 0, q.range?.[1] ?? 0, renderFlow(items, c.s));
  }
  const last = q.items[q.items.length - 1] as Node | undefined;
  if (!last) throw new EditRefused(`${path.join(".")} is empty: edit it in the YAML`);
  const dash = lineStart(c.src, last.range?.[0] ?? 0);
  const dcol = c.src.slice(dash).search(/\S/);
  const e = itemEnd(c.src, last);
  return splice(c.src, e, e, "\n" + " ".repeat(dcol) + at(render([value], c.s), dcol));
}

function removeItem(c: Ctx, path: Path, index: number): string {
  const q = seqAt(c, path);
  const item = q?.items[index] as Node | undefined;
  if (!q || !item) return c.src;
  if (q.flow) {
    const items = ((q.toJSON() as unknown[]) ?? []).filter((_, i) => i !== index);
    return splice(c.src, q.range?.[0] ?? 0, q.range?.[1] ?? 0, renderFlow(items, c.s));
  }
  if (q.items.length === 1) throw new EditRefused(`${path.join(".")} would be empty: edit it in the YAML`);
  return cut(c.src, lineStart(c.src, item.range?.[0] ?? 0), Math.min(c.src.length, itemEnd(c.src, item) + 1));
}

/** Items `i` and `i + 1` of the list at `path` swapped, each with its own lines. */
function swapItems(c: Ctx, path: Path, i: number): string {
  const q = seqAt(c, path);
  const a = q?.items[i] as Node | undefined;
  const b = q?.items[i + 1] as Node | undefined;
  if (!q || !a || !b) return c.src;
  if (q.flow) {
    const items = [...((q.toJSON() as unknown[]) ?? [])];
    [items[i], items[i + 1]] = [items[i + 1], items[i]];
    return splice(c.src, q.range?.[0] ?? 0, q.range?.[1] ?? 0, renderFlow(items, c.s));
  }
  const [as, ae] = [lineStart(c.src, a.range?.[0] ?? 0), itemEnd(c.src, a)];
  const [bs, be] = [lineStart(c.src, b.range?.[0] ?? 0), itemEnd(c.src, b)];
  const ta = c.src.slice(as, ae);
  const tb = c.src.slice(bs, be);
  return c.src.slice(0, as) + tb + c.src.slice(ae, bs) + ta + c.src.slice(be);
}

/** One edit: made in the text, then the text read back and held to `expect`, the old data with
 *  the change made to it. */
function edit(src: string, make: (c: Ctx) => string, expect: (js: Record<string, unknown>) => void): string {
  const d = parseDocument(src);
  if (d.errors.length) throw new EditRefused("the YAML has errors: fix them in the YAML first");
  const c: Ctx = { src, d, s: styleOf(src) };
  const out = make(c);
  const back = parseDocument(out);
  const want = structuredClone((d.toJS() ?? {}) as Record<string, unknown>);
  expect(want);
  if (back.errors.length || JSON.stringify(canon(back.toJS() ?? {})) !== JSON.stringify(canon(want))) throw new EditRefused("that edit can't be made here: make it in the YAML");
  return out;
}

/** Data with each mapping's keys in order: where a key is written doesn't change what it says. */
function canon(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(canon);
  if (v && typeof v === "object")
    return Object.fromEntries(
      Object.keys(v)
        .sort()
        .map((k) => [k, canon((v as Record<string, unknown>)[k])]),
    );
  return v;
}

// ---------------------------------------------------------------- the flow's edits

type Js = Record<string, unknown>;
const obj = (v: unknown): Js => (v && typeof v === "object" && !Array.isArray(v) ? (v as Js) : {});
const jobOf = (js: Js, id: string): Js => obj(obj(js.jobs)[id]);
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : v === undefined || v === null ? [] : [v]);

/** A new job `id` on `runsOn`, with a first step to replace. */
export function addJob(src: string, id: string, runsOn: string): string {
  if (!/^[A-Za-z_][A-Za-z0-9_-]*$/.test(id)) throw new EditRefused(`\`${id}\` isn't a job id: letters, digits, \`_\` and \`-\`, not starting with a digit`);
  if (readFlow(src).jobs.some((j) => j.id === id)) throw new EditRefused(`there is a job \`${id}\` already`);
  const job = { "runs-on": runsOn, steps: [{ run: `echo "${id}"` }] };
  return edit(
    src,
    (c) => setPair(c, ["jobs"], id, job),
    (js) => {
      js.jobs = { ...obj(js.jobs), [id]: job };
    },
  );
}

/** Job `id` removed, and from the `needs` of every job that had it. */
export function removeJob(src: string, id: string): string {
  let out = src;
  for (const j of readFlow(src).jobs) if (j.needs.includes(id)) out = removeNeed(out, j.id, id);
  return edit(
    out,
    (c) => removePair(c, ["jobs"], id),
    (js) => {
      delete obj(js.jobs)[id];
    },
  );
}

/** Job `id` needs `need` too. */
export function addNeed(src: string, id: string, need: string): string {
  return edit(
    src,
    (c) => {
      const n = c.d.getIn(["jobs", id, "needs"], true);
      if (isSeq(n)) return addItem(c, ["jobs", id, "needs"], need);
      const had = text(n);
      return setPair(c, ["jobs", id], "needs", had ? [had, need] : need, ["runs-on", "strategy", "steps"]);
    },
    (js) => {
      const j = jobOf(js, id);
      const had = list(j.needs);
      j.needs = had.length ? [...had, need] : Array.isArray(j.needs) ? [need] : need;
    },
  );
}

export function removeNeed(src: string, id: string, need: string): string {
  return edit(
    src,
    (c) => {
      const n = c.d.getIn(["jobs", id, "needs"], true);
      if (isSeq(n)) {
        const i = n.items.findIndex((x) => text(x) === need);
        if (i < 0) return c.src;
        return n.items.length === 1 ? removePair(c, ["jobs", id], "needs") : removeItem(c, ["jobs", id, "needs"], i);
      }
      return text(n) === need ? removePair(c, ["jobs", id], "needs") : c.src;
    },
    (js) => {
      const j = jobOf(js, id);
      const left = list(j.needs).filter((x) => x !== need);
      if (!left.length) delete j.needs;
      else j.needs = Array.isArray(j.needs) ? left : left[0];
    },
  );
}

/** Job `id` runs on `labels` (one label written as a scalar). */
export function setRunsOn(src: string, id: string, labels: string[]): string {
  if (!labels.length) throw new EditRefused("a job runs somewhere: give it a label");
  const value = labels.length === 1 ? labels[0] : labels;
  return edit(
    src,
    (c) => {
      const n = c.d.getIn(["jobs", id, "runs-on"], true);
      if (isSeq(n) && n.flow && Array.isArray(value)) return splice(c.src, n.range?.[0] ?? 0, n.range?.[1] ?? 0, renderFlow(value, c.s));
      return setPair(c, ["jobs", id], "runs-on", value, ["strategy", "steps"]);
    },
    (js) => {
      jobOf(js, id)["runs-on"] = value;
    },
  );
}

export type NewStep = { run: string } | { uses: string };

export function addStep(src: string, id: string, step: NewStep): string {
  return edit(
    src,
    (c) => (seqAt(c, ["jobs", id, "steps"])?.items.length ? addItem(c, ["jobs", id, "steps"], step) : setPair(c, ["jobs", id], "steps", [step])),
    (js) => {
      const j = jobOf(js, id);
      j.steps = [...list(j.steps), step];
    },
  );
}

export function removeStep(src: string, id: string, index: number): string {
  return edit(
    src,
    (c) => removeItem(c, ["jobs", id, "steps"], index),
    (js) => {
      const j = jobOf(js, id);
      j.steps = list(j.steps).filter((_, i) => i !== index);
    },
  );
}

/** Step `index` of job `id` moved one place: up (`-1`) or down (`1`). */
export function moveStep(src: string, id: string, index: number, by: -1 | 1): string {
  const i = by < 0 ? index - 1 : index;
  return edit(
    src,
    (c) => swapItems(c, ["jobs", id, "steps"], i),
    (js) => {
      const steps = list(jobOf(js, id).steps);
      [steps[i], steps[i + 1]] = [steps[i + 1], steps[i]];
      jobOf(js, id).steps = steps;
    },
  );
}

/** A step's `name`, `run` or `uses` set (an empty name removed). */
export function setStepField(src: string, id: string, index: number, key: "name" | "run" | "uses", value: string): string {
  return edit(
    src,
    (c) => (value === "" && key === "name" ? removePair(c, ["jobs", id, "steps", index], key) : setPair(c, ["jobs", id, "steps", index], key, value)),
    (js) => {
      const step = obj(list(jobOf(js, id).steps)[index]);
      if (value === "" && key === "name") delete step[key];
      else step[key] = value;
    },
  );
}

/** A matrix value as written, typed as YAML types it (`1` a number, `true` a boolean), but a
 *  number YAML would change (`3.10` is 3.1) kept as written. */
function typed(v: string): unknown {
  const d = parseDocument(v);
  const t: unknown = d.errors.length ? v : d.toJS();
  return typeof t === "number" && String(t) !== v.trim() ? v.trim() : t;
}

/** Axis `name` of job `id`'s matrix has `values` (made, with the matrix, when missing). */
export function setAxis(src: string, id: string, name: string, values: string[]): string {
  if (!/^[A-Za-z_][\w-]*$/.test(name) || ["include", "exclude", "where", "platforms"].includes(name)) throw new EditRefused(`\`${name}\` isn't an axis name`);
  const vals = values.map(typed);
  return edit(
    src,
    (c) => {
      const n = c.d.getIn(["jobs", id, "strategy", "matrix", name], true);
      if (isSeq(n) && n.flow) return splice(c.src, n.range?.[0] ?? 0, n.range?.[1] ?? 0, renderFlow(vals, c.s));
      if (isSeq(n) || n !== undefined) return setPair(c, ["jobs", id, "strategy", "matrix"], name, vals);
      // A new axis: on one line, as matrices' axes mostly are, before include and exclude.
      const m = mapAt(c, ["jobs", id, "strategy", "matrix"]);
      if (!m) return setPair(c, ["jobs", id, "strategy"], "matrix", { [name]: vals });
      return setFlowPair(c, ["jobs", id, "strategy", "matrix"], name, vals, ["include", "exclude", "where"]);
    },
    (js) => {
      const j = jobOf(js, id);
      const strategy = obj(j.strategy);
      strategy.matrix = { ...obj(strategy.matrix), [name]: vals };
      // Key order in the data doesn't matter; the check compares what's there.
      j.strategy = strategy;
    },
  );
}

/** `key: [a, b]` added on one line. */
function setFlowPair(c: Ctx, path: Path, key: string, values: unknown[], before: string[]): string {
  const m = mapAt(c, path);
  if (!m || m.flow) throw new EditRefused(`${path.join(".")} isn't a block mapping here`);
  const first = m.items[0];
  const kcol = first ? col(c.src, (first.key as Node).range?.[0] ?? 0) : 0;
  const line = " ".repeat(kcol) + `${renderFlow(key, c.s)}: ${renderFlow(values, c.s)}`;
  const next = m.items.find((p) => before.includes(String(text(p.key))));
  if (next) {
    const ls = lineStart(c.src, (next.key as Node).range?.[0] ?? 0);
    return splice(c.src, ls, ls, line + "\n");
  }
  const last = m.items[m.items.length - 1];
  if (!last) throw new EditRefused(`${path.join(".")} is empty: edit it in the YAML`);
  const e = pairEnd(c.src, last);
  return splice(c.src, e, e, "\n" + line);
}

export function removeAxis(src: string, id: string, name: string): string {
  return edit(
    src,
    (c) => removePair(c, ["jobs", id, "strategy", "matrix"], name),
    (js) => {
      delete obj(obj(jobOf(js, id).strategy).matrix)[name];
    },
  );
}

/** A row of `include` or `exclude`, written `key: value, key: value`. */
export function addRow(src: string, id: string, kind: "include" | "exclude", row: string): string {
  const d = parseDocument(`{${row}}`);
  const value = d.toJS();
  if (d.errors.length || !value || typeof value !== "object" || Array.isArray(value) || !Object.keys(value).length || Object.values(value).some((v) => v === null || v === undefined))
    throw new EditRefused(`\`${row}\` isn't \`key: value, key: value\``);
  return edit(
    src,
    (c) => {
      const q = seqAt(c, ["jobs", id, "strategy", "matrix", kind]);
      if (q?.items.length && !q.flow) {
        // Rows on one line each, as `- { os: mac, n: 1 }`, spaced as the last one is.
        const last = q.items[q.items.length - 1] as Node;
        const dash = lineStart(c.src, last.range?.[0] ?? 0);
        const dcol = c.src.slice(dash).search(/\S/);
        const e = itemEnd(c.src, last);
        const padding = isMap(last) && last.flow ? c.src.slice(last.range?.[0] ?? 0).startsWith("{ ") : true;
        return splice(c.src, e, e, "\n" + " ".repeat(dcol) + "- " + renderFlow(value, { ...c.s, padding }));
      }
      if (q) return addItem(c, ["jobs", id, "strategy", "matrix", kind], value);
      return setPair(c, ["jobs", id, "strategy", "matrix"], kind, [value]);
    },
    (js) => {
      const m = obj(obj(jobOf(js, id).strategy).matrix);
      m[kind] = [...list(m[kind]), value];
    },
  );
}

export function removeRow(src: string, id: string, kind: "include" | "exclude", index: number): string {
  return edit(
    src,
    (c) => {
      const q = seqAt(c, ["jobs", id, "strategy", "matrix", kind]);
      return q && q.items.length === 1 ? removePair(c, ["jobs", id, "strategy", "matrix"], kind) : removeItem(c, ["jobs", id, "strategy", "matrix", kind], index);
    },
    (js) => {
      const m = obj(obj(jobOf(js, id).strategy).matrix);
      const left = list(m[kind]).filter((_, i) => i !== index);
      if (left.length) m[kind] = left;
      else delete m[kind];
    },
  );
}
