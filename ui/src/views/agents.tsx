import * as React from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { Bot, FileText, Plus, SquareTerminal } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent, AiSession, AiTranscriptEntry } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { EmptyState, ErrorState, Facts, PageIntro, SearchField, Skeleton, SkeletonRows } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { absoluteTime } from "@/lib/format";
import { errorMessage, useDocumentTitle, useLocalStorage, useResource } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { RelTime } from "./shared";

type Filter = "all" | "kiln" | "elsewhere";

interface Tool {
  id: string;
  label: string;
  /** The program a new session runs. */
  command: string;
}

const CLAUDE: Tool = { id: "claude", label: "Claude Code", command: "claude" };
const TOOLS: Tool[] = [CLAUDE, { id: "codex", label: "Codex", command: "codex" }, { id: "gemini", label: "Gemini CLI", command: "gemini" }];

const toolLabel = (tool: string) => TOOLS.find((t) => t.id === tool)?.label ?? tool;
const sessionKey = (s: AiSession) => `${s.machine}:${s.pid}`;
const dotStatus = (s: AiSession) => (s.state === "working" ? "running" : s.state === "waiting" ? "timed_out" : "skipped");
const stateLabel = (s: AiSession) => (s.state === "working" ? "working" : s.state === "waiting" ? "waiting for you" : "running");
const terminalLink = (s: AiSession) =>
  `/terminal?session=${encodeURIComponent(s.terminal ?? "")}&target=${encodeURIComponent(`agent:${s.machine}`)}`;

/** A home directory as "~". */
function tildify(path: string) {
  return path.replace(/^\/(?:home|Users)\/[^/]+(?=\/|$)/, "~");
}

/** What a new agent's terminal runs: the tool in its folder, then a login shell once it exits. */
function launchCommand(command: string, dir: string, windows: boolean) {
  if (windows) return command;
  const d = dir.trim() || "~";
  const quote = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;
  const cd = d === "~" ? "cd ~" : d.startsWith("~/") ? `cd ~/${quote(d.slice(2))}` : `cd ${quote(d)}`;
  return `${cd} && ${command}; exec $SHELL -l`;
}

/** Call `fn` at most once per `ms`, trailing. */
function useThrottled(fn: () => void, ms: number) {
  const latest = React.useRef(fn);
  const timer = React.useRef<number | null>(null);
  React.useEffect(() => {
    latest.current = fn;
  });
  React.useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );
  return React.useCallback(() => {
    if (timer.current !== null) return;
    timer.current = window.setTimeout(() => {
      timer.current = null;
      latest.current();
    }, ms);
  }, [ms]);
}

