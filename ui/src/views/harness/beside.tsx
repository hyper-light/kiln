/**
 * Terminals beside an agent, in its folder on its machine, under the conversation: its shell
 * (/terminal, or `!command` in the composer) for what the agent can't or won't run, and its
 * harness's own screen for what only that screen does (/permissions, /doctor, /screen …). kiln
 * keeps them, so they are here again after a reload or on another device. When the harness's own
 * screen closes, the agent starts again once it's idle, to load what was changed there.
 *
 * In the page's flow, never over it: one terminal shown, a select to switch when there are more,
 * what's on its screen added to the message, and on touch screens the keys a phone lacks.
 */
import * as React from "react";
import { CornerDownLeft, X } from "lucide-react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo, type BesideTerminal } from "@/api/harness";
import { charKey, namedKey, type NamedKey } from "@/components/terminal/keys";
import { SelectField } from "@/components/ui/misc";
import { errorMessage, useLocalStorage, useMediaQuery } from "@/lib/hooks";
import { TerminalSession, type SessionHandle, type SessionState } from "@/views/terminal/session";
import { machineName } from "./parts";
import "./beside.css";

/** A terminal asked for (a new `seq` each time). */
export interface BesideRequest {
  what: "shell" | "screen";
  command?: string;
  seq: number;
}

/** Keys a phone's keyboard lacks, for shells and full-screen programs. */
const KEYS: { label: string; aria: string; key?: NamedKey; ctrl?: string; shift?: boolean }[] = [
  { label: "esc", aria: "Escape", key: "Escape" },
  { label: "tab", aria: "Tab", key: "Tab" },
  { label: "⇧tab", aria: "Shift Tab", key: "Tab", shift: true },
  { label: "↑", aria: "Up", key: "ArrowUp" },
  { label: "↓", aria: "Down", key: "ArrowDown" },
  { label: "←", aria: "Left", key: "ArrowLeft" },
  { label: "→", aria: "Right", key: "ArrowRight" },
  { label: "^C", aria: "Control C", ctrl: "c" },
  { label: "⏎", aria: "Enter", key: "Enter" },
];

/** How many lines of what's on screen go into the message. */
const HAND_LINES = 60;

