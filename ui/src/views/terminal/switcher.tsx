/**
 * The terminal page's bar on phones: the session you're in and how many there are, one tap to
 * switch between every terminal and agent, and new terminals and agents always in reach — no
 * sideways-scrolling tab strip hiding things off screen.
 */
import * as React from "react";
import { NameField } from "@/components/ui/name-field";
import { useNavigate } from "@/lib/router";
import { DropdownMenu } from "radix-ui";
import {
  Bot,
  ChevronDown,
  Keyboard,
  MoreHorizontal,
  Plus,
  Server,
  SquareTerminal,
  X,
} from "lucide-react";
import { harnessSessionsRes, useRes } from "@/lib/data";
import { cn } from "@/lib/utils";
import { HarnessMark, StatusPill } from "@/views/harness/parts";

export interface SwitcherTab {
  id: string;
  title: string;
  /** The name a person gave it. */
  name?: string;
  target: string;
  /** open | connecting | reconnecting | exited | error */
  state: string;
}

export interface MoreItem {
  label: string;
  icon?: React.ReactNode;
  onSelect: () => void;
  disabled?: boolean;
  pressed?: boolean;
}

const dot = (s: string) =>
  s === "open"
    ? "succeeded"
    : s === "connecting" || s === "reconnecting"
      ? "queued"
      : s === "error"
        ? "failed"
        : "cancelled";
const targetName = (t: string) =>
  t === "local" ? "server" : t.replace(/^agent:/, "");

