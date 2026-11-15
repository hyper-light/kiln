// Mockup helpers: none of this is production code. It fakes the server's answers so each press shows
// what kiln's would (W21-R15: the control pressed says its request is on its way).
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
function press(el, ms, done) {
  if (!el || el.hasAttribute("data-sending")) return;
  el.setAttribute("data-sending", ""); el.setAttribute("aria-busy", "true");
  setTimeout(() => { el.removeAttribute("data-sending"); el.removeAttribute("aria-busy"); done(); }, ms);
}

/* ------------------------------------------------------------ data every page shares */
const HARNESSES = [
  { id: "claude", name: "Claude Code", version: "2.1.311", models: [["claude-opus-5-5", "Opus 5.5"], ["claude-sonnet-5-5", "Sonnet 5.5"]], egress: ["api.anthropic.com:443", "claude.ai:443", "platform.claude.com:443"] },
  { id: "codex", name: "Codex", version: "0.68.0", models: [["gpt-6.1-sol", "GPT-6.1 Sol"], ["gpt-6.1-sol-mini", "GPT-6.1 Sol mini"]], egress: ["api.openai.com:443", "auth.openai.com:443", "chatgpt.com:443"] },
  { id: "devin", name: "Devin", version: "2026.11.11-b912975", models: [["swe-2", "SWE 2"], ["claude-opus-5-5", "Opus 5.5"]], egress: ["api.devin.ai:443"] },
  { id: "gemini", name: "Gemini CLI", version: "0.12.0", models: [["gemini-3.8-pro", "Gemini 3.8 Pro"]], egress: ["generativelanguage.googleapis.com:443"] },
  { id: "opencode", name: "opencode", version: "1.5.7", models: [], egress: [] },
  { id: "goose", name: "Goose", version: "1.59.0", models: [], egress: [] },
  { id: "amp", name: "Amp", version: "0.167.2", models: [], egress: ["ampcode.com:443"] },
  { id: "antigravity", name: "Antigravity CLI", version: "1.5.2", models: [["gemini-3.8-pro", "Gemini 3.8 Pro"]], egress: ["cloudcode-pa.googleapis.com:443"] },
  { id: "cursor", name: "Cursor CLI", version: "2.2.7", models: [], egress: ["api2.cursor.sh:443"] },
  { id: "pi", name: "Pi", version: "3000.12.43", models: [], egress: [] },
];
const harnessOf = (id) => HARNESSES.find((h) => h.id === id);
const modelName = (h, m) => harnessOf(h)?.models.find(([id]) => id === m)?.[1] ?? (m || "default");
/** The person's secrets in the user scope: names, never values. */
const SECRETS = ["GH_READ_TOKEN", "GH_DEPLOY_KEY", "MANTLE_ACCESS_KEY_ID", "MANTLE_SECRET_ACCESS_KEY", "R2_ACCESS_KEY_ID", "R2_SECRET_ACCESS_KEY", "RELEASES_BEARER", "ANTHROPIC_API_KEY"];

