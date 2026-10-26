/** The highlighter as the tests (node) run it: web-tree-sitter from node_modules, the grammars and queries from src/lib/syntax. */
import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { Highlighter, type Assets, type Highlight } from "../src/lib/syntax/engine.ts";

const SYNTAX = new URL("../src/lib/syntax/", import.meta.url);

export const nodeAssets: Assets = {
  runtime: async () => {
    const rt = await import("web-tree-sitter");
    await rt.Parser.init();
    return rt;
  },
  grammar: async (id) => new Uint8Array(gunzipSync(readFileSync(new URL(`grammars/${id}.wasm.gz`, SYNTAX)))),
  query: async (path) => readFileSync(new URL(path, SYNTAX), "utf8"),
};

export const highlighter = new Highlighter(nodeAssets);

/** `text` as `lang` highlights it, token by token: `[text, class]` for each coloured run. */
export async function tokens(lang: string, text: string): Promise<[string, string][]> {
  if (!(await highlighter.load(lang))) throw new Error(`${lang} didn't load`);
  const h = highlighter.highlight(lang, text);
  if (!h) throw new Error(`${lang} didn't highlight`);
  if (h.wants.length) {
    await Promise.all(h.wants.map((w) => highlighter.load(w)));
    return tokens(lang, text);
  }
  const lines = text.split("\n");
  return h.lines.flatMap((toks, i) => toks.map(([s, e, c]): [string, string] => [(lines[i] ?? "").slice(s, e), c]));
}

/**
 * The class `lang` gives `word` where it's first written in `text` (as a whole word): "" when it's
 * plain, "mixed" when its letters aren't all one class.
 */
export async function classOfWord(lang: string, text: string, word: string): Promise<string> {
  const h = await highlighted(lang, text);
  const edge = (c: string) => /[\p{L}\p{N}_$@#-]/u.test(c);
  let at = text.indexOf(word);
  while (at >= 0 && ((edge(word.charAt(0)) && edge(text.charAt(at - 1))) || (edge(word.charAt(word.length - 1)) && edge(text.charAt(at + word.length))))) at = text.indexOf(word, at + 1);
  if (at < 0) throw new Error(`${JSON.stringify(word)} isn't in the ${lang} sample`);
  const line = text.slice(0, at).split("\n").length - 1;
  const col = at - (text.lastIndexOf("\n", at - 1) + 1);
  const classes = new Set<string>();
  for (let i = col; i < col + word.length; i++) classes.add(h.lines[line]?.find(([s, e]) => s <= i && i < e)?.[2] ?? "");
  return classes.size === 1 ? [...classes][0]! : "mixed";
}

/** `text` highlighted as `lang`, once every language written in it has loaded too. */
export async function highlighted(lang: string, text: string): Promise<Highlight> {
  if (!(await highlighter.load(lang))) throw new Error(`${lang} didn't load`);
  const h = highlighter.highlight(lang, text);
  if (!h) throw new Error(`${lang} didn't highlight`);
  if (!h.wants.length) return h;
  await Promise.all(h.wants.map((w) => highlighter.load(w)));
  return highlighted(lang, text);
}
