// Turns record.mjs's recordings into the operation catalogue (docs/browser-lab/catalogue.json:
// sizes, rates and digests only, small) and the replay the reference servers serve (payloads under
// ~/.cache: replay.json for the origin, docroot/ for the servers' static files). Plan
// 01a1a832e9cc step 3.
//   node catalogue.mjs build RECORDINGS_DIR CATALOGUE.json PAYLOADS_DIR
//   node catalogue.mjs compare RECORDINGS_A RECORDINGS_B   (event bodies, stamps taken out)
import { readFileSync, writeFileSync, readdirSync, mkdirSync, existsSync, statSync, unlinkSync } from "node:fs";
import { join, dirname } from "node:path";
import { createHash } from "node:crypto";
import { gunzipSync } from "node:zlib";

const ID = /[0-9a-f]{32}/g;
const UUID = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/g;

/** A path as an operation's name: ids taken out, the query's keys kept. */
export function pattern(url) {
  const u = new URL(url);
  const path = u.pathname.replace(UUID, ":id").replace(ID, ":id").replace(/\/\d+(?=\/|$)/g, "/:n");
  const keys = [...u.searchParams.keys()].sort();
  return keys.length ? `${path}?${[...new Set(keys)].join("&")}` : path;
}

/** What a stream's frame says, without what differs between two runs of the same seed. */
export function normal(text) {
  return text
    .replace(/@lab:\d{16}/g, "@lab:")
    .replace(/(msg|toolu)_[0-9a-f]{12}/g, "$1_S")
    .replace(UUID, "ID")
    .replace(ID, "ID")
    .replace(/\b\d{10,19}\b/g, "N")
    .replace(/"(rev|seq|born|n|id|epoch)":\s*"?[\w.:-]+"?/g, '"$1":_');
}

const q = (sorted, p) => (sorted.length ? sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1))] : null);
const sizes = (v) => {
  const s = [...v].sort((a, b) => a - b);
  return { n: s.length, p50: q(s, 0.5), p99: q(s, 0.99), max: s.length ? s[s.length - 1] : null, mean: s.length ? Math.round(s.reduce((a, b) => a + b, 0) / s.length) : null };
};

/** A socket's kind, by its path; null for one the catalogue doesn't measure. */
function socketKind(p) {
  if (p.startsWith("/api/harness/sessions/:id/ws")) return "session";
  if (p.startsWith("/api/events")) return "events";
  if (p.startsWith("/api/jobs/:id/logs/ws")) return "log";
  if (p.startsWith("/api/terminal/ws")) return "terminal";
  return null;
}

function read(dir) {
  const journeys = [];
  for (const f of readdirSync(dir).filter((f) => f.endsWith(".jsonl")).sort()) {
    const events = readFileSync(join(dir, f), "utf8")
      .split("\n")
      .filter(Boolean)
      .map((l) => JSON.parse(l));
    journeys.push({ name: f.replace(/\.jsonl$/, ""), events, bodies: join(dir, f.replace(/\.jsonl$/, ".bodies")) });
  }
  return journeys;
}

function frameText(p) {
  const r = p.response;
  if (r.opcode === 1) return r.payloadData;
  const raw = Buffer.from(r.payloadData, "base64");
  try {
    return gunzipSync(raw).toString("utf8");
  } catch {
    return raw.toString("utf8");
  }
}

function frameBytes(p) {
  const r = p.response;
  return r.opcode === 1 ? Buffer.byteLength(r.payloadData) : Buffer.from(r.payloadData, "base64").length;
}

/**
 * A static body at `f`. A path that is also a folder's (`/api/agents` and `/api/agents/:id`) keeps
 * its body as the folder's index.json, whichever comes first.
 */
function put(f, body) {
  const parts = f.split("/");
  for (let i = 2; i < parts.length; i++) {
    const d = parts.slice(0, i).join("/");
    if (existsSync(d) && !statSync(d).isDirectory()) {
      const held = readFileSync(d);
      unlinkSync(d);
      mkdirSync(d, { recursive: true });
      writeFileSync(join(d, "index.json"), held);
    }
  }
  mkdirSync(dirname(f), { recursive: true });
  if (existsSync(f) && statSync(f).isDirectory()) writeFileSync(join(f, "index.json"), body);
  else writeFileSync(f, body);
}

