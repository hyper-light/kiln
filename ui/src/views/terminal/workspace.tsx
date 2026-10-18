import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { Link, Navigate, useLocation, useNavigate } from "@/lib/router";
import { ChevronDown, ClipboardCopy, ClipboardPaste, Copy, Eraser, Eye, Keyboard, Maximize2, Minimize2, Minus, PencilLine, Plus, RotateCcw, Server, SquareTerminal, X } from "lucide-react";
import { api } from "@/api/client";
import type { AiSession, TerminalSessionInfo } from "@/api/types";
import { TerminalKeyboard, type KeyboardFold, type KeyboardMode } from "@/components/terminal/keyboard";
import { PRESETS, presetFor } from "@/components/terminal/presets";
import { Button } from "@/components/ui/button";
import { Confirm, Optic } from "@/components/ui/misc";
import { Chips } from "@/components/ui/status";
import { Tip } from "@/components/ui/tooltip";
import { agentsRes, aiSessionsRes, harnessSessionsRes, terminalSessionsRes, useAgentsLive, useRes, warm } from "@/lib/data";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { usePref } from "@/lib/prefs";
import { invalidate } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { NameField } from "@/components/ui/name-field";
import { toast } from "@/components/ui/toast";
import { HarnessMark } from "@/views/harness/parts";
import { SpawnHereButton } from "@/views/harness/spawn-here";
import { SpawnDialog } from "@/views/harness/spawn";
import { DesktopNew, MobileBar } from "./switcher";
import { PastePanel, SelectTextPanel } from "./clipboard";
import { TerminalSession, type SessionHandle, type SessionState } from "./session";

interface Tab {
  /** Stable React key: the session id, or a placeholder until the server assigns one. */
  id: string;
  /** The server-side session this tab shows (null until a new session is created). */
  sessionId: string | null;
  target: string;
  title: string;
  /** The name a person gave the session (over the window title). */
  name?: string;
  /** The window title the program last set. */
  wtitle?: string;
  /** What the session runs (command + window title), used to pick keyboard shortcuts. */
  hints: string;
  /**
   * The AI harness last recognized in this tab ("shell" when none yet). It stays while the
   * program's title changes: a shortcut row that flips under the finger loses presses.
   */
  auto: string;
  state: SessionState;
}

let tabSeq = 0;

/** Phones and tablets: show the on-screen keyboard by default. */
const isTouchDevice = () => typeof window !== "undefined" && (window.matchMedia?.("(pointer: coarse)").matches || (navigator.maxTouchPoints ?? 0) > 0);

function targetLabel(target: string) {
  return target === "local" ? "server" : target.replace(/^agent:/, "");
}

/** The harness a title shows, or the one seen before when it shows none (titles change as it works). */
const stickyHarness = (prev: string, hints: string) => {
  const now = presetFor(hints).id;
  return now === "shell" ? prev : now;
};

/** "Claude Code", "Codex", … for a reported AI session's tool. */
const harnessLabel = (tool: string) => PRESETS.find((p) => p.id === tool)?.name ?? tool;

/** A tab's mark for the AI session running in its terminal, if any. */
function RunningHere({ session }: { session: AiSession | undefined }) {
  if (!session) return null;
  const label = `${harnessLabel(session.tool)} is running here`;
  return (
    <Tip content={label}>
      <span className="grid place-items-center" aria-label={label}>
        <HarnessMark kind={session.tool} size={14} />
      </span>
    </Tip>
  );
}

const tabTitle = (target: string, title: string) => (title ? `${targetLabel(target)} · ${title}` : targetLabel(target));

function sameState(a: SessionState, b: SessionState) {
  if (a.kind !== b.kind) return false;
  if (a.kind === "reconnecting" && b.kind === "reconnecting") return a.attempt === b.attempt;
  if (a.kind === "exited" && b.kind === "exited") return a.code === b.code;
  if (a.kind === "error" && b.kind === "error") return a.message === b.message;
  return true;
}

