// kiln's bridge, first in every page an agent shows (crate::harness::visuals; docs/previews.md,
// "The bridge"). The conversation's frame (ui/src/views/harness/page.tsx) answers it.
(() => {
  "use strict";
  if (window.kiln) return;
  const host = window.parent;
  const framed = host !== window;
  const post = (method, params) => { if (framed) try { host.postMessage({ jsonrpc: "2.0", method, params }, "*"); } catch (e) {} };

  // Its own height (not the frame's): the root's box, or the body's bottom when that's lower.
  let last = 0;
  const size = () => {
    const d = document.documentElement, b = document.body;
    const bottom = b ? b.getBoundingClientRect().bottom + (parseFloat(getComputedStyle(b).marginBottom) || 0) : 0;
    const h = Math.ceil(Math.max(d.getBoundingClientRect().height, bottom));
    if (h && h !== last) { last = h; post("ui/notifications/size-changed", { height: h }); }
  };

  // Links to the web open beside the conversation, not in the frame.
  addEventListener("click", (e) => {
    if (!framed || e.defaultPrevented) return;
    const a = e.target instanceof Element ? e.target.closest("a[href]") : null;
    if (!a || a.hasAttribute("download")) return;
    const url = a.href;
    if (/^https?:/i.test(url) && !url.startsWith(location.href.split("#")[0] + "#")) { e.preventDefault(); post("ui/open-link", { url }); }
  }, true);

  // Out of view, what moves pauses: frames asked for, animations, media, sound.
  const raf = window.requestAnimationFrame.bind(window);
  let visible = true;
  const held = [];
  window.requestAnimationFrame = (cb) => { if (visible) return raf(cb); held.push(cb); return -held.length; };
  const sounds = new Set();
  for (const name of ["AudioContext", "webkitAudioContext"]) {
    const C = window[name];
    if (C) window[name] = class extends C { constructor(...a) { super(...a); sounds.add(this); } };
  }
  const stopped = new WeakSet();
  const setVisible = (v) => {
    if (v === visible) return;
    visible = v;
    for (const a of document.getAnimations ? document.getAnimations() : []) {
      if (!v && a.playState === "running") { a.pause(); stopped.add(a); }
      else if (v && stopped.has(a)) { stopped.delete(a); a.play(); }
    }
    for (const m of document.querySelectorAll("video, audio")) {
      if (!v && !m.paused) { m.pause(); stopped.add(m); }
      else if (v && stopped.has(m)) { stopped.delete(m); m.play().catch(() => {}); }
    }
    for (const c of sounds) { if (v) c.resume().catch(() => {}); else c.suspend().catch(() => {}); }
    if (v) for (const cb of held.splice(0)) raf(cb);
  };

  // The conversation's look as it changes (MCP Apps' host context).
  const look = (p) => {
    const vars = (p && ((p.styles && p.styles.variables) || (p.theme && p.theme.variables))) || {};
    for (const [k, v] of Object.entries(vars)) if (k.startsWith("--")) document.documentElement.style.setProperty(k, String(v));
    if (p && (p.theme === "light" || p.theme === "dark")) document.documentElement.style.colorScheme = p.theme;
  };

  // What the person did, kept by the conversation, handed back once the page says what to do with
  // it (whenever its scripts set `kiln.onrestore`).
  let restore;
  let pending = false;
  let onrestore = null;
  const give = () => { if (pending && typeof onrestore === "function") { pending = false; const s = restore; restore = undefined; try { onrestore(s); } catch (e) { setTimeout(() => { throw e; }); } } };
  const kiln = {
    reply(value, label) { post("kiln/reply", { value, label: String(label ?? (typeof value === "string" ? value : JSON.stringify(value))).slice(0, 200) }); },
    save(state) { post("kiln/state", { state }); },
    get onrestore() { return onrestore; },
    set onrestore(f) { onrestore = f; queueMicrotask(give); },
  };
  Object.defineProperty(window, "kiln", { value: Object.freeze(kiln) });

  addEventListener("message", (e) => {
    if (e.source !== host) return;
    const m = e.data || {};
    if (m.method === "ui/notifications/host-context-changed") look(m.params);
    else if (m.method === "kiln/visibility") setVisible(!!(m.params && m.params.visible));
    else if (m.method === "kiln/restore") { restore = m.params && m.params.state; pending = true; give(); }
  });

  // Ready once the page's own scripts (deferred and modules too) have run.
  const ready = () => {
    const r = new ResizeObserver(size);
    r.observe(document.documentElement);
    if (document.body) r.observe(document.body);
    size();
    // This document's own mark: what it did is handed back once to it, however often it says it's ready.
    post("ui/notifications/initialized", { kiln: Math.random().toString(36).slice(2) });
  };
  if (document.readyState === "loading") addEventListener("DOMContentLoaded", ready, { once: true });
  else ready();
  addEventListener("load", size);
})();
