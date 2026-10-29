/**
 * kiln's syntax highlighter: tree-sitter (web-tree-sitter) with the grammars Vorpal compiles in
 * (grammars/, ui/scripts/grammars.mjs) and their highlights queries, the way tree-sitter's own
 * highlighter reads them:
 *
 * - each capture names what its node is (`@keyword`, `@function.method`), and theme.ts says how
 *   that looks;
 * - where captures nest, the inner one shows (an escape inside a string); where two patterns
 *   capture the same text, the later one wins (or the first, for queries written that way:
 *   languages.ts), unless one is given a higher `priority`;
 * - languages written inside others (a `<script>` in HTML, a fenced block in Markdown, PHP's HTML)
 *   are found by the injections query and highlighted in place, in their own colours.
 *
 * Predicates web-tree-sitter leaves to its caller are honoured here: Neovim's `#lua-match?`,
 * `#has-ancestor?` and `#has-parent?`, `#contains?`, `#kind-eq?`, and `#offset!` on injections.
 *
 * No React and no bundler: the page and the tests (node) use it alike, each with its own way of
 * fetching the files (Assets).
 */
import type { Language as Grammar, Node, Parser, Query, QueryMatch, QueryPredicate, Range } from "web-tree-sitter";
import { fitQuery } from "./fit";
import { grammarsOf, languageById, languageByName, type Language } from "./languages";
import { CLASS_INDEX, CLASS_LIST, classOf } from "./theme";

/** A run of a line that reads as one kind of token: [start, end, class], in UTF-16 code units. */
export type Tok = [start: number, end: number, cls: string];

export interface Highlight {
  /** Each line's tokens (the text split at "\n"). */
  lines: Tok[][];
  /** Languages written in the text that aren't loaded yet: it reads better once they are. */
  wants: string[];
}

type Runtime = typeof import("web-tree-sitter");

export interface Assets {
  /** web-tree-sitter, initialized. */
  runtime(): Promise<Runtime>;
  /** A grammar's WebAssembly (grammars/<id>.wasm.gz, inflated). */
  grammar(id: string): Promise<Uint8Array>;
  /** A query's text, by its path under src/lib/syntax. */
  query(path: string): Promise<string>;
}

/** Past this much text at once nothing is highlighted: it reads plain, and nothing waits on it. */
export const MAX_CHARS = 400_000;
/** How long one text may take, parsing and querying every language in it, before it's left plain. */
const BUDGET_MS = 250;
/** How deep languages nest in each other (Markdown, its HTML, that HTML's script) before the rest reads as it is. */
const MAX_DEPTH = 4;
/** In-progress matches a query keeps at once: plenty for highlighting, and a bound on pathological text. */
const MATCH_LIMIT = 4096;

interface Parsed {
  parser: Parser;
  highlights: Query;
  injections: Query | null;
}

interface Loaded {
  lang: Language;
  grammars: Map<string, Parsed>;
}

/** A capture that will paint: its text's class from `start` to `end`, and what decides between it and another on the same text. */
interface Paint {
  start: number;
  end: number;
  cls: number;
  priority: number;
  /** Its pattern, and its place among the match's captures. */
  rank: number;
}

class OutOfTime extends Error {}

export class Highlighter {
  private runtimeP: Promise<Runtime> | null = null;
  private grammars = new Map<string, Promise<Grammar>>();
  private loading = new Map<string, Promise<boolean>>();
  private loaded = new Map<string, Loaded>();
  private failed = new Set<string>();
  /** What each grammar's queries lost to fit it (fit.ts), by `<language>/<grammar>`. */
  readonly dropped = new Map<string, string[]>();

  /** `budgetMs`: how long one text may take (the page's [`BUDGET_MS`]; the tests, which aren't watched, give more). */
  constructor(
    private assets: Assets,
    private opts: { budgetMs?: number } = {},
  ) {}

  /** Whether `id` highlights now. */
  ready(id: string): boolean {
    return this.loaded.has(id);
  }

  /** Whether `id` never will (an unknown language, or its grammar or queries didn't load). */
  broken(id: string): boolean {
    return this.failed.has(id);
  }

  /** Loads a language: its grammars and queries (once; later calls get the same answer). */
  load(id: string): Promise<boolean> {
    let p = this.loading.get(id);
    if (!p) {
      p = this.loadNow(id).then(
        (l) => {
          this.loaded.set(id, l);
          return true;
        },
        (e: unknown) => {
          this.failed.add(id);
          console.warn(`syntax: ${id} can't be highlighted:`, e);
          return false;
        },
      );
      this.loading.set(id, p);
    }
    return p;
  }

  private runtime(): Promise<Runtime> {
    this.runtimeP ??= this.assets.runtime();
    return this.runtimeP;
  }

