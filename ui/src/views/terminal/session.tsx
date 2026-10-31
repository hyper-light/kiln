import * as React from "react";
import { wsUrl } from "@/api/client";
import { readFrame } from "@/api/frames";
import type { TerminalControlFrame, TerminalSessionInfo, TerminalUp } from "@/api/types";
import type { UnreadFrame } from "@/api/wire";
import { createTerminal, ensureFont, TERM_FONT, type KilnTerminal } from "@/components/terminal/xterm-setup";
import { copyText, readClipboard } from "./clipboard";

/** A terminal socket's text frames, by tag: a frame with another isn't applied (`api/frames.ts`). */
const TERMINAL_FRAMES = { session: true, missing: true, reconnect: true, size: true, exit: true, error: true, refused: true } satisfies Record<TerminalControlFrame["type"], true>;

export type SessionState =
  | { kind: "connecting" }
  | { kind: "open" }
  /** Lost the connection; the shell keeps running and this view reattaches on its own. */
  | { kind: "reconnecting"; attempt: number }
  | { kind: "exited"; code: number | null }
  | { kind: "error"; message: string };

export interface SessionHandle {
  reconnect: () => void;
  focus: () => void;
  clear: () => void;
  /** Copy the selection; false when nothing is selected. */
  copySelection: () => Promise<boolean>;
  /** Paste the clipboard into the session; false when the browser won't hand it over. */
  paste: () => Promise<boolean>;
  /** Paste this text (bracketed when the program asks for it). */
  pasteText: (text: string) => void;
  /** The session's text: the scrollback and the screen. */
  text: () => string;
  /** Send bytes to the shell as if typed (on-screen keyboard). */
  send: (bytes: string) => void;
  /** The program's application-cursor mode (arrows encode differently in vim, less, TUIs). */
  appCursor: () => boolean;
}

interface Props {
  /** Stable id of the tab this view belongs to (passed back to every callback). */
  tabId: string;
  /** Server session to attach to; null opens a new session on `target`. */
  sessionId: string | null;
  target: string;
  /** The WebSocket path to attach over (default `/api/terminal/ws`; a CI leg's is `/api/ci/legs/:id/terminal`). */
  endpoint?: string;
  /** The selected tab of a visible terminal page: the view the user is using right now. */
  active: boolean;
  fontSize: number;
  copyOnSelect: boolean;
  /** Whether tapping the terminal may open the device keyboard (false while the full kiln keyboard is in use). */
  deviceKeyboard: boolean;
  onState: (tabId: string, s: SessionState) => void;
  onTitle: (tabId: string, title: string) => void;
  /** The server confirmed which session this view is on (first frame of every attach). */
  onSession: (tabId: string, info: TerminalSessionInfo) => void;
  /** The session was ended (from this device or another): this tab should go. */
  onClosed: (tabId: string) => void;
  /** A finger scrolled the terminal, or tapped it (the on-screen keyboard folds and returns like a phone's). */
  onGesture?: (tabId: string, g: "scroll" | "tap") => void;
  onRenderer?: (r: "webgl" | "dom") => void;
}

interface Size {
  cols: number;
  rows: number;
}

const encoder = new TextEncoder();
/** Reattach delays: quick at first (server restarts take ~1-2 s), then patient. */
const BACKOFF_MS = [250, 500, 1000, 1500, 2500, 4000];
/** Window titles change constantly while AI harnesses work (spinners): report at most this often. */
const TITLE_EVERY_MS = 250;
/** While the view's space is changing (keyboard resize, rotation), size changes go out at most this often. */
const RESIZE_SETTLE_MS = 60;
/** Smallest font a view shrinks to while showing a session sized by a bigger screen. */
const MIN_FONT = 6;
/** Finger travel before a touch counts as a scroll instead of a tap. */
const TAP_SLOP_PX = 6;

/**
 * One terminal view: xterm.js attached to a server-side session over /api/terminal/ws.
 *
 * Size: a session has one size, shared by every device viewing it, and the device in use sets it —
 * the selected tab claims its full size when it attaches, when its space changes, and when the user
 * touches or types in it. Every other view (other devices, hidden tabs) draws at exactly the size
 * the server announces, shrinking its font to fit, so a program's cursor-addressed redraws land in
 * the right cells everywhere. Hidden tabs never resize a session and don't render.
 *
 * Connection: the shell lives on the server or a machine, not in this page. When the socket drops,
 * the view keeps reattaching by session id and repaints from the server's replay.
 */