export default function TerminalWorkspace({ visible }: { visible: boolean }) {
  const [tabs, setTabs] = React.useState<Tab[]>([]);
  /** The tab whose name is being edited where it shows. */
  const [renaming, setRenaming] = React.useState<string | null>(null);
  /** Names a tab's session ("" gives it back its window title), on the server for every viewer. */
  const renameTab = (tabId: string, name: string) => {
    const tab = tabs.find((x) => x.id === tabId);
    if (!tab?.sessionId) return;
    setTabs((cur) => cur.map((x) => (x.id === tabId ? { ...x, name: name || undefined, title: tabTitle(x.target, name || x.wtitle || "") } : x)));
    api.renameTerminalSession(tab.sessionId, name).catch((e: unknown) => toast.error("Couldn't rename the terminal", errorMessage(e)));
  };
  const [activeId, setActiveId] = React.useState<string | null>(null);
  const [kbVisible, setKbVisible] = usePref("kiln.term.keyboard", isTouchDevice());
  const [kbMode, setKbMode] = usePref<KeyboardMode>("kiln.term.keyboardMode", isTouchDevice() ? "full" : "keys");
  const [presets, setPresets] = usePref<Record<string, string>>("kiln.term.presets", {});
  // Folded away on purpose (kept per device), or shrunk for the moment while reading.
  const [kbClosed, setKbClosed] = usePref("kiln.term.keyboardClosed", false);
  const [kbShrunk, setKbShrunk] = React.useState(false);
  const [fontSize, setFontSize] = usePref("kiln.term.fontSize", 13);
  const [copyOnSelect, setCopyOnSelect] = usePref("kiln.term.copyOnSelect", false);
  const [renderer, setRenderer] = React.useState<"webgl" | "dom" | null>(null);
  const [full, setFull] = React.useState(false);
  const handles = React.useRef(new Map<string, SessionHandle>());
  /** One ref callback per tab, kept stable so memoized session views don't re-render. */
  const refs = React.useRef(new Map<string, (h: SessionHandle | null) => void>());
  const activeRef = React.useRef(activeId);
  activeRef.current = activeId;
  const panelRef = React.useRef<HTMLDivElement | null>(null);
  const location = useLocation();
  const navigate = useNavigate();

  warm(agentsRes, terminalSessionsRes, aiSessionsRes, harnessSessionsRes);
  const agents = useAgentsLive().data;

  /** Sessions this device dropped: not brought back by a listing that still has them. */
  const dropped = React.useRef(new Set<string>());
  // Tabs for sessions the server has that this device isn't showing yet (opened elsewhere), as
  // each new listing of them arrives.
  const sessions = useRes(terminalSessionsRes).data;
  const [listed, setListed] = React.useState<TerminalSessionInfo[] | null>(null);
  if (sessions !== listed) {
    setListed(sessions);
    const known = new Set(tabs.map((t) => t.sessionId).filter(Boolean));
    const added: Tab[] = sessions
      .filter((s) => !s.exited && !known.has(s.id) && !dropped.current.has(s.id))
      .map((s) => ({
        id: s.id,
        sessionId: s.id,
        target: s.target,
        name: s.name ?? undefined,
        wtitle: s.title,
        title: tabTitle(s.target, s.name || s.title),
        hints: `${s.command ?? ""} ${s.title}`,
        auto: presetFor(`${s.command ?? ""} ${s.title}`).id,
        state: { kind: "connecting" },
      }));
    if (added.length) {
      setTabs([...tabs, ...added]);
      if (activeId === null) setActiveId(added[0]?.id ?? null);
    }
  }

  /** The AI session each terminal session runs, as the machines report it (terminal id → session). */
  const aiList = useRes(aiSessionsRes).data;
  const aiHere = React.useMemo(() => {
    const next: Record<string, AiSession> = {};
    for (const s of aiList) if (s.terminal && !next[s.terminal]) next[s.terminal] = s;
    return next;
  }, [aiList]);
  /** Which AI harness each terminal session runs (terminal id → preset id). */
  const aiTools = React.useMemo(() => {
    const out: Record<string, string> = {};
    for (const [term, s] of Object.entries(aiHere)) if (PRESETS.some((p) => p.id === s.tool)) out[term] = s.tool;
    return out;
  }, [aiHere]);

  const open = React.useCallback((target: string) => {
    const id = `new-${++tabSeq}`;
    setTabs((t) => [...t, { id, sessionId: null, target, title: targetLabel(target), hints: "", auto: "shell", state: { kind: "connecting" } }]);
    setActiveId(id);
  }, []);

  /** Bring up an existing session's tab, adding one if this device has none yet. */
  const show = (sessionId: string, target: string) => {
    const tab = tabs.find((t) => t.sessionId === sessionId);
    setActiveId(tab ? tab.id : sessionId);
    dropped.current.delete(sessionId);
    if (!tab)
      setTabs((cur) =>
        cur.some((t) => t.sessionId === sessionId) ? cur : [...cur, { id: sessionId, sessionId, target, title: targetLabel(target), hints: "", auto: "shell", state: { kind: "connecting" } }],
      );
  };

  /** The server named this tab's session: remember it, and drop any duplicate view of it. */
  const onSession = React.useCallback((tabId: string, info: TerminalSessionInfo) => {
    setTabs((cur) => {
      const dup = cur.find((x) => x.sessionId === info.id && x.id !== tabId);
      const next = cur
        .filter((x) => x !== dup)
        .map((x) =>
          x.id === tabId
            ? {
                ...x,
                sessionId: info.id,
                hints: `${info.command ?? ""} ${info.title}`,
                auto: stickyHarness(x.auto, `${info.command ?? ""} ${info.title}`),
                name: info.name ?? x.name,
                wtitle: info.title || x.wtitle,
                title: (info.name ?? x.name) ? tabTitle(info.target, info.name ?? x.name ?? "") : info.title ? tabTitle(info.target, info.title) : x.title,
              }
            : x,
        );
      if (dup) {
        handles.current.delete(dup.id);
        refs.current.delete(dup.id);
        setActiveId((a) => (a === dup.id ? tabId : a));
      }
      return next;
    });
  }, []);

  /** Drop a tab (and select its neighbour) without touching its session. */
  const dropTab = React.useCallback((id: string) => {
    handles.current.delete(id);
    refs.current.delete(id);
    setTabs((t) => {
      const idx = t.findIndex((x) => x.id === id);
      if (idx === -1) return t;
      const sid = t[idx]?.sessionId;
      if (sid) dropped.current.add(sid);
      const next = t.filter((x) => x.id !== id);
      setActiveId((cur) => (cur === id ? (next[Math.max(0, idx - 1)]?.id ?? null) : cur));
      return next;
    });
  }, []);

  const onState = React.useCallback((tabId: string, state: SessionState) => {
    setTabs((t) => (t.some((x) => x.id === tabId && !sameState(x.state, state)) ? t.map((x) => (x.id === tabId ? { ...x, state } : x)) : t));
  }, []);

  const onTitle = React.useCallback((tabId: string, title: string) => {
    setTabs((t) => {
      const tab = t.find((x) => x.id === tabId);
      if (!tab || tab.hints === title) return t;
      // A named tab keeps its name over whatever title the program sets.
      return t.map((x) => (x.id === tabId ? { ...x, title: x.name ? x.title : tabTitle(x.target, title), wtitle: title, hints: title, auto: stickyHarness(x.auto, title) } : x));
    });
  }, []);

  const refFor = (id: string) => {
    let fn = refs.current.get(id);
    if (!fn) {
      fn = (h: SessionHandle | null) => {
        if (h) handles.current.set(id, h);
      };
      refs.current.set(id, fn);
    }
    return fn;
  };

  /** A running session asked to end (in place, under the tabs): its shell stops on every device. */
  const [ending, setEnding] = React.useState<string | null>(null);
  /** Close a tab. A running session is ended, once that's confirmed. */
  const close = React.useCallback(
    (id: string) => {
      const tab = tabs.find((x) => x.id === id);
      if (!tab) return;
      const running = tab.sessionId && tab.state.kind !== "exited" && tab.state.kind !== "error";
      if (running) setEnding(id);
      else dropTab(id);
    },
    [tabs, dropTab],
  );
  const endSession = (id: string) => {
    const tab = tabs.find((x) => x.id === id);
    if (tab?.sessionId) void api.closeTerminalSession(tab.sessionId).catch(() => undefined);
    setEnding(null);
    dropTab(id);
  };

  // Deep links: ?open=<target> starts a session (command palette, runner cards); ?session=<id>
  // attaches to one (Agents). Each link is followed once, then the address goes back to /terminal.
  const params = new URLSearchParams(location.search);
  const linkTarget = params.get("open");
  const linkSession = params.get("session");
  const link = visible && (linkTarget || linkSession) ? `${location.key}:${location.search}` : null;
  const [followed, setFollowed] = React.useState<string | null>(null);
  if (link && link !== followed) {
    setFollowed(link);
    if (linkTarget) open(linkTarget);
    else if (linkSession) show(linkSession, params.get("target") ?? "");
  }

  const active = tabs.find((t) => t.id === activeId) ?? null;
  const activeSession = active?.sessionId ?? null;
  // The program in the session picks the shortcuts: an AI session the machine reports running in
  // this terminal first, else the harness its titles last showed.
  const linked = activeSession ? aiTools[activeSession] : undefined;
  const activeAi = activeSession ? aiHere[activeSession] : undefined;
  const autoPresetId = linked ?? active?.auto ?? "shell";

  // The keyboard's callbacks act on whichever tab is selected when a key is pressed; they stay the
  // same functions so the memoized keyboard never re-renders for them.
  const kbSend = React.useCallback((bytes: string) => {
    const id = activeRef.current;
    if (id) handles.current.get(id)?.send(bytes);
  }, []);
  const kbAppCursor = React.useCallback(() => {
    const id = activeRef.current;
    return (id && handles.current.get(id)?.appCursor()) || false;
  }, []);
  const kbPreset = React.useCallback(
    (id: string | null) => {
      if (!activeSession) return;
      const next = { ...presets };
      if (id) next[activeSession] = id;
      else delete next[activeSession];
      setPresets(next);
    },
    [activeSession, presets, setPresets],
  );
  const kbFold: KeyboardFold = kbClosed ? "closed" : kbShrunk ? "shrunk" : "open";
  const kbOnFold = React.useCallback(
    (f: "open" | "closed") => {
      setKbShrunk(false);
      setKbClosed(f === "closed");
    },
    [setKbClosed],
  );
  /** Like a phone keyboard: scrolling the terminal shrinks it, tapping the terminal brings it back. */
  const onGesture = React.useCallback(
    (_tabId: string, g: "scroll" | "tap") => {
      if (g === "scroll") setKbShrunk(true);
      else {
        setKbShrunk(false);
        setKbClosed(false);
      }
    },
    [setKbClosed],
  );
  const kbOnMode = React.useCallback(
    (m: KeyboardMode) => {
      setKbMode(m);
      // Switching to the device keyboard: focus the terminal so it opens.
      if (m === "keys")
        requestAnimationFrame(() => {
          const id = activeRef.current;
          if (id) handles.current.get(id)?.focus();
        });
    },
    [setKbMode],
  );
  // Job pods run CI jobs; they aren't machines to open shells on.
  const connected = agents.filter((a) => a.connected && !a.ephemeral);

  // Phones: one pinned bar (current session, a switcher, new terminal/agents) instead of tabs that scroll off screen.
  const narrow = useMediaQuery("(max-width: 639px)");
  const [spawn, setSpawn] = React.useState<{ target?: string; shell?: string } | null>(null);
  // Copy and paste: the selection or a plain-text view of the session; the clipboard, or a box to paste into.
  const [selectText, setSelectText] = React.useState<string | null>(null);
  const [pasteBox, setPasteBox] = React.useState(false);
  const focusActive = React.useCallback(() => {
    const id = activeRef.current;
    if (id) handles.current.get(id)?.focus();
  }, []);
  // After a button, keys go back to the terminal (an Enter must not press the button again).
  const copyFromActive = React.useCallback(async () => {
    const id = activeRef.current;
    const h = id ? handles.current.get(id) : undefined;
    if (!h) return;
    if (await h.copySelection()) h.focus();
    else setSelectText(h.text());
  }, []);
  const pasteIntoActive = React.useCallback(async () => {
    const id = activeRef.current;
    const h = id ? handles.current.get(id) : undefined;
    if (!h) return;
    if (await h.paste()) h.focus();
    else setPasteBox(true);
  }, []);
  const targets = ["local", ...connected.map((a) => `agent:${a.name}`)];
  const switcherTabs = tabs.map((t) => ({ id: t.id, title: t.title, name: t.name, target: t.target, state: t.state.kind }));

  // The panel goes full screen (and comes back) by the browser's own events too (Escape).
  const panel = React.useCallback((el: HTMLDivElement | null) => {
    panelRef.current = el;
    if (!el) return;
    const onFs = () => setFull(Boolean(document.fullscreenElement));
    document.addEventListener("fullscreenchange", onFs);
    return () => {
      document.removeEventListener("fullscreenchange", onFs);
      panelRef.current = null;
    };
  }, []);

  const toggleFull = async () => {
    const el = panelRef.current;
    if (!el) return;
    try {
      if (!document.fullscreenElement) {
        await el.requestFullscreen();
        setFull(true);
      } else {
        await document.exitFullscreen();
        setFull(false);
      }
    } catch {
      setFull((f) => !f);
    }
  };
  const targetMenu = (trigger: React.ReactNode) => (
    <DropdownMenu.Root onOpenChange={(o) => o && invalidate("agents")}>
      <DropdownMenu.Trigger asChild>{trigger}</DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content className="popover w-72" sideOffset={6} align="start">
          <DropdownMenu.Label className="label px-2.5 pt-1.5 pb-1">Target</DropdownMenu.Label>
          <DropdownMenu.Item className="menu-item" onSelect={() => open("local")}>
            <SquareTerminal size={14} className="text-[#9a9da6]" />
            <span className="flex-1">server</span>
            <span className="font-mono text-[10px] text-[#7f838d]">local</span>
          </DropdownMenu.Item>
          {connected.length > 0 && <DropdownMenu.Separator className="my-1 h-px bg-white/10" />}
          {connected.map((a) => (
            <DropdownMenu.Item key={a.name} className="menu-item" onSelect={() => open(`agent:${a.name}`)}>
              <Server size={14} className="text-[#9a9da6]" />
              <span className="min-w-0 flex-1 truncate">{a.name}</span>
              <span className="truncate font-mono text-[10px] text-[#7f838d]">{a.ephemeral ? "pod" : a.hostname}</span>
            </DropdownMenu.Item>
          ))}
          {connected.length === 0 && <div className="px-2.5 py-2 text-[11px] text-[#7f838d]">No agents connected.</div>}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );

  return (
    <div ref={panel} className="flex min-h-0 flex-1 flex-col bg-[#08090a]" style={{ height: full ? "100dvh" : "calc(100dvh - var(--header-h) - 1px)" }}>
      {visible && <title>Terminal · kiln</title>}
      {link && <Navigate to="/terminal" replace />}
      {narrow && (
        <MobileBar
          tabs={switcherTabs}
          activeId={activeId}
          onSelect={setActiveId}
          onClose={close}
          targets={targets}
          onOpenTarget={open}
          onNewAgents={() => setSpawn({})}
          kbVisible={kbVisible}
          onToggleKb={() => setKbVisible(!kbVisible)}
          renaming={renaming !== null && renaming === activeId}
          onRename={(n) => active && renameTab(active.id, n)}
          onRenameDone={() => setRenaming(null)}
          more={[
            { label: "Copy", disabled: !active, onSelect: () => void copyFromActive() },
            { label: "Paste", disabled: !active, onSelect: () => void pasteIntoActive() },
            { label: "Select text", disabled: !active, onSelect: () => active && setSelectText(handles.current.get(active.id)?.text() ?? "") },
            { label: "Rename terminal", disabled: !active?.sessionId, onSelect: () => active && setRenaming(active.id) },
            { label: "Start agents in this shell", onSelect: () => active && setSpawn({ target: active.target, shell: active.sessionId ?? undefined }), disabled: !active?.sessionId },
            { label: "Larger text", onSelect: () => setFontSize(Math.min(24, fontSize + 1)) },
            { label: "Smaller text", onSelect: () => setFontSize(Math.max(9, fontSize - 1)) },
            { label: "Copy on select", pressed: copyOnSelect, onSelect: () => setCopyOnSelect(!copyOnSelect) },
            { label: "Clear scrollback", disabled: !active, onSelect: () => active && handles.current.get(active.id)?.clear() },
            { label: "Reconnect", disabled: !active, onSelect: () => active && handles.current.get(active.id)?.reconnect() },
            { label: full ? "Exit full screen" : "Full screen", onSelect: () => void toggleFull() },
          ]}
        />
      )}
      {/* tab strip */}
      <div className={cn("h-10 shrink-0 items-stretch border-b border-[#1d1f23]", narrow ? "hidden" : "flex")}>
        <div role="tablist" aria-label="Terminal sessions" className="term-tabs flex min-w-0 flex-1 items-stretch overflow-x-auto">
          {tabs.map((t, i) => (
            <div
              key={t.id}
              role="tab"
              tabIndex={0}
              aria-selected={t.id === activeId}
              className="term-tab cursor-pointer"
              onClick={() => setActiveId(t.id)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") setActiveId(t.id);
              }}
              onAuxClick={(e) => {
                if (e.button === 1) close(t.id);
              }}
            >
              <i
                className="dot"
                data-status={t.state.kind === "open" ? "succeeded" : t.state.kind === "connecting" || t.state.kind === "reconnecting" ? "queued" : t.state.kind === "error" ? "failed" : "cancelled"}
                aria-hidden="true"
              />
              <span className="font-mono text-[9px] text-[#6d7078]">{String(i + 1).padStart(2, "0")}</span>
              <RunningHere session={t.sessionId ? aiHere[t.sessionId] : undefined} />
              {renaming === t.id ? (
                // The tab activates on clicks and keys; the field keeps them to itself.
                <span className="term-name" onClick={(e) => e.stopPropagation()} onMouseDown={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
                  <NameField value={t.name ?? ""} placeholder={t.title} label="Terminal name" onSave={(n) => renameTab(t.id, n)} onDone={() => setRenaming(null)} className="term-name-field" />
                </span>
              ) : (
                <span
                  className="max-w-[180px] truncate"
                  title={t.sessionId ? "Double-click to rename" : undefined}
                  onDoubleClick={(e) => {
                    e.stopPropagation();
                    if (t.sessionId) setRenaming(t.id);
                  }}
                >
                  {t.title}
                </span>
              )}
              {t.state.kind === "exited" && <span className="font-mono text-[9px] text-[#8c8f98] uppercase">exited{t.state.code !== null ? ` ${t.state.code}` : ""}</span>}
              {t.id === activeId && t.sessionId && renaming !== t.id && (
                <button
                  className="grid size-7 place-items-center rounded-full text-[#7f838d] hover:bg-white/[0.06] hover:text-white"
                  aria-label={`Rename ${t.title}`}
                  title="Rename"
                  onClick={(e) => {
                    e.stopPropagation();
                    setRenaming(t.id);
                  }}
                >
                  <PencilLine size={12} />
                </button>
              )}
              <button
                className="grid size-7 place-items-center rounded-full text-[#7f838d] hover:bg-white/[0.06] hover:text-white"
                aria-label={`Close ${t.title}`}
                onClick={(e) => {
                  e.stopPropagation();
                  close(t.id);
                }}
              >
                <X size={13} />
              </button>
            </div>
          ))}
        </div>
        <DesktopNew targets={targets} onOpenTarget={open} onNewAgents={() => setSpawn({})} />
        <div className="flex shrink-0 items-center gap-0.5 border-l border-[#1d1f23] px-1.5">
          {renderer && (
            <span className="mr-2 hidden font-mono text-[9px] tracking-[0.08em] text-[#6d7078] uppercase md:inline" title="Active renderer">
              {renderer}
            </span>
          )}
          {active && <SpawnHereButton target={active.target} shell={active.sessionId} />}
          <Tip content={kbVisible ? "Hide keyboard" : "Show keyboard"}>
            <button
              className={cn("icon-btn", kbVisible && "!text-[#d8d0f2]")}
              data-variant="bare"
              data-size="sm"
              aria-pressed={kbVisible}
              aria-label="Toggle on-screen keyboard"
              onClick={() => setKbVisible(!kbVisible)}
            >
              <Keyboard size={14} />
            </button>
          </Tip>
          <Tip content="Smaller text">
            <button className="icon-btn" data-variant="bare" data-size="sm" aria-label="Decrease font size" onClick={() => setFontSize(Math.max(9, fontSize - 1))}>
              <Minus size={14} />
            </button>
          </Tip>
          <span className="w-6 text-center font-mono text-[10px] text-[#8f939e]">{fontSize}</span>
          <Tip content="Larger text">
            <button className="icon-btn" data-variant="bare" data-size="sm" aria-label="Increase font size" onClick={() => setFontSize(Math.min(24, fontSize + 1))}>
              <Plus size={14} />
            </button>
          </Tip>
          <Tip content={copyOnSelect ? "Copy on select: on" : "Copy on select: off"}>
            <button
              className={cn("icon-btn", copyOnSelect && "!text-[#d8d0f2]")}
              data-variant="bare"
              data-size="sm"
              aria-pressed={copyOnSelect}
              aria-label="Toggle copy on select"
              onClick={() => setCopyOnSelect(!copyOnSelect)}
            >
              <ClipboardCopy size={14} />
            </button>
          </Tip>
          <Tip content="Copy (⌘C / Ctrl+C with text selected)">
            <button className="icon-btn" data-variant="bare" data-size="sm" aria-label="Copy" disabled={!active} onClick={() => void copyFromActive()}>
              <Copy size={14} />
            </button>
          </Tip>
          <Tip content="Paste (⌘V / Ctrl+Shift+V)">
            <button className="icon-btn" data-variant="bare" data-size="sm" aria-label="Paste" disabled={!active} onClick={() => void pasteIntoActive()}>
              <ClipboardPaste size={14} />
            </button>
          </Tip>
          <Tip content="Clear scrollback">
            <button
              className="icon-btn hidden sm:inline-grid"
              data-variant="bare"
              data-size="sm"
              aria-label="Clear scrollback"
              disabled={!active}
              onClick={() => active && handles.current.get(active.id)?.clear()}
            >
              <Eraser size={14} />
            </button>
          </Tip>
          <Tip content="Reconnect session">
            <button className="icon-btn" data-variant="bare" data-size="sm" aria-label="Reconnect" disabled={!active} onClick={() => active && handles.current.get(active.id)?.reconnect()}>
              <RotateCcw size={14} />
            </button>
          </Tip>
          <Tip content={full ? "Exit full screen" : "Full screen"}>
            <button className="icon-btn" data-variant="bare" data-size="sm" aria-label="Toggle full screen" onClick={toggleFull}>
              {full ? <Minimize2 size={14} /> : <Maximize2 size={14} />}
            </button>
          </Tip>
        </div>
      </div>

      {/* Asked and answered in the page's flow, between the tabs and the terminal. */}
      {ending && (
        <div className="term-band">
          <Confirm
            label="End this session"
            tone="danger"
            actions={
              <>
                <Button size="xs" variant="danger" onClick={() => endSession(ending)}>
                  End session
                </Button>
                <Button size="xs" variant="ghost" onClick={() => setEnding(null)}>
                  Cancel
                </Button>
              </>
            }
          >
            <p>End this session? Its shell stops on every device.</p>
          </Confirm>
        </div>
      )}
      {selectText !== null && (
        <SelectTextPanel
          text={selectText}
          onClose={() => {
            setSelectText(null);
            focusActive();
          }}
        />
      )}
      {pasteBox && (
        <PastePanel
          onPaste={(text) => {
            const id = activeRef.current;
            if (id) handles.current.get(id)?.pasteText(text);
          }}
          onClose={() => {
            setPasteBox(false);
            focusActive();
          }}
        />
      )}
      <SpawnDialog
        open={spawn !== null}
        onOpenChange={(o) => !o && setSpawn(null)}
        defaults={spawn ?? undefined}
        onStarted={(started) => {
          setSpawn(null);
          navigate(started.length === 1 && started[0] ? `/agents/${started[0].id}` : "/agents");
        }}
      />
      {/* sessions */}
      <div className="relative min-h-0 flex-1">
        {tabs.map((t) => (
          <TerminalSession
            key={t.id}
            ref={refFor(t.id)}
            tabId={t.id}
            sessionId={t.sessionId}
            target={t.target}
            active={visible && t.id === activeId}
            fontSize={fontSize}
            copyOnSelect={copyOnSelect}
            deviceKeyboard={!(kbVisible && kbMode === "full")}
            onState={onState}
            onTitle={onTitle}
            onSession={onSession}
            onClosed={dropTab}
            onGesture={onGesture}
            onRenderer={setRenderer}
          />
        ))}

        {activeAi && active?.state.kind === "open" && (
          <Link
            to={`/agents/sessions?session=${encodeURIComponent(`${activeAi.machine}:${activeAi.pid}`)}`}
            className="absolute top-3 right-4 z-10 flex items-center gap-2 rounded-full border border-white/10 bg-[#111215]/95 py-1 pr-3 pl-1.5 font-mono text-[10px] tracking-[0.06em] text-[#a1a4ad] uppercase shadow-2xl backdrop-blur hover:text-white"
          >
            <HarnessMark kind={activeAi.tool} size={16} />
            <span className="hidden sm:inline">{harnessLabel(activeAi.tool)} running here ·</span>
            <Eye size={12} /> Watch
          </Link>
        )}

        {active && active.state.kind === "reconnecting" && (
          <div className="pointer-events-none absolute top-3 right-4 z-10 rounded-full border border-white/10 bg-[#111215]/95 px-3 py-1 font-mono text-[10px] tracking-[0.06em] text-[#a1a4ad] uppercase shadow-2xl backdrop-blur">
            Reconnecting… the session is still running
          </div>
        )}

        {active && (active.state.kind === "exited" || active.state.kind === "error") && (
          <div className="absolute right-4 bottom-4 z-10 flex items-center gap-3 rounded-full border border-white/10 bg-[#111215]/95 py-1 pr-1 pl-4 shadow-2xl backdrop-blur">
            <span className="font-mono text-[10px] tracking-[0.06em] text-[#a1a4ad] uppercase">
              {active.state.kind === "exited" ? `Session ended${active.state.code !== null ? ` · code ${active.state.code}` : ""}` : active.state.message}
            </span>
            <Button size="xs" variant="secondary" onClick={() => handles.current.get(active.id)?.reconnect()}>
              <RotateCcw /> Reconnect
            </Button>
          </div>
        )}

        {tabs.length === 0 && (
          <div className="absolute inset-0 grid place-items-center overflow-y-auto px-4 py-10">
            <div className="flex max-w-[640px] flex-col items-center text-center">
              <Optic size={200} />
              <div className="eyebrow mt-6">
                <span className="spectrum-rule" /> 07 / Terminal
              </div>
              <h1 className="mt-4 text-[clamp(28px,3.4vw,40px)] leading-[1.1] font-[450] tracking-[-0.045em]">Full control, any machine.</h1>
              <p className="mt-3 max-w-[460px] text-[14px] leading-[1.8] text-[#a1a4ad]">
                A real PTY over WebSocket — k9s, htop, vim and less render exactly as they would locally. Keys go straight to the shell while the terminal has focus.
              </p>
              <div className="mt-7 flex flex-wrap justify-center gap-2">
                <Button onClick={() => open("local")}>
                  <SquareTerminal /> Open on server
                </Button>
                {targetMenu(
                  <Button variant="secondary">
                    Choose target <ChevronDown />
                  </Button>,
                )}
              </div>
              {connected.length > 0 && (
                <div className="mt-10 w-full text-left">
                  <div className="label mb-2">Connected agents</div>
                  <div className="row-list">
                    {connected.slice(0, 8).map((a) => (
                      <button
                        key={a.name}
                        onClick={() => open(`agent:${a.name}`)}
                        className="flex min-h-11 w-full items-center gap-3 px-2 text-left text-[13px] text-[#c8cad0] transition-colors hover:bg-[#101114]"
                      >
                        <i className="dot" data-status="succeeded" aria-hidden="true" />
                        <span className="min-w-0 flex-1 truncate">{a.name}</span>
                        <span className="hidden sm:inline">
                          <Chips labels={a.labels} max={3} />
                        </span>
                        <Plus size={14} className="text-[#7f838d]" />
                      </button>
                    ))}
                  </div>
                </div>
              )}
            </div>
          </div>
        )}
      </div>

      {/* On-screen keyboard: shown by default on touch devices, toggled from the toolbar. */}
      {active && kbVisible && (
        <TerminalKeyboard
          send={kbSend}
          appCursor={kbAppCursor}
          autoPresetId={autoPresetId}
          presetId={(activeSession && presets[activeSession]) || null}
          onPreset={kbPreset}
          mode={kbMode}
          onMode={kbOnMode}
          fold={kbFold}
          onFold={kbOnFold}
          onPaste={pasteIntoActive}
          onCopy={copyFromActive}
        />
      )}
    </div>
  );
}
