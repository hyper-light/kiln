/**
 * The highlighter as the page has it: web-tree-sitter and each grammar fetched the first time code
 * in its language shows (built, a grammar is plain WebAssembly kiln sends Brotli-encoded; in the dev
 * server, the gzip file the source keeps, inflated here), its queries with it.
 * Code shows plain until then, and in its colours once the grammar is here: a component asks
 * through `useHighlighter`, which subscribes to what has loaded (useSyncExternalStore, no
 * effects) and starts loading what it needs while rendering, once, as lib/resource.ts does.
 * On the server, and while the page hydrates, everything reads plain.
 */
import * as React from "react";
import { Highlighter, type Assets, type Tok } from "./engine";
import { languageById, languageOfFence, languageOfFile, type Language } from "./languages";

/**
 * The grammars' and queries' files as the bundler makes them: each grammar an asset of its own,
 * each query a chunk of its own. Read only once something is highlighted (never outside a page:
 * the tests import views that hold code, and Node has no import.meta.glob).
 */
let files: { grammars: Record<string, string>; queries: Record<string, () => Promise<string>> } | null = null;
const filesOf = () =>
  (files ??= {
    grammars: import.meta.glob<string>("./grammars/*.wasm.gz", { query: "?url&no-inline", import: "default", eager: true }),
    queries: import.meta.glob<string>(["./grammars/queries/**/*.scm", "./queries/*.scm"], { query: "?raw", import: "default" }),
  });

const assets: Assets = {
  runtime: async () => {
    const [rt, wasm] = await Promise.all([import("web-tree-sitter"), import("web-tree-sitter/web-tree-sitter.wasm?url")]);
    await rt.Parser.init({ locateFile: () => wasm.default });
    return rt;
  },
  grammar: async (id) => {
    const url = filesOf().grammars[`./grammars/${id}.wasm.gz`];
    if (!url) throw new Error(`no grammar ${id}`);
    const res = await fetch(url);
    if (!res.ok || !res.body) throw new Error(`${url}: HTTP ${res.status}`);
    if (!url.endsWith(".gz")) return new Uint8Array(await res.arrayBuffer());
    return new Uint8Array(await new Response(res.body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());
  },
  query: (path) => {
    const load = filesOf().queries[`./${path}`];
    return load ? load() : Promise.reject(new Error(`no query ${path}`));
  },
};

const highlighter = new Highlighter(assets);

/** Bumped as each language loads (or fails to): what subscribers re-read. */
let loads = 0;
const listeners = new Set<() => void>();
const subscribe = (fn: () => void) => {
  listeners.add(fn);
  return () => listeners.delete(fn);
};
const asked = new Set<string>();

/**
 * Once the page has loaded what it started with (its load event): the highlighter's WebAssembly is
 * hundreds of KB, and on a slow link the page's own code mustn't wait behind it (goal 8, plan
 * 01a1a832ebfd step 6). Code shows plain until then, as it does until its grammar is here.
 */
const pageLoaded = (): Promise<void> => (document.readyState === "complete" ? Promise.resolve() : new Promise((done) => window.addEventListener("load", () => done(), { once: true })));

/** Starts loading a language, once. */
function want(id: string) {
  if (asked.has(id) || typeof window === "undefined") return;
  asked.add(id);
  void pageLoaded()
    .then(() => highlighter.load(id))
    .then(() => {
      loads++;
      for (const l of listeners) l();
    });
}

/** What a highlight of `l` depends on: whether it's here, and, for code that holds other languages, every load since. */
const stateOf = (l: Language) => (!highlighter.ready(l.id) ? 0 : l.injections ? loads + 1 : 1);

/**
 * A language by what a caller says of it: its name (an id, a fence's info string), else the file's
 * path, else the text's `#!` line. A name kiln doesn't know (```console, ```text) is no language:
 * whoever wrote it said what it is.
 */
export function resolveLanguage(name: string | null | undefined, path?: string | null, text?: string): Language | undefined {
  if (name) return languageById(name) ?? languageOfFence(name);
  return languageOfFile(path, text);
}

/** Highlights text as `lang`: each line's tokens, or null while its grammar loads (or never will). */
export type Highlight = (text: string) => Tok[][] | null;

/**
 * The highlighter for a language, once its grammar is here (null until then, on the server and
 * while hydrating). It changes when that language loads, and, for one that holds others (HTML,
 * Markdown, Vue), as each of those does.
 */
export function useHighlighter(lang: Language | null | undefined): Highlight | null {
  const state = React.useSyncExternalStore(
    subscribe,
    () => (lang ? stateOf(lang) : 0),
    () => 0,
  );
  if (lang && !state) want(lang.id);
  return React.useMemo(() => {
    if (!lang || !state) return null;
    return (text: string) => {
      const h = highlighter.highlight(lang.id, text);
      for (const w of h?.wants ?? []) want(w);
      return h?.lines ?? null;
    };
  }, [lang, state]);
}

/**
 * Highlights `text` as the language `id` now, outside React (an editor's view plugin): its lines'
 * tokens, or null while its grammar loads (asked for here, once), as `useHighlighter`'s do.
 */
export function highlightNow(id: string, text: string): Tok[][] | null {
  if (!highlighter.ready(id)) {
    want(id);
    return null;
  }
  const h = highlighter.highlight(id, text);
  for (const w of h?.wants ?? []) want(w);
  return h?.lines ?? null;
}

/** Calls `fn` as each language loads (or fails to): what's highlighted reads better then. Gives back what stops it. */
export function onLoad(fn: () => void): () => void {
  const off = subscribe(fn);
  return () => void off();
}

export type { Tok } from "./engine";
