// Mockup helpers. Nothing here is production code: it fakes the server's answers so each press
// shows what kiln's would (W21-R15: the control pressed says its request is on its way, the rest
// of the page stays usable).
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

/** A press whose request takes `ms`: the control says it's on its way, then `done` runs. */
function press(el, ms, done) {
  if (el.hasAttribute("data-sending")) return;
  el.setAttribute("data-sending", "");
  el.setAttribute("aria-busy", "true");
  setTimeout(() => {
    el.removeAttribute("data-sending");
    el.removeAttribute("aria-busy");
    done();
  }, ms);
}

/** A Segmented control: `on(value)` when a pill is pressed. */
function segmented(root, on) {
  root.addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b || b.disabled || !root.contains(b)) return;
    on(b.dataset.v, b);
  });
}
function segSet(root, v) {
  $$("button", root).forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.v === v)));
}

const HARNESSES = [
  { id: "claude", name: "Claude Code", version: "2.1.311", models: [["claude-opus-5-5", "Opus 5.5"], ["claude-sonnet-5-5", "Sonnet 5.5"]], egress: ["api.anthropic.com:443", "claude.ai:443", "platform.claude.com:443"] },
  { id: "codex", name: "Codex", version: "0.68.0", models: [["gpt-6.1-sol", "GPT-6.1 Sol"], ["gpt-6.1-sol-mini", "GPT-6.1 Sol mini"]], egress: ["api.openai.com:443", "auth.openai.com:443", "chatgpt.com:443"] },
  { id: "devin", name: "Devin", version: "2026.11.11-b912975", models: [["swe-2", "SWE 2"], ["claude-opus-5-5", "Opus 5.5"]], egress: ["api.devin.ai:443"] },
  { id: "gemini", name: "Gemini CLI", version: "0.12.0", models: [["gemini-3.8-pro", "Gemini 3.8 Pro"]], egress: ["generativelanguage.googleapis.com:443", "oauth2.googleapis.com:443"] },
  { id: "opencode", name: "opencode", version: "1.5.7", models: [], egress: [] },
  { id: "goose", name: "Goose", version: "1.59.0", models: [], egress: [] },
  { id: "amp", name: "Amp", version: "0.167.2", models: [], egress: ["ampcode.com:443"] },
  { id: "antigravity", name: "Antigravity CLI", version: "1.5.2", models: [["gemini-3.8-pro", "Gemini 3.8 Pro"]], egress: ["cloudcode-pa.googleapis.com:443"] },
  { id: "cursor", name: "Cursor CLI", version: "2.2.7", models: [], egress: ["api2.cursor.sh:443"] },
  { id: "pi", name: "Pi", version: "3000.12.43", models: [], egress: [] },
];
const harnessOf = (id) => HARNESSES.find((h) => h.id === id);

/** The person's secrets, by name and scope: names only, never a value (W09). */
const SECRETS = [
  ["user", "GH_READ_TOKEN"],
  ["user", "GH_DEPLOY_KEY"],
  ["user", "MANTLE_ACCESS_KEY_ID"],
  ["user", "MANTLE_SECRET_ACCESS_KEY"],
  ["user", "R2_ACCESS_KEY_ID"],
  ["user", "R2_SECRET_ACCESS_KEY"],
  ["user", "RELEASES_BEARER"],
  ["server", "ANTHROPIC_API_KEY"],
];
function secretOptions(selected, none = "None") {
  return `<option value="">${esc(none)}</option>` + SECRETS.map(([s, n]) => `<option value="${esc(n)}"${n === selected ? " selected" : ""}>${esc(n)} · ${esc(s)}</option>`).join("");
}
function select(id, options, value, attrs = "") {
  return `<div class="select"><select id="${id}" ${attrs}>${options.map(([v, l, dis]) => `<option value="${esc(v)}"${v === value ? " selected" : ""}${dis ? " disabled" : ""}>${esc(l)}</option>`).join("")}</select><svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="m6 9 6 6 6-6"/></svg></div>`;
}
const CHEV = `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="m6 9 6 6 6-6"/></svg>`;
/** An address's parts (the page bundler reads a literal constructor call as a file to bundle). */
const parseAddress = (x) => Reflect.construct(globalThis["U" + "RL"], [x]);
