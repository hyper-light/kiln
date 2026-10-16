import * as React from "react";
import { wsUrl } from "@/api/client";
import type { TerminalControlFrame, TerminalSessionInfo } from "@/api/types";
import { createTerminal, ensureFont, safeFit, type KilnTerminal } from "@/components/terminal/xterm-setup";

export type SessionState =
  | { kind: "connecting" }
  | { kind: "open" }
  /** The socket dropped (server restart, network, sleep); the shell keeps running and we reattach. */
  | { kind: "reconnecting"; attempt: number }
  | { kind: "exited"; code: number | null }
  | { kind: "error"; message: string };

export interface SessionHandle {
  reconnect: () => void;
  focus: () => void;
  clear: () => void;
  copySelection: () => Promise<void>;
  setFontSize: (px: number) => void;
  /** Send bytes to the shell as if typed (on-screen keyboard). */
  send: (bytes: string) => void;
  /** The program's application-cursor mode (arrows encode differently in vim, less, TUIs). */
  appCursor: () => boolean;
}

interface Props {
  /** Server session to attach to; null opens a new session on `target`. */
  sessionId: string | null;
  target: string;
  active: boolean;
  /** workspace visible (route) */
  visible: boolean;
  fontSize: number;
  copyOnSelect: boolean;
  /** Whether tapping the terminal may open the device keyboard (false while the full kiln keyboard is in use). */
  deviceKeyboard: boolean;
  onState: (s: SessionState) => void;
  onTitle: (title: string) => void;
  /** The server confirmed which session this view is on (first frame of every attach). */
  onSession: (info: TerminalSessionInfo) => void;
  onRenderer?: (r: "webgl" | "dom") => void;
}

const encoder = new TextEncoder();
/** Reattach delays: quick at first (server restarts take ~1-2 s), then patient. */
const BACKOFF_MS = [250, 500, 1000, 1500, 2500, 4000];

/**
 * One terminal view: xterm.js attached to a server-side session over /api/terminal/ws. The shell
 * lives on the server or a machine, not in this page: when the socket drops the view keeps
 * reattaching by session id and repaints from the server's replay, so a restart or a flaky network
 * costs a moment of "reconnecting", never the shell.
 */