  private grammar(rt: Runtime, id: string): Promise<Grammar> {
    let p = this.grammars.get(id);
    if (!p) {
      p = this.assets.grammar(id).then((bytes) => rt.Language.load(bytes));
      this.grammars.set(id, p);
    }
    return p;
  }

  private async loadNow(id: string): Promise<Loaded> {
    const lang = languageById(id);
    if (!lang) throw new Error(`no language ${id}`);
    const rt = await this.runtime();
    const [highlights, injections] = await Promise.all([this.queryText(lang.highlights), lang.injections ? this.queryText(lang.injections) : null]);
    const grammars = new Map<string, Parsed>();
    for (const gid of grammarsOf(lang)) {
      const grammar = await this.grammar(rt, gid);
      const parser = new rt.Parser();
      parser.setLanguage(grammar);
      const h = fitQuery(rt, grammar, highlights);
      const i = injections ? fitQuery(rt, grammar, injections) : null;
      this.dropped.set(`${id}/${gid}`, [...h.dropped, ...(i?.dropped ?? [])]);
      grammars.set(gid, { parser, highlights: h.query, injections: i?.query ?? null });
    }
    return { lang, grammars };
  }

  private async queryText(paths: string[]): Promise<string> {
    return (await Promise.all(paths.map((p) => this.assets.query(p)))).join("\n");
  }

  /** `text` highlighted as `id` (null when `id` isn't loaded, the text is too long or its parse fails or runs out of time). */
  highlight(id: string, text: string): Highlight | null {
    const l = this.loaded.get(id);
    if (!l || text.length > MAX_CHARS) return null;
    const paint = new Uint8Array(text.length);
    const wants = new Set<string>();
    const deadline = now() + (this.opts.budgetMs ?? BUDGET_MS);
    try {
      this.layer(l, text, null, paint, wants, deadline, 0, l.lang.id);
    } catch (e) {
      if (!(e instanceof OutOfTime)) console.warn(`syntax: highlighting ${id} failed:`, e);
      return null;
    }
    return { lines: linesOf(text, paint), wants: [...wants] };
  }

  /** One language's layer of the text (all of it, or `ranges` of it): its captures painted, then what's written inside it. */
  private layer(l: Loaded, text: string, ranges: Range[] | null, paint: Uint8Array, wants: Set<string>, deadline: number, depth: number, parent: string) {
    const body = ranges ? text.slice(ranges[0]?.startIndex ?? 0, ranges[ranges.length - 1]?.endIndex ?? 0) : text;
    const gid = l.lang.alt?.when(body) ? l.lang.alt.grammar : (l.lang.grammar ?? l.lang.id);
    const g = l.grammars.get(gid);
    if (!g) return;
    const late = () => now() > deadline;
    const tree = g.parser.parse(text, null, { includedRanges: ranges ?? undefined, progressCallback: late });
    if (!tree) {
      g.parser.reset();
      throw new OutOfTime();
    }
    try {
      const opts = { matchLimit: MATCH_LIMIT, progressCallback: late };
      this.paintCaptures(g.highlights, g.highlights.matches(tree.rootNode, opts), text, ranges, paint, l.lang.precedence === "first");
      if (late()) throw new OutOfTime();
      if (!g.injections || depth >= MAX_DEPTH) return;
      for (const inj of injectionsOf(g.injections, g.injections.matches(tree.rootNode, opts), text, l.lang.id, parent)) {
        const sub = languageByName(inj.lang);
        if (!sub) continue;
        const loaded = this.loaded.get(sub.id);
        if (!loaded) {
          if (!this.failed.has(sub.id)) wants.add(sub.id);
          continue;
        }
        // Its colours go over the outer language's: what it doesn't colour keeps the outer's (a
        // comment's words, around the JSDoc tags in it).
        this.layer(loaded, text, inj.ranges, paint, wants, deadline, depth + 1, l.lang.id);
      }
    } finally {
      tree.delete();
    }
  }

  private paintCaptures(q: Query, matches: QueryMatch[], text: string, ranges: Range[] | null, paint: Uint8Array, first: boolean) {
    // The winning capture for each stretch of text (the same node, or one with the same span).
    const best = new Map<number, Paint>();
    for (const m of matches) {
      if (!holds(q.predicates[m.patternIndex], m, text)) continue;
      const priority = Number(m.setProperties?.priority ?? 100) || 100;
      m.captures.forEach((c, i) => {
        const name = classOf(c.name);
        if (name === null) return;
        const start = c.node.startIndex;
        const end = c.node.endIndex;
        if (end <= start) return;
        const rank = m.patternIndex * 64 + i;
        const p: Paint = { start, end, cls: CLASS_INDEX.get(name) ?? 0, priority, rank: first ? -rank : rank };
        const key = start * 1_048_576 + end;
        const had = best.get(key);
        if (!had || p.priority > had.priority || (p.priority === had.priority && p.rank > had.rank)) best.set(key, p);
      });
    }
    // Outer spans first, so what's inside them paints over them; a higher priority over everything.
    const order = [...best.values()].sort((a, b) => a.priority - b.priority || a.start - b.start || b.end - a.end);
    for (const p of order) {
      if (!ranges) {
        paint.fill(p.cls, p.start, p.end);
        continue;
      }
      // An injected language paints only its own text, never what the outer language left out of it.
      for (const r of ranges) {
        if (r.endIndex <= p.start) continue;
        if (r.startIndex >= p.end) break;
        paint.fill(p.cls, Math.max(p.start, r.startIndex), Math.min(p.end, r.endIndex));
      }
    }
  }
}

