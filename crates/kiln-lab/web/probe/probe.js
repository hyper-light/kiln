// The browser lab's probe page (docs/browser-lab.md, "The probe"; plan 01a1a832e9cc step 4). The
// driver opens it on the target's own origin, so everything it opens is same-origin, as the UI's
// requests and sockets are. It opens what the UI opens for each operation and reads each frame as
// the UI does (JSON.parse of each text frame, gunzip of a packed snapshot), and notes, for each
// operation, when its server stamped it, when it was scheduled, when the page had it parsed, and
// its size above the transport. Times are the server's clock: the page's own (timeOrigin + now())
// plus the offset its calibration found.
"use strict";

const STAMP = /@lab:(\d{16})/g;
const lab = {
  offsetUs: 0,
  samples: [],
  protocols: {},
  errors: [],
  open: [],
};
window.__lab = lab;

/** The page's clock, in µs, on the server's clock. */
function nowUs() {
  return (performance.timeOrigin + performance.now()) * 1000 + lab.offsetUs;
}

function note(s) {
  lab.samples.push(s);
}

function err(where, e) {
  lab.errors.push({ where, at_us: Math.round(nowUs()), error: String(e && e.message ? e.message : e) });
}

/** Each stamp in `text` newer than `since`, once each in `seen`: one operation each. */
function stamps(text, since, seen) {
  const out = [];
  for (const m of text.matchAll(STAMP)) {
    const s = Number(m[1]);
    if (s >= since && !seen.has(s)) {
      seen.add(s);
      out.push(s);
    }
  }
  return out;
}

const gunzip = (data) => new Response(new Blob([data]).stream().pipeThrough(new DecompressionStream("gzip"))).text();
const utf8 = new TextDecoder();

/**
 * Calibration over the unshaped control path (a WebSocket echo of the lab's): `n` echoes, the
 * offset from the one with the least round trip, then `check` more read against it. Returns the
 * round trips and residuals in µs.
 */
// Echoes a calibration takes its least round trip from (its first, and each group of the check's).
const GROUP = 40;

lab.calibrate = async function calibrate(url, n = 200, check = 400) {
  const ws = new WebSocket(url);
  await new Promise((ok, bad) => {
    ws.onopen = ok;
    ws.onerror = () => bad(new Error(`the control echo at ${url} didn't open`));
  });
  const once = () =>
    new Promise((ok) => {
      const t0 = (performance.timeOrigin + performance.now()) * 1000;
      ws.onmessage = (ev) => {
        const t1 = (performance.timeOrigin + performance.now()) * 1000;
        ok({ rtt: t1 - t0, offset: Number(ev.data) - (t0 + t1) / 2 });
      };
      ws.send(String(t0));
    });
  const first = [];
  for (let i = 0; i < n; i++) first.push(await once());
  first.sort((a, b) => a.rtt - b.rtt);
  lab.offsetUs = first[0].offset;
  // The calibration again, in groups of GROUP echoes each taken the same way: how far each group's
  // offset lies from the one the page uses is the calibration's residual. Each echo's own offset
  // (off by up to half its round trip) is kept beside it. The groups are interleaved (echo i goes
  // to group i mod groups), so each group's echoes span the whole check as the page's own span its
  // calibration: on a loaded machine a run of echoes all waiting for a core (tens of ms) delays
  // every echo of a group taken back to back, and that group's least round trip with them.
  const groups = Math.max(1, Math.floor(check / GROUP));
  const taken = Array.from({ length: groups }, () => []);
  const echoes = [];
  const rtts = [];
  for (let i = 0; i < groups * GROUP; i++) {
    const r = await once();
    taken[i % groups].push(r);
    rtts.push(r.rtt);
    echoes.push(Math.abs(r.offset - lab.offsetUs));
  }
  const residuals = taken.map((group) => {
    group.sort((a, b) => a.rtt - b.rtt);
    return Math.abs(group[0].offset - lab.offsetUs);
  });
  ws.close();
  return { offset_us: lab.offsetUs, min_rtt_us: first[0].rtt, rtts_us: rtts, residuals_us: residuals, echo_residuals_us: echoes };
};

/** The protocol each resource came over (h2, h3, http/1.1), by path. */
function protocols() {
  for (const e of performance.getEntriesByType("resource")) {
    const path = new URL(e.name).pathname.replace(/\/[0-9a-f]{32}(?=\/|$)/g, "/:id");
    if (e.nextHopProtocol) lab.protocols[path] = e.nextHopProtocol;
  }
  performance.clearResourceTimings();
}