const TerminalSessionView = React.forwardRef<SessionHandle, Props>(function TerminalSession(
  { tabId, sessionId, target, endpoint, active, fontSize, copyOnSelect, deviceKeyboard, onState, onTitle, onSession, onClosed, onGesture, onRenderer },
  ref,
) {
  const hostRef = React.useRef<HTMLDivElement>(null);
  const kRef = React.useRef<KilnTerminal | null>(null);
  const wsRef = React.useRef<WebSocket | null>(null);
  const sessionRef = React.useRef<string | null>(sessionId);
  /** The connection to the session: started once the terminal exists, again on reconnect(). */
  const connRef = React.useRef<{ stop: () => void } | null>(null);
  // Everything handlers read lives in refs: handlers never go stale and nothing re-renders for them.
  const live = React.useRef({ tabId, endpoint, active, fontSize, copyOnSelect, deviceKeyboard, onState, onTitle, onSession, onClosed, onGesture, onRenderer });
  live.current = { tabId, endpoint, active, fontSize, copyOnSelect, deviceKeyboard, onState, onTitle, onSession, onClosed, onGesture, onRenderer };
  /** The session's size as the server last announced it. */
  const sessionSize = React.useRef<Size | null>(null);
  /** The size that fills this view at the user's font size (measured on layout changes only). */
  const desired = React.useRef<Size | null>(null);
  const pendingSize = React.useRef<Size | null>(null);
  const sendTimer = React.useRef(0);

  /** Cells that fill the view at the user's font size. Reads layout: only on layout changes. */
  const measure = React.useCallback((): Size | null => {
    const k = kRef.current;
    const host = hostRef.current;
    if (!k || !host || host.clientWidth < 20 || host.clientHeight < 20) return null;
    const base = live.current.fontSize;
    if (k.term.options.fontSize !== base) k.term.options.fontSize = base;
    const d = k.fit.proposeDimensions();
    if (!d || !Number.isFinite(d.cols) || !Number.isFinite(d.rows)) return null;
    desired.current = { cols: Math.max(2, d.cols), rows: Math.max(2, d.rows) };
    return desired.current;
  }, []);

  /** Draw at the view's own scale again (it is the one in use, or the session fits it). */
  const unscale = React.useCallback(() => {
    const el = kRef.current?.term.element;
    if (el?.style.transform) el.style.transform = "";
  }, []);

  /** Send the claimed size, unless the session already has it. */
  const flushSize = React.useCallback(() => {
    window.clearTimeout(sendTimer.current);
    sendTimer.current = 0;
    const d = pendingSize.current;
    pendingSize.current = null;
    const ws = wsRef.current;
    if (!d || ws?.readyState !== WebSocket.OPEN) return;
    const cur = sessionSize.current;
    if (cur && cur.cols === d.cols && cur.rows === d.rows) return;
    sessionSize.current = d;
    ws.send(JSON.stringify({ type: "resize", cols: d.cols, rows: d.rows } satisfies TerminalUp));
  }, []);

  /**
   * This view is the one in use: draw at its full size and make that the session's size.
   * `remeasure` reads layout (space changed); typing passes false and costs no layout work.
   * `urgent` sends at once; space changes settle first so a drag doesn't flood the program.
   */
  const claim = React.useCallback(
    (remeasure: boolean, urgent: boolean) => {
      const k = kRef.current;
      const d = remeasure ? measure() : desired.current;
      if (!k || !d) return;
      const base = live.current.fontSize;
      if (k.term.options.fontSize !== base) k.term.options.fontSize = base;
      unscale();
      if (k.term.cols !== d.cols || k.term.rows !== d.rows) k.term.resize(d.cols, d.rows);
      const cur = sessionSize.current;
      if (cur && cur.cols === d.cols && cur.rows === d.rows) {
        pendingSize.current = null;
        return;
      }
      pendingSize.current = d;
      if (urgent) flushSize();
      else if (!sendTimer.current) sendTimer.current = window.setTimeout(flushSize, RESIZE_SETTLE_MS);
    },
    [measure, flushSize, unscale],
  );

  /**
   * Draw the session at the size the server announced: shrink the font when it is bigger than
   * this view, and below the smallest font, scale the whole grid down so it is all still in view
   * (until this device is used, which takes the size back).
   */
  const follow = React.useCallback(() => {
    const k = kRef.current;
    const want = sessionSize.current;
    if (!k || !want) return;
    const fits = measure(); // null while hidden: then only the grid follows
    let shrinkMore = false;
    if (fits) {
      const base = live.current.fontSize;
      let font = Math.max(MIN_FONT, Math.floor(base * Math.min(1, fits.cols / want.cols, fits.rows / want.rows) * 2) / 2);
      // Cell sizes round, so check the guess against the real metrics and step down until it fits.
      for (let i = 0; i < 6; i++) {
        if (k.term.options.fontSize !== font) k.term.options.fontSize = font;
        const d = k.fit.proposeDimensions();
        if (!d || (d.cols >= want.cols && d.rows >= want.rows)) break;
        if (font <= MIN_FONT) {
          shrinkMore = true;
          break;
        }
        font = Math.max(MIN_FONT, font - 0.5);
      }
    }
    if (k.term.cols !== want.cols || k.term.rows !== want.rows) k.term.resize(want.cols, want.rows);
    const el = k.term.element;
    const host = hostRef.current;
    const screen = el?.querySelector<HTMLElement>(".xterm-screen");
    if (shrinkMore && el && host && screen && screen.offsetWidth > 0) {
      const scale = Math.min(1, host.clientWidth / screen.offsetWidth, host.clientHeight / screen.offsetHeight);
      el.style.transformOrigin = "0 0";
      el.style.transform = `scale(${scale.toFixed(4)})`;
    } else {
      unscale();
    }
  }, [measure, unscale]);

  /** Lay the view out for its role after its space changed. */
  const layout = React.useCallback(() => {
    if (live.current.active) claim(true, false);
    else follow();
  }, [claim, follow]);

  const sendBytes = React.useCallback(
    (data: string | Uint8Array<ArrayBuffer>) => {
      const ws = wsRef.current;
      if (ws?.readyState !== WebSocket.OPEN) return;
      // Typing here makes this the device in use (tmux's "latest" sizing): take the size back.
      if (live.current.active) claim(false, true);
      ws.send(typeof data === "string" ? encoder.encode(data) : data);
    },
    [claim],
  );

  /* ---- device keyboard: suppressed while the kiln keyboard is in use ---- */
  const applyInputMode = React.useCallback(() => {
    const ta = kRef.current?.term.textarea;
    if (!ta) return;
    if (live.current.deviceKeyboard) ta.removeAttribute("inputmode");
    else ta.setAttribute("inputmode", "none");
    ta.setAttribute("autocapitalize", "off");
    ta.setAttribute("autocorrect", "off");
    ta.setAttribute("spellcheck", "false");
  }, []);

  /* ---- websocket: attach, and reattach whenever it drops ---- */
  const startConnection = React.useCallback(() => {
    const k = kRef.current;
    if (!k) return;
    connRef.current?.stop();
    const term = k.term;
    let stopped = false;
    let ended = false;
    let attempt = 0;
    let timer = 0;
    let ws: WebSocket | null = null;

    const schedule = (fast = false) => {
      if (stopped || ended) return;
      window.clearTimeout(timer);
      const delay = fast ? 150 : (BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length - 1)] ?? 4000);
      attempt += 1;
      live.current.onState(live.current.tabId, { kind: "reconnecting", attempt });
      timer = window.setTimeout(connect, delay);
    };

    function connect() {
      if (stopped || ended) return;
      const params = new URLSearchParams();
      const reattach = sessionRef.current !== null;
      if (sessionRef.current) params.set("session", sessionRef.current);
      else {
        const d = measure();
        params.set("target", target);
        params.set("cols", String(d?.cols ?? term.cols));
        params.set("rows", String(d?.rows ?? term.rows));
      }
      const sock = new WebSocket(wsUrl(`${live.current.endpoint ?? "/api/terminal/ws"}?${params}`));
      sock.binaryType = "arraybuffer";
      ws = sock;
      wsRef.current = sock;
      if (!reattach) live.current.onState(live.current.tabId, { kind: "connecting" });

      sock.onmessage = (ev) => {
        if (typeof ev.data !== "string") {
          term.write(new Uint8Array(ev.data as ArrayBuffer));
          return;
        }
        // Applied only when it reads as one of the socket's frames; else the server, which sent it, is told.
        const read = readFrame<TerminalControlFrame>(ev.data, "type", TERMINAL_FRAMES);
        if ("unread" in read) {
          if (sock.readyState === WebSocket.OPEN) sock.send(JSON.stringify(read.unread satisfies UnreadFrame));
          return;
        }
        const msg = read.frame;
        switch (msg.type) {
          case "refused":
            // What the server couldn't read of what this view sent: noted there, on the runner's page.
            return;
          case "session": {
            sessionRef.current = msg.session.id;
            live.current.onSession(live.current.tabId, msg.session);
            // The replay that follows repaints the session from scratch, at the session's size.
            if (reattach) term.reset();
            sessionSize.current = { cols: msg.session.cols, rows: msg.session.rows };
            follow();
            attempt = 0;
            live.current.onState(live.current.tabId, { kind: "open" });
            if (live.current.active) {
              // After the replay is drawn: the program then redraws for this view's size.
              requestAnimationFrame(() => claim(true, true));
              term.focus();
            }
            return;
          }
          case "size":
            // The device in use (maybe this one) set the session's size.
            sessionSize.current = { cols: msg.cols, rows: msg.rows };
            follow();
            return;
          case "missing":
            // Its machine is reconnecting (or the server just restarted): keep trying.
            schedule();
            sock.close();
            return;
          case "reconnect":
            schedule(true);
            sock.close();
            return;
          case "exit":
            ended = true;
            live.current.onState(live.current.tabId, { kind: "exited", code: msg.code ?? null });
            term.write(`\r\n\x1b[2m\x1b[38;2;151;153;159m[process exited${msg.code !== null && msg.code !== undefined ? ` with code ${msg.code}` : ""}]\x1b[0m\r\n`);
            return;
          case "error":
            ended = true;
            if (msg.message === "session closed") {
              // Ended on purpose (here or on another device): the tab goes everywhere.
              live.current.onClosed(live.current.tabId);
              return;
            }
            live.current.onState(live.current.tabId, { kind: "error", message: msg.message });
            term.write(`\r\n\x1b[38;2;209;170;173m[error] ${msg.message}\x1b[0m\r\n`);
            return;
        }
      };
      sock.onclose = () => {
        if (wsRef.current === sock) wsRef.current = null;
        if (stopped || ended) return;
        schedule();
      };
    }

    // Back to the page or the network: retry at once instead of waiting out the backoff, and
    // picking the device up again makes it the one in use.
    const kick = () => {
      if (!stopped && !ended && (!ws || ws.readyState === WebSocket.CLOSED)) {
        window.clearTimeout(timer);
        attempt = 0;
        connect();
      }
    };
    const onVisible = () => {
      if (document.visibilityState !== "visible") return;
      kick();
      if (live.current.active) claim(true, true);
    };
    window.addEventListener("online", kick);
    document.addEventListener("visibilitychange", onVisible);

    connect();
    connRef.current = {
      stop: () => {
        stopped = true;
        window.clearTimeout(timer);
        window.removeEventListener("online", kick);
        document.removeEventListener("visibilitychange", onVisible);
        if (wsRef.current === ws) wsRef.current = null;
        ws?.close();
      },
    };
  }, [measure, follow, claim, target]);

  /* ---- terminal instance: created with its element, gone with it ---- */
  const hostEl = React.useCallback((host: HTMLDivElement | null) => {
    if (!host) return;
    hostRef.current = host;
    let disposed = false;
    let k: KilnTerminal | null = null;
    const subs: { dispose(): void }[] = [];
    let titleTimer = 0;
    let pendingTitle: string | null = null;

    void ensureFont().then(() => {
      if (disposed) return;
      k = createTerminal(host, { fontSize: live.current.fontSize });
      kRef.current = k;
      live.current.onRenderer?.(k.renderer());
      const term = k.term;

      // Let the browser handle copy/paste chords; everything else goes to the PTY.
      // Copy and paste like a desktop terminal. The selection is xterm's own (not the page's), so
      // copying is done here, and works over plain HTTP too. Ctrl+C with nothing selected still
      // interrupts; Ctrl+V still reaches the program (vim's block select); Cmd+V and Ctrl+Shift+V
      // paste through the browser's paste event.
      term.attachCustomKeyEventHandler((e) => {
        if (e.type !== "keydown") return true;
        const key = e.key.toLowerCase();
        const mod = e.ctrlKey || e.metaKey;
        if (mod && key === "c" && (e.shiftKey || e.metaKey || term.hasSelection())) {
          const sel = term.getSelection();
          if (sel) void copyText(sel);
          if (sel || e.metaKey || e.shiftKey) {
            e.preventDefault();
            return false;
          }
        }
        if ((e.metaKey && key === "v") || (e.ctrlKey && e.shiftKey && key === "v")) return false;
        return true;
      });

      subs.push(
        term.onData((d) => sendBytes(d)),
        term.onBinary((d) => {
          const buf = new Uint8Array(d.length);
          for (let i = 0; i < d.length; i++) buf[i] = d.charCodeAt(i) & 0xff;
          sendBytes(buf);
        }),
        // Coalesced: at most one report per TITLE_EVERY_MS, with the latest title.
        term.onTitleChange((t) => {
          pendingTitle = t;
          if (titleTimer) return;
          titleTimer = window.setTimeout(() => {
            titleTimer = 0;
            if (pendingTitle !== null) live.current.onTitle(live.current.tabId, pendingTitle);
            pendingTitle = null;
          }, TITLE_EVERY_MS);
        }),
        term.onSelectionChange(() => {
          if (!live.current.copyOnSelect) return;
          const sel = term.getSelection();
          if (sel) void copyText(sel);
        }),
      );

      // A web font that lands after the first measurement (slow phone link) leaves cells sized for
      // the fallback font and glyphs overlapping: measure again once it arrives.
      if (!document.fonts.check(`13px "Geist Mono Variable"`)) {
        const remeasure = () => {
          const t = kRef.current?.term;
          if (!t) return;
          // A different but equivalent family string makes xterm re-measure its cells.
          t.options.fontFamily = t.options.fontFamily === TERM_FONT ? `${TERM_FONT}, monospace` : TERM_FONT;
          requestAnimationFrame(layout);
        };
        document.fonts.addEventListener("loadingdone", remeasure, { once: true });
        subs.push({ dispose: () => document.fonts.removeEventListener("loadingdone", remeasure) });
      }

      // The terminal exists: set up its keyboard, connect, and take the size if this view is in use.
      applyInputMode();
      startConnection();
      if (live.current.active)
        requestAnimationFrame(() => {
          claim(true, true);
          kRef.current?.term.focus();
        });
    });

    // Space changes (rotation, keyboard shown/resized, window resize): lay out once per frame.
    let raf = 0;
    const ro = new ResizeObserver(() => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(layout);
    });
    ro.observe(host);

    // Touching the terminal makes this the device in use.
    const onPointerDown = () => {
      if (live.current.active) claim(false, true);
    };
    host.addEventListener("pointerdown", onPointerDown, { passive: true });

    // Scrolling, by finger and by wheel, handled here in the capture phase (xterm's own handling
    // moves 3 rows per wheel notch however fast the wheel spins, and its touch gestures would
    // scroll a second time or turn a swipe into a tap). Shells scroll their scrollback; programs
    // that track the mouse get wheel input and full-screen programs get arrow keys, as xterm
    // sends them for a mouse wheel.
    let carry = 0;
    const rowHeight = () => {
      const t = kRef.current?.term;
      const screen = host.querySelector(".xterm-screen");
      return t && screen && t.rows > 0 ? Math.max(4, screen.clientHeight / t.rows) : 16;
    };
    /** The program gets wheel input (mouse tracking) or arrow keys (full screen) rather than a scroll. */
    const programScrolls = (t: KilnTerminal["term"]) => t.modes.mouseTrackingMode !== "none" || t.buffer.active.type === "alternate";
    const scrollRows = (n: number) => {
      const t = kRef.current?.term;
      if (!t || n === 0) return;
      if (programScrolls(t)) {
        host.querySelector(".xterm-screen")?.dispatchEvent(new WheelEvent("wheel", { deltaY: n, deltaMode: 1, bubbles: true, cancelable: true }));
      } else {
        t.scrollLines(n);
      }
    };
    const advance = (px: number) => {
      carry += px;
      const h = rowHeight();
      const rows = Math.trunc(carry / h);
      if (rows !== 0) {
        carry -= rows * h;
        scrollRows(rows);
      }
    };

    // Wheel: a trackpad scrolls with the fingers, pixel for pixel (its momentum comes from the
    // system); a mouse wheel moves 3 rows a notch when turned slowly and up to 15 when spun.
    let lastNotch = 0;
    const onWheel = (e: WheelEvent) => {
      const t = kRef.current?.term;
      if (!t || e.ctrlKey || e.deltaY === 0 || Math.abs(e.deltaX) > Math.abs(e.deltaY) || programScrolls(t)) return;
      e.preventDefault();
      e.stopPropagation();
      const legacy = (e as WheelEvent & { wheelDeltaY?: number }).wheelDeltaY ?? 0;
      const notch = e.deltaMode === WheelEvent.DOM_DELTA_LINE || (legacy !== 0 && legacy % 120 === 0 && Math.abs(e.deltaY) >= 50);
      if (e.deltaMode === WheelEvent.DOM_DELTA_PAGE) {
        t.scrollLines(Math.sign(e.deltaY) * t.rows);
      } else if (notch) {
        const now = performance.now();
        const gap = now - lastNotch;
        lastNotch = now;
        const perNotch = gap > 160 ? 3 : gap > 90 ? 5 : gap > 45 ? 9 : 15;
        const notches = e.deltaMode === WheelEvent.DOM_DELTA_LINE ? Math.max(1, Math.round(Math.abs(e.deltaY) / 3)) : Math.max(1, Math.round(Math.abs(legacy) / 120));
        t.scrollLines(Math.sign(e.deltaY) * perNotch * notches);
      } else {
        advance(e.deltaY);
      }
    };
    host.addEventListener("wheel", onWheel, { passive: false, capture: true });

    // Touch: the content follows the finger row for row, then glides on with momentum (iOS-like
    // deceleration); flicking again while it glides adds to the speed.
    let startY = 0;
    let lastY: number | null = null;
    let lastT = 0;
    let scrolling = false;
    let velocity = 0;
    let glide = 0;
    let glideV = 0;
    let boost = 0;
    const stopGlide = () => {
      cancelAnimationFrame(glide);
      glide = 0;
    };
    const onTouchStart = (e: TouchEvent) => {
      // Catching a glide stops it; its speed carries into a flick the same way.
      boost = glide ? glideV : 0;
      stopGlide();
      scrolling = false;
      const t = e.touches[0];
      if (e.touches.length !== 1 || !t) {
        lastY = null;
        return;
      }
      startY = lastY = t.clientY;
      lastT = performance.now();
      carry = 0;
      velocity = 0;
    };
    const onTouchMove = (e: TouchEvent) => {
      const t = e.touches[0];
      if (lastY === null || !t || e.touches.length !== 1) return;
      if (!scrolling && Math.abs(startY - t.clientY) < TAP_SLOP_PX) return;
      if (!scrolling) {
        // Reading, not typing: put the keyboards away, as a phone does when a list is dragged.
        kRef.current?.term.blur();
        live.current.onGesture?.(live.current.tabId, "scroll");
      }
      scrolling = true;
      e.preventDefault();
      e.stopPropagation();
      const dy = lastY - t.clientY;
      const now = performance.now();
      velocity = 0.8 * (dy / Math.max(1, now - lastT)) + 0.2 * velocity;
      lastY = t.clientY;
      lastT = now;
      advance(dy);
    };
    const onTouchEnd = (e: TouchEvent) => {
      if (scrolling) {
        e.stopPropagation();
        // A finger that stopped before lifting doesn't glide.
        if (performance.now() - lastT < 80 && Math.abs(velocity) > 0.2) {
          let v = Math.sign(boost) === Math.sign(velocity) ? velocity + boost : velocity; // px per ms
          let prev = performance.now();
          const step = (now: number) => {
            const dt = Math.min(48, now - prev);
            prev = now;
            advance(v * dt);
            v *= Math.pow(0.998, dt);
            glideV = v;
            glide = Math.abs(v) > 0.02 ? requestAnimationFrame(step) : 0;
          };
          glide = requestAnimationFrame(step);
        }
      } else if (lastY !== null && e.type === "touchend") {
        live.current.onGesture?.(live.current.tabId, "tap");
      }
      scrolling = false;
      lastY = null;
    };
    host.addEventListener("touchstart", onTouchStart, { passive: true, capture: true });
    host.addEventListener("touchmove", onTouchMove, { passive: false, capture: true });
    host.addEventListener("touchend", onTouchEnd, { capture: true });
    host.addEventListener("touchcancel", onTouchEnd, { capture: true });

    return () => {
      disposed = true;
      cancelAnimationFrame(raf);
      stopGlide();
      window.clearTimeout(titleTimer);
      window.clearTimeout(sendTimer.current);
      ro.disconnect();
      host.removeEventListener("pointerdown", onPointerDown);
      host.removeEventListener("wheel", onWheel, { capture: true });
      host.removeEventListener("touchstart", onTouchStart, { capture: true });
      host.removeEventListener("touchmove", onTouchMove, { capture: true });
      host.removeEventListener("touchend", onTouchEnd, { capture: true });
      host.removeEventListener("touchcancel", onTouchEnd, { capture: true });
      subs.forEach((s) => s.dispose());
      connRef.current?.stop();
      connRef.current = null;
      wsRef.current?.close();
      wsRef.current = null;
      k?.dispose();
      kRef.current = null;
      hostRef.current = null;
    };
    // Created once; everything it reads changes through refs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /* ---- what the props say, applied to the terminal there is (each as it changes) ---- */
  // The device keyboard: suppressed while the kiln keyboard is in use.
  const keyboardSync = React.useCallback(
    (el: HTMLSpanElement | null) => {
      if (el) applyInputMode();
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [deviceKeyboard, applyInputMode],
  );
  // Becoming the view in use: claim the size, take focus.
  const activeSync = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el || !active || !kRef.current) return;
      const id = requestAnimationFrame(() => {
        claim(true, true);
        kRef.current?.term.focus();
      });
      return () => cancelAnimationFrame(id);
    },
    [active, claim],
  );
  // Another text size: lay the view out again.
  const fontSync = React.useCallback(
    (el: HTMLSpanElement | null) => {
      if (!el || !kRef.current) return;
      const id = requestAnimationFrame(layout);
      return () => cancelAnimationFrame(id);
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [fontSize, layout],
  );

  React.useImperativeHandle(
    ref,
    () => ({
      reconnect: () => {
        // A fresh session on the same target (the old one ended).
        const k = kRef.current;
        if (k) {
          k.term.reset();
          k.term.clear();
        }
        sessionRef.current = null;
        sessionSize.current = null;
        connRef.current?.stop();
        wsRef.current?.close();
        wsRef.current = null;
        startConnection();
      },
      focus: () => kRef.current?.term.focus(),
      clear: () => kRef.current?.term.clear(),
      copySelection: async () => {
        const s = kRef.current?.term.getSelection();
        return s ? copyText(s) : false;
      },
      paste: async () => {
        const t = await readClipboard();
        if (t === null) return false;
        if (t) {
          kRef.current?.term.scrollToBottom();
          kRef.current?.term.paste(t);
        }
        return true;
      },
      pasteText: (text: string) => {
        kRef.current?.term.scrollToBottom();
        kRef.current?.term.paste(text);
      },
      text: () => {
        const b = kRef.current?.term.buffer.active;
        if (!b) return "";
        const lines: string[] = [];
        for (let i = 0; i < b.length; i++) {
          const line = b.getLine(i);
          if (!line) continue;
          // Wrapped rows join the row before them: one logical line.
          if (line.isWrapped && lines.length) lines[lines.length - 1] += line.translateToString(true);
          else lines.push(line.translateToString(true));
        }
        while (lines.length && !lines[lines.length - 1]?.trim()) lines.pop();
        return lines.join("\n");
      },
      send: (bytes: string) => {
        kRef.current?.term.scrollToBottom();
        sendBytes(bytes);
      },
      appCursor: () => kRef.current?.term.modes.applicationCursorKeysMode ?? false,
    }),
    [sendBytes, startConnection],
  );

  // Hidden views are display:none, which also pauses xterm's rendering for them.
  return (
    <div ref={activeSync} className="term-host" data-terminal style={active ? undefined : { display: "none" }} aria-hidden={!active}>
      <div ref={hostEl} className="h-full w-full" />
      <span ref={keyboardSync} hidden />
      <span ref={fontSync} hidden />
    </div>
  );
});

/** Re-renders only when its own props change: callbacks are stable and titles don't flow back in. */
export const TerminalSession = React.memo(TerminalSessionView);