function build(recDir, cataloguePath, payloadDir) {
  const journeys = read(recDir);
  const sockets = new Map();
  const requests = new Map();
  const leftOut = new Map();
  const replay = { streams: {}, writes: {} };
  mkdirSync(join(payloadDir, "docroot"), { recursive: true });
  for (const j of journeys) {
    const start = j.events.find((e) => e.ev === "journey")?.wall_us;
    const end = j.events.find((e) => e.ev === "end")?.wall_us;
    const secs = (end - start) / 1e6;
    const ws = new Map();
    const req = new Map();
    for (const e of j.events) {
      if (e.ev === "webSocketCreated") ws.set(e.requestId, { url: e.url, kind: socketKind(pattern(e.url)), pattern: pattern(e.url) });
      if (e.ev === "requestWillBeSent") req.set(e.requestId, { method: e.request.method, url: e.request.url, pattern: pattern(e.request.url), body: e.request.postData, at: e.wall_us });
      if (e.ev === "responseReceived") Object.assign(req.get(e.requestId) || {}, { status: e.response.status, mime: e.response.mimeType, protocol: e.response.protocol });
      if (e.ev === "loadingFinished") Object.assign(req.get(e.requestId) || {}, { wire: e.encodedDataLength, done: e.wall_us });
      if (e.ev === "webSocketFrameReceived" || e.ev === "webSocketFrameSent") {
        const s = ws.get(e.requestId);
        if (!s) continue;
        const key = s.kind || s.pattern;
        const entry = sockets.get(key) || { kind: s.kind, pattern: s.pattern, journeys: new Set(), down: [], up: [], downText: [], secs: 0, stamped: 0, binary: 0 };
        entry.journeys.add(j.name);
        if (e.ev === "webSocketFrameReceived") {
          const text = frameText(e);
          entry.down.push(frameBytes(e));
          if (e.response.opcode !== 1) entry.binary += 1;
          // The packed snapshot a session socket opens with: the reference servers serve it as a read.
          // (the `snapshot` journey's, a session no turn reached, as a run's reads open one).
          if (e.response.opcode !== 1 && s.kind === "session" && (j.name === "snapshot" || replay.snapshot === undefined)) replay.snapshot = text;
          if (/@lab:\d{16}/.test(text)) entry.stamped += 1;
          // The replay: live frames as the UI got them (a packed snapshot is a read, not a frame).
          if (e.response.opcode === 1 || s.kind === "log" || s.kind === "terminal") entry.downText.push(text);
        } else entry.up.push(frameBytes(e));
        sockets.set(key, entry);
      }
    }
    for (const s of sockets.values()) if (s.journeys.has(j.name)) s.secs += secs;
    for (const [id, r] of req) {
      const p = r.pattern;
      const u = new URL(r.url);
      const api = u.pathname.startsWith("/api/") && !socketKind(p);
      if (!api) {
        const key = `${r.method} ${p}`;
        const why = u.pathname.startsWith("/api/") ? "a socket's opening request (its frames are counted)" : "a page, a static file or the UI server's server functions: page loads are goal 8's";
        leftOut.set(key, { what: key, why, n: (leftOut.get(key)?.n || 0) + 1 });
        continue;
      }
      const key = `${r.method} ${p}`;
      const entry = requests.get(key) || { method: r.method, pattern: p, n: 0, bodies: [], wire: [], protocols: new Set(), journeys: new Set(), example: r.url, secs: 0 };
      entry.n += 1;
      entry.journeys.add(j.name);
      if (r.protocol) entry.protocols.add(r.protocol);
      if (r.wire) entry.wire.push(r.wire);
      const bf = join(j.bodies, id.replace(/[^\w.-]/g, "_"));
      if (existsSync(bf)) {
        const body = readFileSync(bf);
        entry.bodies.push(body.length);
        entry.last = body;
      }
      entry.lastRequest = r;
      requests.set(key, entry);
    }
    for (const r of requests.values()) if (r.journeys.has(j.name)) r.secs += secs;
  }
  const operations = [];
  for (const s of sockets.values()) {
    if (!s.kind) {
      leftOut.set(`socket ${s.pattern}`, { what: `socket ${s.pattern}`, why: "not one of the UI's live streams the bar names", n: s.down.length });
      continue;
    }
    const frames = s.downText;
    replay.streams[s.kind] = frames;
    const digest = createHash("sha256").update(frames.map(normal).join("\n")).digest("hex");
    operations.push({ case: "stream", op: s.kind, socket: s.pattern, journeys: [...s.journeys], frames_down: sizes(s.down), frames_up: sizes(s.up), stamped_frames: s.stamped, binary_frames: s.binary, rate_per_s: +(s.down.length / Math.max(1, s.secs)).toFixed(3), recorded_s: Math.round(s.secs), digest });
  }
  for (const r of requests.values()) {
    const read = r.method === "GET";
    if (read && r.last) put(join(payloadDir, "docroot", r.pattern.split("?")[0].replace(/:id/g, "lab").replace(/:n/g, "1")), r.last);
    if (!read) replay.writes[`${r.method} ${r.pattern.split("?")[0].replace(/:id/g, "lab").replace(/:n/g, "1")}`] = { status: 200, body: r.last ? r.last.toString("utf8") : "" };
    operations.push({
      case: "request",
      op: `${r.method} ${r.pattern}`,
      kind: read ? "read" : "write",
      journeys: [...r.journeys],
      bodies: sizes(r.bodies),
      wire: sizes(r.wire),
      protocols: [...r.protocols],
      rate_per_s: +(r.n / Math.max(1, r.secs)).toFixed(3),
      request_body: r.lastRequest?.body ? Buffer.byteLength(r.lastRequest.body) : 0,
      digest: r.last ? createHash("sha256").update(normal(r.last.toString("utf8"))).digest("hex") : null,
    });
  }
  if (replay.snapshot !== undefined) put(join(payloadDir, "docroot/api/harness/sessions/lab/snapshot"), replay.snapshot);
  operations.sort((a, b) => (a.case + a.op).localeCompare(b.case + b.op));
  const catalogue = { built_from: recDir, journeys: journeys.map((j) => j.name), operations, left_out: [...leftOut.values()].sort((a, b) => a.what.localeCompare(b.what)) };
  writeFileSync(cataloguePath, JSON.stringify(catalogue, null, 1) + "\n");
  writeFileSync(join(payloadDir, "replay.json"), JSON.stringify(replay));
  return catalogue;
}