/** A stream: frames from a socket, an event source or a streaming fetch, each stamp one sample. */
lab.stream = function stream(op) {
  const since = Math.round(nowUs());
  const seen = new Set();
  const take = (text, bytes) => {
    const at = nowUs();
    const found = stamps(text, since, seen);
    found.forEach((s, i) => note({ op: op.name, case: op.case, stamp_us: s, at_us: Math.round(at), lat_us: Math.round(at - s), app_bytes: i === 0 ? bytes : 0 }));
    if (found.length === 0) note({ op: op.name, case: op.case, frame: true, at_us: Math.round(at), app_bytes: bytes });
  };
  const url = new URL(op.path, location.href);
  if (op.carrier === "ws") {
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    let inbox = Promise.resolve();
    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        inbox = inbox.then(() => {
          try {
            JSON.parse(ev.data);
          } catch (e) {
            if (op.json !== false) err(op.name, e);
          }
          take(ev.data, ev.data.length);
        });
        return;
      }
      const data = ev.data;
      // A packed snapshot gunzipped and read line by line; raw output (logs, a terminal) decoded.
      inbox = inbox.then(async () => {
        const text = op.packed ? await gunzip(data).catch(() => utf8.decode(data)) : utf8.decode(data);
        if (op.packed) for (const line of text.split("\n")) if (line) JSON.parse(line);
        take(text, data.byteLength);
      });
    };
    ws.onerror = () => err(op.name, "socket error");
    ws.onclose = (ev) => {
      if (!lab.closing) err(op.name, `socket closed ${ev.code}`);
    };
    if (op.send) ws.onopen = () => ws.send(op.send);
    lab.open.push(() => ws.close());
  } else if (op.carrier === "sse") {
    const es = new EventSource(url);
    es.onmessage = (ev) => {
      if (op.json !== false) JSON.parse(ev.data);
      take(ev.data, ev.data.length);
    };
    es.onerror = () => {
      if (!lab.closing) err(op.name, "event source error");
    };
    lab.open.push(() => es.close());
  } else if (op.carrier === "fetch-stream") {
    const ctl = new AbortController();
    lab.open.push(() => ctl.abort());
    (async () => {
      const r = await fetch(url, { signal: ctl.signal, cache: "no-store" });
      const reader = r.body.getReader();
      let rest = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        rest += utf8.decode(value, { stream: true });
        let nl;
        while ((nl = rest.indexOf("\n")) >= 0) {
          const line = rest.slice(0, nl);
          rest = rest.slice(nl + 1);
          if (!line) continue;
          if (op.json !== false) JSON.parse(line);
          take(line, line.length + 1);
        }
      }
    })().catch((e) => {
      if (!lab.closing) err(op.name, e);
    });
  } else {
    err(op.name, `no carrier ${op.carrier}`);
  }
};

/**
 * Requests on an open-loop schedule: each sent at its time whether or not those before it were
 * answered, timed from when it should have gone to its parsed body (no coordinated omission).
 */
lab.requests = function requests(op) {
  const start = performance.now() + (op.start_in_ms || 200);
  const timers = [];
  op.schedule_ms.forEach((at, i) => {
    const due = start + at;
    timers.push(
      setTimeout(async () => {
        const sched = (performance.timeOrigin + due) * 1000 + lab.offsetUs;
        const req = op.requests[i % op.requests.length];
        // A write's id is new each time and every link's its own, as every UI's are (`{id}` in its
        // body): ten characters, as long as the recorded UI's (`lab-rec-12`), so the answer, which
        // carries it twice, is the same size; a repeated id would be answered as a repeat.
        const id = `l${op.client ?? 0}${(i + 1).toString(36).padStart(8, "0")}`;
        const body = req.body ? req.body.replaceAll("{id}", id) : undefined;
        try {
          if (req.ws_snapshot) {
            // A session socket opened for its packed snapshot, then closed: the UI's read of a session.
            const u = new URL(req.path, location.href);
            u.protocol = u.protocol === "https:" ? "wss:" : "ws:";
            const ws = new WebSocket(u);
            ws.binaryType = "arraybuffer";
            const data = await new Promise((ok, bad) => {
              ws.onmessage = (ev) => typeof ev.data !== "string" && ok(ev.data);
              ws.onerror = () => bad(new Error("snapshot socket error"));
            });
            const text = await gunzip(data);
            for (const line of text.split("\n")) if (line) JSON.parse(line);
            ws.close();
            const at = nowUs();
            note({ op: req.name, case: op.case, sched_us: Math.round(sched), at_us: Math.round(at), lat_us: Math.round(at - sched), app_bytes: data.byteLength, ok: true });
            return;
          }
          const r = await fetch(req.path, { method: req.method || "GET", body, headers: body ? { "content-type": "application/json" } : undefined, cache: "no-store" });
          const buf = await r.arrayBuffer();
          const text = utf8.decode(buf);
          if (req.ndjson) for (const line of text.split("\n")) line && JSON.parse(line);
          else if (text) JSON.parse(text);
          const at = nowUs();
          // A write's answer is kept whole with its sample, to hold against the UI's recorded ones.
          note({ op: req.name, case: op.case, sched_us: Math.round(sched), at_us: Math.round(at), lat_us: Math.round(at - sched), app_bytes: buf.byteLength, status: r.status, ok: r.ok, ...(body ? { answer: text } : {}) });
        } catch (e) {
          const at = nowUs();
          note({ op: req.name, case: op.case, sched_us: Math.round(sched), at_us: Math.round(at), lat_us: Math.round(at - sched), app_bytes: 0, ok: false });
          err(req.name, e);
        }
      }, Math.max(0, due - performance.now())),
    );
  });
  lab.open.push(() => timers.forEach(clearTimeout));
};

/** What came since the last drain. */
lab.drain = function drain() {
  protocols();
  const out = { samples: lab.samples, errors: lab.errors, protocols: lab.protocols };
  lab.samples = [];
  lab.errors = [];
  return out;
};

lab.close = function close() {
  lab.closing = true;
  for (const c of lab.open.splice(0)) c();
};