/** Live AI coding sessions across machines: kiln-hosted ones to drive, others to watch. */
export default function Agents() {
  useDocumentTitle("Agents");
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const sessions = useResource((s) => api.aiSessions(s), [], { pollMs: 20_000 });
  const silent = useResource((s) => api.aiSilent(s), [], { pollMs: 30_000 });
  const [filter, setFilter] = React.useState<Filter>("all");
  const [q, setQ] = React.useState("");
  const [newOpen, setNewOpen] = React.useState(params.get("new") === "1");
  const selected = params.get("session");

  // Links from before CI machines were called runners.
  React.useEffect(() => {
    if (params.has("agent") || params.has("join")) navigate(`/runners?${params.toString()}`, { replace: true });
  }, [params, navigate]);
  React.useEffect(() => {
    if (params.get("new") === "1") {
      setNewOpen(true);
      const n = new URLSearchParams(params);
      n.delete("new");
      setParams(n, { replace: true });
    }
  }, [params, setParams]);

  const reload = useThrottled(sessions.reload, 1000);
  const reloadSilent = useThrottled(silent.reload, 2000);
  useServerEvents((ev) => {
    if (ev.type === "ai_sessions" || (ev.type === "agent" && !ev.agent.connected)) reload();
    if (ev.type === "ai_sessions" || ev.type === "agent") reloadSilent();
  });
  const quiet = silent.data?.machines ?? [];

  const all = sessions.data ?? [];
  const counts: Record<Filter, number> = {
    all: all.length,
    kiln: all.filter((s) => s.terminal).length,
    elsewhere: all.filter((s) => !s.terminal).length,
  };
  const ql = q.trim().toLowerCase();
  const list = all
    .filter((s) => (filter === "kiln" ? Boolean(s.terminal) : filter === "elsewhere" ? !s.terminal : true))
    .filter((s) => !ql || `${s.title} ${s.name} ${s.cwd} ${s.branch} ${s.machine} ${toolLabel(s.tool)} ${s.last_prompt}`.toLowerCase().includes(ql));
  const working = all.filter((s) => s.state === "working").length;
  const waiting = all.filter((s) => s.state === "waiting").length;
  const open = all.find((s) => sessionKey(s) === selected) ?? null;

  const select = (s: AiSession | null) => {
    const n = new URLSearchParams(params);
    if (s) n.set("session", sessionKey(s));
    else n.delete("session");
    setParams(n, { replace: true });
  };

  return (
    <div className="container-console page">
      <PageIntro
        index={5}
        label="Agents"
        title="Live sessions"
        lead={
          <>
            {sessions.data ? `${all.length} running · ${working} working · ${waiting} waiting for you. ` : ""}
            Claude Code, Codex and Gemini sessions on your machines. Ones started in kiln can be driven from any device; ones started elsewhere show a
            live, read-only transcript.
          </>
        }
        actions={
          <Button onClick={() => setNewOpen(true)}>
            <Plus /> New agent
          </Button>
        }
      />

      <div className="mb-5 flex flex-wrap items-center gap-3">
        <Segmented
          label="Filter sessions"
          value={filter}
          onChange={setFilter}
          options={[
            { value: "all", label: "All", count: counts.all },
            { value: "kiln", label: "In kiln", count: counts.kiln },
            { value: "elsewhere", label: "Elsewhere", count: counts.elsewhere },
          ]}
        />
        <SearchField className="ml-auto w-full min-w-0 sm:w-64" label="Search sessions" placeholder="Title, folder, machine…" value={q} onChange={setQ} />
      </div>
      {quiet.length > 0 && (
        <p className="mb-5 text-[12px] leading-relaxed text-[#8f939e]">
          Not reporting sessions: <span className="font-mono text-[#c8cad0]">{quiet.join(", ")}</span>. {quiet.length === 1 ? "It runs" : "They run"} an older
          kiln; {quiet.length === 1 ? "its" : "their"} sessions show up here once {quiet.length === 1 ? "it is" : "they are"} updated.
        </p>
      )}

      {sessions.error && !sessions.data ? (
        <ErrorState error={sessions.error} onRetry={sessions.reload} />
      ) : !sessions.data ? (
        <SkeletonRows rows={4} />
      ) : all.length === 0 ? (
        <EmptyState
          icon={<Bot size={24} />}
          title="No AI sessions are running."
          action={
            <Button variant="secondary" onClick={() => setNewOpen(true)}>
              <Plus /> New agent
            </Button>
          }
        >
          Start Claude Code, Codex or Gemini here and drive it from any device. Sessions you start in your own terminals on a machine with a kiln runner
          show up here too.
        </EmptyState>
      ) : list.length === 0 ? (
        <EmptyState
          title="No sessions match."
          action={
            <Button
              variant="secondary"
              onClick={() => {
                setFilter("all");
                setQ("");
              }}
            >
              Clear filters
            </Button>
          }
        />
      ) : (
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-2 2xl:grid-cols-3">
          {list.map((s) => (
            <SessionCard key={sessionKey(s)} s={s} onTranscript={() => select(s)} />
          ))}
        </div>
      )}

      <NewAgentDialog open={newOpen} onOpenChange={setNewOpen} />
      <TranscriptDrawer session={open} onClose={() => select(null)} />
    </div>
  );
}

