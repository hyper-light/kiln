import * as React from "react";
import { wsUrl } from "@/api/client";
import type { TerminalControlFrame, TerminalSessionInfo } from "@/api/types";
import { createTerminal, ensureFont, safeFit, type KilnTerminal } from "@/components/terminal/xterm-setup";

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
  copySelection: () => Promise<void>;
  setFontSize: (px: number) => void;
  /** Bytes from the on-screen keyboard, delivered exactly as if typed. */
  send: (bytes: string) => void;
  /** Application-cursor mode (DECCKM), which changes how arrows are encoded. */
  appCursor: () => boolean;
}

interface Props {
  /** Attach to this server-side session; null opens a new one on `target`. */
  sessionId: string | null;
  target: string;
  active: boolean;
  /** workspace visible (route) */
  visible: boolean;
  fontSize: number;
  copyOnSelect: boolean;
  /** The on-screen keyboard is in use: keep the device keyboard closed on tap. */
  suppressDeviceKeyboard: boolean;
  onState: (s: SessionState) => void;
  onTitle: (title: string) => void;
  /** The server assigned or confirmed this view's session. */
  onSession: (info: TerminalSessionInfo) => void;
  onRenderer?: (r: "webgl" | "dom") => void;
}

const encoder = new TextEncoder();
/** Reattach backoff: quick first tries (a server restart takes a few seconds), then every 5 s. */
const backoff = (attempt: number) => Math.min(5000, 300 * 2 ** Math.min(attempt, 5));

/**
 * One view of a server-side terminal session: xterm.js bound to /api/terminal/ws. The shell lives
 * on the server or a machine, not in this tab: closing the socket only detaches, and a dropped
 * connection reattaches by session id, the server replaying the screen so nothing is lost.
 * Binary frames carry keystrokes/output; text frames carry identity, resize, exit and errors.
 */
