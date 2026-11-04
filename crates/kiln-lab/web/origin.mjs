// The reference servers' origin (docs/browser-lab.md, "The baselines"; plan 01a1a832e9cc step 5):
// what h2o and Caddy pass streams and writes to, over a Unix socket, so each serves the same
// operations kiln does with the same bodies. Requests' bodies are static files the servers serve
// themselves (their best path); this serves:
//   GET  /stream/<kind>          a stream as Server-Sent Events (`data: <frame>`, one per event)
//   GET  /fetch-stream/<kind>    the same frames, one per line, for a streaming fetch
//   POST /api/...                a write: its recorded answer
//   GET  /__lab/probe.js|html    the probe page, cross-origin isolated
// and, on a second socket only kiln-lab reaches (never through a server):
//   POST /replay {kind, events, every_ms, from?}  send `events` of the kind's recorded frames
//        to every open stream of it, `every_ms` apart, each stamped `@lab:<µs>` as it goes out,
//        the stamp keeping its length so every frame is the size kiln's was.
//   node origin.mjs PAYLOADS_DIR PUBLIC_SOCK CONTROL_SOCK
import { createServer } from "node:http";
import { readFileSync, existsSync, unlinkSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const [payloads, publicSock, controlSock] = process.argv.slice(2);
const replay = JSON.parse(readFileSync(join(payloads, "replay.json"), "utf8"));
const nowUs = () => Math.round((performance.timeOrigin + performance.now()) * 1000);
const STAMP = /@lab:\d{16}/g;
const streams = new Map();

const stamp = (frame) => {
  const s = `@lab:${String(nowUs()).padStart(16, "0")}`;
  return frame.replace(STAMP, s);
};

const PROBE_HTML = `<!doctype html><meta charset="utf-8"><title>kiln browser lab probe</title><script src="/__lab/probe.js"></script><p>probe</p>`;
const isolated = { "cross-origin-opener-policy": "same-origin", "cross-origin-embedder-policy": "require-corp", "cache-control": "no-store" };

function open(req, res, kind, carrier) {
  res.writeHead(200, { "content-type": carrier === "sse" ? "text/event-stream" : "application/x-ndjson", "cache-control": "no-store", "x-accel-buffering": "no" });
  res.flushHeaders();
  res.socket.setNoDelay(true);
  const set = streams.get(kind) || new Set();
  const s = { res, carrier };
  set.add(s);
  streams.set(kind, set);
  req.on("close", () => set.delete(s));
}

function send(kind, frame) {
  for (const s of streams.get(kind) || []) s.res.write(s.carrier === "sse" ? `data: ${frame}\n\n` : `${frame}\n`);
}

const pub = createServer((req, res) => {
  const u = new URL(req.url, "http://origin");
  const m = u.pathname.match(/^\/(stream|fetch-stream)\/([\w-]+)$/);
  if (m) return open(req, res, m[2], m[1] === "stream" ? "sse" : "fetch-stream");
  if (u.pathname === "/__lab/probe.js") {
    res.writeHead(200, { ...isolated, "content-type": "text/javascript" });
    return res.end(readFileSync(join(HERE, "probe/probe.js")));
  }
  if (u.pathname === "/__lab/probe.html") {
    res.writeHead(200, { ...isolated, "content-type": "text/html" });
    return res.end(PROBE_HTML);
  }
  const w = (replay.writes || {})[`${req.method} ${u.pathname}`];
  if (w !== undefined) {
    req.resume();
    req.on("end", () => {
      res.writeHead(w.status || 200, { "content-type": "application/json", "cache-control": "no-store" });
      res.end(w.body);
    });
    return;
  }
  res.writeHead(404, { "content-type": "application/json" });
  res.end('{"error":"not in the catalogue"}');
});

const ctl = createServer((req, res) => {
  let body = "";
  req.on("data", (d) => (body += d));
  req.on("end", () => {
    if (req.url !== "/replay") {
      res.writeHead(404);
      return res.end();
    }
    const r = JSON.parse(body || "{}");
    const frames = (replay.streams || {})[r.kind] || [];
    if (!frames.length) {
      res.writeHead(400);
      return res.end(`no frames for ${r.kind}`);
    }
    let i = 0;
    const from = r.from || 0;
    const tick = () => {
      if (i >= r.events) return;
      send(r.kind, stamp(frames[(from + i) % frames.length]));
      i += 1;
      if (i < r.events) setTimeout(tick, r.every_ms);
    };
    tick();
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ kind: r.kind, events: r.events, open: (streams.get(r.kind) || new Set()).size }));
  });
});

for (const [srv, path] of [
  [pub, publicSock],
  [ctl, controlSock],
]) {
  if (existsSync(path)) unlinkSync(path);
  srv.keepAliveTimeout = 600_000;
  srv.requestTimeout = 0;
  srv.listen(path);
}
