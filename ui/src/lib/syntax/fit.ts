/**
 * A query fitted to the grammar it runs on. Queries are written for one version of one grammar,
 * and kiln reads some with another: TypeScript's with JavaScript's (an older TypeScript, without a
 * keyword JavaScript has since), OCaml's interfaces with OCaml's, PHP alone with PHP's. One pattern
 * naming a node the grammar hasn't got and the whole query fails, so before compiling:
 *
 * - a node, token or field the grammar hasn't got takes out the least that holds it: the
 *   alternative of a `[…]` it's in, else its whole pattern;
 * - what still doesn't compile (syntax a newer tree-sitter rejects) takes out the pattern the
 *   error is in, and for a list of tokens, each token that can't appear.
 *
 * What's taken out is kept, for the tests to see.
 */
import type { Language as Grammar, Query } from "web-tree-sitter";

type Runtime = typeof import("web-tree-sitter");

interface Item {
  kind: "list" | "alt" | "str" | "word" | "pred" | "anchor";
  start: number;
  /** Past its quantifier and captures. */
  end: number;
  items: Item[];
  /** A word's name, or a string's token. */
  text?: string;
  field?: string;
}

/** The query's patterns, each with its parts, in source order. */
export function parseQuery(src: string): Item[] {
  let i = 0;
  const skip = () => {
    for (;;) {
      while (i < src.length && /\s/.test(src.charAt(i))) i++;
      if (src.charAt(i) !== ";") return;
      while (i < src.length && src.charAt(i) !== "\n") i++;
    }
  };
  const word = () => {
    const m = /^[^\s()[\]"@;:]+/.exec(src.slice(i));
    return m ? m[0] : "";
  };
  const string = (): string => {
    let out = "";
    i++;
    while (i < src.length && src.charAt(i) !== '"') {
      if (src.charAt(i) === "\\") {
        const n = src.charAt(++i);
        out += n === "n" ? "\n" : n === "t" ? "\t" : n === "r" ? "\r" : n === "0" ? "\0" : n;
      } else out += src.charAt(i);
      i++;
    }
    i++;
    return out;
  };
  const suffixes = (it: Item) => {
    for (;;) {
      skip();
      const c = src.charAt(i);
      if (c === "*" || c === "+" || c === "?") i++;
      else if (c === "@") {
        i++;
        i += word().length;
      } else break;
    }
    it.end = i;
  };
  const form = (): Item | null => {
    skip();
    if (i >= src.length) return null;
    const start = i;
    let field: string | undefined;
    const w0 = word();
    if (src.charAt(i + w0.length) === ":" && w0) {
      field = w0.replace(/^!/, "");
      i += w0.length + 1;
      skip();
    } else if (w0.startsWith("!")) {
      i += w0.length;
      const it: Item = { kind: "word", start, end: i, items: [], field: w0.slice(1) };
      return it;
    }
    const c = src.charAt(i);
    let it: Item;
    if (c === "(" || c === "[") {
      i++;
      skip();
      const pred = c === "(" && src.charAt(i) === "#";
      it = { kind: pred ? "pred" : c === "(" ? "list" : "alt", start, end: i, items: [], field };
      const close = c === "(" ? ")" : "]";
      if (pred) {
        // A predicate's arguments aren't patterns: skip to its end.
        while (i < src.length && src.charAt(i) !== ")") {
          if (src.charAt(i) === '"') string();
          else i++;
        }
        i++;
        it.end = i;
        return it;
      }
      for (;;) {
        skip();
        if (i >= src.length) break;
        if (src.charAt(i) === close) {
          i++;
          break;
        }
        const child = form();
        if (!child) break;
        it.items.push(child);
      }
    } else if (c === '"') {
      it = { kind: "str", start, end: i, items: [], text: string(), field };
    } else if (c === ".") {
      i++;
      return { kind: "anchor", start, end: i, items: [] };
    } else {
      const w = word();
      if (!w) {
        i++;
        return { kind: "anchor", start, end: i, items: [] };
      }
      i += w.length;
      it = { kind: "word", start, end: i, items: [], text: w, field };
    }
    suffixes(it);
    return it;
  };
  const out: Item[] = [];
  for (;;) {
    const f = form();
    if (!f) return out;
    out.push(f);
  }
}

/** The ranges to take out of the query for it to fit `grammar`. */
function misfits(grammar: Grammar, patterns: Item[]): [number, number][] {
  const cut: [number, number][] = [];
  const bad = (it: Item): boolean => {
    if (it.field && grammar.fieldIdForName(it.field) === null) return true;
    if (it.kind === "str") return grammar.idForNodeType(it.text ?? "", false) === null;
    if (it.kind === "list") {
      const head = it.items[0];
      if (head?.kind === "word" && !head.field) {
        const name = head.text ?? "";
        if (name !== "_" && name !== "ERROR" && name !== "MISSING" && !name.includes("/") && grammar.idForNodeType(name, true) === null) return true;
      }
    }
    return false;
  };
  // Whether `it` holds anything that doesn't fit; an alternative that doesn't is cut on its own.
  const walk = (it: Item): boolean => {
    if (bad(it)) return true;
    if (it.kind === "alt") {
      const kept = it.items.filter((alt) => {
        if (!walk(alt)) return true;
        cut.push([alt.start, alt.end]);
        return false;
      });
      return kept.length === 0;
    }
    return it.items.some(walk);
  };
  for (const p of patterns) if (walk(p)) cut.push([p.start, p.end]);
  return cut;
}

/** `src` without `cuts` (a cut inside another goes with it). */
function without(src: string, cuts: [number, number][]): string {
  const sorted = [...cuts].sort((a, b) => a[0] - b[0] || b[1] - a[1]);
  let out = "";
  let at = 0;
  for (const [s, e] of sorted) {
    if (s < at) continue;
    out += src.slice(at, s);
    at = e;
  }
  return out + src.slice(at);
}

/** `src` compiled for `grammar`, after taking out what doesn't fit it (and what was taken out). */
export function fitQuery(rt: Runtime, grammar: Grammar, src: string): { query: Query; dropped: string[] } {
  const dropped: string[] = [];
  const note = (s: string) => dropped.push(s.replace(/\s+/g, " ").trim().slice(0, 120));
  const cuts = misfits(grammar, parseQuery(src));
  for (const [s, e] of cuts) note(src.slice(s, e));
  let text = without(src, cuts);
  for (let tries = 0; ; tries++) {
    try {
      return { query: new rt.Query(grammar, text), dropped };
    } catch (e) {
      const at = (e as { index?: number }).index;
      if (tries >= 64 || typeof at !== "number") throw e;
      const patterns = parseQuery(text);
      const p = patterns.find((x) => x.start <= at && at < x.end) ?? patterns.find((x) => x.start >= at);
      if (!p) throw e;
      // A list of tokens loses only those that can't appear.
      if (p.kind === "alt" && p.items.every((x) => x.kind === "str")) {
        const gone = p.items.filter((x) => {
          try {
            new rt.Query(grammar, text.slice(x.start, x.end) + " @_").delete();
            return false;
          } catch {
            return true;
          }
        });
        if (gone.length && gone.length < p.items.length) {
          for (const g of gone) note(text.slice(g.start, g.end));
          text = without(
            text,
            gone.map((g) => [g.start, g.end]),
          );
          continue;
        }
      }
      note(text.slice(p.start, p.end));
      text = without(text, [[p.start, p.end]]);
    }
  }
}