/* ------------------------------------------------------------ icons (lucide's shapes, kiln's set) */
const ICONS = {
  microvm: '<path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/><path d="m3.3 7 8.7 5 8.7-5M12 22V12"/>',
  host: '<rect width="20" height="8" x="2" y="2" rx="2"/><rect width="20" height="8" x="2" y="14" rx="2"/><path d="M6 6h.01M6 18h.01"/>',
  cold: '<path d="M2 12h20M12 2v20m8-6-4-4 4-4M4 8l4 4-4 4m12-12-4 4-4-4M8 20l4-4 4 4"/>',
  template: '<path d="m12 2 10 5-10 5L2 7z"/><path d="m2 17 10 5 10-5M2 12l10 5 10-5"/>',
  warm: '<path d="M8.5 14.5A2.5 2.5 0 0 0 11 12c0-1.38-.5-2-1-3-1.07-2.14-.22-4.05 2-6 .5 2.5 2 4.9 4 6.5 2 1.6 3 3.5 3 5.5a7 7 0 1 1-14 0c0-1.15.43-2.29 1-3a2.5 2.5 0 0 0 2.5 2.5z"/>',
  process: '<path d="m4 17 6-6-6-6M12 19h8"/>',
  folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  git: '<path d="M6 3v12"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/>',
  bucket: '<path d="m4 6 1.6 13.1A2 2 0 0 0 7.6 21h8.8a2 2 0 0 0 2-1.9L20 6"/><ellipse cx="12" cy="6" rx="8" ry="3"/>',
  archive: '<rect width="20" height="5" x="2" y="3" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8M10 12h4"/>',
  play: '<path d="M6 3l14 9-14 9z"/>',
  pause: '<rect x="14" y="4" width="4" height="16" rx="1"/><rect x="6" y="4" width="4" height="16" rx="1"/>',
  stop: '<rect width="14" height="14" x="5" y="5" rx="2"/>',
  restart: '<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/>',
  more: '<circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/><circle cx="5" cy="12" r="1"/>',
  chart: '<path d="M3 3v16a2 2 0 0 0 2 2h16"/><path d="m19 9-5 5-4-4-3 3"/>',
  chev: '<path d="m6 9 6 6 6-6"/>',
  plus: '<path d="M5 12h14M12 5v14"/>',
  x: '<path d="M18 6 6 18M6 6l12 12"/>',
  build: '<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/>',
  lock: '<rect width="18" height="11" x="3" y="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
  key: '<circle cx="7.5" cy="15.5" r="5.5"/><path d="m21 2-9.6 9.6M15.5 7.5l3 3L22 7l-3-3"/>',
  home: '<path d="M15 21v-8a1 1 0 0 0-1-1h-4a1 1 0 0 0-1 1v8"/><path d="M3 10a2 2 0 0 1 .7-1.5l7-6a2 2 0 0 1 2.6 0l7 6A2 2 0 0 1 21 10v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  swap: '<path d="m16 3 4 4-4 4M20 7H4M8 21l-4-4 4-4M4 17h16"/>',
  terminal: '<path d="m4 17 6-6-6-6M12 19h8"/>',
  out: '<path d="M7 7h10v10M7 17 17 7"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  crown: '<path d="M11.56 3.27a.5.5 0 0 1 .88 0l2.95 5.6a1 1 0 0 0 1.52.29l4.27-3.66a.5.5 0 0 1 .82.5l-2.83 10.25a1 1 0 0 1-.96.73H5.8a1 1 0 0 1-.96-.73L2 6a.5.5 0 0 1 .82-.5l4.27 3.66a1 1 0 0 0 1.52-.3z"/><path d="M5 21h14"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>',
  send: '<path d="m22 2-7 20-4-9-9-4z"/><path d="M22 2 11 13"/>',
  open: '<path d="M15 3h6v6M10 14 21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>',
  copy: '<rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>',
  split: '<path d="M16 3h5v5M8 3H3v5M12 22v-8.3a4 4 0 0 0-1.17-2.83L3 3M21 3l-7.83 7.83A4 4 0 0 0 12 13.7"/>',
  replay: '<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/><path d="m10 9 5 3-5 3z"/>',
  history: '<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5M12 7v5l4 2"/>',
  trash: '<path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>',
};
/** An icon; with `tip`, it says what it means on hover and to a screen reader. */
const icon = (n, tip, size = 14) => `<svg class="ic" viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" ${tip ? `role="img" aria-label="${esc(tip)}"><title>${esc(tip)}</title>` : 'aria-hidden="true">'}${ICONS[n]}</svg>`;
/** A start kind's icon in its own colour, the same colour as its share in every stacked bar: the legend everywhere. */
const startIcon = (k, tip) => `<span style="color:${START_COLORS[k]};display:inline-flex">${icon(k === "process" ? "process" : k, tip || START_TIP[k]).replace('class="ic"', 'class="ic" style="color:inherit"')}</span>`;
const ENGINE_TIP = { microvm: "microVM", host: "Host" };
const START_TIP = { cold: "Cold start", template: "From template", warm: "Warm start", process: "Host process" };
const SOURCE_TIP = { path: "Path", git: "Git", s3: "S3 bucket", https: "HTTPS tarball" };
const SOURCE_ICON = { path: "folder", git: "git", s3: "bucket", https: "archive" };

/* ------------------------------------------------------------ status: one colour and one shape each (dataviz status palette) */
const STATES = {
  ok: ["#0ca30c", "Healthy", '<circle cx="6" cy="6" r="5"/>'],
  run: ["#0ca30c", "Running", '<circle cx="6" cy="6" r="5"/>'],
  idle: ["#0ca30c", "Idle", '<circle cx="6" cy="6" r="3.6" fill="none" stroke-width="2.4"/>'],
  start: ["#3987e5", "Starting", '<circle cx="6" cy="6" r="3.6" fill="none" stroke-width="2.4" stroke-dasharray="3 2"/>'],
  warn: ["#fab219", "Degraded", '<path d="M6 .8 11.4 10.6H.6z"/>'],
  drain: ["#fab219", "Draining", '<path d="M.8 2h10.4L6 11z"/>'],
  paused: ["#a6b2c6", "Paused", '<rect x="1.5" y="1" width="3" height="10" rx="1"/><rect x="7.5" y="1" width="3" height="10" rx="1"/>'],
  stopped: ["#898781", "Stopped", '<rect x="1.5" y="1.5" width="9" height="9" rx="1.5"/>'],
  failed: ["#d03b3b", "Failed", '<path d="M2 2l8 8M10 2l-8 8" stroke-width="2.6" fill="none"/>'],
  off: ["#898781", "Unavailable", '<circle cx="6" cy="6" r="4.6" fill="none" stroke-width="1.8"/><path d="M2.8 9.2 9.2 2.8" stroke-width="1.8"/>'],
};
const st = (s, tip) => { const [c, l, shape] = STATES[s]; return `<svg class="st" viewBox="0 0 12 12" width="10" height="10" fill="${c}" stroke="${c}" role="img" aria-label="${esc(tip || l)}"><title>${esc(tip || l)}</title>${shape}</svg>`; };

