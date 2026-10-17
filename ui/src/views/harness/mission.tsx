/**
 * Mission control: every agent kiln runs, on every machine, at a glance — what each is doing, how
 * far along it is, what it changed, what it costs — with one inbox for everything waiting on you
 * and messages to one agent or many at once.
 */
import * as React from "react";
import "./harness.css";
import { Link, useNavigate, useSearchParams } from "react-router";
import { Check, CheckSquare, Plus, Send, Square, X } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import { clientId, harnessApi, type AgentInfo, type ApprovalBrief, type SendMode } from "@/api/harness";
import type { AiSession } from "@/api/types";
import { Button } from "@/components/ui/button";
import { EmptyState, ErrorState, PageIntro, SkeletonRows } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { errorMessage, useDocumentTitle, useLocalStorage, useNow } from "@/lib/hooks";
import { relativeTime } from "@/lib/format";
import { money, plain, tildify } from "./derive";
import { DiffStat } from "./diff";
import { HarnessMark, StatusPill, ToolIcon } from "./parts";
import { AccountLimits } from "./limits";
import { SpawnDialog } from "./spawn";

type Group = "status" | "shell" | "machine";

const machineOf = (a: AgentInfo) => (a.target === "local" ? "server" : a.target.replace(/^agent:/, ""));
const ORDER: Record<AgentInfo["status"], number> = { waiting: 0, running: 1, starting: 2, idle: 3, disconnected: 4, exited: 5 };

function useAgents() {
  const [list, setList] = React.useState<AgentInfo[] | null>(null);
  const [error, setError] = React.useState<Error | null>(null);
  const load = React.useCallback(() => {
    harnessApi.list().then(
      (l) => {
        setList(l);
        setError(null);
      },
      (e: unknown) => setError(e instanceof Error ? e : new Error(String(e))),
    );
  }, []);
  React.useEffect(load, [load]);
  useServerEvents((ev) => {
    if (ev.type === "harness")
      setList((l) => {
        const cur = l ?? [];
        const i = cur.findIndex((a) => a.id === ev.info.id);
        if (i === -1) return [...cur, ev.info];
        const next = cur.slice();
        next[i] = ev.info;
        return next;
      });
    else if (ev.type === "harness_closed") setList((l) => (l ?? []).filter((a) => a.id !== ev.id));
  });
  return { list, error, reload: load };
}

