import * as React from "react";
import { Dialog } from "radix-ui";
import { useNavigate } from "react-router";
import {
  Activity,
  ArrowUpRight,
  Bot,
  Box,
  CornerDownLeft,
  GitBranch,
  LayoutGrid,
  LogOut,
  Play,
  Search,
  Server,
  Settings,
  SquareTerminal,
  Cpu,
  ListTree,
} from "lucide-react";
import { api } from "@/api/client";
import type { Agent, Repo, Run } from "@/api/types";
import { StatusDot } from "@/components/ui/status";
import { relativeTime, shortSha } from "@/lib/format";
import { cn } from "@/lib/utils";

/* tiny store so the header button and the hotkey share open state */
let open = false;
const subs = new Set<() => void>();
export function setPaletteOpen(v: boolean) {
  open = v;
  subs.forEach((s) => s());
}
function usePaletteOpen() {
  return React.useSyncExternalStore(
    (cb) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },
    () => open,
  );
}

/** True when keystrokes belong to a terminal (xterm) and must not be hijacked. */
export function inTerminal(target: EventTarget | null): boolean {
  return target instanceof Element && Boolean(target.closest(".xterm, [data-terminal]"));
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
  const isOpen = usePaletteOpen();
  const navigate = useNavigate();
  const [q, setQ] = React.useState("");
  const [active, setActive] = React.useState(0);
  const [repos, setRepos] = React.useState<Repo[]>([]);
  const [runs, setRuns] = React.useState<Run[]>([]);
  const [agents, setAgents] = React.useState<Agent[]>([]);
  const listRef = React.useRef<HTMLDivElement>(null);

  React.useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (inTerminal(e.target)) return; // the PTY owns every key while focused
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPaletteOpen(!open);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  React.useEffect(() => {
    if (!isOpen) return;
    setQ("");
    setActive(0);
    const ctl = new AbortController();
    api.repos(ctl.signal).then(setRepos).catch(() => undefined);
    api.runs({ limit: 40 }, ctl.signal).then(setRuns).catch(() => undefined);
    api.agents(ctl.signal).then(setAgents).catch(() => undefined);
    return () => ctl.abort();
  }, [isOpen]);

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
      { id: "p-repos", group: "Pages", label: "Repositories", icon: <GitBranch size={15} />, run: () => go("/repos") },
      { id: "p-runners", group: "Pages", label: "Runners", icon: <Server size={15} />, run: () => go("/runners") },
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
    const scored = items
      .map((it) => ({ it, s: score(query, `${it.label} ${it.hint ?? ""} ${it.keywords ?? ""} ${it.group}`) }))
      .filter((x) => x.s > 0);
    if (query) scored.sort((a, b) => b.s - a.s);
    if (query) return scored.slice(0, 40).map((x) => x.it);
    // No query: everything except a long tail of runs.
    let runCount = 0;
    return scored
      .map((x) => x.it)
      .filter((it) => it.group !== "Recent runs" || runCount++ < 8);
  }, [items, query]);

  React.useEffect(() => setActive(0), [query]);
  React.useEffect(() => {
    listRef.current?.querySelector(`[data-idx="${active}"]`)?.scrollIntoView({ block: "nearest" });
  }, [active]);

  let lastGroup = "";
  return (
    <Dialog.Root open={isOpen} onOpenChange={setPaletteOpen}>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className="dialog !top-[10vh] !w-[min(640px,calc(100vw-24px))] !p-0" aria-describedby={undefined}>
          <Dialog.Title className="sr-only">Command palette</Dialog.Title>
          <div className="relative">
            <Search size={17} className="absolute top-1/2 left-[18px] -translate-y-1/2 text-[#8f939e]" aria-hidden="true" />
            <input
              autoFocus
              className="cmdk-input"
              placeholder="Jump to a page, repo, run, or terminal…"
              value={q}
              onChange={(e) => setQ(e.target.value)}
              role="combobox"
              aria-expanded="true"
              aria-controls="cmdk-list"
              aria-activedescendant={filtered[active] ? `cmdk-${filtered[active].id}` : undefined}
              onKeyDown={(e) => {
                if (e.key === "ArrowDown") {
                  e.preventDefault();
                  setActive((a) => Math.min(filtered.length - 1, a + 1));
                } else if (e.key === "ArrowUp") {
                  e.preventDefault();
                  setActive((a) => Math.max(0, a - 1));
                } else if (e.key === "Enter") {
                  e.preventDefault();
                  filtered[active]?.run();
                }
              }}
            />
          </div>
          <div ref={listRef} id="cmdk-list" role="listbox" className="max-h-[52vh] overflow-y-auto p-2">
            {filtered.length === 0 && <div className="px-4 py-10 text-center text-[13px] text-[#8f939e]">No matches.</div>}
            {filtered.map((it, i) => {
              const header = it.group !== lastGroup ? it.group : null;
              lastGroup = it.group;
              return (
                <React.Fragment key={it.id}>
                  {header && <div className="label px-3.5 pt-3 pb-1.5">{header}</div>}
                  <div
                    id={`cmdk-${it.id}`}
                    data-idx={i}
                    role="option"
                    aria-selected={i === active}
                    className="cmdk-item"
                    onMouseMove={() => setActive(i)}
                    onClick={() => it.run()}
                  >
                    <span className="grid w-4 place-items-center text-[#9a9da6]">{it.icon}</span>
                    <span className="min-w-0 truncate">{it.label}</span>
                    {it.hint && <span className="min-w-0 flex-1 truncate font-mono text-[10px] text-[#7f838d]">{it.hint}</span>}
                    <span className={cn("ml-auto text-[#8f939e]", i === active ? "opacity-100" : "opacity-0")}>
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
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
