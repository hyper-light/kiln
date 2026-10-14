import * as React from "react";
import { wsUrl } from "@/api/client";
import type { TerminalControlFrame } from "@/api/types";
import { createTerminal, ensureFont, safeFit, type KilnTerminal } from "@/components/terminal/xterm-setup";

export type SessionState =
  | { kind: "connecting" }
  | { kind: "open" }
  | { kind: "exited"; code: number | null }
  | { kind: "error"; message: string };

export interface SessionHandle {
  reconnect: () => void;
  focus: () => void;
  clear: () => void;
  copySelection: () => Promise<void>;
  setFontSize: (px: number) => void;
}

interface Props {
  target: string;
  active: boolean;
  /** workspace visible (route) */
  visible: boolean;
  fontSize: number;
  copyOnSelect: boolean;
  onState: (s: SessionState) => void;
  onTitle: (title: string) => void;
  onRenderer?: (r: "webgl" | "dom") => void;
}

const encoder = new TextEncoder();

/**
 * One PTY session: xterm.js bound to /api/terminal/ws.
 * Binary frames carry keystrokes/output; text frames carry resize/exit/error JSON.
 */
export const TerminalSession = React.forwardRef<SessionHandle, Props>(function TerminalSession(
  { target, active, visible, fontSize, copyOnSelect, onState, onTitle, onRenderer },
  ref,
) {
  const hostRef = React.useRef<HTMLDivElement>(null);
  const kRef = React.useRef<KilnTerminal | null>(null);
  const wsRef = React.useRef<WebSocket | null>(null);
  const [gen, setGen] = React.useState(0);
  const cbs = React.useRef({ onState, onTitle, onRenderer, copyOnSelect });
  cbs.current = { onState, onTitle, onRenderer, copyOnSelect };

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
        term.onData((d) => {
          const ws = wsRef.current;
          if (ws?.readyState === WebSocket.OPEN) ws.send(encoder.encode(d));
        }),
        term.onBinary((d) => {
          const ws = wsRef.current;
          if (ws?.readyState !== WebSocket.OPEN) return;
          const buf = new Uint8Array(d.length);
          for (let i = 0; i < d.length; i++) buf[i] = d.charCodeAt(i) & 0xff;
          ws.send(buf);
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

    return () => {
      disposed = true;
      cancelAnimationFrame(raf);
      ro.disconnect();
      subs.forEach((s) => s.dispose());
      wsRef.current?.close();
      wsRef.current = null;
      k?.dispose();
      kRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /* ---- websocket: (re)created per generation ---- */
  React.useEffect(() => {
    const k = kRef.current;
    const host = hostRef.current;
    if (!k || !host || gen === 0) return;
    const term = k.term;
    safeFit(k, host);
    const cols = Math.max(2, term.cols);
    const rows = Math.max(1, term.rows);
    const params = new URLSearchParams({ target, cols: String(cols), rows: String(rows) });
    const ws = new WebSocket(wsUrl(`/api/terminal/ws?${params}`));
    ws.binaryType = "arraybuffer";
    wsRef.current = ws;
    cbs.current.onState({ kind: "connecting" });
    let ended = false;

    ws.onopen = () => {
      cbs.current.onState({ kind: "open" });
      // Re-assert the size in case layout moved between URL creation and open.
      ws.send(JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }));
      if (active) term.focus();
    };
    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        let msg: TerminalControlFrame;
        try {
          msg = JSON.parse(ev.data) as TerminalControlFrame;
        } catch {
          term.write(ev.data);
          return;
        }
        if (msg.type === "exit") {
          ended = true;
          cbs.current.onState({ kind: "exited", code: msg.code ?? null });
          term.write(`\r\n\x1b[2m\x1b[38;2;151;153;159m[process exited${msg.code !== null && msg.code !== undefined ? ` with code ${msg.code}` : ""}]\x1b[0m\r\n`);
        } else if (msg.type === "error") {
          ended = true;
          cbs.current.onState({ kind: "error", message: msg.message });
          term.write(`\r\n\x1b[38;2;209;170;173m[error] ${msg.message}\x1b[0m\r\n`);
        }
        return;
      }
      term.write(new Uint8Array(ev.data as ArrayBuffer));
    };
    ws.onclose = (ev) => {
      if (wsRef.current !== ws) return;
      wsRef.current = null;
      if (!ended) {
        cbs.current.onState(
          ev.code === 1000 ? { kind: "exited", code: null } : { kind: "error", message: ev.reason || `connection closed (${ev.code})` },
        );
        term.write(`\r\n\x1b[2m\x1b[38;2;151;153;159m[disconnected]\x1b[0m\r\n`);
      }
    };
    return () => {
      ended = true;
      if (wsRef.current === ws) wsRef.current = null;
      ws.close();
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
        const k = kRef.current;
        if (k) {
          k.term.reset();
          k.term.clear();
        }
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
    }),
    [],
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
