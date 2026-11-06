// The reference servers' origin (docs/browser-lab.md, "The baselines"; plan 01a1a832e9cc step 5):
// what h2o and Caddy pass streams and writes to, over a Unix socket, so each serves the same
// operations kiln does with the same bodies. Requests' bodies are static files the servers serve
// themselves (their best path); this serves:
//   GET  /stream/<kind>          a stream as Server-Sent Events (`data: <frame>`, one per event)
//   GET  /fetch-stream/<kind>    the same frames, one per line, for a streaming fetch
//   POST /api/...                a write: its recorded answer
//   GET  /__lab/probe.js|html    the probe page, cross-origin isolated
// and, on a second socket only kiln-lab reaches (never through a server):
//   POST /replay {kind, events, every_ms, from?}  send `events` of the kind's recorded events
//        to every open stream of it, `every_ms` apart, each stamped `@lab:<µs>` as it goes out,
//        the stamp keeping its length so every frame is the size kiln's was. An event is a frame
//        with a stamp; the recorded frames between two (a session's state, an entity's patch) go
//        with the event after them, as kiln sends them, so `events` here are as many as kiln's
//        stand-ins write (plan 01a1a832e9cc step 8: counting frames instead gave the baselines
//        about 65% of kiln's events). Answers `frames`, how many recorded frames that took (the
//        next replay's `from`). `frames: N` in place of `events` sends N recorded frames as they are.
//   node origin.mjs PAYLOADS_DIR PUBLIC_SOCK CONTROL_SOCK
import { createServer } from "node:http";
import { readFileSync, existsSync, unlinkSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const [payloads, publicSock, controlSock] = process.argv.slice(2);
const replay = JSON.parse(readFileSync(join(payloads, "replay.json"), "utf8"));
// The kinds whose frames carry stamps (each stamped frame is an event), found once, not per replay.
const stampedKinds = new Set(Object.entries(replay.streams || {}).filter(([, fs]) => fs.some((f) => /@lab:\d{16}/.test(f))).map(([k]) => k));
const nowUs = () => Math.round((performance.timeOrigin + performance.now()) * 1000);
const STAMP = /@lab:\d{16}/g;
const STAMPED = /@lab:\d{16}/;
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

// A terminal's and a log's output is text, not JSON, and neither carrier frames it as it is: an
// event's data ends at any CR or LF, and the streaming fetch's frame at a LF, while a terminal's
// frames end in CR CR LF and a log's may hold several lines. Each goes as a JSON string, so the page
// gets every byte back (JSON.parse) and the escapes are the carrier's cost; JSON frames go as they
// are (JSON.stringify writes no raw line end).
const TEXT = new Set(["terminal", "log"]);

function send(kind, frame) {
  const f = TEXT.has(kind) ? JSON.stringify(frame) : frame;
  for (const s of streams.get(kind) || []) s.res.write(s.carrier === "sse" ? `data: ${f}\n\n` : `${f}\n`);
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
    // Each event's frames: the unstamped ones before it, then it (a kind with no stamp at all
    // sends a frame an event).
    const groups = [];
    let at = r.from || 0;
    // `frames` instead of `events`: exactly that many recorded frames, one a tick (check-origin's
    // replay of the whole recording).
    const anyStamped = r.frames === undefined && stampedKinds.has(r.kind);
    while (groups.length < (r.frames ?? r.events)) {
      const g = [];
      for (;;) {
        const f = frames[at % frames.length];
        at += 1;
        g.push(f);
        if (!anyStamped || STAMPED.test(f)) break;
      }
      groups.push(g);
    }
    let i = 0;
    const tick = () => {
      if (i >= groups.length) return;
      for (const f of groups[i]) send(r.kind, stamp(f));
      i += 1;
      if (i < groups.length) setTimeout(tick, r.every_ms);
    };
    tick();
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ kind: r.kind, events: r.events, frames: at - (r.from || 0), open: (streams.get(r.kind) || new Set()).size }));
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
