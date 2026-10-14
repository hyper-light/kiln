import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { useLocation, useNavigate } from "react-router";
import {
  ChevronDown,
  ClipboardCopy,
  Eraser,
  Maximize2,
  Minimize2,
  Minus,
  Plus,
  RotateCcw,
  Server,
  SquareTerminal,
  X,
} from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Optic } from "@/components/ui/misc";
import { Chips } from "@/components/ui/status";
import { Tip } from "@/components/ui/tooltip";
import { useLocalStorage } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { TerminalSession, type SessionHandle, type SessionState } from "./session";

interface Tab {
  id: number;
  target: string;
  title: string;
  state: SessionState;
}

let tabSeq = 0;

function targetLabel(target: string) {
  return target === "local" ? "server" : target.replace(/^agent:/, "");
}

export default function TerminalWorkspace({ visible }: { visible: boolean }) {
  const [tabs, setTabs] = React.useState<Tab[]>([]);
  const [activeId, setActiveId] = React.useState<number | null>(null);
  const [agents, setAgents] = React.useState<Agent[]>([]);
  const [fontSize, setFontSize] = useLocalStorage("kiln.term.fontSize", 13);
  const [copyOnSelect, setCopyOnSelect] = useLocalStorage("kiln.term.copyOnSelect", false);
  const [renderer, setRenderer] = React.useState<"webgl" | "dom" | null>(null);
  const [full, setFull] = React.useState(false);
  const handles = React.useRef(new Map<number, SessionHandle>());
  const panelRef = React.useRef<HTMLDivElement>(null);
  const location = useLocation();
  const navigate = useNavigate();

  React.useEffect(() => {
    if (visible) document.title = "Terminal · kiln";
  }, [visible]);

  const loadAgents = React.useCallback(() => {
    api.agents().then(setAgents).catch(() => undefined);
  }, []);
  React.useEffect(() => {
    if (visible) loadAgents();
  }, [visible, loadAgents]);
  useServerEvents((ev) => {
    if (ev.type === "agent")
      setAgents((list) => {
        const i = list.findIndex((a) => a.name === ev.agent.name);
        if (i === -1) return [...list, ev.agent];
        const copy = list.slice();
        copy[i] = ev.agent;
        return copy;
      });
  });

  const open = React.useCallback((target: string) => {
    const id = ++tabSeq;
    setTabs((t) => [...t, { id, target, title: targetLabel(target), state: { kind: "connecting" } }]);
    setActiveId(id);
  }, []);

  const close = React.useCallback((id: number) => {
    handles.current.delete(id);
    setTabs((t) => {
      const idx = t.findIndex((x) => x.id === id);
      const next = t.filter((x) => x.id !== id);
      setActiveId((cur) => (cur === id ? (next[Math.max(0, idx - 1)]?.id ?? null) : cur));
      return next;
    });
  }, []);

  // ?open=<target> deep links (command palette, agent cards)
  React.useEffect(() => {
    if (!visible) return;
    const params = new URLSearchParams(location.search);
    const target = params.get("open");
    if (target) {
      open(target);
      navigate("/terminal", { replace: true });
    }
  }, [visible, location.search, open, navigate]);

  const patch = (id: number, p: Partial<Tab>) => setTabs((t) => t.map((x) => (x.id === id ? { ...x, ...p } : x)));

  const active = tabs.find((t) => t.id === activeId) ?? null;
  const connected = agents.filter((a) => a.connected);

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
  React.useEffect(() => {
    const onFs = () => setFull(Boolean(document.fullscreenElement));
    document.addEventListener("fullscreenchange", onFs);
    return () => document.removeEventListener("fullscreenchange", onFs);
  }, []);

  const targetMenu = (trigger: React.ReactNode) => (
    <DropdownMenu.Root onOpenChange={(o) => o && loadAgents()}>
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
    <div ref={panelRef} className="flex min-h-0 flex-1 flex-col bg-[#08090a]" style={{ height: full ? "100dvh" : "calc(100dvh - var(--header-h) - 1px)" }}>
      {/* tab strip */}
      <div className="flex h-10 shrink-0 items-stretch border-b border-[#1d1f23]">
        <div role="tablist" aria-label="Terminal sessions" className="flex min-w-0 flex-1 items-stretch overflow-x-auto">
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
                data-status={t.state.kind === "open" ? "succeeded" : t.state.kind === "connecting" ? "queued" : t.state.kind === "error" ? "failed" : "cancelled"}
                aria-hidden="true"
              />
              <span className="font-mono text-[9px] text-[#6d7078]">{String(i + 1).padStart(2, "0")}</span>
              <span className="max-w-[180px] truncate">{t.title}</span>
              {t.state.kind === "exited" && <span className="font-mono text-[9px] text-[#8c8f98] uppercase">exited{t.state.code !== null ? ` ${t.state.code}` : ""}</span>}
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
          {targetMenu(
            <button className="flex shrink-0 items-center gap-1.5 px-3 text-[12px] text-[#a1a4ad] hover:bg-white/[0.04] hover:text-white" aria-label="New terminal session">
              <Plus size={14} /> <span className="hidden sm:inline">New</span> <ChevronDown size={12} />
            </button>,
          )}
        </div>
        <div className="flex shrink-0 items-center gap-0.5 border-l border-[#1d1f23] px-1.5">
          {renderer && (
            <span className="mr-2 hidden font-mono text-[9px] tracking-[0.08em] text-[#6d7078] uppercase md:inline" title="Active renderer">
              {renderer}
            </span>
          )}
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
          <Tip content="Clear scrollback">
            <button className="icon-btn hidden sm:inline-grid" data-variant="bare" data-size="sm" aria-label="Clear scrollback" disabled={!active} onClick={() => active && handles.current.get(active.id)?.clear()}>
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

      {/* sessions */}
      <div className="relative min-h-0 flex-1">
        {tabs.map((t) => (
          <TerminalSession
            key={t.id}
            ref={(h) => {
              if (h) handles.current.set(t.id, h);
            }}
            target={t.target}
            active={t.id === activeId}
            visible={visible}
            fontSize={fontSize}
            copyOnSelect={copyOnSelect}
            onState={(s) => patch(t.id, { state: s })}
            onTitle={(title) => patch(t.id, { title: title ? `${targetLabel(t.target)} · ${title}` : targetLabel(t.target) })}
            onRenderer={setRenderer}
          />
        ))}

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
                <span className="spectrum-rule" /> 05 / Terminal
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
    </div>
  );
}