const now = () => (typeof performance !== "undefined" ? performance.now() : Date.now());

/** Each line's runs of one class (plain runs left out). */
function linesOf(text: string, paint: Uint8Array): Tok[][] {
  const out: Tok[][] = [];
  let start = 0;
  for (;;) {
    const nl = text.indexOf("\n", start);
    const end = nl < 0 ? text.length : nl;
    const toks: Tok[] = [];
    for (let i = start; i < end;) {
      const c = paint[i] ?? 0;
      let j = i + 1;
      while (j < end && paint[j] === c) j++;
      if (c) toks.push([i - start, j - start, CLASS_LIST[c] ?? ""]);
      i = j;
    }
    out.push(toks);
    if (nl < 0) return out;
    start = nl + 1;
  }
}

/* ------------------------------------------------------------------ predicates */

const luaClasses: Record<string, string> = {
  a: "A-Za-z",
  d: "0-9",
  l: "a-z",
  u: "A-Z",
  w: "A-Za-z0-9",
  x: "0-9A-Fa-f",
  s: "\\s",
  p: "!-/:-@\\[-`{-~",
  c: "\\x00-\\x1f\\x7f",
};

/** A Lua pattern as a RegExp (the parts queries use: classes, sets, anchors, `-` for lazy); null when it can't be one. */
export function luaRegex(pattern: string): RegExp | null {
  let out = "";
  let inSet = false;
  for (let i = 0; i < pattern.length; i++) {
    const ch = pattern.charAt(i);
    if (ch === "%") {
      const n = pattern.charAt(++i);
      const cls = luaClasses[n.toLowerCase()];
      if (cls) {
        const negated = n !== n.toLowerCase();
        out += inSet ? (negated ? "" : cls) : `[${negated ? "^" : ""}${cls}]`;
        if (inSet && negated) return null;
      } else if (n === "b" || n === "f") return null;
      else out += `\\${n}`;
    } else if (ch === "[" && !inSet) {
      inSet = true;
      out += "[";
      if (pattern.charAt(i + 1) === "^") {
        out += "^";
        i++;
      }
    } else if (ch === "]" && inSet) {
      inSet = false;
      out += "]";
    } else if (inSet) out += /[\\\]^-]/.test(ch) && ch !== "-" ? `\\${ch}` : ch;
    else if (ch === "-") out += "*?";
    else if ("|{}/\\".includes(ch)) out += `\\${ch}`;
    else out += ch;
  }
  try {
    return new RegExp(out);
  } catch {
    return null;
  }
}

const regexes = new Map<string, RegExp | null>();
function cachedRegex(kind: string, source: string): RegExp | null {
  const key = `${kind}\0${source}`;
  if (!regexes.has(key)) {
    let re: RegExp | null;
    try {
      re = kind === "lua" ? luaRegex(source) : new RegExp(source);
    } catch {
      re = null;
    }
    regexes.set(key, re);
  }
  return regexes.get(key) ?? null;
}

function stringsOf(p: QueryPredicate): string[] {
  return p.operands.slice(1).flatMap((o) => (o.type === "string" ? [o.value] : []));
}

/** Whether a match passes the predicates web-tree-sitter left to its caller (the rest it checked itself). */
function holds(preds: readonly QueryPredicate[] | undefined, m: QueryMatch, text: string): boolean {
  if (!preds?.length) return true;
  for (const p of preds) {
    const first = p.operands[0];
    if (!first || first.type !== "capture") continue;
    const nodes = m.captures.filter((c) => c.name === first.name).map((c) => c.node);
    const negated = p.operator.startsWith("not-");
    const op = negated ? p.operator.slice(4) : p.operator;
    const args = stringsOf(p);
    let test: ((n: Node) => boolean) | null = null;
    if (op === "lua-match?" || op === "vim-match?") {
      const re = cachedRegex(op === "lua-match?" ? "lua" : "js", args[0] ?? "");
      if (re) test = (n) => re.test(text.slice(n.startIndex, n.endIndex));
    } else if (op === "has-ancestor?") {
      const types = new Set(args);
      test = (n) => {
        for (let a = n.parent; a; a = a.parent) if (types.has(a.type)) return true;
        return false;
      };
    } else if (op === "has-parent?") {
      const types = new Set(args);
      test = (n) => !!n.parent && types.has(n.parent.type);
    } else if (op === "contains?" || op === "any-contains?") {
      test = (n) => args.some((a) => text.slice(n.startIndex, n.endIndex).includes(a));
    } else if (op === "kind-eq?") {
      const types = new Set(args);
      test = (n) => types.has(n.type);
    }
    // Directives (`#offset!`, `#gsub!`) and predicates no theme needs pass.
    if (test && nodes.some((n) => test(n) === negated)) return false;
  }
  return true;
}

