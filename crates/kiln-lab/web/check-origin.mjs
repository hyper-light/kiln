// Step 5's check of the reference servers' origin (docs/browser-lab.md, "The baselines"; plan
// 01a1a832e9cc step 5): every stream frame the origin replays hashes equal to step 3's recording,
// stamps aside. It starts origin.mjs on sockets of its own, opens each recorded stream on both
// carriers (Server-Sent Events and the streaming fetch), has the origin replay every frame, and
// reads them back as a browser would. Two holds:
//   - the replay the origin reads (payloads/replay.json) is the recording: each stream's frames
//     hash to the catalogue's digest for it;
//   - each frame the origin sent, on each carrier, is the replay's frame (both in normal form:
//     stamps and ids out), in order, none missing; a terminal's or a log's text comes as a JSON
//     string and is read back with JSON.parse, as the probe reads it.
//   node check-origin.mjs PAYLOADS_DIR CATALOGUE.json    (exit 0 when every frame holds)
// and, through a reference server (kiln-lab's start-stop runs it in a client's namespace, with the
// server's origin running): the same frames read over the server's HTTP/2, gzip asked for, so what
// the server sends on (compressed, as it compresses its streams) is checked too:
//   node check-origin.mjs served ORIGIN_URL CONTROL_SOCK PAYLOADS_DIR
import { spawn } from "node:child_process";
import { request } from "node:http";
import { connect as h2connect } from "node:http2";
import { createGunzip } from "node:zlib";
import { readFileSync, mkdtempSync, rmSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { tmpdir } from "node:os";
import { createHash } from "node:crypto";
import { fileURLToPath, pathToFileURL } from "node:url";
import { normal } from "./catalogue.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const TEXT = new Set(["terminal", "log"]);
const sha = (frames) => createHash("sha256").update(frames.map(normal).join("\n")).digest("hex");

/** Server-Sent Events as a browser's EventSource reads them: each event's data lines joined by \n. */
export function sseEvents(text) {
  const out = [];
  let data = null;
  for (const line of text.split(/\r\n|\r|\n/)) {
    if (line === "") {
      if (data !== null) out.push(data.join("\n"));
      data = null;
    } else if (line.startsWith("data")) {
      const v = line.slice(4).startsWith(":") ? line.slice(5).replace(/^ /, "") : "";
      (data ||= []).push(v);
    }
  }
  return out;
}

/** The streaming fetch's frames as the probe reads them: one a line. */
export function lines(text) {
  return text.split("\n").slice(0, -1);
}

/** A frame as the page reads it off a carrier: a terminal's or a log's text from its JSON string. */
export function received(kind, data) {
  if (!TEXT.has(kind)) return data;
  try {
    return String(JSON.parse(data));
  } catch {
    return `(not a JSON string) ${data}`;
  }
}

/** Frames received against the replay's: the first that differ, or that are missing. */
export function holds(kind, sent, got) {
  const bad = [];
  for (let i = 0; i < Math.max(sent.length, got.length); i++) {
    const want = i < sent.length ? normal(sent[i]) : null;
    const have = i < got.length ? normal(received(kind, got[i])) : null;
    if (want !== have) bad.push({ i, want: want && want.slice(0, 120), got: have && have.slice(0, 120) });
  }
  return bad;
}

const wait = (ms) => new Promise((ok) => setTimeout(ok, ms));

function open(sock, path, onText) {
  return new Promise((ok, bad) => {
    const req = request({ socketPath: sock, path, method: "GET" }, (res) => {
      res.setEncoding("utf8");
      res.on("data", onText);
      ok(req);
    });
    req.on("error", bad);
    req.end();
  });
}

function post(sock, path, body) {
  return new Promise((ok, bad) => {
    const req = request({ socketPath: sock, path, method: "POST", headers: { "content-type": "application/json" } }, (res) => {
      let t = "";
      res.setEncoding("utf8");
      res.on("data", (d) => (t += d));
      res.on("end", () => ok({ status: res.statusCode, body: t }));
    });
    req.on("error", bad);
    req.end(JSON.stringify(body));
  });
}

export async function check(payloads, cataloguePath) {
  const replay = JSON.parse(readFileSync(join(payloads, "replay.json"), "utf8"));
  const catalogue = JSON.parse(readFileSync(cataloguePath, "utf8"));
  const dir = mkdtempSync(join(tmpdir(), "kiln-lab-origin-"));
  const pub = join(dir, "pub.sock");
  const ctl = join(dir, "ctl.sock");
  const origin = spawn(process.execPath, [join(HERE, "origin.mjs"), payloads, pub, ctl], { stdio: ["ignore", "inherit", "inherit"] });
  const report = [];
  let pass = true;
  try {
    for (let i = 0; i < 100 && !(existsSync(pub) && existsSync(ctl)); i++) await wait(50);
    for (const [kind, frames] of Object.entries(replay.streams || {})) {
      const op = (catalogue.operations || []).find((o) => o.case === "stream" && o.op === kind);
      const recorded = op ? op.digest === sha(frames) : null;
      if (recorded === false) pass = false;
      const texts = { sse: "", "fetch-stream": "" };
      const reqs = [await open(pub, `/stream/${kind}`, (d) => (texts.sse += d)), await open(pub, `/fetch-stream/${kind}`, (d) => (texts["fetch-stream"] += d))];
      const r = await post(ctl, "/replay", { kind, events: frames.length, every_ms: 0 });
      if (r.status !== 200) throw new Error(`replay of ${kind}: ${r.status} ${r.body}`);
      const read = () => ({ sse: sseEvents(texts.sse), "fetch-stream": lines(texts["fetch-stream"]) });
      // The origin sends a frame a timer tick (about 1 ms): read until every frame is there, or until
      // a second passes with none new (a frame the carrier split in two shows as more frames).
      let last = -1;
      for (;;) {
        await wait(1000);
        const counts = Object.values(read()).map((g) => g.length);
        const now = counts.reduce((a, b) => a + b, 0);
        if (counts.every((n) => n >= frames.length) || now === last) break;
        last = now;
      }
      reqs.forEach((q) => q.destroy());
      const line = { kind, frames: frames.length, recording: op ? (recorded ? "the catalogue's digest" : "**not the catalogue's digest**") : "not in the catalogue" };
      for (const [carrier, got] of Object.entries(read())) {
        const bad = holds(kind, frames, got);
        if (bad.length) pass = false;
        line[carrier] = bad.length ? { received: got.length, differ: bad.length, first: bad[0] } : `${got.length} of ${frames.length} equal`;
      }
      report.push(line);
    }
  } finally {
    origin.kill();
    rmSync(dir, { recursive: true, force: true });
  }
  return { pass, report };
}

/** Reads every frame of each stream through a server (HTTP/2, gzip), the origin replaying them. */
export async function served(originUrl, ctl, payloads) {
  const replay = JSON.parse(readFileSync(join(payloads, "replay.json"), "utf8"));
  const session = h2connect(originUrl, { rejectUnauthorized: false });
  session.on("error", () => {});
  const report = [];
  let pass = true;
  try {
    for (const [kind, frames] of Object.entries(replay.streams || {})) {
      const texts = { sse: "", "fetch-stream": "" };
      const encodings = {};
      const delays = { sse: [], "fetch-stream": [] };
      const streams = [];
      // A server may hold a compressed stream's headers until it has bytes enough to compress
      // (Caddy's encoder does), so nothing waits for them: each is read when it comes.
      for (const [carrier, path] of [["sse", `/stream/${kind}`], ["fetch-stream", `/fetch-stream/${kind}`]]) {
        const st = session.request({ ":path": path, "accept-encoding": "gzip" });
        streams.push(st);
        st.on("error", () => {});
        st.on("response", (h) => {
          encodings[carrier] = h["content-encoding"] || "identity";
          const body = h["content-encoding"] === "gzip" ? st.pipe(createGunzip({ flush: 2 })) : st;
          body.setEncoding("utf8");
          body.on("data", (d) => {
            texts[carrier] += d;
            // How long each frame took from the origin's stamp to here: a server that held frames
            // back to compress them more would show here.
            const now = Math.round((performance.timeOrigin + performance.now()) * 1000);
            for (const m of d.matchAll(/@lab:(\d{16})/g)) delays[carrier].push(now - Number(m[1]));
          });
          body.on("error", () => {});
        });
      }
      // Both streams open at the origin before it replays (each server opens its upstream when
      // its client's request comes): a replay of no events answers how many are open.
      for (let i = 0; i < 100; i++) {
        const o = await post(ctl, "/replay", { kind, events: 0, every_ms: 0 });
        if ((JSON.parse(o.body || "{}").open || 0) >= 2) break;
        await wait(100);
      }
      const r = await post(ctl, "/replay", { kind, events: frames.length, every_ms: 0 });
      if (r.status !== 200) throw new Error(`replay of ${kind}: ${r.status} ${r.body}`);
      const read = () => ({ sse: sseEvents(texts.sse), "fetch-stream": lines(texts["fetch-stream"]) });
      let last = -1;
      for (;;) {
        await wait(1000);
        const counts = Object.values(read()).map((g) => g.length);
        const now = counts.reduce((a, b) => a + b, 0);
        if (counts.every((n) => n >= frames.length) || now === last) break;
        last = now;
      }
      streams.forEach((st) => st.close());
      const line = { kind, frames: frames.length };
      for (const [carrier, got] of Object.entries(read())) {
        const bad = holds(kind, frames, got);
        if (bad.length) pass = false;
        const d = delays[carrier].sort((a, b) => a - b);
        const q = (p) => (d.length ? Math.round(d[Math.min(d.length - 1, Math.floor(p * d.length))] / 100) / 10 : null);
        const held = `stamp to here p50 ${q(0.5)} ms, p99 ${q(0.99)} ms`;
        line[carrier] = bad.length ? { encoding: encodings[carrier], received: got.length, differ: bad.length, first: bad[0] } : `${got.length} of ${frames.length} equal (${encodings[carrier]}; ${held})`;
      }
      report.push(line);
    }
  } finally {
    session.close();
  }
  return { pass, report };
}

const main = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (main && process.argv[2] === "served") {
  const [originUrl, ctl, payloads] = process.argv.slice(3);
  const { pass, report } = await served(originUrl, ctl, payloads);
  for (const l of report) console.log(JSON.stringify(l));
  console.log(pass ? "every frame the server sent on holds" : "some frames the server sent on differ");
  process.exitCode = pass ? 0 : 1;
} else if (main) {
  const [payloads, cataloguePath] = process.argv.slice(2);
  if (!payloads || !cataloguePath) {
    console.error("usage: node check-origin.mjs PAYLOADS_DIR CATALOGUE.json");
    process.exitCode = 2;
  } else {
    const { pass, report } = await check(payloads, cataloguePath);
    for (const l of report) console.log(JSON.stringify(l));
    console.log(pass ? "every replayed frame holds" : "some replayed frames differ");
    process.exitCode = pass ? 0 : 1;
  }
}