export const TerminalSession = React.forwardRef<SessionHandle, Props>(function TerminalSession(
  { sessionId, target, active, visible, fontSize, copyOnSelect, deviceKeyboard, onState, onTitle, onSession, onRenderer },
  ref,
) {
  const hostRef = React.useRef<HTMLDivElement>(null);
  const kRef = React.useRef<KilnTerminal | null>(null);
  const wsRef = React.useRef<WebSocket | null>(null);
  const sessionRef = React.useRef<string | null>(sessionId);
  const [gen, setGen] = React.useState(0);
  const cbs = React.useRef({ onState, onTitle, onSession, onRenderer, copyOnSelect });
  cbs.current = { onState, onTitle, onSession, onRenderer, copyOnSelect };

  const sendBytes = React.useCallback((data: string | Uint8Array<ArrayBuffer>) => {
    const ws = wsRef.current;
    if (ws?.readyState !== WebSocket.OPEN) return;
    ws.send(typeof data === "string" ? encoder.encode(data) : data);
  }, []);

  /* ---- terminal instance: created once per component ---- */
  React.useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    let disposed = false;
    let k: KilnTerminal | null = null;
    const subs: { dispose(): void }[] = [];

    void ensureFont().then(() => {
      if (disposed) return;
      k = createTerminal(host, { fontSize });
      kRef.current = k;
      cbs.current.onRenderer?.(k.renderer());
      const term = k.term;

      // Let the browser handle copy/paste chords; everything else goes to the PTY.
      term.attachCustomKeyEventHandler((e) => {
        if (e.type !== "keydown") return true;
        const mod = e.ctrlKey || e.metaKey;
        if (mod && e.shiftKey && (e.key === "C" || e.key === "c")) {
          const sel = term.getSelection();
          if (sel) void navigator.clipboard?.writeText(sel).catch(() => undefined);
          e.preventDefault();
          return false;
        }
        if (mod && e.shiftKey && (e.key === "V" || e.key === "v")) return false; // native paste event
        if (e.metaKey && (e.key === "c" || e.key === "v") && term.hasSelection()) return false;
        return true;
      });

      subs.push(
        term.onData((d) => sendBytes(d)),
        term.onBinary((d) => {
          const buf = new Uint8Array(d.length);
          for (let i = 0; i < d.length; i++) buf[i] = d.charCodeAt(i) & 0xff;
          sendBytes(buf);
        }),
        term.onResize(({ cols, rows }) => {
          const ws = wsRef.current;
          if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: "resize", cols, rows }));
        }),
        term.onTitleChange((t) => cbs.current.onTitle(t)),
        term.onSelectionChange(() => {
          if (!cbs.current.copyOnSelect) return;
          const sel = term.getSelection();
          if (sel) void navigator.clipboard?.writeText(sel).catch(() => undefined);
        }),
      );

      safeFit(k, host);
      setGen((g) => g + 1); // kick the connection effect now that the terminal exists
    });

    let raf = 0;
    const ro = new ResizeObserver(() => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => {
        if (kRef.current) safeFit(kRef.current, host);
      });
    });
    ro.observe(host);

    // Touch: a one-finger drag scrolls. It is replayed as wheel events on xterm, which then does
    // what a wheel does there: scrollback in a shell, wheel reports to programs that track the
    // mouse, arrow keys in full-screen programs that don't. Short touches stay taps.
    let lastY: number | null = null;
    let moved = 0;
    const onTouchStart = (e: TouchEvent) => {
      if (e.touches.length !== 1) return;
      lastY = e.touches[0]?.clientY ?? null;
      moved = 0;
    };
    const onTouchMove = (e: TouchEvent) => {
      const t = e.touches[0];
      if (lastY === null || !t || e.touches.length !== 1) return;
      const dy = lastY - t.clientY;
      moved += Math.abs(dy);
      if (moved < 8) return;
      lastY = t.clientY;
      e.preventDefault();
      const screen = host.querySelector(".xterm-screen");
      screen?.dispatchEvent(
        new WheelEvent("wheel", { deltaY: dy * 2, deltaMode: 0, clientX: t.clientX, clientY: t.clientY, bubbles: true, cancelable: true }),
      );
    };
    const onTouchEnd = () => {
      lastY = null;
    };
    host.addEventListener("touchstart", onTouchStart, { passive: true });
    host.addEventListener("touchmove", onTouchMove, { passive: false });
    host.addEventListener("touchend", onTouchEnd, { passive: true });
    host.addEventListener("touchcancel", onTouchEnd, { passive: true });

    return () => {
      disposed = true;
      cancelAnimationFrame(raf);
      ro.disconnect();
      host.removeEventListener("touchstart", onTouchStart);
      host.removeEventListener("touchmove", onTouchMove);
      host.removeEventListener("touchend", onTouchEnd);
      host.removeEventListener("touchcancel", onTouchEnd);
      subs.forEach((s) => s.dispose());
      wsRef.current?.close();
      wsRef.current = null;
      k?.dispose();
      kRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /* ---- device keyboard: suppressed while the kiln keyboard is in use ---- */
  React.useEffect(() => {
    const ta = kRef.current?.term.textarea;
    if (!ta) return;
    if (deviceKeyboard) ta.removeAttribute("inputmode");
    else ta.setAttribute("inputmode", "none");
  }, [deviceKeyboard, gen]);

  /* ---- websocket: attach, and reattach whenever it drops ---- */
  React.useEffect(() => {
    const k = kRef.current;
    const host = hostRef.current;
    if (!k || !host || gen === 0) return;
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
      cbs.current.onState({ kind: "reconnecting", attempt });
      timer = window.setTimeout(connect, delay);
    };

    function connect() {
      if (stopped || ended) return;
      safeFit(k!, host!);
      const params = new URLSearchParams();
      const reattach = sessionRef.current !== null;
      if (sessionRef.current) params.set("session", sessionRef.current);
      else {
        params.set("target", target);
        params.set("cols", String(Math.max(2, term.cols)));
        params.set("rows", String(Math.max(1, term.rows)));
      }
      const sock = new WebSocket(wsUrl(`/api/terminal/ws?${params}`));
      sock.binaryType = "arraybuffer";
      ws = sock;
      wsRef.current = sock;
      if (!reattach) cbs.current.onState({ kind: "connecting" });

      sock.onmessage = (ev) => {
        if (typeof ev.data === "string") {
          let msg: TerminalControlFrame;
          try {
            msg = JSON.parse(ev.data) as TerminalControlFrame;
          } catch {
            term.write(ev.data);
            return;
          }
          switch (msg.type) {
            case "session": {
              sessionRef.current = msg.session.id;
              cbs.current.onSession(msg.session);
              if (msg.session.title) cbs.current.onTitle(msg.session.title);
              // The replay that follows repaints the whole session: start from a clean terminal.
              if (reattach) term.reset();
              attempt = 0;
              cbs.current.onState({ kind: "open" });
              // Our size wins on attach (the replay was rendered for the session's last size).
              sock.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
              if (active) term.focus();
              return;
            }
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
              cbs.current.onState({ kind: "exited", code: msg.code ?? null });
              term.write(`\r\n\x1b[2m\x1b[38;2;151;153;159m[process exited${msg.code !== null && msg.code !== undefined ? ` with code ${msg.code}` : ""}]\x1b[0m\r\n`);
              return;
            case "error":
              ended = true;
              cbs.current.onState({ kind: "error", message: msg.message });
              term.write(`\r\n\x1b[38;2;209;170;173m[error] ${msg.message}\x1b[0m\r\n`);
              return;
          }
          return;
        }
        term.write(new Uint8Array(ev.data as ArrayBuffer));
      };
      sock.onclose = () => {
        if (wsRef.current === sock) wsRef.current = null;
        if (stopped || ended) return;
        schedule();
      };
    }

    // Coming back to the page or the network: retry at once instead of waiting out the backoff.
    const kick = () => {
      if (!stopped && !ended && (!ws || ws.readyState === WebSocket.CLOSED)) {
        window.clearTimeout(timer);
        attempt = 0;
        connect();
      }
    };
    const onVisible = () => {
      if (document.visibilityState === "visible") kick();
    };
    window.addEventListener("online", kick);
    document.addEventListener("visibilitychange", onVisible);

    connect();
    return () => {
      stopped = true;
      window.clearTimeout(timer);
      window.removeEventListener("online", kick);
      document.removeEventListener("visibilitychange", onVisible);
      if (wsRef.current === ws) wsRef.current = null;
      ws?.close();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [gen, target]);

  /* ---- visibility/focus: refit when shown ---- */
  React.useEffect(() => {
    if (!active || !visible) return;
    const k = kRef.current;
    const host = hostRef.current;
    if (!k || !host) return;
    const id = requestAnimationFrame(() => {
      safeFit(k, host);
      k.term.focus();
    });
    return () => cancelAnimationFrame(id);
  }, [active, visible, gen]);

  React.useEffect(() => {
    const k = kRef.current;
    const host = hostRef.current;
    if (!k || !host) return;
    k.term.options.fontSize = fontSize;
    requestAnimationFrame(() => safeFit(k, host));
  }, [fontSize]);

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
        wsRef.current?.close();
        wsRef.current = null;
        setGen((g) => g + 1);
      },
      focus: () => kRef.current?.term.focus(),
      clear: () => kRef.current?.term.clear(),
      copySelection: async () => {
        const s = kRef.current?.term.getSelection();
        if (s) await navigator.clipboard.writeText(s);
      },
      setFontSize: (px: number) => {
        if (kRef.current) kRef.current.term.options.fontSize = px;
      },
      send: (bytes: string) => {
        kRef.current?.term.scrollToBottom();
        sendBytes(bytes);
      },
      appCursor: () => kRef.current?.term.modes.applicationCursorKeysMode ?? false,
    }),
    [sendBytes],
  );

  return (
    <div
      className="term-host"
      data-terminal
      style={{ visibility: active ? "visible" : "hidden", zIndex: active ? 1 : 0 }}
      aria-hidden={!active}
    >
      <div ref={hostRef} className="h-full w-full" />
    </div>
  );
});
