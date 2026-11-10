/**
 * kiln's UI server: TanStack Start's handler (dist/server/server.js) under Bun. kiln (the Rust
 * server) runs it and forwards pages, server functions and server components here; it serves the
 * static files (dist/client) itself.
 *
 *   bun ui/server/main.ts           PORT (7073), HOST (127.0.0.1)
 *   KILN_UI_STATIC=1                also serve dist/client (no kiln in front: tests)
 *   KILN_API_URL                    where server functions reach kiln's API (http://127.0.0.1:7070)
 */
const PORT = Number(process.env.PORT ?? 7073);
const HOST = process.env.HOST ?? "127.0.0.1";
const DIST = new URL("../dist/", import.meta.url);
const STATIC = process.env.KILN_UI_STATIC === "1";
const { privateResponse } = await import("./private-response");
const { agentEntry, preloadLinks, withEntryPreloads } = await import("./entry-preloads");
const { staticAssets } = await import("./static-assets");
const staticFile = STATIC
  ? await staticAssets(new URL("client/", DIST), new URL("encoded/", DIST), async (url) => {
      const file = Bun.file(url);
      return (await file.exists()) ? { body: file, bytes: file.size, type: file.type } : null;
    })
  : async (_req: Request): Promise<Response | null> => null;
const ENTRY_PRELOADS = await (async () => {
  try {
    const manifest = Bun.file(new URL("client/_entry-preloads.json", DIST));
    return (await manifest.exists()) && manifest.size <= 65536 ? preloadLinks(await manifest.json()) : "";
  } catch {
    return "";
  }
})();

const { default: start } = (await import(new URL("server/server.js", DIST).href)) as {
  default: { fetch: (request: Request) => Response | Promise<Response> };
};

/**
 * The request as the browser made it. kiln's proxy says how it was asked (scheme and host); Start
 * checks a server function call's Origin against the URL, so it must be the browser's.
 */
function asAsked(req: Request): Request {
  const proto = req.headers.get("x-forwarded-proto");
  const host = req.headers.get("x-forwarded-host");
  if (!proto && !host) return req;
  const url = new URL(req.url);
  if (proto === "http" || proto === "https") url.protocol = `${proto}:`;
  if (host) url.host = host;
  return new Request(url, req);
}

/**
 * The import map the build's chunks name each other through (scripts/stable-names.mjs, goal 8 step
 * 4): every page needs it before its first module, so it goes in right after `<head>`.
 */
const IMPORT_MAP = await (async () => {
  const f = Bun.file(new URL("client/_importmap.json", DIST));
  return (await f.exists()) ? `<script type="importmap">${await f.text()}</script>` : "";
})();

function withImportMap(res: Response): Response {
  if (!IMPORT_MAP || !res.body || !(res.headers.get("content-type") ?? "").includes("text/html")) return res;
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  let head = "";
  let done = false;
  const body = res.body.pipeThrough(
    new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, out) {
        if (done) return out.enqueue(chunk);
        head += dec.decode(chunk, { stream: true });
        const at = head.indexOf("<head>");
        if (at < 0 && head.length < 65536) return;
        done = true;
        out.enqueue(enc.encode(at < 0 ? head : head.slice(0, at + 6) + IMPORT_MAP + head.slice(at + 6)));
      },
      flush(out) {
        if (!done) out.enqueue(enc.encode(head + dec.decode()));
      },
    }),
  );
  const headers = new Headers(res.headers);
  headers.delete("content-length");
  return new Response(body, { status: res.status, statusText: res.statusText, headers });
}

const server = Bun.serve({
  hostname: HOST,
  port: PORT,
  async fetch(req) {
    try {
      return (await staticFile(req)) ?? privateResponse(withEntryPreloads(withImportMap(await start.fetch(asAsked(req))), agentEntry(new URL(req.url).pathname) ? ENTRY_PRELOADS : ""));
    } catch (e) {
      console.error("ui server:", e);
      return privateResponse(
        new Response("kiln's UI server failed on this request", {
          status: 500,
        }),
      );
    }
  },
});

console.log(`kiln ui server on http://${server.hostname}:${server.port}`);

// Started by kiln: stop when kiln does, however it stopped (it can't always stop this first).
const parent = process.ppid;
if (process.env.KILN_UI_STATIC !== "1") {
  setInterval(() => {
    if (process.ppid !== parent) {
      console.log("kiln ui server: kiln is gone; stopping");
      server.stop(true);
      process.exit(0);
    }
  }, 1000).unref();
}
