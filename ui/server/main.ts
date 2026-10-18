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

async function staticFile(req: Request): Promise<Response | null> {
  if (!STATIC || (req.method !== "GET" && req.method !== "HEAD")) return null;
  const path = new URL(req.url).pathname;
  if (path === "/" || path.includes("..")) return null;
  const file = Bun.file(new URL(`client${path}`, DIST));
  if (!(await file.exists())) return null;
  const immutable = path.startsWith("/assets/");
  return new Response(file, { headers: { "cache-control": immutable ? "public, max-age=31536000, immutable" : "no-cache" } });
}

const server = Bun.serve({
  hostname: HOST,
  port: PORT,
  async fetch(req) {
    try {
      return (await staticFile(req)) ?? (await start.fetch(asAsked(req)));
    } catch (e) {
      console.error("ui server:", e);
      return new Response("kiln's UI server failed on this request", { status: 500 });
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