export const TerminalSession = React.forwardRef<SessionHandle, Props>(function TerminalSession(
  { sessionId, target, active, visible, fontSize, copyOnSelect, suppressDeviceKeyboard, onState, onTitle, onSession, onRenderer },
  ref,
) {
  const hostRef = React.useRef<HTMLDivElement>(null);
  const kRef = React.useRef<KilnTerminal | null>(null);
  const wsRef = React.useRef<WebSocket | null>(null);
  const sessionRef = React.useRef<string | null>(sessionId);
  const [gen, setGen] = React.useState(0);
  const cbs = React.useRef({ onState, onTitle, onSession, onRenderer, copyOnSelect, active });
  cbs.current = { onState, onTitle, onSession, onRenderer, copyOnSelect, active };

  const sendBytes = React.useCallback((data: string | Uint8Array) => {
    const ws = wsRef.current;
    if (ws?.readyState === WebSocket.OPEN) ws.send(typeof data === "string" ? encoder.encode(data) : data);
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

    // Touch scrolling: a vertical swipe becomes wheel input for xterm, which does the right thing
    // in every mode (scrollback in a shell, wheel reports to mouse-aware programs, arrow keys in a
    // full-screen program's alternate screen). A tap stays a tap (focus / click).
    let touchY: number | null = null;
    let moved = 0;
    const onTouchStart = (e: TouchEvent) => {
      if (e.touches.length !== 1) return;
      touchY = e.touches[0]?.clientY ?? null;
      moved = 0;
    };
    const onTouchMove = (e: TouchEvent) => {
      const t = e.touches[0];
      if (touchY === null || !t || e.touches.length !== 1) return;
      const dy = touchY - t.clientY;
      moved += Math.abs(dy);
      if (moved < 8) return;
      touchY = t.clientY;
      e.preventDefault();
      const target = host.querySelector(".xterm-screen") ?? host;
      target.dispatchEvent(
        new WheelEvent("wheel", { deltaY: dy * 2, deltaMode: 0, clientX: t.clientX, clientY: t.clientY, bubbles: true, cancelable: true }),
      );
    };
    const onTouchEnd = () => {
      touchY = null;
    };
    host.addEventListener("touchstart", onTouchStart, { passive: true });
    host.addEventListener("touchmove", onTouchMove, { passive: false });
    host.addEventListener("touchend", onTouchEnd, { passive: true });

    return () => {
      disposed = true;
      cancelAnimationFrame(raf);
      ro.disconnect();
      host.removeEventListener("touchstart", onTouchStart);
      host.removeEventListener("touchmove", onTouchMove);
      host.removeEventListener("touchend", onTouchEnd);
      subs.forEach((s) => s.dispose());
      wsRef.current?.close();
      wsRef.current = null;
      k?.dispose();
      kRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /* ---- device keyboard: closed while the kiln keyboard is in use ---- */
  React.useEffect(() => {
    const ta = kRef.current?.term.textarea;
    if (!ta) return;
    ta.setAttribute("inputmode", suppressDeviceKeyboard ? "none" : "text");
    ta.setAttribute("autocapitalize", "off");
    ta.setAttribute("autocorrect", "off");
    ta.setAttribute("spellcheck", "false");
  }, [suppressDeviceKeyboard, gen]);

  /* ---- websocket: attach (or open), and reattach whenever the connection drops ---- */
  React.useEffect(() => {
    const kt = kRef.current;
    const hostEl = hostRef.current;
    if (!kt || !hostEl || gen === 0) return;
    const term = kt.term;
    let ws: WebSocket | null = null;
    let stopped = false;
    let ended = false;
    let attempt = 0;
    let timer = 0;

    const schedule = () => {
      if (stopped || ended) return;
      window.clearTimeout(timer);
      cbs.current.onState({ kind: "reconnecting", attempt });
      timer = window.setTimeout(connect, backoff(attempt));
      attempt += 1;
    };

    function connect() {
      if (stopped || ended) return;
      window.clearTimeout(timer);
      safeFit(kt, hostEl);
      const cols = Math.max(2, term.cols);
      const rows = Math.max(1, term.rows);
      const id = sessionRef.current;
      const params = id ? new URLSearchParams({ session: id }) : new URLSearchParams({ target, cols: String(cols), rows: String(rows) });
      const sock = new WebSocket(wsUrl(`/api/terminal/ws?${params}`));
      sock.binaryType = "arraybuffer";
      ws = sock;
      wsRef.current = sock;
      if (attempt === 0) cbs.current.onState({ kind: "connecting" });

      sock.onopen = () => {
        sock.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
      };
      sock.onmessage = (ev) => {
        if (typeof ev.data !== "string") {
          term.write(new Uint8Array(ev.data as ArrayBuffer));
          return;
        }
        let msg: TerminalControlFrame;
        try {
          msg = JSON.parse(ev.data) as TerminalControlFrame;
        } catch {
          term.write(ev.data);
          return;
        }
        switch (msg.type) {
          case "session": {
            // The replay that follows rebuilds the screen from scratch.
            if (sessionRef.current) term.reset();
            sessionRef.current = msg.session.id;
            attempt = 0;
            cbs.current.onSession(msg.session);
            cbs.current.onState({ kind: "open" });
            if (cbs.current.active) term.focus();
            break;
          }
          case "missing":
          case "reconnect":
            // Its machine is reconnecting (or the server just restarted): try again shortly.
            sock.close();
            break;
          case "exit":
            ended = true;
            cbs.current.onState({ kind: "exited", code: msg.code ?? null });
            term.write(`\r\n\x1b[2m\x1b[38;2;151;153;159m[process exited${msg.code !== null && msg.code !== undefined ? ` with code ${msg.code}` : ""}]\x1b[0m\r\n`);
            break;
          case "error":
            ended = true;
            cbs.current.onState({ kind: "error", message: msg.message });
            term.write(`\r\n\x1b[38;2;209;170;173m[error] ${msg.message}\x1b[0m\r\n`);
            break;
        }
      };
      sock.onclose = () => {
        if (wsRef.current === sock) wsRef.current = null;
        if (!stopped && !ended) schedule();
      };
    }

    // Back from the background / back online: retry at once instead of waiting out the backoff.
    const retryNow = () => {
      if (document.visibilityState === "visible" && !ended && !stopped && (!ws || ws.readyState === WebSocket.CLOSED)) {
        attempt = 0;
        connect();
      }
    };
    document.addEventListener("visibilitychange", retryNow);
    window.addEventListener("online", retryNow);

    connect();
    return () => {
      stopped = true;
      window.clearTimeout(timer);
      document.removeEventListener("visibilitychange", retryNow);
      window.removeEventListener("online", retryNow);
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
      if (!suppressDeviceKeyboard) k.term.focus();
    });
    return () => cancelAnimationFrame(id);
  }, [active, visible, gen, suppressDeviceKeyboard]);

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
        // After an exit or error: a fresh session on the same target.
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
        const term = kRef.current?.term;
        if (!term) return;
        term.scrollToBottom();
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