export function BesideTerminals({
  agent,
  info,
  request,
  onTaken,
  onInsert,
}: {
  agent: string;
  info: AgentInfo | null;
  /** What was asked for; `onTaken` once it's on its way (so it's asked for once). */
  request: BesideRequest | null;
  onTaken: () => void;
  onInsert: (text: string) => void;
}) {
  const [list, setList] = React.useState<BesideTerminal[]>([]);
  const [current, setCurrent] = React.useState<string | null>(null);
  const [states, setStates] = React.useState<Record<string, SessionState>>({});
  const [error, setError] = React.useState<string | null>(null);
  const [said, setSaid] = React.useState<string | null>(null);
  const handle = React.useRef<SessionHandle | null>(null);
  const root = React.useRef<HTMLDivElement>(null);
  const [fontSize] = useLocalStorage("kiln.term.fontSize", 13);
  const [copyOnSelect] = useLocalStorage("kiln.term.copyOnSelect", false);
  const touch = useMediaQuery("(pointer: coarse)");
  const harness = info?.kind_name ?? "The harness";

  // The ones already open beside it (here earlier, or on another device).
  React.useEffect(() => {
    let live = true;
    harnessApi.terminals(agent).then(
      (l) => {
        if (!live) return;
        const open = l.filter((b) => !b.terminal.exited);
        setList(open);
        setCurrent((c) => c ?? open[0]?.terminal.id ?? null);
      },
      () => undefined,
    );
    return () => {
      live = false;
    };
  }, [agent]);

  // What was asked for: its shell (again, with a command run in it), or its own screen.
  const seq = request?.seq;
  React.useEffect(() => {
    if (!request) return;
    let live = true;
    setError(null);
    setSaid(null);
    const width = root.current?.clientWidth || 760;
    onTaken();
    harnessApi.openTerminal(agent, { what: request.what, command: request.command, cols: Math.max(40, Math.floor((width - 28) / 7.8)), rows: 26 }).then(
      (b) => {
        if (!live) return;
        setList((l) => [b, ...l.filter((x) => x.terminal.id !== b.terminal.id)]);
        setCurrent(b.terminal.id);
        requestAnimationFrame(() => root.current?.scrollIntoView({ block: "nearest", behavior: "smooth" }));
      },
      (e: unknown) => live && setError(errorMessage(e)),
    );
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- each request once, by its seq
  }, [seq]);

  const shown = list.find((b) => b.terminal.id === current) ?? list[0] ?? null;
  const st = shown ? states[shown.terminal.id] : undefined;

  const onState = React.useCallback((id: string, s: SessionState) => setStates((m) => ({ ...m, [id]: s })), []);
  const noop = React.useCallback(() => undefined, []);
  /** It ended (here, on another device, or by itself): it goes; a screen says what happens next. */
  const onClosed = React.useCallback(
    (id: string) => {
      setList((l) => {
        const gone = l.find((b) => b.terminal.id === id);
        if (gone?.what === "screen") setSaid(`${harness}'s own screen closed. The agent starts again once it's idle, to load what you changed; the conversation continues.`);
        return l.filter((b) => b.terminal.id !== id);
      });
      setCurrent((c) => (c === id ? null : c));
    },
    [harness],
  );

  const close = async (b: BesideTerminal) => {
    try {
      await api.closeTerminalSession(b.terminal.id);
    } catch {
      /* already gone */
    }
    onClosed(b.terminal.id);
  };

  /** The end of what's on its screen, as a block for the message. */
  const hand = () => {
    const text = handle.current?.text() ?? "";
    const lines = text.split("\n");
    const tail = lines.slice(-HAND_LINES).join("\n").trim();
    if (!tail) return;
    onInsert(`From the ${shown?.what === "screen" ? `${harness} screen` : "terminal"} beside you:\n\n\`\`\`\n${tail}\n\`\`\``);
  };

  const key = (k: (typeof KEYS)[number]) => {
    const h = handle.current;
    if (!h) return;
    h.send(k.ctrl ? charKey(k.ctrl, { ctrl: true }) : namedKey(k.key ?? "Enter", { shift: k.shift }, h.appCursor()));
    h.focus();
  };

  if (!shown && !error && !said) return null;
  const where = info ? `${machineName(info.target)} · ${info.cwd || "~"}` : "";
  const labelOf = (b: BesideTerminal) => (b.what === "shell" ? "Shell" : `${harness}${b.command ? ` · ${b.command}` : ""}`);

  return (
    <section ref={root} className="bt" aria-label="Terminal beside the agent">
      {error && <p className="bt-error">{error}</p>}
      {said && !shown && <p className="bt-said">{said}</p>}
      {shown && (
        <>
          <div className="bt-head">
            <div className="bt-what">
              <span className="bt-name">{shown.what === "shell" ? "Shell" : `${harness} · its own screen`}</span>
              <span className="bt-where">{where}</span>
            </div>
            <div className="bt-actions">
              <button type="button" className="bt-btn" onClick={hand} disabled={st?.kind !== "open"}>
                <CornerDownLeft size={13} aria-hidden="true" /> Add to message
              </button>
              <button type="button" className="bt-btn bt-close" aria-label={`Close ${labelOf(shown)}`} onClick={() => void close(shown)}>
                <X size={13} aria-hidden="true" /> Close
              </button>
            </div>
          </div>
          {list.length > 1 && (
            <SelectField className="bt-pick" label="Terminal" hideLabel value={shown.terminal.id} onChange={setCurrent} options={list.map((b) => ({ value: b.terminal.id, label: labelOf(b) }))} />
          )}
          {shown.what === "screen" && (
            <p className="bt-note">
              {shown.to_type && shown.command ? (
                <>
                  Type <code>{shown.command}</code> in it.{" "}
                </>
              ) : null}
              When you quit it, the agent starts again once it&rsquo;s idle, to load what you changed; the conversation continues.
            </p>
          )}
          <div className="bt-screen">
            <TerminalSession
              key={shown.terminal.id}
              ref={handle}
              tabId={shown.terminal.id}
              sessionId={shown.terminal.id}
              target={shown.terminal.target}
              active
              fontSize={fontSize}
              copyOnSelect={copyOnSelect}
              deviceKeyboard
              onState={onState}
              onTitle={noop}
              onSession={noop}
              onClosed={onClosed}
            />
          </div>
          {st?.kind === "reconnecting" && <p className="bt-note">Reconnecting…</p>}
          {st?.kind === "error" && <p className="bt-error">{st.message}</p>}
          {touch && (
            <div className="bt-keys" role="group" aria-label="Keys">
              {KEYS.map((k) => (
                <button key={k.aria} type="button" className="bt-key" aria-label={k.aria} onPointerDown={(e) => e.preventDefault()} onClick={() => key(k)}>
                  {k.label}
                </button>
              ))}
            </div>
          )}
        </>
      )}
    </section>
  );
}
