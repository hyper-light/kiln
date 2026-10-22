/**
 * The command palette: jump to a page, a repository, a run or a terminal. It opens (Ctrl/⌘ K, or
 * Search) as a modal over the page, which doesn't move under it: Escape or a press outside it closes
 * it, and focus goes back where it was. Its pages and actions are there the moment it opens; repositories, recent runs and terminals join them as they
 * arrive (asked for by the action that opens it), so what you type is never lost to a loader.
 */
import * as React from "react";
import { useNavigate } from "@/lib/router";
import { create } from "zustand";
import {
  Activity,
  ArrowUpRight,
  Bot,
  Box,
  CornerDownLeft,
  Cpu,
  FolderSync,
  GitBranch,
  LayoutGrid,
  ListChecks,
  ListTree,
  LogOut,
  Play,
  Rocket,
  Search,
  Server,
  Settings,
  SquareTerminal,
} from "lucide-react";
import { api } from "@/api/client";
import type { Agent, Repo, Run } from "@/api/types";
import { StatusDot } from "@/components/ui/status";
import { relativeTime, shortSha } from "@/lib/format";
import { cn } from "@/lib/utils";

interface PaletteState {
  open: boolean;
  repos: Repo[];
  runs: Run[];
  agents: Agent[];
}

/** Shared by the header's Search button, the hotkey and the palette itself. */
export const usePalette = create<PaletteState>(() => ({ open: false, repos: [], runs: [], agents: [] }));

let loading: AbortController | null = null;
/** What had focus when it opened: it gets it back when the palette closes. */
let returnTo: HTMLElement | null = null;

export function setPaletteOpen(open: boolean) {
  if (open === usePalette.getState().open) return;
  if (open) returnTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  usePalette.setState({ open });
  loading?.abort();
  loading = null;
  if (!open) return;
  // What it lists besides pages and actions, fresh each time it opens.
  const ctl = (loading = new AbortController());
  api.repos(ctl.signal).then(
    (repos) => usePalette.setState({ repos }),
    () => undefined,
  );
  api.runs({ limit: 40 }, ctl.signal).then(
    (runs) => usePalette.setState({ runs }),
    () => undefined,
  );
  api.agents(ctl.signal).then(
    (agents) => usePalette.setState({ agents }),
    () => undefined,
  );
}

/** True when keystrokes belong to a terminal (xterm) and must not be hijacked. */
export function inTerminal(target: EventTarget | null): boolean {
  return target instanceof Element && Boolean(target.closest(".xterm, [data-terminal]"));
}

if (typeof window !== "undefined") {
  window.addEventListener("keydown", (e) => {
    if (inTerminal(e.target)) return; // the PTY owns every key while focused
    if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "k") {
      e.preventDefault();
      setPaletteOpen(!usePalette.getState().open);
    }
  });
}

interface Item {
  id: string;
  group: string;
  label: string;
  hint?: string;
  icon: React.ReactNode;
  keywords?: string;
  run: () => void;
}

function score(q: string, text: string): number {
  if (!q) return 1;
  const t = text.toLowerCase();
  const idx = t.indexOf(q);
  if (idx !== -1) return 100 - idx;
  // subsequence match
  let i = 0;
  for (const ch of t) {
    if (ch === q[i]) i++;
    if (i === q.length) return 10;
  }
  return 0;
}

export function CommandPalette() {
  const open = usePalette((s) => s.open);
  return open ? <Palette /> : null;
}

