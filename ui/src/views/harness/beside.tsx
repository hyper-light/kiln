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
import { create } from "zustand";
import { CornerDownLeft, X } from "lucide-react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo, type BesideTerminal } from "@/api/harness";
import { charKey, namedKey, type NamedKey } from "@/components/terminal/keys";
import { SelectField } from "@/components/ui/misc";
import { errorMessage, useLocalStorage, useMediaQuery } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { TerminalSession, type SessionHandle, type SessionState } from "@/views/terminal/session";
import { machineName } from "./parts";
import "./beside.css";

/** What was done here to an agent's terminals: opened, closed, the one shown, what went wrong or what it said. */
interface Here {
  opened: BesideTerminal[];
  closed: string[];
  current: string | null;
  error: string | null;
  said: string | null;
}

const NOTHING: Here = { opened: [], closed: [], current: null, error: null, said: null };

const useHere = create<{ by: Record<string, Here> }>()(() => ({ by: {} }));
const change = (agent: string, f: (h: Here) => Here) => useHere.setState((s) => ({ by: { ...s.by, [agent]: f(s.by[agent] ?? NOTHING) } }));

/**
 * Opens a terminal beside an agent (from the composer: `!command`, /terminal, a command only its
 * own screen does): its shell, with `command` run in it, or its harness's own screen on `command`.
 * It shows under the conversation, brought into view.
 */
export function openBeside(agent: string, what: "shell" | "screen", command?: string) {
  change(agent, (h) => ({ ...h, error: null, said: null }));
  const width = document.querySelector<HTMLElement>(".hx-timeline .hx-flow")?.clientWidth || 760;
  harnessApi.openTerminal(agent, { what, command, cols: Math.max(40, Math.floor((width - 28) / 7.8)), rows: 26 }).then(
    (b) => {
      change(agent, (h) => ({ ...h, opened: [b, ...h.opened.filter((x) => x.terminal.id !== b.terminal.id)], closed: h.closed.filter((id) => id !== b.terminal.id), current: b.terminal.id }));
      requestAnimationFrame(() => document.querySelector(".bt")?.scrollIntoView({ block: "nearest", behavior: "smooth" }));
    },
    (e: unknown) => change(agent, (h) => ({ ...h, error: errorMessage(e) })),
  );
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

type BesideProps = {
  agent: string;
  info: AgentInfo | null;
  onInsert: (text: string) => void;
};

/** The terminals beside an agent; nothing until it's known whether there are any. */
export function BesideTerminals(props: BesideProps) {
  return (
    <React.Suspense fallback={null}>
      <Terminals {...props} />
    </React.Suspense>
  );
}

function Terminals({ agent, info, onInsert }: BesideProps) {
  // The ones already open beside it (here earlier, or on another device), and what was done here since.
  const there = useSuspenseResource(`harness/terminals/${agent}`, () => harnessApi.terminals(agent).catch((): BesideTerminal[] => []));
  const here = useHere((s) => s.by[agent] ?? NOTHING);
  const list = React.useMemo(() => {
    const opened = new Set(here.opened.map((b) => b.terminal.id));
    return [...here.opened, ...there.data.filter((b) => !b.terminal.exited && !opened.has(b.terminal.id))].filter((b) => !here.closed.includes(b.terminal.id));
  }, [here.opened, here.closed, there.data]);
  const { current, error, said } = here;
  const setCurrent = (id: string) => change(agent, (h) => ({ ...h, current: id }));
  const [states, setStates] = React.useState<Record<string, SessionState>>({});
  const handle = React.useRef<SessionHandle | null>(null);
  const [fontSize] = useLocalStorage("kiln.term.fontSize", 13);
  const [copyOnSelect] = useLocalStorage("kiln.term.copyOnSelect", false);
  const touch = useMediaQuery("(pointer: coarse)");
  const harness = info?.kind_name ?? "The harness";

  const shown = list.find((b) => b.terminal.id === current) ?? list[0] ?? null;
  const st = shown ? states[shown.terminal.id] : undefined;

  const onState = React.useCallback((id: string, s: SessionState) => setStates((m) => ({ ...m, [id]: s })), []);
  const noop = React.useCallback(() => undefined, []);
  /** It ended (here, on another device, or by itself): it goes; a screen says what happens next. */
  const onClosed = React.useCallback(
    (id: string) => {
      const gone = list.find((b) => b.terminal.id === id);
      change(agent, (h) => ({
        ...h,
        closed: h.closed.includes(id) ? h.closed : [...h.closed, id],
        current: h.current === id ? null : h.current,
        said: gone?.what === "screen" ? `${harness}'s own screen closed. The agent starts again once it's idle, to load what you changed; the conversation continues.` : h.said,
      }));
    },
    [agent, list, harness],
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
    <section className="bt" aria-label="Terminal beside the agent">
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