/* ------------------------------------------------------------ metrics as marks, the number beside */
const tone = (p) => (p >= 90 ? "#d03b3b" : p >= 70 ? "#fab219" : "#0ca30c");
/** A usage bar coloured by threshold (under 70 healthy, under 90 warning, else critical). */
const ubar = (pct, text, w = 44, color) => `<span class="u" title="${esc(text)}"><span class="u-t" style="width:${w}px"><i style="width:${Math.min(100, Math.max(3, pct))}%;background:${color || tone(pct)}"></i></span><span class="ell">${esc(text)}</span></span>`;
/** A sparkline: the last hour, its newest value beside it. */
/** One point per pixel of the mark's width: the server sends a series already reduced to `w` buckets. */
function perPixel(vals, w) { const n = vals.length; if (!w) return []; return Array.from({ length: w }, (_, i) => { const a = Math.floor((i * n) / w), b = Math.max(a + 1, Math.floor(((i + 1) * n) / w)); let t = 0; for (let j = a; j < b; j++) t += vals[Math.min(j, n - 1)]; return t / (b - a); }); }
const sparkPath = (vals, w, h) => { const pts = perPixel(vals, w), max = Math.max(...pts, 1); return pts.map((v, i) => `${i ? "L" : "M"}${i},${(h - 1 - (v / max) * (h - 3)).toFixed(1)}`).join(""); };
/** A sparkline: one SVG path, one point per pixel; `live` names it for the page's stream. */
function spark(vals, text, color = "#3987e5", w = 52, h = 16, live) {
  return `<span class="u" title="${esc(text)}, last hour" ${live ? `data-live="${esc(live)}"` : ""}>${w ? `<svg class="spark" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" aria-hidden="true"><path d="${sparkPath(vals, w, h)}" fill="none" stroke="${color}" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round"/></svg>` : ""}<span class="ell">${esc(text)}</span></span>`;
}
/** A share as one stacked bar: parts [{v, color, tip}], 2px of surface between them. */
function stack(parts, w = 64, text) {
  const tot = parts.reduce((a, p) => a + p.v, 0) || 1;
  return `<span class="u" title="${esc(parts.map((p) => `${p.tip} ${p.v}`).join(" · "))}"><span class="stk" style="width:${w}px">${parts.filter((p) => p.v > 0).map((p) => `<i style="flex:${p.v} 0 0;background:${p.color}"></i>`).join("")}</span>${text !== undefined ? `<span class="ell">${esc(text)}</span>` : ""}</span>`;
}
const wdots = (n, of) => `<span class="u" title="${n} warm of ${of}"><span class="wd">${Array.from({ length: of }, (_, i) => `<i data-on="${i < n}"></i>`).join("")}</span></span>`;
/** An interval with whiskers on a shared axis [a0, a1], its median beside. */
function whisk([lo, mid, hi], [a0, a1], color, text, win, w = 64) {
  const p = (x) => ((x - a0) / (a1 - a0 || 1)) * 100;
  return `<span class="u" title="${esc(`median ${text}, 95% ${lo}–${hi}`)}"><span class="wk" style="width:${w}px"><b style="left:${p(lo)}%;width:${Math.max(2, p(hi) - p(lo))}%;background:${color}"></b><s style="left:calc(${p(mid)}% - 1px);background:${color}"></s></span>${win ? `<span class="win">${icon("check", "Clear winner", 12)}</span>` : ""}<span class="ell">${esc(text)}</span></span>`;
}
/** The validated categorical slots (dataviz dark steps, checked on kiln's #0a0c0f): blue, orange, aqua, yellow. */
const SERIES4 = ["#3987e5", "#d95926", "#199e70", "#c98500"];
const KEYS = "ABCD";
const key3 = (i) => `<span class="key3" style="background:${SERIES4[i]}" title="Variant ${KEYS[i]}">${KEYS[i]}</span>`;
const START_COLORS = { warm: "#d95926", template: "#3987e5", cold: "#199e70", process: "#c98500" };

/* ------------------------------------------------------------ charts */
const CHARTS = {};
let chartN = 0;
function walk(n, base, spread, seed0) { let s = seed0, v = base; return Array.from({ length: n }, () => { s = (s * 16807) % 2147483647; v = Math.max(0, v + ((s / 2147483647) - 0.5) * spread); return Math.round(v * 10) / 10; }); }
function chart(title, lines, unit, h = 72) {
  const id = "c" + chartN++, W = 300, n = lines[0].values.length;
  const top = Math.max(...lines.flatMap((l) => l.values)) * 1.15 || 1;
  const x = (i) => (i / (n - 1)) * W, y = (v) => h - (v / top) * (h - 6) - 1;
  CHARTS[id] = { lines, unit, n, W };
  const path = (vals) => vals.map((v, i) => `${i ? "L" : "M"}${x(i).toFixed(1)},${y(v).toFixed(1)}`).join("");
  const read = (i) => lines.map((l) => `${lines.length > 1 ? `<i style="background:${l.color}"></i>` : ""}${lines.length > 1 ? esc(l.name) + " " : ""}${l.values[i]}${unit}`).join(" ");
  return `<figure class="chart" data-chart="${id}"><figcaption><span>${esc(title)}</span><span class="chart-read val">${read(n - 1)}</span></figcaption>
    <svg viewBox="0 0 ${W} ${h}" preserveAspectRatio="none" height="${h}" role="img" aria-label="${esc(title)}">
      ${[0.33, 0.66].map((f) => `<line x1="0" x2="${W}" y1="${(h * f).toFixed(1)}" y2="${(h * f).toFixed(1)}" class="chart-grid"/>`).join("")}
      ${lines.length === 1 ? `<path d="${path(lines[0].values)}L${W},${h}L0,${h}Z" fill="${lines[0].color}" opacity=".1"/>` : ""}
      ${lines.map((l) => `<path d="${path(l.values)}" fill="none" stroke="${l.color}" stroke-width="2" stroke-linejoin="round" vector-effect="non-scaling-stroke"/>`).join("")}
      <line class="chart-x" x1="0" x2="0" y1="0" y2="${h}" style="display:none" vector-effect="non-scaling-stroke"/></svg>
    <div class="axis"><span>1h</span><span>now</span></div></figure>`;
}
document.addEventListener("pointermove", (e) => {
  const fig = e.target.closest?.("figure.chart");
  $$(".chart-x").forEach((l) => { if (!fig || !fig.contains(l)) l.style.display = "none"; });
  if (!fig) return; const c = CHARTS[fig.dataset.chart]; if (!c) return;
  const r = fig.querySelector("svg").getBoundingClientRect(); const i = Math.round(Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)) * (c.n - 1));
  const ln = fig.querySelector(".chart-x"); ln.style.display = ""; ln.setAttribute("x1", (i / (c.n - 1)) * c.W); ln.setAttribute("x2", (i / (c.n - 1)) * c.W);
  fig.querySelector(".chart-read").innerHTML = `${60 - Math.round((i / (c.n - 1)) * 60)}m · ` + c.lines.map((l) => `${c.lines.length > 1 ? `<i style="background:${l.color}"></i>${esc(l.name)} ` : ""}${l.values[i]}${c.unit}`).join(" ");
});