/* ------------------------------------------------------------------ injections */

interface Injection {
  lang: string;
  ranges: Range[];
}

/** What an injections query finds: each language written in the text, and the ranges of it. */
function injectionsOf(q: Query, matches: QueryMatch[], text: string, self: string, parent: string): Injection[] {
  const single: Injection[] = [];
  const combined = new Map<string, Injection>();
  for (const m of matches) {
    if (!holds(q.predicates[m.patternIndex], m, text)) continue;
    const props = { ...q.setProperties[m.patternIndex], ...m.setProperties };
    const named = m.captures.find((c) => c.name === "injection.language");
    const lang = "injection.self" in props ? self : "injection.parent" in props ? parent : (props["injection.language"] ?? (named ? text.slice(named.node.startIndex, named.node.endIndex) : null));
    if (!lang) continue;
    const offset = q.predicates[m.patternIndex]?.find((p) => p.operator === "offset!");
    const ranges: Range[] = [];
    for (const c of m.captures) {
      if (c.name !== "injection.content") continue;
      ranges.push(...contentRanges(c.node, "injection.include-children" in props, offset && offset.operands[0]?.type === "capture" && offset.operands[0].name === c.name ? offset : undefined));
    }
    if (!ranges.length) continue;
    if ("injection.combined" in props) {
      const key = `${m.patternIndex}\0${lang}`;
      const had = combined.get(key);
      if (had) had.ranges.push(...ranges);
      else combined.set(key, { lang, ranges });
    } else single.push({ lang, ranges: tidy(ranges) });
  }
  for (const c of combined.values()) single.push({ lang: c.lang, ranges: tidy(c.ranges) });
  return single;
}

/**
 * A node's text to inject: all of it, or (by default) all but its named children's, as Neovim
 * reads it (a fenced block's code keeps the brackets tree-sitter-markdown makes tokens of it, and
 * loses what's only Markdown's: a quote's `>` before each line).
 */
function contentRanges(n: Node, children: boolean, offset: QueryPredicate | undefined): Range[] {
  let whole: Range = { startIndex: n.startIndex, endIndex: n.endIndex, startPosition: n.startPosition, endPosition: n.endPosition };
  if (offset) {
    const [sr, sc, er, ec] = stringsOf(offset).map(Number);
    // Only the columns move (what queries use it for: the quotes of a string); a shift of rows is left as it is.
    if (sr === 0 && er === 0 && Number.isFinite(sc) && Number.isFinite(ec)) {
      whole = {
        startIndex: whole.startIndex + (sc ?? 0),
        endIndex: whole.endIndex + (ec ?? 0),
        startPosition: { row: whole.startPosition.row, column: whole.startPosition.column + (sc ?? 0) },
        endPosition: { row: whole.endPosition.row, column: whole.endPosition.column + (ec ?? 0) },
      };
    }
    return whole.endIndex > whole.startIndex ? [whole] : [];
  }
  if (children || n.namedChildCount === 0) return [whole];
  const out: Range[] = [];
  let at = { index: n.startIndex, position: n.startPosition };
  for (const c of n.namedChildren) {
    if (!c) continue;
    if (c.startIndex > at.index) out.push({ startIndex: at.index, endIndex: c.startIndex, startPosition: at.position, endPosition: c.startPosition });
    at = { index: c.endIndex, position: c.endPosition };
  }
  if (n.endIndex > at.index) out.push({ startIndex: at.index, endIndex: n.endIndex, startPosition: at.position, endPosition: n.endPosition });
  return out;
}

/** Ranges in order, none empty, none overlapping (as a parser takes them). */
function tidy(ranges: Range[]): Range[] {
  const sorted = ranges.filter((r) => r.endIndex > r.startIndex).sort((a, b) => a.startIndex - b.startIndex);
  const out: Range[] = [];
  for (const r of sorted) {
    const last = out[out.length - 1];
    if (last && r.startIndex < last.endIndex) {
      if (r.endIndex > last.endIndex) out[out.length - 1] = { ...last, endIndex: r.endIndex, endPosition: r.endPosition };
    } else out.push(r);
  }
  return out;
}