function InboxRow({ agent, a }: { agent: AgentInfo; a: ApprovalBrief }) {
  const [busy, setBusy] = React.useState(false);
  const decide = async (decision: "allow" | "deny") => {
    setBusy(true);
    try {
      await harnessApi.command(agent.id, { type: "approve", id: a.id, decision });
    } catch (e) {
      toast.error("Couldn't answer", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const simple = a.tool !== "question" && a.tool !== "plan";
  return (
    <li className="hx-inbox-row">
      <HarnessMark kind={agent.kind} name={agent.kind_name} size={20} />
      <span className="hx-inbox-who">{agent.name}</span>
      <ToolIcon kind={a.tool} size={13} className="text-[var(--color-warn)]" />
      <Link to={`/agents/${agent.id}`} className="hx-inbox-what">
        {a.title}
      </Link>
      <span className="hx-inbox-actions">
        {simple && (
          <>
            <Button size="xs" disabled={busy} onClick={() => decide("allow")}>
              <Check /> Allow
            </Button>
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => decide("deny")} aria-label="Deny">
              <X />
            </Button>
          </>
        )}
        <Button size="xs" variant="secondary" asChild>
          <Link to={`/agents/${agent.id}`}>{simple ? "Review" : "Answer"}</Link>
        </Button>
      </span>
    </li>
  );
}

const AgentCard = React.memo(function AgentCard({ a, selected, onSelect, now }: { a: AgentInfo; selected: boolean; onSelect: (id: string, on: boolean) => void; now: number }) {
  const navigate = useNavigate();
  const [msg, setMsg] = React.useState("");
  const send = async () => {
    const text = msg.trim();
    if (!text) return;
    try {
      await harnessApi.command(a.id, { type: "send", id: clientId(), text, mode: "queue" });
      setMsg("");
    } catch (e) {
      toast.error(`Couldn't reach ${a.name}`, errorMessage(e));
    }
  };
  const total = a.todos_total ?? 0;
  const done = a.todos_done ?? 0;
  const waiting = a.approvals.length;
  return (
    <article className="hx-card" data-status={a.status} data-selected={selected || undefined}>
      <div className="hx-card-top">
        <button type="button" className="hx-card-check" aria-pressed={selected} aria-label={selected ? `Deselect ${a.name}` : `Select ${a.name}`} onClick={() => onSelect(a.id, !selected)}>
          {selected ? <CheckSquare size={15} /> : <Square size={15} />}
        </button>
        <HarnessMark kind={a.kind} name={a.kind_name} size={24} />
        <button type="button" className="hx-card-name" onClick={() => navigate(`/agents/${a.id}`)}>
          <span className="hx-trunc">{a.name}</span>
          <span className="hx-card-where hx-trunc">
            {machineOf(a)} · {tildify(a.cwd)}
          </span>
        </button>
        <StatusPill status={a.status} className="ml-auto" />
      </div>

      <button type="button" className="hx-card-body" onClick={() => navigate(`/agents/${a.id}`)}>
        <div className="hx-card-goal">{a.goal || a.title || "No task yet"}</div>
        {a.status === "running" && (a.doing || a.activity) ? (
          <div className="hx-card-now">
            <span className="hx-live-dot" aria-hidden="true" />
            <span className="hx-trunc">{a.doing || a.activity}</span>
          </div>
        ) : a.summary ? (
          <div className="hx-card-summary">{plain(a.summary)}</div>
        ) : null}
        {waiting > 0 && (
          <div className="hx-card-wait">
            <ToolIcon kind={a.approvals[0]?.tool ?? "other"} size={12} />
            <span className="hx-trunc">{a.approvals[0]?.title}</span>
            {waiting > 1 && <span>+{waiting - 1}</span>}
          </div>
        )}
      </button>

      {total > 0 && (
        <div className="hx-card-plan" title={`${done} of ${total} steps done`}>
          <div className="hx-progress">
            <span style={{ width: `${(done / total) * 100}%` }} />
          </div>
          <span className="hx-meta-num">
            {done}/{total}
          </span>
        </div>
      )}

      <div className="hx-card-meters">
        {(a.files_changed ?? 0) > 0 && (
          <span>
            {a.files_changed} file{a.files_changed === 1 ? "" : "s"} <DiffStat add={a.lines_added ?? 0} del={a.lines_removed ?? 0} />
          </span>
        )}
        {a.tasks_running > 0 && (
          <span>
            {a.tasks_running} subagent{a.tasks_running === 1 ? "" : "s"}
          </span>
        )}
        {a.queued > 0 && <span>{a.queued} queued</span>}
        {(a.context_pct ?? 0) >= 1 && <span>context {Math.round(a.context_pct ?? 0)}%</span>}
        {a.cost_usd > 0 && <span>{money(a.cost_usd)}</span>}
        <span className="ml-auto">{relativeTime(Math.floor(a.last_activity / 1000), now)}</span>
      </div>

      <div className="hx-card-say">
        <input
          className="hx-input"
          value={msg}
          onChange={(e) => setMsg(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void send();
            }
          }}
          placeholder={a.status === "running" ? "Queue a message…" : "Tell it…"}
          aria-label={`Message ${a.name}`}
        />
        <button type="button" className="hx-icon" aria-label="Send" disabled={!msg.trim()} onClick={() => void send()}>
          <Send size={13} />
        </button>
      </div>
    </article>
  );
});

function Broadcast({ ids, agents, onDone }: { ids: string[]; agents: AgentInfo[]; onDone: () => void }) {
  const [text, setText] = React.useState("");
  const [mode, setMode] = React.useState<SendMode>("queue");
  const [busy, setBusy] = React.useState(false);
  const send = async () => {
    const body = text.trim();
    if (!body) return;
    setBusy(true);
    let failed = ids.length;
    try {
      const r = await harnessApi.broadcast(ids, { type: "send", id: clientId(), text: body, mode });
      failed = ids.length - r.sent.length;
    } catch {
      /* counted as failed */
    }
    setBusy(false);
    if (failed) toast.error(`${failed} of ${ids.length} didn't get it`, "Their machines may be reconnecting.");
    else toast.success(`Sent to ${ids.length} agent${ids.length === 1 ? "" : "s"}`);
    setText("");
    onDone();
  };
  const names = agents.filter((a) => ids.includes(a.id)).map((a) => a.name);
  return (
    <div className="hx-broadcast" role="region" aria-label="Message the selected agents">
      <div className="hx-broadcast-head">
        <span className="hx-eyebrow">To {ids.length} agents</span>
        <span className="hx-dim hx-trunc">{names.join(", ")}</span>
        <button type="button" className="hx-icon ml-auto" aria-label="Clear selection" onClick={onDone}>
          <X size={13} />
        </button>
      </div>
      <div className="hx-compose-box">
        <textarea className="hx-compose-input" rows={2} value={text} onChange={(e) => setText(e.target.value)} placeholder="Message every selected agent" />
        <div className="hx-compose-bar">
          <div className="hx-modes" role="radiogroup" aria-label="How to send">
            {(["queue", "steer", "now"] as SendMode[]).map((m) => (
              <button key={m} type="button" role="radio" aria-checked={mode === m} className="hx-mode" data-mode={m} onClick={() => setMode(m)}>
                {m === "queue" ? "Queue" : m === "steer" ? "Steer" : "Now"}
              </button>
            ))}
          </div>
          <span className="flex-1" />
          <Button size="sm" onClick={send} disabled={busy || !text.trim()}>
            <Send /> Send to {ids.length}
          </Button>
        </div>
      </div>
    </div>
  );
}

export default function Mission() {
  useDocumentTitle("Agents");
  const { list, error, reload } = useAgents();
  const [found, setFound] = React.useState<AiSession[]>([]);
  const [group, setGroup] = useLocalStorage<Group>("kiln.agents.group", "status");
  const [spawn, setSpawn] = React.useState(false);
  const [selected, setSelected] = React.useState<Set<string>>(new Set());
  const now = useNow();
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  // ?new=1 (links from elsewhere): open New agents.
  React.useEffect(() => {
    if (params.get("new") !== "1") return;
    setSpawn(true);
    const n = new URLSearchParams(params);
    n.delete("new");
    setParams(n, { replace: true });
  }, [params, setParams]);

  React.useEffect(() => {
    api.aiSessions().then(setFound, () => undefined);
  }, []);
  useServerEvents((ev) => {
    if (ev.type === "ai_sessions") api.aiSessions().then(setFound, () => undefined);
  });

  const agents = React.useMemo(() => (list ?? []).slice().sort((a, b) => ORDER[a.status] - ORDER[b.status] || b.last_activity - a.last_activity), [list]);
  const inbox = agents.flatMap((a) => a.approvals.map((ap) => ({ agent: a, ap }))).sort((x, y) => x.ap.at - y.ap.at);
  const working = agents.filter((a) => a.status === "running").length;
  const cost = agents.reduce((n, a) => n + a.cost_usd, 0);
  // Sessions found on machines that kiln isn't running itself (started in a plain terminal).
  const elsewhere = found.filter((s) => !agents.some((a) => a.harness_session && a.harness_session === s.id));

  const groups = React.useMemo(() => {
    const m = new Map<string, AgentInfo[]>();
    for (const a of agents) {
      const k = group === "machine" ? machineOf(a) : group === "shell" ? (a.shell ? `shell ${a.shell.slice(0, 8)}` : "No shell") : "all";
      m.set(k, [...(m.get(k) ?? []), a]);
    }
    return [...m.entries()];
  }, [agents, group]);

  const select = React.useCallback((id: string, on: boolean) => {
    setSelected((s) => {
      const n = new Set(s);
      if (on) n.add(id);
      else n.delete(id);
      return n;
    });
  }, []);

  return (
    <div className="container-console page hx-mission">
      <PageIntro
        index={5}
        label="Agents"
        title="Mission control"
        lead={
          list
            ? `${agents.length} agent${agents.length === 1 ? "" : "s"} · ${working} working · ${inbox.length} waiting on you · ${money(cost)} so far`
            : "Every coding agent kiln runs, on every machine."
        }
        actions={
          <Button onClick={() => setSpawn(true)}>
            <Plus /> New agents
          </Button>
        }
      />

      <AccountLimits agents={agents} />

      {inbox.length > 0 && (
        <section className="hx-inbox" aria-label="Waiting on you">
          <div className="hx-section-head">
            <span className="hx-eyebrow">Waiting on you · {inbox.length}</span>
          </div>
          <ul>
            {inbox.map(({ agent, ap }) => (
              <InboxRow key={`${agent.id}-${ap.id}`} agent={agent} a={ap} />
            ))}
          </ul>
        </section>
      )}

      <div className="hx-section-head mt-8">
        <span className="hx-eyebrow">Agents</span>
        {agents.length > 0 && (
          <Segmented<Group>
            label="Group agents"
            value={group}
            onChange={setGroup}
            options={[
              { value: "status", label: "All" },
              { value: "shell", label: "By shell" },
              { value: "machine", label: "By machine" },
            ]}
          />
        )}
      </div>

      {error && !list ? (
        <ErrorState error={error} onRetry={reload} />
      ) : !list ? (
        <SkeletonRows rows={4} cols={3} />
      ) : agents.length === 0 ? (
        <EmptyState
          title="No agents yet"
          action={
            <Button onClick={() => setSpawn(true)}>
              <Plus /> New agents
            </Button>
          }
        >
          Start Claude Code, Codex or any harness your machines have — one or a dozen. kiln runs them and is their whole front end: steer them, approve what they do, review their changes.
        </EmptyState>
      ) : (
        groups.map(([k, as]) => (
          <section key={k} className="hx-group">
            {group !== "status" && <div className="hx-group-head">{k}</div>}
            <div className="hx-board">
              {as.map((a) => (
                <AgentCard key={a.id} a={a} selected={selected.has(a.id)} onSelect={select} now={now} />
              ))}
            </div>
          </section>
        ))
      )}

      {elsewhere.length > 0 && (
        <section className="mt-10">
          <div className="hx-section-head">
            <span className="hx-eyebrow">Also on your machines · {elsewhere.length}</span>
            <Link to="/agents/sessions" className="text-link text-[12px]">
              See all
            </Link>
          </div>
          <ul className="hx-found">
            {elsewhere.slice(0, 6).map((s) => (
              <li key={`${s.machine}:${s.pid}`}>
                <HarnessMark kind={s.tool} size={18} />
                <span className="hx-trunc">{s.title || s.name || s.cwd}</span>
                <span className="hx-dim hx-trunc">
                  {s.machine} · {tildify(s.cwd)}
                </span>
                <Link to={`/agents/sessions?session=${encodeURIComponent(`${s.machine}:${s.pid}`)}`} className="text-link ml-auto text-[12px]">
                  Watch
                </Link>
              </li>
            ))}
          </ul>
        </section>
      )}

      {selected.size > 0 && <Broadcast ids={[...selected]} agents={agents} onDone={() => setSelected(new Set())} />}

      <SpawnDialog
        open={spawn}
        onOpenChange={setSpawn}
        onStarted={(started) => {
          setSpawn(false);
          if (started.length === 1 && started[0]) navigate(`/agents/${started[0].id}`);
          else reload();
        }}
      />
    </div>
  );
}