function compare(a, b) {
  const digests = (dir) => {
    const out = {};
    for (const j of read(dir))
      for (const e of j.events)
        if (e.ev === "webSocketFrameReceived" && e.response.opcode === 1 && /@lab:\d{16}/.test(e.response.payloadData)) {
          const t = JSON.parse(e.response.payloadData);
          // An event's body: what the stand-in wrote, as the server sent it on, keyed by the turn
          // and place in it its ids carry (`msg_<session's first 12>_<turn>_<n>`).
          for (const ev of t.events || (t.event ? [t.event] : [])) {
            const text = JSON.stringify(ev);
            const m = text.match(/(?:msg|toolu)_[0-9a-f]{12}_(\d+)_(\d+)/);
            if (!m) continue;
            const k = `${j.name} ${ev.kind || ""} ${ev.status || ""} ${m[1]}_${m[2]}`;
            out[k] = createHash("sha256").update(normal(text)).digest("hex");
          }
        }
    return out;
  };
  const da = digests(a);
  const db = digests(b);
  const both = Object.keys(da).filter((k) => k in db);
  const differ = both.filter((k) => da[k] !== db[k]);
  return { a: Object.keys(da).length, b: Object.keys(db).length, compared: both.length, equal: both.length - differ.length, differ: differ.slice(0, 20) };
}

const [mode, ...args] = process.argv.slice(2);
if (mode === "build") console.log(JSON.stringify(build(...args).operations.map((o) => `${o.case} ${o.op}`)));
else if (mode === "compare") console.log(JSON.stringify(compare(...args), null, 1));
else {
  console.error("usage: node catalogue.mjs build RECORDINGS CATALOGUE.json PAYLOADS | compare A B");
  process.exitCode = 2;
}
