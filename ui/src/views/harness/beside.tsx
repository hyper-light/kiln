/**
 * Terminals beside an agent, in its folder on its machine, under the conversation: its shell
 * (/terminal, or `!command` in the composer) for what the agent can't or won't run, and its
 * harness's own screen for what only that screen does (/permissions, /doctor, /screen …). kiln
 * keeps them, so they are here again after a reload or on another device, and the list follows
 * the server's word on terminals. When the harness's own screen closes, the agent starts again
 * once it's idle, to load what was changed there.
 *
 * In the page's flow, never over it: one terminal shown, a select to switch when there are more,
 * what's on its screen added to the message, and on touch screens the keys a phone lacks.
 */
import * as React from "react";
import { CornerDownLeft, X } from "lucide-react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo, type BesideTerminal } from "@/api/harness";
import { charKey, namedKey, type NamedKey } from "@/components/terminal/keys";
import { Loadable } from "@/components/ui/loading";
import { SelectField } from "@/components/ui/misc";
import { errorMessage, useLocalStorage, useMediaQuery } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { TerminalSession, type SessionHandle, type SessionState } from "@/views/terminal/session";
import { machineName } from "./parts";
import "./beside.css";

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

/** Beside one agent, here: the terminal picked, the last one opened, and what to say. */
interface Here {
  current: string | null;
  /** The last one opened, and when: brought into view once it's shown. */
  opened: { id: string; at: number } | null;
  /** Why the last one asked for didn't open. */
  error: string | null;
  /** What happens now the harness's own screen has closed. */
  said: string | null;
}

const NONE: Here = { current: null, opened: null, error: null, said: null };
let here: Record<string, Here> = {};
const subs = new Set<() => void>();

function put(agent: string, patch: Partial<Here>) {
  here = { ...here, [agent]: { ...(here[agent] ?? NONE), ...patch } };
  subs.forEach((s) => s());
}

const subscribe = (cb: () => void) => {
  subs.add(cb);
  return () => void subs.delete(cb);
};
const useHere = (agent: string) => React.useSyncExternalStore(subscribe, () => here[agent] ?? NONE);

const besideKey = (agent: string) => `beside/${agent}`;

/**
 * Opens a terminal beside `agent`: its shell (the one open, with `command` run in it), or its
 * harness's own screen. It shows once the list has it.
 */
export async function openBeside(agent: string, o: { what: "shell" | "screen"; command?: string }) {
  put(agent, { error: null, said: null });
  // About the width it will have; it fits itself once it's shown.
  const width = document.querySelector<HTMLElement>(".bt")?.clientWidth || Math.min(window.innerWidth, 760);
  try {
    const b = await harnessApi.openTerminal(agent, { what: o.what, command: o.command, cols: Math.max(40, Math.floor((width - 28) / 7.8)), rows: 26 });
    put(agent, { current: b.terminal.id, opened: { id: b.terminal.id, at: Date.now() } });
    invalidate(besideKey(agent));
  } catch (e) {
    put(agent, { error: errorMessage(e) });
  }
}

export function BesideTerminals(props: { agent: string; info: AgentInfo | null; onInsert: (text: string) => void }) {
  return (
    <Loadable label="Loading the terminals beside it" fallback={null}>
      <Terminals key={props.agent} {...props} />
    </Loadable>
  );
}

function Terminals({ agent, info, onInsert }: { agent: string; info: AgentInfo | null; onInsert: (text: string) => void }) {
  // The ones open beside it, here earlier or on another device.
  const res = useSuspenseResource(besideKey(agent), () => harnessApi.terminals(agent), { on: ["terminals"] });
  const { current, opened, error, said } = useHere(agent);
  const [states, setStates] = React.useState<Record<string, SessionState>>({});
  const handle = React.useRef<SessionHandle | null>(null);
  const [fontSize] = useLocalStorage("kiln.term.fontSize", 13);
  const [copyOnSelect] = useLocalStorage("kiln.term.copyOnSelect", false);
  const touch = useMediaQuery("(pointer: coarse)");
  const harness = info?.kind_name ?? "The harness";

  const open = res.data.filter((b) => !b.terminal.exited);
  // The one picked stays shown when it ends, with how it ended, until it's closed.
  const picked = res.data.find((b) => b.terminal.id === current);
  const shown = picked ?? open[0] ?? null;
  const list = picked?.terminal.exited ? [picked, ...open] : open;
  const st = shown ? states[shown.terminal.id] : undefined;
  const ended = Boolean(shown?.terminal.exited) || st?.kind === "exited";

  // A terminal just opened comes into view once it's shown (and again when it's picked).
  const bringIn = React.useCallback(
    (el: HTMLElement | null) => {
      if (el && opened) el.scrollIntoView({ block: "nearest", behavior: "smooth" });
    },
    [opened],
  );

  /** It ended on purpose (here or on another device): it goes; a screen says what happens next. */
  const gone = (id: string) => {
    if (res.data.find((b) => b.terminal.id === id)?.what === "screen")
      put(agent, { said: `${harness}'s own screen closed. The agent starts again once it's idle, to load what you changed; the conversation continues.` });
    res.setData((l) => l.filter((b) => b.terminal.id !== id));
  };

  const close = async (b: BesideTerminal) => {
    try {
      await api.closeTerminalSession(b.terminal.id);
    } catch {
      /* already gone */
    }
    gone(b.terminal.id);
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
    <section ref={shown && opened?.id === shown.terminal.id ? bringIn : undefined} className="bt" aria-label="Terminal beside the agent">
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
              <button type="button" className="bt-btn" onClick={hand} disabled={st?.kind !== "open" && st?.kind !== "exited"}>
                <CornerDownLeft size={13} aria-hidden="true" /> Add to message
              </button>
              <button type="button" className="bt-btn bt-close" aria-label={`Close ${labelOf(shown)}`} onClick={() => void close(shown)}>
                <X size={13} aria-hidden="true" /> Close
              </button>
            </div>
          </div>
          {list.length > 1 && (
            <SelectField
              className="bt-pick"
              label="Terminal"
              hideLabel
              value={shown.terminal.id}
              onChange={(id) => put(agent, { current: id })}
              options={list.map((b) => ({ value: b.terminal.id, label: labelOf(b) }))}
            />
          )}
          {shown.what === "screen" && (
            <p className="bt-note">
              {ended ? (
                "It has closed: the agent starts again once it's idle, to load what you changed; the conversation continues."
              ) : (
                <>
                  {shown.to_type && shown.command ? (
                    <>
                      Type <code>{shown.command}</code> in it.{" "}
                    </>
                  ) : null}
                  When you quit it, the agent starts again once it&rsquo;s idle, to load what you changed; the conversation continues.
                </>
              )}
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
              onState={(id, s) => setStates((m) => ({ ...m, [id]: s }))}
              onTitle={() => undefined}
              onSession={() => undefined}
              onClosed={gone}
            />
          </div>
          {st?.kind === "reconnecting" && <p className="bt-note">Reconnecting…</p>}
          {st?.kind === "error" && <p className="bt-error">{st.message}</p>}
          {touch && !ended && (
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