/* ------------------------------------------------------------ small controls */
const sel = (opts, value, attrs = "") => `<select class="sel3" ${attrs}>${opts.map(([v, l]) => `<option value="${esc(v)}"${v === value ? " selected" : ""}>${esc(l)}</option>`).join("")}</select>`;
const ib = (ic, tip, attrs = "") => `<button type="button" class="ib" title="${esc(tip)}" aria-label="${esc(tip)}" ${attrs}>${icon(ic)}</button>`;
/** The same control slots on every agent, session and VM row: run or resume, pause, restart, stop, more. */
function ctl(state, id, more = true) {
  const can = { play: ["paused", "stopped", "failed", "idle-def"].includes(state), pause: ["run", "idle", "start"].includes(state), restart: state !== "stopped", stop: state !== "stopped" };
  return `<span class="acts ctl">${ib(state === "idle-def" ? "play" : "play", state === "idle-def" ? "Run (Enter)" : "Resume (P)", `data-do="play" data-id="${id}" ${can.play ? "" : "disabled"}`)}${ib("pause", "Pause (P)", `data-do="pause" data-id="${id}" ${can.pause ? "" : "disabled"}`)}${ib("restart", "Restart (R)", `data-do="restart" data-id="${id}" ${can.restart ? "" : "disabled"}`)}${ib("stop", "Stop (S)", `data-do="stop" data-id="${id}" data-tone="danger" data-confirm="Stop?" ${can.stop ? "" : "disabled"}`)}${more ? ib("more", "More", `data-do="more" data-id="${id}"`) : ""}</span>`;
}