function Palette() {
  const navigate = useNavigate();
  const repos = usePalette((s) => s.repos);
  const runs = usePalette((s) => s.runs);
  const agents = usePalette((s) => s.agents);
  const [q, setQ] = React.useState("");
  const [active, setActive] = React.useState(0);

  const go = React.useCallback(
    (to: string) => {
      setPaletteOpen(false);
      navigate(to);
    },
    [navigate],
  );

  const items: Item[] = React.useMemo(() => {
    const pages: Item[] = [
      { id: "p-overview", group: "Pages", label: "Overview", icon: <LayoutGrid size={15} />, run: () => go("/") },
      { id: "p-runs", group: "Pages", label: "Runs", icon: <Activity size={15} />, run: () => go("/runs") },
      { id: "p-deployments", group: "Pages", label: "Deployments", hint: "sync and health", icon: <Rocket size={15} />, run: () => go("/deployments") },
      { id: "p-environments", group: "Pages", label: "Environments", hint: "what runs in each, what drifts, what waits to deploy", icon: <Rocket size={15} />, run: () => go("/deployments/environments") },
      { id: "p-releases", group: "Pages", label: "Releases", hint: "apps across environments: deploy, promote, roll back, approve", icon: <Rocket size={15} />, run: () => go("/deployments/releases") },
      { id: "p-deploy-plans", group: "Pages", label: "Deploy plans", hint: "what a branch, commit or pull request would deploy", icon: <GitBranch size={15} />, run: () => go("/deployments/plans") },
      { id: "p-repos", group: "Pages", label: "Repositories", icon: <GitBranch size={15} />, run: () => go("/repos") },
      { id: "p-sync", group: "Pages", label: "Synced folders", hint: "in Repositories: folders kept the same on several machines", icon: <FolderSync size={15} />, run: () => go("/repos?show=synced") },
      { id: "p-runners", group: "Pages", label: "Runners", icon: <Server size={15} />, run: () => go("/runners") },
      { id: "p-plans", group: "Pages", label: "Plans", hint: "kept plans: change, share, run on any agent", icon: <ListChecks size={15} />, run: () => go("/agents/plans") },
      { id: "p-programs", group: "Pages", label: "Programs", hint: "multi-stack programs: each track's worker, goals, pass and usage", icon: <ListChecks size={15} />, run: () => go("/agents/programs") },
      { id: "p-agents", group: "Pages", label: "Agents", hint: "AI sessions", icon: <Bot size={15} />, run: () => go("/agents") },
      { id: "p-term", group: "Pages", label: "Terminal", icon: <SquareTerminal size={15} />, run: () => go("/terminal") },
      { id: "p-metrics", group: "Pages", label: "Metrics", icon: <Cpu size={15} />, run: () => go("/telemetry") },
      { id: "p-system", group: "Pages", label: "System", icon: <ListTree size={15} />, run: () => go("/system") },
      { id: "p-settings", group: "Pages", label: "Settings", icon: <Settings size={15} />, run: () => go("/settings") },
    ];
    const actions: Item[] = [
      { id: "a-term-local", group: "Actions", label: "Open terminal on server", hint: "local", icon: <SquareTerminal size={15} />, run: () => go("/terminal?open=local") },
      { id: "a-new-agent", group: "Actions", label: "New agent", hint: "Claude Code, Codex, Gemini", icon: <Bot size={15} />, run: () => go("/agents?new=1") },
      { id: "a-add-machines", group: "Actions", label: "Add machines", hint: "runners", icon: <Server size={15} />, run: () => go("/runners/add") },
      { id: "a-add-repo", group: "Actions", label: "Add repository", icon: <GitBranch size={15} />, run: () => go("/repos?add=1") },
      {
        id: "a-logout",
        group: "Actions",
        label: "Sign out",
        icon: <LogOut size={15} />,
        run: () => {
          setPaletteOpen(false);
          void api.logout();
        },
      },
    ];
    const repoItems: Item[] = repos.map((r) => ({
      id: `r-${r.id}`,
      group: "Repositories",
      label: r.name,
      hint: r.branches.join(", "),
      icon: <Box size={15} />,
      run: () => go(`/runs?repo=${encodeURIComponent(r.id)}`),
    }));
    const runItems: Item[] = runs.map((r) => ({
      id: `run-${r.id}`,
      group: "Recent runs",
      label: `${r.repo_name} #${r.number}`,
      hint: `${r.branch} · ${shortSha(r.sha)} · ${relativeTime(r.created_at)}`,
      keywords: `${r.message} ${r.pipeline} ${r.author} ${r.sha}`,
      icon: <StatusDot status={r.status} />,
      run: () => go(`/runs/${r.id}`),
    }));
    const agentItems: Item[] = agents
      .filter((a) => a.connected && !a.ephemeral)
      .map((a) => ({
        id: `ag-${a.name}`,
        group: "Terminals",
        label: `Terminal on ${a.name}`,
        hint: a.hostname,
        icon: <Play size={15} />,
        run: () => go(`/terminal?open=${encodeURIComponent(`agent:${a.name}`)}`),
      }));
    return [...pages, ...actions, ...repoItems, ...runItems, ...agentItems];
  }, [repos, runs, agents, go]);

  const query = q.trim().toLowerCase();
  const filtered = React.useMemo(() => {
    const scored = items.map((it) => ({ it, s: score(query, `${it.label} ${it.hint ?? ""} ${it.keywords ?? ""} ${it.group}`) })).filter((x) => x.s > 0);
    if (query) scored.sort((a, b) => b.s - a.s);
    if (query) return scored.slice(0, 40).map((x) => x.it);
    // No query: everything except a long tail of runs.
    let runCount = 0;
    return scored.map((x) => x.it).filter((it) => it.group !== "Recent runs" || runCount++ < 8);
  }, [items, query]);
  const current = Math.min(active, Math.max(0, filtered.length - 1));

  /** Move the highlight, keeping it in view. */
  const move = (to: number) => {
    setActive(to);
    const it = filtered[to];
    if (it) document.getElementById(`cmdk-${it.id}`)?.scrollIntoView({ block: "nearest" });
  };

  // A modal over the page: the page under it can't be reached and doesn't move. Escape or a press on
  // its backdrop (the dialog itself, outside its panel) closes it; focus then goes back where it was.
  const modal = React.useCallback((el: HTMLDialogElement | null) => {
    if (!el) return;
    if (!el.open) el.showModal();
    const onCancel = (e: Event) => {
      e.preventDefault();
      setPaletteOpen(false);
    };
    const onDown = (e: PointerEvent) => {
      if (e.target !== el) return;
      // The press only closes it: its mouse events don't follow, so focus stays where it goes back to.
      e.preventDefault();
      setPaletteOpen(false);
    };
    el.addEventListener("cancel", onCancel);
    el.addEventListener("pointerdown", onDown);
    return () => {
      el.removeEventListener("cancel", onCancel);
      el.removeEventListener("pointerdown", onDown);
      if (el.open) el.close();
      const back = returnTo;
      returnTo = null;
      if (back?.isConnected) back.focus({ preventScroll: true });
    };
  }, []);

  let lastGroup = "";
  return (
    <dialog ref={modal} className="palette-dialog" aria-label="Command palette">
      <div className="palette-inner" role="search">
        <div className="relative">
          <Search size={17} className="absolute top-1/2 left-[18px] -translate-y-1/2 text-[#8f939e]" aria-hidden="true" />
          <input
            autoFocus
            className="cmdk-input"
            placeholder="Jump to a page, repo, run, or terminal…"
            value={q}
            onChange={(e) => {
              setQ(e.target.value);
              setActive(0);
            }}
            role="combobox"
            aria-expanded="true"
            aria-controls="cmdk-list"
            aria-activedescendant={filtered[current] ? `cmdk-${filtered[current].id}` : undefined}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                move(Math.min(filtered.length - 1, current + 1));
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                move(Math.max(0, current - 1));
              } else if (e.key === "Enter") {
                e.preventDefault();
                filtered[current]?.run();
              } else if (e.key === "Escape") {
                e.preventDefault();
                setPaletteOpen(false);
              } else if (e.key === "Tab") {
                // Its field is all there is to it (the list goes by the arrows): Tab stays here,
                // where a modal dialog would otherwise let it out to the browser's own stops.
                e.preventDefault();
              }
            }}
          />
        </div>
        <div id="cmdk-list" role="listbox" className="palette-list">
          {filtered.length === 0 && <p className="empty-line px-4 py-10 text-center">No matches.</p>}
          {filtered.map((it, i) => {
            const header = it.group !== lastGroup ? it.group : null;
            lastGroup = it.group;
            return (
              <React.Fragment key={it.id}>
                {header && <div className="label px-3.5 pt-3 pb-1.5">{header}</div>}
                <div id={`cmdk-${it.id}`} role="option" aria-selected={i === current} className="cmdk-item" onMouseMove={() => i !== current && setActive(i)} onClick={() => it.run()}>
                  <span className="grid w-4 place-items-center text-[#9a9da6]">{it.icon}</span>
                  <span className="min-w-0 truncate">{it.label}</span>
                  {it.hint && <span className="min-w-0 flex-1 truncate font-mono text-[10px] text-[#7f838d]">{it.hint}</span>}
                  <span className={cn("ml-auto text-[#8f939e]", i === current ? "opacity-100" : "opacity-0")}>
                    {it.group === "Repositories" ? <ArrowUpRight size={14} /> : <CornerDownLeft size={13} />}
                  </span>
                </div>
              </React.Fragment>
            );
          })}
        </div>
        <div className="flex items-center gap-4 border-t border-[#22262d] px-4 py-2.5 font-mono text-[9px] tracking-[0.08em] text-[#7f838d] uppercase">
          <span>↑↓ navigate</span>
          <span>↵ open</span>
          <span>esc close</span>
        </div>
      </div>
    </dialog>
  );
}
