/**
 * Syntax highlighting for the review page's diffs, with the editor's grammars (CodeMirror's Lezer
 * parsers, W21-R01; W14-R06): each language loads only when a diff first needs it, and a run of
 * lines (a hunk's old side, or its new side) is parsed as one piece of code, so strings and
 * comments that span lines read right. The colours are the editor's (components/code/editor.tsx).
 */
import type { Parser } from "@lezer/common";
import { highlightTree, tagHighlighter, tags as t } from "@lezer/highlight";

/** A run of a line that reads as one kind of token: [start, end, class]. */
export type Tok = [number, number, string];

const highlighter = tagHighlighter([
  { tag: [t.propertyName, t.definition(t.propertyName), t.attributeName], class: "hl-prop" },
  { tag: [t.string, t.special(t.string), t.regexp, t.character], class: "hl-str" },
  { tag: [t.number, t.bool, t.null, t.atom, t.unit], class: "hl-num" },
  { tag: [t.comment, t.lineComment, t.blockComment, t.docComment], class: "hl-com" },
  { tag: [t.keyword, t.controlKeyword, t.operatorKeyword, t.definitionKeyword, t.moduleKeyword, t.modifier, t.self], class: "hl-kw" },
  { tag: [t.operator, t.punctuation, t.separator, t.bracket, t.derefOperator], class: "hl-punct" },
  { tag: [t.meta, t.labelName, t.macroName, t.annotation, t.processingInstruction], class: "hl-meta" },
  { tag: [t.typeName, t.className, t.namespace, t.standard(t.typeName)], class: "hl-type" },
  { tag: [t.function(t.variableName), t.function(t.propertyName)], class: "hl-fn" },
  { tag: [t.heading, t.strong], class: "hl-strong" },
  { tag: [t.link, t.url], class: "hl-link" },
]);

/** Each language's parser, loaded on first use. */
const LOADERS: Record<string, () => Promise<Parser>> = {
  rust: () => import("@codemirror/lang-rust").then((m) => m.rust().language.parser),
  ts: () => import("@codemirror/lang-javascript").then((m) => m.javascript({ typescript: true }).language.parser),
  tsx: () => import("@codemirror/lang-javascript").then((m) => m.javascript({ typescript: true, jsx: true }).language.parser),
  js: () => import("@codemirror/lang-javascript").then((m) => m.javascript().language.parser),
  jsx: () => import("@codemirror/lang-javascript").then((m) => m.javascript({ jsx: true }).language.parser),
  python: () => import("@codemirror/lang-python").then((m) => m.python().language.parser),
  go: () => import("@codemirror/lang-go").then((m) => m.go().language.parser),
  css: () => import("@codemirror/lang-css").then((m) => m.css().language.parser),
  json: () => import("@codemirror/lang-json").then((m) => m.json().language.parser),
  markdown: () => import("@codemirror/lang-markdown").then((m) => m.markdown().language.parser),
  html: () => import("@codemirror/lang-html").then((m) => m.html().language.parser),
  yaml: () => import("@codemirror/lang-yaml").then((m) => m.yaml().language.parser),
  toml: () => Promise.all([import("@codemirror/language"), import("@codemirror/legacy-modes/mode/toml")]).then(([l, m]) => l.StreamLanguage.define(m.toml).parser),
  shell: () => Promise.all([import("@codemirror/language"), import("@codemirror/legacy-modes/mode/shell")]).then(([l, m]) => l.StreamLanguage.define(m.shell).parser),
  dockerfile: () => Promise.all([import("@codemirror/language"), import("@codemirror/legacy-modes/mode/dockerfile")]).then(([l, m]) => l.StreamLanguage.define(m.dockerFile).parser),
};

const loaded = new Map<string, Promise<Parser | null>>();

/** Language `lang`'s parser (null when kiln has none, or it failed to load). */
export function parserFor(lang: string | null): Promise<Parser | null> {
  if (!lang) return Promise.resolve(null);
  const load = LOADERS[lang];
  if (!load) return Promise.resolve(null);
  let p = loaded.get(lang);
  if (!p) {
    p = load().catch(() => null);
    loaded.set(lang, p);
  }
  return p;
}

/** Past this much code at once, a run isn't highlighted (it reads plain; nothing waits on it). */
const MAX_CHARS = 400_000;

/** `lines` as one piece of code, each line's tokens. */
export function highlightLines(parser: Parser, lines: string[]): Tok[][] {
  const out: Tok[][] = lines.map(() => []);
  const text = lines.join("\n");
  if (text.length > MAX_CHARS) return out;
  // Where each line starts in the text.
  const starts: number[] = [];
  let at = 0;
  for (const l of lines) {
    starts.push(at);
    at += l.length + 1;
  }
  let li = 0;
  highlightTree(parser.parse(text), highlighter, (from, to, cls) => {
    // A token may run over several lines (a block comment): each line takes its part.
    while (li < lines.length - 1 && (starts[li + 1] ?? Infinity) <= from) li++;
    let k = li;
    let s = from;
    while (s < to && k < lines.length) {
      const ls = starts[k] ?? 0;
      const le = ls + (lines[k]?.length ?? 0);
      const e = Math.min(to, le);
      if (e > s) out[k]?.push([s - ls, e - ls, cls]);
      k++;
      s = le + 1;
    }
  });
  return out;
}
