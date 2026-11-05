// Step 5's check of the reference servers' origin (docs/browser-lab.md, "The baselines"; plan
// 01a1a832e9cc step 5): every stream frame the origin replays hashes equal to step 3's recording,
// stamps aside. It starts origin.mjs on sockets of its own, opens each recorded stream on both
// carriers (Server-Sent Events and the streaming fetch), has the origin replay every frame, and
// reads them back as a browser would. Two holds:
//   - the replay the origin reads (payloads/replay.json) is the recording: each stream's frames
//     hash to the catalogue's digest for it;
//   - each frame the origin sent, on each carrier, is the replay's frame (both in normal form:
//     stamps and ids out), in order, none missing; a terminal's or a log's line end is the
//     carrier's framing, so it is left out of both.
//   node check-origin.mjs PAYLOADS_DIR CATALOGUE.json    (exit 0 when every frame holds)
import { spawn } from "node:child_process";
import { request } from "node:http";
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

/** How a frame should read once it has crossed a carrier. */
export function expected(kind, frame) {
  return normal(TEXT.has(kind) ? frame.replace(/\r?\n$/, "") : frame);
}

/** Frames received against the replay's: the first that differ, or that are missing. */
export function holds(kind, sent, got) {
  const bad = [];
  for (let i = 0; i < Math.max(sent.length, got.length); i++) {
    const want = i < sent.length ? expected(kind, sent[i]) : null;
    const have = i < got.length ? normal(got[i]) : null;
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
      // Every frame is written at once; what's still in the socket comes within a moment, and a
      // frame the carrier splits in two shows as more frames than were sent.
      for (let i = 0; i < 200 && Object.values(read()).some((g) => g.length < frames.length); i++) await wait(50);
      await wait(100);
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

const main = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (main) {
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
