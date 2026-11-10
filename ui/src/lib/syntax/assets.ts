/** The same grammar/query assets in the page's synchronous highlighter and W26-R15's worker. */
import type { Assets } from "./engine";

let files: { grammars: Record<string, string>; queries: Record<string, () => Promise<string>> } | null = null;
const filesOf = () =>
  (files ??= {
    grammars: import.meta.glob<string>("./grammars/*.wasm.gz", { query: "?url&no-inline", import: "default", eager: true }),
    queries: import.meta.glob<string>(["./grammars/queries/**/*.scm", "./queries/*.scm"], { query: "?raw", import: "default" }),
  });

export const assets: Assets = {
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