function SessionCard({ s, onTranscript }: { s: AiSession; onTranscript: () => void }) {
  return (
    <article className="card flex min-w-0 flex-col" data-live={s.state === "working" ? "true" : undefined}>
      <div className="flex min-w-0 items-center gap-2.5 px-4 pt-4">
        <StatusDot status={dotStatus(s)} title={stateLabel(s)} />
        <span className="shrink-0 font-mono text-[10px] tracking-[0.06em] text-[#a1a4ad] uppercase">{toolLabel(s.tool)}</span>
        <span className="min-w-0 truncate font-mono text-[10px] text-[#7b7e88]">· {s.machine}</span>
        <span className="ml-auto shrink-0 font-mono text-[10px] text-[#7b7e88]">
          <RelTime ts={s.last_activity} />
        </span>
      </div>
      <h3 className="mt-2 line-clamp-2 px-4 text-[15px] font-[480] tracking-[-0.3px] break-words text-[#e4e5e7]">{s.title || s.name || toolLabel(s.tool)}</h3>
      <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 px-4 font-mono text-[10.5px] text-[#8f939e]">
        <span className="max-w-full min-w-0 truncate" title={s.cwd}>
          {tildify(s.cwd) || "—"}
        </span>
        {s.branch && <span className="chip">{s.branch}</span>}
        {s.name && s.title && <span className="chip">{s.name}</span>}
      </div>
      {s.last_prompt && (
        <p className="mt-3 line-clamp-2 px-4 text-[12.5px] break-words text-[#cdc4e0]">
          <span className="text-[#6d7078] select-none">› </span>
          {s.last_prompt}
        </p>
      )}
      {s.last_reply && <p className="mt-1.5 line-clamp-3 px-4 text-[12.5px] leading-relaxed break-words text-[#a1a4ad]">{s.last_reply}</p>}
      <div className="mt-auto flex flex-wrap items-center gap-2 px-4 pt-4 pb-4">
        {s.terminal ? (
          <Button asChild size="sm">
            <Link to={terminalLink(s)}>
              <SquareTerminal /> Open terminal
            </Link>
          </Button>
        ) : (
          <span className="font-mono text-[10px] text-[#7b7e88]">started outside kiln · watch only</span>
        )}
        {s.transcript && (
          <Button variant="ghost" size="sm" onClick={onTranscript}>
            <FileText /> Transcript
          </Button>
        )}
        <span className={cn("ml-auto font-mono text-[10px]", s.state === "waiting" ? "text-[#cbb69a]" : "text-[#7b7e88]")}>{stateLabel(s)}</span>
      </div>
    </article>
  );
}

function TranscriptDrawer({ session, onClose }: { session: AiSession | null; onClose: () => void }) {
  const [entries, setEntries] = React.useState<AiTranscriptEntry[] | null>(null);
  const [err, setErr] = React.useState<string | null>(null);
  const scroller = React.useRef<HTMLDivElement>(null);
  const atBottom = React.useRef(true);
  const machine = session?.machine;
  const pid = session?.pid;
  const activity = session?.last_activity;

  React.useEffect(() => {
    setEntries(null);
    setErr(null);
    atBottom.current = true;
  }, [machine, pid]);

  // Refetched whenever the machine reports new activity in this session.
  React.useEffect(() => {
    if (!machine || pid === undefined) return;
    const ctl = new AbortController();
    api
      .aiTranscript(machine, pid, ctl.signal)
      .then((e) => {
        setEntries(e);
        setErr(null);
      })
      .catch((e) => !ctl.signal.aborted && setErr(errorMessage(e)));
    return () => ctl.abort();
  }, [machine, pid, activity]);

  // Follow new messages unless the reader scrolled up.
  React.useLayoutEffect(() => {
    const el = scroller.current;
    if (el && atBottom.current) el.scrollTop = el.scrollHeight;
  }, [entries]);

  return (
    <Dialog open={Boolean(session)} onOpenChange={(o) => !o && onClose()}>
      {session && (
        <DialogContent
          variant="drawer"
          title={session.title || session.name || toolLabel(session.tool)}
          eyebrow={`${toolLabel(session.tool)} · ${session.machine} · ${stateLabel(session)}`}
          description={tildify(session.cwd)}
        >
          <div className="flex flex-wrap gap-2">
            {session.terminal ? (
              <Button asChild size="sm">
                <Link to={terminalLink(session)}>
                  <SquareTerminal /> Open terminal
                </Link>
              </Button>
            ) : (
              <p className="text-[12px] leading-relaxed text-[#8f939e]">Started outside kiln, so it can be watched here but not typed into.</p>
            )}
          </div>
          <div className="mt-5">
            <Facts
              items={[
                { label: "Model", value: session.model || "—" },
                { label: "Branch", value: session.branch || "—" },
                { label: "Started", value: absoluteTime(session.started_at) },
                { label: "Last activity", value: <RelTime ts={session.last_activity} /> },
                { label: "Process", value: `pid ${session.pid}` },
                ...(session.id ? [{ label: "Session", value: <span className="break-all">{session.id}</span> }] : []),
              ]}
            />
          </div>
          <div className="mt-6">
            <div className="label mb-2">Transcript</div>
            {err ? (
              <ErrorState error={err} compact />
            ) : !entries ? (
              <Skeleton className="h-64 w-full" />
            ) : entries.length === 0 ? (
              <p className="text-[12.5px] text-[#8f939e]">Nothing said yet.</p>
            ) : (
              <div
                ref={scroller}
                className="max-h-[62dvh] space-y-2 overflow-y-auto overscroll-contain pr-1"
                onScroll={(e) => {
                  const el = e.currentTarget;
                  atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
                }}
              >
                {entries.map((e, i) => (
                  <EntryView key={i} e={e} />
                ))}
              </div>
            )}
          </div>
        </DialogContent>
      )}
    </Dialog>
  );
}

function EntryView({ e }: { e: AiTranscriptEntry }) {
  if (e.role === "tool") {
    return (
      <div className="truncate font-mono text-[11px] text-[#8c8f98]" title={e.text}>
        <span className="text-[#6d7078]">⏵ </span>
        {e.text}
      </div>
    );
  }
  const user = e.role === "user";
  return (
    <div
      className={cn(
        "rounded-md text-[12.5px] leading-relaxed break-words whitespace-pre-wrap",
        user ? "border border-[#2b2e39] bg-[#101217] px-3 py-2 text-[#e4e5e7]" : "px-1 py-1 text-[#c8cad0]",
      )}
    >
      {user && <div className="label mb-1">Prompt</div>}
      {e.text}
    </div>
  );
}

function NewAgentDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (o: boolean) => void }) {
  const navigate = useNavigate();
  const [machines, setMachines] = React.useState<Agent[] | null>(null);
  const [machine, setMachine] = useLocalStorage<string>("kiln.agents.new.machine", "");
  const [toolId, setToolId] = useLocalStorage<string>("kiln.agents.new.tool", CLAUDE.id);
  const [dirs, setDirs] = useLocalStorage<Record<string, string>>("kiln.agents.new.dirs", {});
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);

  React.useEffect(() => {
    if (!open) return;
    setErr(null);
    const ctl = new AbortController();
    api
      .agents(ctl.signal)
      .then((l) => setMachines(l.filter((a) => a.connected && !a.ephemeral)))
      .catch((e) => !ctl.signal.aborted && setErr(errorMessage(e)));
    return () => ctl.abort();
  }, [open]);

  const target = machines?.find((m) => m.name === machine) ?? machines?.[0] ?? null;
  const tool = TOOLS.find((t) => t.id === toolId) ?? CLAUDE;
  const windows = target?.os === "windows";
  const dir = target ? (dirs[target.name] ?? "~") : "~";

  const start = async () => {
    if (!target) return;
    setBusy(true);
    setErr(null);
    try {
      const info = await api.createTerminalSession({ target: `agent:${target.name}`, cmd: launchCommand(tool.command, dir, windows) });
      onOpenChange(false);
      navigate(`/terminal?session=${encodeURIComponent(info.id)}&target=${encodeURIComponent(info.target)}`);
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title="New agent"
        eyebrow="Agents / New"
        description="Runs in a persistent kiln terminal: it keeps going when you close the tab or lose the connection, and any device can pick it up again."
        className="!w-[min(560px,calc(100vw-24px))]"
      >
        {machines === null && !err ? (
          <Skeleton className="h-40 w-full" />
        ) : machines !== null && machines.length === 0 ? (
          <p className="text-[12.5px] leading-relaxed text-[#8f939e]">
            No runner is connected. Agents run on runners;{" "}
            <Link to="/runners?join=1" className="text-link" onClick={() => onOpenChange(false)}>
              add one
            </Link>{" "}
            first.
          </p>
        ) : (
          <form
            className="space-y-5"
            onSubmit={(e) => {
              e.preventDefault();
              void start();
            }}
          >
            <div>
              <span className="field-label label">Agent</span>
              <Segmented label="Agent" value={tool.id} onChange={setToolId} options={TOOLS.map((t) => ({ value: t.id, label: t.label }))} />
            </div>
            <label className="block">
              <span className="field-label label">Machine</span>
              <span className="field">
                <select
                  className="min-h-[38px] w-full min-w-0 bg-transparent pr-3 text-[12.5px] text-[#e3e4e9] outline-none"
                  value={target?.name ?? ""}
                  onChange={(e) => setMachine(e.target.value)}
                >
                  {(machines ?? []).map((m) => (
                    <option key={m.name} value={m.name} className="bg-[#101217]">
                      {m.name} · {m.hostname} · {m.os}/{m.arch}
                    </option>
                  ))}
                </select>
              </span>
            </label>
            {!windows && (
              <label className="block">
                <span className="field-label label">Folder</span>
                <span className="field">
                  <input
                    value={dir}
                    onChange={(e) => target && setDirs({ ...dirs, [target.name]: e.target.value })}
                    placeholder="~ or /path/to/project"
                    autoCapitalize="off"
                    autoCorrect="off"
                    spellCheck={false}
                    className="font-mono"
                  />
                </span>
                <span className="mt-1.5 block text-[11.5px] leading-relaxed text-[#8f939e]">
                  Where {tool.label} works. When it exits, the terminal stays open there.
                </span>
              </label>
            )}
            {err && <ErrorState error={err} compact />}
            <div className="flex justify-end gap-2">
              <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
                Cancel
              </Button>
              <Button type="submit" disabled={busy || !target}>
                <Bot /> {busy ? "Starting…" : `Start ${tool.label}`}
              </Button>
            </div>
          </form>
        )}
      </DialogContent>
    </Dialog>
  );
}