function NewMenu({
  targets,
  onOpen,
  onNewAgents,
  trigger,
}: {
  targets: string[];
  onOpen: (t: string) => void;
  onNewAgents: () => void;
  trigger: React.ReactNode;
}) {
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>{trigger}</DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          className="popover w-64"
          sideOffset={6}
          align="end"
        >
          <DropdownMenu.Item className="menu-item" onSelect={onNewAgents}>
            <Bot size={14} className="text-[#b9dccb]" />
            <span className="flex-1">New agents…</span>
          </DropdownMenu.Item>
          <DropdownMenu.Separator className="my-1 h-px bg-white/10" />
          <DropdownMenu.Label className="label px-2.5 pt-1 pb-1">
            New terminal on
          </DropdownMenu.Label>
          {targets.map((t) => (
            <DropdownMenu.Item
              key={t}
              className="menu-item"
              onSelect={() => onOpen(t)}
            >
              {t === "local" ? (
                <SquareTerminal size={14} className="text-[#9a9da6]" />
              ) : (
                <Server size={14} className="text-[#9a9da6]" />
              )}
              <span className="flex-1 truncate">{targetName(t)}</span>
            </DropdownMenu.Item>
          ))}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

/** Every terminal and agent, to jump to; new ones at the bottom. Opens under the bar, in the page's flow. */
function SwitcherSheet({
  onDone,
  tabs,
  activeId,
  onSelect,
  onClose,
  targets,
  onOpenTarget,
  onNewAgents,
}: {
  onDone: () => void;
  tabs: SwitcherTab[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  targets: string[];
  onOpenTarget: (t: string) => void;
  onNewAgents: () => void;
}) {
  const navigate = useNavigate();
  const list = useRes(harnessSessionsRes).data;
  const agents = list.slice().sort((a, b) => Number(b.approvals.length > 0) - Number(a.approvals.length > 0) || b.last_activity - a.last_activity);
  const done = (fn: () => void) => () => {
    onDone();
    fn();
  };
  return (
    <section
      className="sw-sheet sw-sheet-flow"
      aria-label="Sessions"
      onKeyDown={(e) => {
        if (e.key === "Escape") onDone();
      }}
    >
      <p className="sw-sheet-sum">{`${tabs.length} terminal${tabs.length === 1 ? "" : "s"} · ${agents.length} agent${agents.length === 1 ? "" : "s"}`}</p>
      <section>
        <div className="sw-head">Terminals</div>
        {tabs.length === 0 && <div className="sw-empty">No terminals open.</div>}
        <ul>
          {tabs.map((t, i) => (
            <li key={t.id} className={cn("sw-row", t.id === activeId && "sw-current")}>
              <button type="button" className="sw-main" onClick={done(() => onSelect(t.id))}>
                <i className="dot" data-status={dot(t.state)} aria-hidden="true" />
                <span className="sw-num">{String(i + 1).padStart(2, "0")}</span>
                <span className="sw-title">{t.title}</span>
                <span className="sw-sub">{targetName(t.target)}</span>
              </button>
              <button type="button" className="sw-x" aria-label={`Close ${t.title}`} onClick={() => onClose(t.id)}>
                <X size={14} />
              </button>
            </li>
          ))}
        </ul>
      </section>
      <section>
        <div className="sw-head">Agents</div>
        {agents.length === 0 && <div className="sw-empty">No agents running.</div>}
        <ul>
          {agents.map((a) => (
            <li key={a.id} className="sw-row">
              <button type="button" className="sw-main" onClick={done(() => navigate(`/agents/${a.id}`))}>
                <HarnessMark kind={a.kind} name={a.kind_name} size={20} />
                <span className="sw-title">{a.name}</span>
                {a.approvals.length > 0 ? <span className="sw-needs">needs you</span> : <StatusPill status={a.status} className="sw-pill" />}
              </button>
            </li>
          ))}
        </ul>
      </section>
      <div className="sw-new">
        <button type="button" className="sw-new-btn" onClick={done(onNewAgents)}>
          <Bot size={15} /> New agents
        </button>
        {targets.map((t) => (
          <button key={t} type="button" className="sw-new-btn" onClick={done(() => onOpenTarget(t))}>
            <Plus size={15} /> Terminal on {targetName(t)}
          </button>
        ))}
      </div>
    </section>
  );
}

export function MobileBar({
  tabs,
  activeId,
  onSelect,
  onClose,
  targets,
  onOpenTarget,
  onNewAgents,
  kbVisible,
  onToggleKb,
  more,
  renaming = false,
  onRename,
  onRenameDone,
}: {
  tabs: SwitcherTab[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  targets: string[];
  onOpenTarget: (t: string) => void;
  onNewAgents: () => void;
  kbVisible: boolean;
  onToggleKb: () => void;
  more: MoreItem[];
  /** The current session's name is being edited, in the bar's place for it. */
  renaming?: boolean;
  onRename?: (name: string) => void;
  onRenameDone?: () => void;
}) {
  const [sheet, setSheet] = React.useState(false);
  const list = useRes(harnessSessionsRes).data;
  const waiting = list.filter((a) => a.approvals.length > 0).length;
  const idx = tabs.findIndex((t) => t.id === activeId);
  const current = idx >= 0 ? tabs[idx] : null;
  return (
    <>
    <div className="sw-bar">
      {renaming && current ? (
        <div className="sw-current-btn sw-renaming">
          <NameField
            value={current.name ?? ""}
            placeholder={
              current.title.replace(/^[^·]*·\s*/, "") || current.title
            }
            label="Terminal name"
            onSave={(n) => onRename?.(n)}
            onDone={() => onRenameDone?.()}
            className="sw-name-field"
          />
        </div>
      ) : (
        <button
          type="button"
          className="sw-current-btn"
          onClick={() => setSheet(!sheet)}
          aria-label="Switch session"
          aria-expanded={sheet}
        >
          {current ? (
            <i
              className="dot"
              data-status={dot(current.state)}
              aria-hidden="true"
            />
          ) : null}
          <span className="sw-label">
            <span className="sw-title">
              {current
                ? current.title.replace(/^[^·]*·\s*/, "") || current.title
                : "No session"}
            </span>
            <span className="sw-meta">
              {current ? targetName(current.target) : "tap to start one"}
              {tabs.length > 0 && ` · ${idx + 1} of ${tabs.length}`}
            </span>
          </span>
          {waiting > 0 && (
            <span
              className="sw-badge"
              title={`${waiting} agent${waiting === 1 ? "" : "s"} waiting on you`}
            >
              {waiting}
            </span>
          )}
          <ChevronDown size={14} />
        </button>
      )}
      <NewMenu
        targets={targets}
        onOpen={onOpenTarget}
        onNewAgents={onNewAgents}
        trigger={
          <button
            type="button"
            className="sw-icon sw-plus"
            aria-label="New terminal or agents"
          >
            <Plus size={17} />
          </button>
        }
      />
      <button
        type="button"
        className={cn("sw-icon", kbVisible && "sw-on")}
        aria-pressed={kbVisible}
        aria-label="Toggle on-screen keyboard"
        onClick={onToggleKb}
      >
        <Keyboard size={16} />
      </button>
      <DropdownMenu.Root>
        <DropdownMenu.Trigger asChild>
          <button type="button" className="sw-icon" aria-label="More">
            <MoreHorizontal size={17} />
          </button>
        </DropdownMenu.Trigger>
        <DropdownMenu.Portal>
          <DropdownMenu.Content
            className="popover w-60"
            sideOffset={6}
            align="end"
          >
            {more.map((m) => (
              <DropdownMenu.Item
                key={m.label}
                className="menu-item"
                disabled={m.disabled}
                onSelect={m.onSelect}
              >
                {m.icon}
                <span className="flex-1">{m.label}</span>
                {m.pressed !== undefined && (
                  <span className="font-mono text-[10px] text-[#7f838d]">
                    {m.pressed ? "on" : "off"}
                  </span>
                )}
              </DropdownMenu.Item>
            ))}
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu.Root>
    </div>
    {sheet && (
      <SwitcherSheet
        onDone={() => setSheet(false)}
        tabs={tabs}
        activeId={activeId}
        onSelect={onSelect}
        onClose={onClose}
        targets={targets}
        onOpenTarget={onOpenTarget}
        onNewAgents={onNewAgents}
      />
    )}
    </>
  );
}

/** Desktop: "+ New" pinned next to the tabs (never scrolled away), with new agents in it too. */
export function DesktopNew({
  targets,
  onOpenTarget,
  onNewAgents,
}: {
  targets: string[];
  onOpenTarget: (t: string) => void;
  onNewAgents: () => void;
}) {
  return (
    <NewMenu
      targets={targets}
      onOpen={onOpenTarget}
      onNewAgents={onNewAgents}
      trigger={
        <button
          className="flex shrink-0 items-center gap-1.5 border-l border-[#1d1f23] px-3 text-[12px] text-[#a1a4ad] hover:bg-white/[0.04] hover:text-white"
          aria-label="New terminal session"
        >
          <Plus size={14} /> <span>New</span> <ChevronDown size={12} />
        </button>
      }
    />
  );
}