/* ------------------------------------------------------------ ⌘K: every action, from anywhere (kiln's op registry) */
let paletteItems = () => [];
function palette(open = true) {
  let d = $("#pal");
  if (!d) {
    document.body.insertAdjacentHTML("beforeend", `<dialog id="pal" class="palette-dialog v3 pal3" aria-label="Command palette"><div class="pal-in">${icon("search")}<input class="pal-q" placeholder="Run, pause, switch…" aria-label="Command"></div><div class="pal-list" role="listbox"></div><div class="pal-foot"><span class="lbl">↑↓ choose</span><span class="lbl">↵ run</span><span class="lbl">esc close</span></div></dialog>`);
    d = $("#pal");
    const q = $(".pal-q", d);
    q.addEventListener("input", () => fill(d, q.value));
    q.addEventListener("keydown", (e) => {
      const items = $$(".pal-it", d); let i = items.findIndex((x) => x.getAttribute("aria-selected") === "true");
      if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); i = (i + (e.key === "ArrowDown" ? 1 : -1) + items.length) % items.length; items.forEach((x, j) => x.setAttribute("aria-selected", String(j === i))); items[i]?.scrollIntoView({ block: "nearest" }); }
      if (e.key === "Enter") { e.preventDefault(); items[i]?.click(); }
    });
    d.addEventListener("click", (e) => { if (e.target === d) d.close(); const it = e.target.closest(".pal-it"); if (it) { d.close(); paletteItems()[Number(it.dataset.i)]?.run?.(); } });
  }
  if (!open) { d.close(); return; }
  $(".pal-q", d).value = ""; fill(d, ""); if (!d.open) d.showModal(); $(".pal-q", d).focus();
}
function fill(d, q) {
  const all = paletteItems().map((it, i) => ({ ...it, i })).filter((it) => !q || `${it.verb} ${it.obj} ${it.group}`.toLowerCase().includes(q.toLowerCase()));
  let last = null;
  $(".pal-list", d).innerHTML = all.map((it, j) => { const h = it.group !== last ? `<div class="lbl pal-g">${esc(it.group)}</div>` : ""; last = it.group; return `${h}<div class="pal-it" role="option" data-i="${it.i}" aria-selected="${j === 0}" title="${esc(it.op || "")}">${icon(it.icon)}<span class="lbl hi">${esc(it.verb)}</span><span class="val ell">${esc(it.obj)}</span>${it.keys ? `<span class="lbl pal-k">${esc(it.keys)}</span>` : ""}</div>`; }).join("") || `<div class="pal-it"><span class="lbl">No match</span></div>`;
}
document.addEventListener("keydown", (e) => { if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") { e.preventDefault(); palette(); } });

/* ------------------------------------------------------------ ⋯: the rare things, one menu */
function menu(anchor, items) {
  $("#menu3")?.remove();
  const r = anchor.getBoundingClientRect();
  document.body.insertAdjacentHTML("beforeend", `<div id="menu3" class="popover v3 menu3" role="menu" style="position:absolute;top:${r.bottom + scrollY + 4}px;left:${Math.max(8, r.right + scrollX - 180)}px">${items.map((it, i) => `<button class="menu-item" role="menuitem" data-i="${i}" ${it.danger ? "data-danger" : ""}>${icon(it.icon)}<span>${esc(it.label)}</span></button>`).join("")}</div>`);
  const m = $("#menu3");
  m.addEventListener("click", (e) => { const b = e.target.closest("[data-i]"); if (!b) return; m.remove(); items[Number(b.dataset.i)].run(); });
  setTimeout(() => document.addEventListener("click", function off(e) { if (!m.contains(e.target)) { m.remove(); document.removeEventListener("click", off); } }), 0);
  m.querySelector("button")?.focus();
}

/* ------------------------------------------------------------ peek: a hover card that acts in place */
let peekHtml = () => "", peekAct = () => {};
let peekTimer = null;
function peekAt(el) {
  clearTimeout(peekTimer);
  $("#peek3")?.remove();
  const html = peekHtml(el.dataset.peek); if (!html) return;
  const r = el.getBoundingClientRect();
  document.body.insertAdjacentHTML("beforeend", `<div id="peek3" class="popover v3 peek3" data-for="${esc(el.dataset.peek)}" style="position:absolute;top:${r.bottom + scrollY + 6}px;left:${Math.min(r.left + scrollX, document.documentElement.clientWidth - 316)}px">${html}</div>`);
  const p = $("#peek3");
  p.addEventListener("mouseenter", () => clearTimeout(peekTimer));
  p.addEventListener("mouseleave", () => { peekTimer = setTimeout(() => p.remove(), 250); });
  p.addEventListener("click", (e) => { const b = e.target.closest("[data-do]"); if (b) peekAct(p.dataset.for, b.dataset.do, b); });
}
document.addEventListener("mouseover", (e) => { const el = e.target.closest?.("[data-peek]"); if (el && !el.closest("#peek3")) { clearTimeout(peekTimer); peekTimer = setTimeout(() => peekAt(el), 280); } });
document.addEventListener("mouseout", (e) => { const el = e.target.closest?.("[data-peek]"); if (el) { clearTimeout(peekTimer); peekTimer = setTimeout(() => $("#peek3")?.remove(), 300); } });

/* ------------------------------------------------------------ the mockup's own bar: the four pages and ⌘K */
function mockBar(here) {
  return `<nav class="mk3" aria-label="Mockups">${[["runners", "Runners"], ["agents", "Agents"], ["agentfile", "Agentfile"], ["chat", "Chat"]].map(([f, l]) => `<a href="${f}.html"${f === here ? ' aria-current="page"' : ""}>${l}</a>`).join("")}<button class="srch3 right" data-palette title="Search or run anything (${KEY_K})">${icon("search")}<span class="lbl h640">Search or run</span><span class="lbl kbd3">${KEY_K}</span></button></nav>`;
}
document.addEventListener("click", (e) => { if (e.target.closest("[data-palette]")) palette(); });

/* ------------------------------------------------------------ audit: sizes, line-heights, wraps and card edges, logged */
function audit() {
  const sizes = new Map(), lhs = new Set(), wraps = [], over = [];
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const t = n.textContent.trim(); const el = n.parentElement; if (!t || !el || el.closest("script,style,title,option,textarea,#pal:not([open]) ")) continue;
    const cs = getComputedStyle(el); if (cs.display === "none" || cs.visibility === "hidden" || !el.getClientRects().length) continue;
    const k = `${cs.fontSize} ${cs.fontFamily.split(",")[0].replace(/"/g, "")}`; sizes.set(k, (sizes.get(k) || 0) + 1); lhs.add((parseFloat(cs.lineHeight) / parseFloat(cs.fontSize)).toFixed(2));
    const rg = document.createRange(); rg.selectNodeContents(n); const tops = new Set([...rg.getClientRects()].map((r) => Math.round(r.top))); if (tops.size > 1) wraps.push(t.slice(0, 40));
  }
  $$(".card3").forEach((c, ci) => {
    const left = c.getBoundingClientRect().left + 1; const edges = new Set();
    $$(".ch, .sh, .fr, .sum3, .form3, .charts3, .tabs3, .t3 tr", c).forEach((row) => { if (!row.getClientRects().length) return; const first = row.matches("tr") ? row.firstElementChild : row; if (!first || first.classList.contains("detail")) return; const cs = getComputedStyle(first); const x = first.getBoundingClientRect().left + parseFloat(cs.paddingLeft) - left; edges.add(Math.round(x)); });
    over.push(`card${ci}:${[...edges].join("/")}`);
  });
  console.info(`AUDIT w=${innerWidth} sizes=${[...sizes.entries()].map(([k, v]) => `${k}×${v}`).join(", ")} | lh=${[...lhs].join(",")} | wraps=${wraps.length}${wraps.length ? " " + JSON.stringify(wraps.slice(0, 6)) : ""} | edges ${over.join(" ")}`);
}
window.addEventListener("load", () => { if (window.AUDIT) setTimeout(audit, 400); });

/* ------------------------------------------------------------ tables: the name column a share of the width, metrics fixed, a spacer, controls pinned right */
const narrow = () => innerWidth < 640;
/** cols: [{h, w (px or %), c (hide class), name (the first column), spacer, ctl}] */
function th3(cols) {
  return `<thead><tr>${cols[0]?.name ? '<th class="lead"></th>' : ""}${cols.map((c, i) => `<th title="${esc(c.tip || "")}" class="${c.c || ""}${c.spacer ? " sp" : ""}" ${c.name ? "data-name" : ""} ${i === cols.length - 1 && !c.h && c.w ? `data-ctl data-w="${c.w}"` : ""} style="${c.spacer ? "" : c.name ? (narrow() || !c.w ? "" : `width:${c.w}`) : `width:${c.w}px`}">${c.h || ""}</th>`).join("")}</tr></thead>`;
}
/** A row's cells; when the table leads with a name, the first cell's glyph sits in its own lead column. */
const td3 = (cols, cells) => cells.map((x, i) => { const cls = `${cols[i].c || ""}${cols[i].spacer ? " sp" : ""}`; if (i === 0 && cols[0]?.name) { const o = typeof x === "object" && x ? x : { lead: "", body: x ?? "" }; return `<td class="lead">${o.lead}</td><td class="${cls}">${o.body}</td>`; } return `<td class="${cls}">${x ?? ""}</td>`; }).join("");
/** The first cell: a status glyph and a name, on the card's edge. */
/** A status glyph (for the lead column) and a name, cut in the middle when it doesn't fit, its tail kept. */
const named = (state, name, attrs = "", tip, extra = "") => ({ lead: st(state, tip), body: `<span class="cr"><span class="ell mid" ${attrs} ${attrs.includes("data-peek") ? 'tabindex="0"' : ""} title="${esc(name)}" data-full="${esc(name)}">${esc(name)}</span>${extra}</span>` });
/** Names that don't fit lose their middle, never their tail: adas-mac…local-2. */
function fitMiddle() {
  const cut = (full, head, tail) => (head + tail >= full.length ? full : full.slice(0, head) + "…" + full.slice(full.length - tail));
  const fits = (el, t) => { el.textContent = t; return el.scrollWidth <= el.clientWidth + 1; };
  // First the longest cut that fits, keeping the tail; then, where two names in one table read alike, the cut moves until each reads apart.
  const els = $$(".mid");
  els.forEach((el) => {
    const full = el.dataset.full; el._k = full.length;
    if (fits(el, full)) { el._shown = full; return; }
    let lo = 3, hi = full.length - 1, best = 3;
    while (lo <= hi) { const k = (lo + hi) >> 1; if (fits(el, cut(full, k - Math.min(Math.ceil(k / 2), 9), Math.min(Math.ceil(k / 2), 9)))) { best = k; lo = k + 1; } else hi = k - 1; }
    el._k = best; el._shown = cut(full, best - Math.min(Math.ceil(best / 2), 9), Math.min(Math.ceil(best / 2), 9)); el.textContent = el._shown;
  });
  els.forEach((el) => {
    if (el._shown === el.dataset.full) return;
    const peers = $$(".mid", el.closest("table") || document).filter((x) => x !== el && x.dataset.full !== el.dataset.full);
    const full = el.dataset.full, k = el._k;
    for (let tail = Math.min(Math.ceil(k / 2), 9); tail <= k - 1; tail++) {
      const t = cut(full, k - tail, tail);
      if (!peers.some((x) => (x._shown || x.textContent) === t)) { el._shown = t; el.textContent = t; return; }
    }
  });
}
addEventListener("resize", () => requestAnimationFrame(fitMiddle));
/** A count of a whole as blocks: ▮▮▮▮▮▯▯ 5 of 7. */
const blocks = (n, of, color, tip) => `<span class="u" title="${esc(tip)}"><span class="blk">${Array.from({ length: of }, (_, i) => `<i style="${i < n ? `background:${color}` : ""}"></i>`).join("")}</span></span>`;
/** Start kinds as their icons, each with its count, in the colours their bars use. */
const starts3 = (w, t, c) => `<span class="u" title="Warm ${w} · template ${t} · cold ${c}">${[["warm", w], ["template", t], ["cold", c]].map(([k, n]) => `<span class="kv3">${startIcon(k)}<span>${n}</span></span>`).join("")}</span>`;
/** A mock-only control, outside the page: it isn't part of the design. */
function mockChip(html) { let c = $("#mockchip"); if (!c) { document.body.insertAdjacentHTML("beforeend", `<div id="mockchip" class="v3 mockchip"><span class="lbl">mock</span><span id="mockchip-in"></span></div>`); c = $("#mockchip"); } $("#mockchip-in").innerHTML = html; }
/** Which width band the window is in: tables lose columns at 640, 900 and 1100. */
const band = () => (innerWidth < 640 ? 0 : innerWidth < 900 ? 1 : innerWidth < 1100 ? 2 : 3);
/** How many of a table's columns show at this width: what a row spanning the table spans. */
const shown = (cols) => (cols[0]?.name ? 1 : 0) + cols.filter((c) => !(c.spacer && narrow()) && !(c.c === "h1100" && innerWidth < 1100) && !(c.c === "h900" && innerWidth < 900) && !(c.c === "h640" && innerWidth < 640)).length;
/** Draw again when the window crosses a band. */
function onBand(render) { let b = band(); addEventListener("resize", () => { if (band() !== b) { b = band(); render(); } }); }

/* ------------------------------------------------------------ the person's rules, as code (docs/ui/principles.md) */
const isMac = /Mac|iP(hone|ad)/.test(navigator.platform || navigator.userAgent);
const KEY_K = isMac ? "⌘K" : "Ctrl K";

/** A peek's controls: safe ones alone (open, pause, resume); never a destructive one. */
const peekCtl = (state, id) => `<span class="acts ctl">${["paused", "stopped", "failed"].includes(state) ? ib("play", "Resume", `data-do="play" data-id="${id}" ${state === "paused" ? "" : "disabled"}`) : ib("pause", "Pause", `data-do="pause" data-id="${id}" ${["run", "idle", "start"].includes(state) ? "" : "disabled"}`)}</span>`;

/** Destructive icons confirm in place: the icon becomes "Stop? ✓ ✕" on its row; ✓ does it, ✕ puts the icon back. */
document.addEventListener("click", (e) => {
  const yes = e.target.closest(".cf3 [data-yes]"), no = e.target.closest(".cf3 [data-no]");
  if (yes || no) { e.stopImmediatePropagation(); const cf = e.target.closest(".cf3"); const btn = cf.previousElementSibling; cf.remove(); btn.hidden = false; if (yes) { btn.dataset.armed = "1"; btn.click(); } return; }
  const b = e.target.closest("[data-confirm]");
  if (!b || b.disabled) return;
  if (b.dataset.armed) { delete b.dataset.armed; return; }
  e.stopImmediatePropagation(); e.preventDefault();
  $$(".cf3").forEach((c) => { c.previousElementSibling.hidden = false; c.remove(); });
  b.hidden = true;
  b.insertAdjacentHTML("afterend", `<span class="cf3" role="group" aria-label="${esc(b.dataset.confirm)}"><span class="lbl hi">${esc(b.dataset.confirm)}</span>${ib("check", "Confirm", 'data-yes')}${ib("x", "Keep", 'data-no')}</span>`);
  $(".cf3 [data-yes]")?.focus();
}, true);

/** Reversible actions need no question: they happen, and a toast offers Undo. */
function toast(label, undo) {
  let vp = $("#toasts3"); if (!vp) { document.body.insertAdjacentHTML("beforeend", `<div id="toasts3" class="toast-viewport v3" role="status"></div>`); vp = $("#toasts3"); }
  const id = "t" + Date.now();
  vp.insertAdjacentHTML("beforeend", `<div class="toast toast3" id="${id}">${icon("check")}<span class="lbl hi">${esc(label)}</span>${undo ? `<button class="b3 g" data-undo="${id}">Undo</button>` : ""}</div>`);
  const el = $("#" + id); el.querySelector("[data-undo]")?.addEventListener("click", () => { undo(); el.remove(); });
  setTimeout(() => el.remove(), 5000);
}

/** One open row per list, kept in the address (?open=<id>): reload, Back and shared links reopen it; Back closes it. */
const OPEN = { get: () => { try { return new URLSearchParams(location.search).get("open"); } catch { return null; } }, scroll: 0 };
function setOpen(id, render) {
  try { const u = new URL(location.href); if (id) u.searchParams.set("open", id); else u.searchParams.delete("open"); history.pushState({ open: id }, "", u); } catch {}
  if (id) OPEN.scroll = scrollY;
  render();
  if (!id) scrollTo(0, OPEN.scroll);
}
function onOpenChange(fn) { addEventListener("popstate", () => fn(OPEN.get())); }

/** j/k move between a list's rows, Enter opens one, Esc closes it. */
document.addEventListener("keydown", (e) => {
  if (e.target.matches?.("input,select,textarea") || e.metaKey || e.ctrlKey || e.altKey) return;
  if (e.key !== "j" && e.key !== "k") return;
  const rows = $$("tr[tabindex]").filter((r) => r.offsetParent);
  const i = rows.indexOf(document.activeElement.closest?.("tr[tabindex]"));
  const next = rows[Math.min(rows.length - 1, Math.max(0, i + (e.key === "j" ? 1 : -1)))] || rows[0];
  if (next) { e.preventDefault(); next.focus(); next.scrollIntoView({ block: "nearest" }); }
});

/** Peeks open on hover, on focus, and on a tap where there's no hover. */
document.addEventListener("focusin", (e) => { const el = e.target.closest?.("[data-peek]"); if (el) peekAt(el); });
document.addEventListener("click", (e) => { const el = e.target.closest?.("[data-peek]"); if (el && matchMedia("(hover: none)").matches) { e.stopPropagation(); peekAt(el); } }, true);

/** One stream per page; only rows on screen take its updates, drawn once per frame. */
const LIVE = { seen: new Set(), io: null, dirty: new Set(), frame: 0 };
function liveScan() {
  LIVE.io?.disconnect();
  LIVE.io = new IntersectionObserver((es) => es.forEach((x) => { const k = x.target.dataset.live; x.isIntersecting ? LIVE.seen.add(k) : LIVE.seen.delete(k); }));
  LIVE.seen.clear(); $$("[data-live]").forEach((el) => LIVE.io.observe(el));
}
/** Each tick: `advance()` moves the data on; then each live element on screen is drawn again from `draw(key)`, in one frame. */
function liveStream(ms, advance, draw) {
  setInterval(() => {
    if (document.hidden) return;
    advance();
    LIVE.seen.forEach((k) => LIVE.dirty.add(k));
    if (LIVE.frame) return;
    LIVE.frame = requestAnimationFrame(() => {
      LIVE.frame = 0;
      LIVE.dirty.forEach((k) => { const html = draw(k); if (html == null) return; $$(`[data-live="${CSS.escape(k)}"]`).forEach((el) => { const t = document.createElement("template"); t.innerHTML = html.trim(); const n = t.content.firstElementChild; if (n) { n.dataset.live = k; el.replaceWith(n); LIVE.io?.observe(n); } }); });
      LIVE.dirty.clear();
    });
  }, ms);
}
/** kiln's ops registry, as the palette reads it: each op's verb, icon and key. A page names only the op and its target. */
const OPS = {
  "agentdefs.run": { verb: "Run", icon: "play", keys: "⌘↵" },
  "agentdefs.restore": { verb: "Restore", icon: "history" },
  "agentdefs.create": { verb: "Copy", icon: "copy" },
  "experiments.start": { verb: "Start experiment", icon: "split" },
  "experiments.promote": { verb: "Promote", icon: "crown" },
  "sessions.pause": { verb: "Pause", icon: "pause", keys: "P" },
  "sessions.resume": { verb: "Resume", icon: "play", keys: "P" },
  "sessions.restart": { verb: "Restart", icon: "restart", keys: "R" },
  "runners.engine": { verb: "Switch engine", icon: "host" },
  "runners.drain": { verb: "Drain", icon: "pause" },
  "images.build": { verb: "Build image", icon: "build" },
  "agentfiles.update": { verb: "Save", icon: "check" },
};
/** Palette entries from the registry: [op, target, run, group]. */
const fromOps = (list) => list.map(([op, obj, run, group]) => ({ op, obj, run, group, ...OPS[op] }));

/** Free width first, one grid: every table marked data-grid gets each slot's widest natural width across its group,
 *  so columns line up and nothing truncates while there's room; the name takes what's left, and only when the
 *  window can't hold them all does anything give (the slots in proportion, the name down to its floor). */
function fitGrid() {
  const groups = {};
  $$("table.t3[data-grid]").forEach((t) => { if (t.offsetParent) (groups[t.dataset.grid] ||= []).push(t); });
  Object.values(groups).forEach((tables) => {
    const nat = [];
    tables.forEach((t) => {
      t.querySelectorAll(":scope > thead > tr, :scope > tbody > tr").forEach((tr) => {
        if (tr.querySelector("td.detail")) return;
        [...tr.children].forEach((cell, i) => {
          if (cell.offsetParent === null) return;
          const cs = getComputedStyle(cell); const r = document.createRange(); r.selectNodeContents(cell);
          const w = Math.ceil(r.getBoundingClientRect().width + parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight));
          nat[i] = Math.max(nat[i] || 0, w);
        });
      });
    });
    tables.forEach((t) => {
      const ths = [...t.querySelectorAll(":scope > thead > tr > th")]; const avail = t.clientWidth;
      const nameIdx = ths.findIndex((th) => th.hasAttribute("data-name"));
      const fixed = ths.map((th, i) => (th.offsetParent === null || i === nameIdx ? 0 : th.classList.contains("lead") ? 40 : th.hasAttribute("data-ctl") ? parseFloat(th.dataset.w) : Math.max(nat[i] || 0, 40)));
      const floor = Math.min(160, nat[nameIdx] || 160);
      const flexSum = fixed.reduce((a, w, i) => a + (ths[i].hasAttribute("data-ctl") || ths[i].classList.contains("lead") ? 0 : w), 0);
      const rigid = fixed.reduce((a, w) => a + w, 0) - flexSum;
      const scale = Math.min(1, Math.max(0, (avail - floor - rigid) / (flexSum || 1)));
      ths.forEach((th, i) => { if (i === nameIdx || th.offsetParent === null) { th.style.width = ""; return; } const flex = !(th.hasAttribute("data-ctl") || th.classList.contains("lead")); th.style.width = `${Math.floor(flex ? fixed[i] * scale : fixed[i])}px`; });
    });
  });
}
addEventListener("resize", () => requestAnimationFrame(() => { fitGrid(); fitMiddle(); }));
ICONS.adhoc = '<path d="M10.1 2.18a9.93 9.93 0 0 1 3.8 0M17.6 3.71a9.95 9.95 0 0 1 2.69 2.7M21.82 10.1a9.93 9.93 0 0 1 0 3.8M20.29 17.6a9.95 9.95 0 0 1-2.7 2.69M13.9 21.82a9.94 9.94 0 0 1-3.8 0M6.4 20.29a9.95 9.95 0 0 1-2.69-2.7M2.18 13.9a9.93 9.93 0 0 1 0-3.8M3.71 6.4a9.95 9.95 0 0 1 2.7-2.69"/>';
