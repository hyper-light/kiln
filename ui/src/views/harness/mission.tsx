/**
 * Mission control: one list of your agents, the ones that need you first. Each row says in one line
 * what that agent is doing — or what it wants, or what it finished — and you answer it right there.
 */
import * as React from "react";
import "./harness.css";
import "./mission.css";
import { Link, useNavigate, useSearchParams } from "react-router";
import { ArrowUpRight, ChevronRight, Plus, Send, X } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import { clientId, harnessApi, type AgentInfo, type ApprovalBrief, type SendMode } from "@/api/harness";
import type { AiSession } from "@/api/types";
import { Button } from "@/components/ui/button";
import { ErrorState, PageIntro, SkeletonRows } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage, useDocumentTitle, useLocalStorage, useNow } from "@/lib/hooks";
import { relativeTime } from "@/lib/format";
import { plain } from "./derive";
import { AgentMenu } from "./actions";
import { FoundSessions } from "./found";
import { machineName } from "./parts";
import { useSeen } from "./seen";
import { SpawnDialog } from "./spawn";
import { useAgents } from "./use-agents";

/** Where an agent stands, in the order you'd want to deal with them. */
type Stand = "needs" | "crashed" | "working" | "finished" | "ready" | "ended";
const RANK: Record<Stand, number> = { needs: 0, crashed: 1, working: 2, finished: 3, ready: 4, ended: 5 };

function standOf(a: AgentInfo, seen: number | undefined): Stand {
  if (a.approvals.length > 0 || a.status === "waiting") return "needs";
  if (a.status === "exited" && a.exit?.error) return "crashed";
  if (a.status === "running" || a.status === "starting") return "working";
  if (a.status === "idle") return a.last_activity > (seen ?? 0) + 1500 ? "finished" : "ready";
  return "ended";
}

const WORD: Record<Stand, string> = { needs: "Needs you", crashed: "Stopped", working: "Working", finished: "Finished", ready: "Ready", ended: "Ended" };

/** The one line that matters for this agent right now. */
function lineOf(a: AgentInfo, stand: Stand): string {
  switch (stand) {
    case "needs": {
      const ap = a.approvals[0];
      if (!ap) return "Waiting for you";
      const more = a.approvals.length > 1 ? ` (+${a.approvals.length - 1} more)` : "";
      return ap.tool === "question" ? `Asks: ${ap.title}${more}` : ap.tool === "plan" ? `Proposes a plan${more}` : `${ap.tool_name || ap.tool}: ${ap.title}${more}`;
    }
    case "crashed":
      return a.exit?.error ?? "Stopped unexpectedly";
    case "working": {
      const plan = (a.todos_total ?? 0) > 0 ? `plan ${a.todos_done ?? 0}/${a.todos_total}` : "";
      return [a.status === "starting" ? "Starting" : a.doing || a.activity || "Working", plan].filter(Boolean).join(" · ");
    }
    case "finished":
      return a.summary ? plain(a.summary) : a.goal || a.title || "Done";
    case "ready":
      return a.goal || a.title || "No task yet";
    case "ended":
      return a.status === "disconnected" ? "Its machine is offline" : a.goal || a.title || "Ended";
  }
}

