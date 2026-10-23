/**
 * Where a position is in a YAML document, by its indentation alone (no parser: the editor asks on
 * every keystroke and hover, the text often half-typed): the keys from the top down to the mapping
 * it's in, `[]` for a sequence's item, and the key it's on. Inside a block scalar (`run: |`) or a
 * multi-line plain scalar there is no mapping, and nothing is said. And what a JSON Schema says of a
 * path: the schemas it may follow, `anyOf` flattened.
 */

export interface YamlSpot {
  /** Keys from the top to the mapping the position is in; `[]` for a sequence's item. */
  path: string[];
  /** The key the position is on, if any, and where it is. */
  key: { name: string; from: number; to: number } | null;
}

interface Line {
  /** Where its text starts (a dash, or a key), and whether it has any (not blank, not a comment). */
  start: number;
  text: boolean;
  /** Its leading `- `s. */
  dashes: number;
  /** Where its content starts, after the dashes. */
  contentCol: number;
  key: string | null;
  keyEnd: number;
  /** What follows the key's colon, trimmed, its comment left out. */
  value: string;
}

const KEY = /^("[^"]*"|'[^']*'|[A-Za-z0-9_$./][\w$./-]*)\s*:(?=\s|$)/;

function parse(l: string): Line {
  const indent = l.length - l.trimStart().length;
  let at = indent;
  let dashes = 0;
  while (l[at] === "-" && (l[at + 1] === " " || l[at + 1] === undefined)) {
    dashes++;
    at += 1;
    while (l[at] === " ") at++;
  }
  const rest = l.slice(at);
  const text = rest.trim() !== "" && !rest.trimStart().startsWith("#") ? true : dashes > 0;
  const m = KEY.exec(rest);
  const raw = m?.[1] ?? null;
  const key = raw === null ? null : raw.replace(/^(["'])(.*)\1$/, "$2");
  const value = m
    ? rest
        .slice(m[0].length)
        .replace(/(^|\s)#.*$/, "")
        .trim()
    : "";
  return { start: indent, text, dashes, contentCol: at, key, keyEnd: at + (raw?.length ?? 0), value };
}

export function yamlSpot(text: string, pos: number): YamlSpot | null {
  const lines = text.split("\n");
  let start = 0;
  let li = 0;
  for (; li < lines.length - 1; li++) {
    const len = (lines[li] ?? "").length;
    if (pos <= start + len) break;
    start += len + 1;
  }
  const col = pos - start;
  const here = parse(lines[li] ?? "");
  const path: string[] = [];
  const key = here.key !== null && col >= here.contentCol && col <= here.keyEnd ? { name: here.key, from: start + here.contentCol, to: start + here.keyEnd } : null;
  // What the walk up looks for: the parent of what starts at `want` (an item, at its dash).
  let want: number;
  let item: boolean;
  if (here.dashes > 0) {
    for (let i = 0; i < here.dashes; i++) path.unshift("[]");
    want = here.start;
    item = true;
  } else {
    want = here.text ? here.start : col;
    item = false;
  }
  for (let i = li - 1; i >= 0; i--) {
    const p = parse(lines[i] ?? "");
    if (!p.text || p.start > want) continue;
    if (p.start === want) {
      // A sequence written at its key's own indentation: the key above its first item owns it.
      if (item && p.dashes === 0 && p.key !== null && p.value === "") {
        path.unshift(p.key);
        item = false;
      }
      continue;
    }
    if (p.dashes > 0 && p.contentCol === want && !item) {
      // The first line of the item whose mapping this is.
      for (let k = 0; k < p.dashes; k++) path.unshift("[]");
      want = p.start;
      item = true;
      continue;
    }
    if (p.key !== null) {
      // A block or plain scalar's lines aren't a mapping's.
      if (p.value !== "") return null;
      path.unshift(p.key);
      for (let k = 0; k < p.dashes; k++) path.unshift("[]");
      want = p.dashes > 0 ? p.start : p.contentCol;
      item = p.dashes > 0;
      continue;
    }
    if (p.dashes > 0) {
      for (let k = 0; k < p.dashes; k++) path.unshift("[]");
      want = p.start;
      item = true;
      continue;
    }
    return null;
  }
  return { path, key };
}

/**
 * What is written for `key` beside the mapping the position is in (a step's `uses:`, for a
 * position in its `with:`), by indentation as `yamlSpot`, quotes taken off; null when nothing is.
 */
export function siblingValue(text: string, pos: number, key: string): string | null {
  const lines = text.split("\n");
  let start = 0;
  let li = 0;
  for (; li < lines.length - 1; li++) {
    const len = (lines[li] ?? "").length;
    if (pos <= start + len) break;
    start += len + 1;
  }
  const here = parse(lines[li] ?? "");
  const col = here.text ? here.start : pos - start;
  // The key whose mapping the position is in: the nearest line above that starts left of it.
  let owner = -1;
  for (let i = li - 1; i >= 0; i--) {
    const p = parse(lines[i] ?? "");
    if (!p.text || p.contentCol >= col) continue;
    if (p.key === null || p.value !== "") return null;
    owner = i;
    break;
  }
  if (owner < 0) return null;
  const c = parse(lines[owner] ?? "").contentCol;
  const read = (p: Line) => (p.key === key ? p.value.replace(/^(["'])(.*)\1$/, "$2") || null : undefined);
  // Its siblings: up to the line its mapping starts on (an item's dash), then down to its end.
  for (let i = owner; i >= 0; i--) {
    const p = parse(lines[i] ?? "");
    if (!p.text || p.contentCol > c) continue;
    if (p.contentCol < c) break;
    const v = read(p);
    if (v !== undefined) return v;
    if (p.dashes > 0) break;
  }
  for (let i = owner + 1; i < lines.length; i++) {
    const p = parse(lines[i] ?? "");
    if (!p.text || p.contentCol > c) continue;
    if (p.contentCol < c || p.dashes > 0) break;
    const v = read(p);
    if (v !== undefined) return v;
  }
  return null;
}

/** The part of a JSON Schema that says what a document may hold. */
export interface Schema {
  type?: string | string[];
  description?: string;
  properties?: Record<string, Schema>;
  patternProperties?: Record<string, Schema>;
  additionalProperties?: boolean | Schema;
  items?: Schema;
  anyOf?: Schema[];
  oneOf?: Schema[];
  enum?: unknown[];
}

function flat(xs: Schema[]): Schema[] {
  return xs.flatMap((s) => [s, ...flat([...(s.anyOf ?? []), ...(s.oneOf ?? [])])]);
}

/** The schemas the value at `path` may follow. */
export function schemaAt(root: Schema, path: string[]): Schema[] {
  let cur = flat([root]);
  for (const seg of path) {
    const next: Schema[] = [];
    for (const s of cur) {
      if (seg === "[]") {
        if (s.items) next.push(s.items);
        continue;
      }
      const own = s.properties?.[seg];
      if (own) {
        next.push(own);
        continue;
      }
      const matched = Object.entries(s.patternProperties ?? {}).filter(([re]) => new RegExp(re).test(seg));
      matched.forEach(([, sub]) => next.push(sub));
      if (matched.length === 0 && typeof s.additionalProperties === "object") next.push(s.additionalProperties);
    }
    cur = flat(next);
  }
  return cur;
}

/** The keys a mapping at `path` may have, each with what its schema says of it. */
export function keysAt(root: Schema, path: string[]): { key: string; description?: string }[] {
  const seen = new Map<string, string | undefined>();
  for (const s of schemaAt(root, path))
    for (const [k, v] of Object.entries(s.properties ?? {})) if (!seen.has(k) || !seen.get(k)) seen.set(k, v.description ?? flat([v]).find((x) => x.description)?.description);
  return [...seen.entries()].map(([key, description]) => ({ key, description }));
}

/** What the schema says of the key at the end of `path`. */
export function describe(root: Schema, path: string[]): string | null {
  return schemaAt(root, path).find((s) => s.description)?.description ?? null;
}