function Decide({ a, ap }: { a: AgentInfo; ap: ApprovalBrief }) {
  const [busy, setBusy] = React.useState(false);
  const decide = async (decision: "allow" | "deny") => {
    setBusy(true);
    try {
      await harnessApi.command(a.id, { type: "approve", id: ap.id, decision });
    } catch (e) {
      toast.error("Couldn't answer", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  if (ap.tool === "question" || ap.tool === "plan")
    return (
      <Button size="xs" asChild>
        <Link to={`/agents/${a.id}`}>{ap.tool === "plan" ? "Review the plan" : "Answer"}</Link>
      </Button>
    );
  return (
    <>
      <Button size="xs" disabled={busy} onClick={() => decide("allow")}>
        Allow
      </Button>
      <Button size="xs" variant="secondary" disabled={busy} onClick={() => decide("deny")}>
        Deny
      </Button>
      <Link to={`/agents/${a.id}`} className="ms-link">
        Review
      </Link>
    </>
  );
}

const Row = React.memo(function Row({ a, stand, selected, onSelect, now }: { a: AgentInfo; stand: Stand; selected: boolean; onSelect: (id: string, on: boolean) => void; now: number }) {
  const navigate = useNavigate();
  const open = () => navigate(`/agents/${a.id}`);
  const ap = a.approvals[0];
  return (
    <li className="ms-row" data-stand={stand} data-selected={selected || undefined}>
      <label className="ms-check">
        <input type="checkbox" checked={selected} onChange={(e) => onSelect(a.id, e.target.checked)} aria-label={selected ? `Deselect ${a.name}` : `Select ${a.name}`} />
      </label>
      <button type="button" className="ms-main" onClick={open}>
        <span className="ms-name">{a.name}</span>
        <span className="ms-line">
          <span className="ms-word">
            <i aria-hidden="true" />
            {WORD[stand]}
          </span>
          <span className="ms-text">{lineOf(a, stand)}</span>
        </span>
      </button>
      <span className="ms-meta">
        <span>
          {machineName(a.target)} · {relativeTime(Math.floor(a.last_activity / 1000), now)}
        </span>
        {(a.left_behind ?? 0) > 0 && <span className="ms-left">{a.left_behind} left running</span>}
        {(a.files_changed ?? 0) > 0 && (
          <span className="ms-changes">
            {a.files_changed} file{a.files_changed === 1 ? "" : "s"} <b>+{a.lines_added ?? 0}</b> <i>−{a.lines_removed ?? 0}</i>
          </span>
        )}
      </span>
      <span className="ms-actions">
        {stand === "needs" && ap && <Decide a={a} ap={ap} />}
        {stand === "crashed" && (
          <Button size="xs" variant="secondary" onClick={() => void harnessApi.command(a.id, { type: "restart" }).catch((e: unknown) => toast.error("Couldn't restart", errorMessage(e)))}>
            Restart
          </Button>
        )}
        <AgentMenu a={a} />
        <button type="button" className="ms-go" onClick={open} aria-label={`Open ${a.name}`}>
          <ArrowUpRight size={16} />
        </button>
      </span>
    </li>
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
    <div className="ms-broadcast" role="region" aria-label="Message the selected agents">
      <div className="ms-broadcast-head">
        <span className="ms-eyebrow">To {ids.length} agents</span>
        <span className="ms-broadcast-names">{names.join(", ")}</span>
        <button
          type="button"
          className="ms-link ms-danger"
          onClick={() => {
            if (!window.confirm(`End ${ids.length} agent${ids.length === 1 ? "" : "s"}? Their processes stop on the machines.`)) return;
            void Promise.allSettled(ids.map((id) => harnessApi.stop(id))).then(onDone);
          }}
        >
          End {ids.length}
        </button>
        <button type="button" className="hx-icon" aria-label="Clear selection" onClick={onDone}>
          <X size={14} />
        </button>
      </div>
      <textarea className="ms-broadcast-input" rows={2} value={text} onChange={(e) => setText(e.target.value)} placeholder="Message every selected agent" />
      <div className="ms-broadcast-bar">
        <div className="ms-modes" role="radiogroup" aria-label="How to send">
          {(["queue", "steer", "now"] as SendMode[]).map((m) => (
            <button key={m} type="button" role="radio" aria-checked={mode === m} onClick={() => setMode(m)}>
              {m === "queue" ? "Queue" : m === "steer" ? "Steer" : "Now"}
            </button>
          ))}
        </div>
        <Button size="sm" onClick={send} disabled={busy || !text.trim()}>
          <Send /> Send to {ids.length}
        </Button>
      </div>
    </div>
  );
}

export default function Mission() {
  useDocumentTitle("Agents");
  const { list, error, reload } = useAgents();
  const seen = useSeen();
  const [found, setFound] = React.useState<AiSession[]>([]);
  const [quiet, setQuiet] = React.useState<string[]>([]);
  const [showEnded, setShowEnded] = useLocalStorage<boolean>("kiln.agents.showEnded", false);
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

  // Sessions found on your machines: told when they change, and re-read now and then for their
  // state and last activity.
  const readFound = React.useCallback(() => {
    api.aiSessions().then(setFound, () => undefined);
    api.aiSilent().then((r) => setQuiet(r.machines), () => undefined);
  }, []);
  React.useEffect(() => {
    readFound();
    const t = window.setInterval(readFound, 20_000);
    return () => window.clearInterval(t);
  }, [readFound]);
  useServerEvents((ev) => {
    if (ev.type === "ai_sessions") readFound();
  });

  const rows = React.useMemo(
    () =>
      (list ?? [])
        .map((a) => ({ a, stand: standOf(a, seen[a.id]) }))
        .sort((x, y) => RANK[x.stand] - RANK[y.stand] || (x.stand === "needs" ? (x.a.approvals[0]?.at ?? 0) - (y.a.approvals[0]?.at ?? 0) : y.a.last_activity - x.a.last_activity)),
    [list, seen],
  );
  const live = rows.filter((r) => r.stand !== "ended");
  const ended = rows.filter((r) => r.stand === "ended");
  const n = (s: Stand) => rows.filter((r) => r.stand === s).length;
  const machines = [...new Set(live.map((r) => machineName(r.a.target)))];
  // Sessions found on your machines that kiln doesn't run itself (its own agents are rows above).
  const elsewhere = found.filter((s) => !s.managed_by && !rows.some((r) => r.a.harness_session && r.a.harness_session === s.id));

  const status = [n("needs") && `${n("needs")} need${n("needs") === 1 ? "s" : ""} you`, n("crashed") && `${n("crashed")} stopped`, n("working") && `${n("working")} working`, n("finished") && `${n("finished")} finished`]
    .filter(Boolean)
    .join(", ");
  const lead = !list
    ? "Every coding agent kiln runs, on every machine."
    : rows.length === 0
      ? "Start coding agents on any of your machines and run them all from here."
      : `${status || "All quiet"}${machines.length ? ` — on ${machines.join(" and ")}` : ""}.`;

  const select = React.useCallback((id: string, on: boolean) => {
    setSelected((s) => {
      const next = new Set(s);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  }, []);

  return (
    <div className="container-console page ms-page" data-selecting={selected.size > 0 || undefined}>
      <PageIntro
        index={5}
        label="Agents"
        title="Mission control."
        lead={lead}
        actions={
          <Button onClick={() => setSpawn(true)}>
            New agents <Plus />
          </Button>
        }
      />

      {error && !list ? (
        <ErrorState error={error} onRetry={reload} />
      ) : !list ? (
        <SkeletonRows rows={4} cols={3} />
      ) : rows.length === 0 ? (
        <div className="ms-empty">
          <p className="ms-empty-title">No agents yet.</p>
          <p>Claude Code, Codex, or any harness your machines have — one or a dozen. kiln runs them: steer them, answer them, review what they change.</p>
          <Button onClick={() => setSpawn(true)}>
            New agents <Plus />
          </Button>
        </div>
      ) : live.length > 0 ? (
        <ul className="ms-list">
          {live.map(({ a, stand }) => (
            <Row key={a.id} a={a} stand={stand} selected={selected.has(a.id)} onSelect={select} now={now} />
          ))}
        </ul>
      ) : (
        <p className="ms-quiet">Nothing running. Start something new, or pick up an ended agent below.</p>
      )}

      <FoundSessions sessions={elsewhere} quiet={quiet} now={now} />

      {ended.length > 0 && (
        <section className="ms-ended">
          <div className="ms-fold">
            <button type="button" className="ms-fold-btn" aria-expanded={showEnded} onClick={() => setShowEnded(!showEnded)}>
              <ChevronRight size={13} className="ms-chev" aria-hidden="true" />
              Ended · {ended.length}
            </button>
            <button type="button" className="ms-link" onClick={() => void Promise.allSettled(ended.map((r) => harnessApi.stop(r.a.id))).then(() => reload())}>
              Clear
            </button>
          </div>
          {showEnded && (
            <ul className="ms-list">
              {ended.map(({ a, stand }) => (
                <Row key={a.id} a={a} stand={stand} selected={selected.has(a.id)} onSelect={select} now={now} />
              ))}
            </ul>
          )}
        </section>
      )}

      {selected.size > 0 && <Broadcast ids={[...selected]} agents={rows.map((r) => r.a)} onDone={() => setSelected(new Set())} />}

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
