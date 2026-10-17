/**
 * Mission control: one list of your agents, newest first, each keeping its place: a row says in one
 * line what that agent is doing — or what it wants, or what it finished — and you answer it right
 * there. Nothing moves as agents update; only their words and marks change.
 */
import * as React from "react";
import "./harness.css";
import "./mission.css";
import { Link, useNavigate, useSearchParams } from "react-router";
import { ArrowUpRight, ChevronRight, Plus } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import { harnessApi, type AgentInfo, type ApprovalBrief } from "@/api/harness";
import type { AiSession } from "@/api/types";
import { Button } from "@/components/ui/button";
import { ErrorState, PageIntro, SkeletonRows } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage, useDocumentTitle, useLocalStorage } from "@/lib/hooks";
import { plain } from "./derive";
import { AgentMenu } from "./actions";
import { MessageMany } from "./broadcast";
import { DEFAULT_VIEW, isFiltered, ListTools, matches, sortRows, type View } from "./list-tools";
import { byStart, FoundSessions, sessionKey } from "./found";
import { Ago, machineName } from "./parts";
import { useSeen } from "./seen";
import { SpawnDialog } from "./spawn";
import { useAgents } from "./use-agents";

/** Where an agent stands. */
type Stand = "needs" | "crashed" | "working" | "finished" | "ready" | "offline" | "ended";

function standOf(a: AgentInfo, seen: number | undefined): Stand {
  if (a.approvals.length > 0 || a.status === "waiting") return "needs";
  if (a.status === "exited" && a.exit?.error) return "crashed";
  if (a.status === "running" || a.status === "starting") return "working";
  if (a.status === "idle") return a.last_activity > (seen ?? 0) + 1500 ? "finished" : "ready";
  // Its machine is reconnecting (a runner or server restart): it stays where it is.
  if (a.status === "disconnected") return "offline";
  return "ended";
}

const WORD: Record<Stand, string> = { needs: "Needs you", crashed: "Stopped", working: "Working", finished: "Finished", ready: "Ready", offline: "Offline", ended: "Ended" };

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
    case "offline":
      return "Its machine is offline";
    case "ended":
      return a.goal || a.title || "Ended";
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

/** A row re-renders only when its own agent changes (or is selected); its clock counts by itself. */
const Row = React.memo(function Row({ a, stand, selected, onSelect }: { a: AgentInfo; stand: Stand; selected: boolean; onSelect: (id: string, on: boolean) => void }) {
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
          {machineName(a.target)} · <Ago className="ms-time" ts={Math.floor(a.last_activity / 1000)} />
        </span>
        <span className="ms-meta-2">
          {(a.files_changed ?? 0) > 0 && (
            <span className="ms-changes">
              {a.files_changed} file{a.files_changed === 1 ? "" : "s"} <b>+{a.lines_added ?? 0}</b> <i>−{a.lines_removed ?? 0}</i>
            </span>
          )}
          {(a.left_behind ?? 0) > 0 && <span className="ms-left">{a.left_behind} left running</span>}
        </span>
      </span>
      <span className="ms-actions">
        {/* What to answer; on a phone it takes the meta line's place, so the row's corner keeps one width. */}
        <span className="ms-decide">
          {stand === "needs" && ap && <Decide a={a} ap={ap} />}
          {stand === "crashed" && (
            <Button size="xs" variant="secondary" onClick={() => void harnessApi.command(a.id, { type: "restart" }).catch((e: unknown) => toast.error("Couldn't restart", errorMessage(e)))}>
              Restart
            </Button>
          )}
        </span>
        <span className="ms-tail">
          <AgentMenu a={a} />
          <button type="button" className="ms-go" onClick={open} aria-label={`Open ${a.name}`}>
            <ArrowUpRight size={16} />
          </button>
        </span>
      </span>
    </li>
  );
});

export default function Mission() {
  useDocumentTitle("Agents");
  const { list, error, reload } = useAgents();
  const seen = useSeen();
  const [found, setFound] = React.useState<AiSession[]>([]);
  const [quiet, setQuiet] = React.useState<string[]>([]);
  const [showEnded, setShowEnded] = useLocalStorage<boolean>("kiln.agents.showEnded", false);
  const [spawn, setSpawn] = React.useState(false);
  const [selected, setSelected] = React.useState<Set<string>>(new Set());
  // Message agents: open with rows selected, or from the header for every agent (or a filter).
  const [messaging, setMessaging] = React.useState(false);
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
  // state and last activity. A scan can miss a session for a moment, so a row goes only once two
  // reads in a row miss it, rather than blinking out and back.
  const kept = React.useRef(new Map<string, { s: AiSession; missed: number }>());
  const readFound = React.useCallback(() => {
    api.aiSessions().then((list) => {
      const m = kept.current;
      const here = new Set(list.map(sessionKey));
      // An unchanged session keeps its object, so its row doesn't re-render.
      for (const s of list) {
        const k = sessionKey(s);
        const was = m.get(k)?.s;
        m.set(k, { s: was && JSON.stringify(was) === JSON.stringify(s) ? was : s, missed: 0 });
      }
      for (const [k, v] of m) if (!here.has(k) && ++v.missed >= 2) m.delete(k);
      setFound([...m.values()].map((v) => v.s).sort(byStart));
    }, () => undefined);
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

  const all = React.useMemo(() => (list ?? []).map((a) => ({ a, stand: standOf(a, seen[a.id]) })), [list, seen]);
  // How the list is shown, kept across visits. Created order never changes by itself; any other
  // sort is taken when chosen (or asked again), so rows don't reorder while agents update: new
  // agents come in on top, and the tools offer "Re-sort" once the order has gone stale.
  const [stored, setView] = useLocalStorage<Partial<View>>("kiln.agents.view", DEFAULT_VIEW);
  const view = React.useMemo(() => ({ ...DEFAULT_VIEW, ...stored }), [stored]);
  const [taken, setTaken] = React.useState<string[] | null>(null);
  const latest = React.useRef(all);
  React.useEffect(() => {
    latest.current = all;
  });
  const loaded = list !== null;
  const resort = React.useCallback(() => {
    setTaken(view.sort === "created" ? null : sortRows(latest.current, view.sort, view.dir).map((r) => r.a.id));
  }, [view.sort, view.dir]);
  React.useEffect(resort, [resort, loaded]);
  const ordered = React.useMemo(() => {
    if (!taken) return sortRows(all, view.sort, view.dir);
    const at = new Map(taken.map((id, i) => [id, i]));
    const since = sortRows(
      all.filter((r) => !at.has(r.a.id)),
      "created",
      "desc",
    );
    return [...since, ...all.filter((r) => at.has(r.a.id)).sort((x, y) => (at.get(x.a.id) ?? 0) - (at.get(y.a.id) ?? 0))];
  }, [all, taken, view.sort, view.dir]);
  const stale = React.useMemo(() => taken !== null && sortRows(all, view.sort, view.dir).some((r, i) => r.a.id !== ordered[i]?.a.id), [all, taken, ordered, view.sort, view.dir]);
  const rows = ordered.filter((r) => matches(view, r));
  const live = rows.filter((r) => r.stand !== "ended");
  const ended = rows.filter((r) => r.stand === "ended");
  const n = (s: Stand) => all.filter((r) => r.stand === s).length;
  const machines = [...new Set(all.filter((r) => r.stand !== "ended").map((r) => machineName(r.a.target)))];
  // Sessions found on your machines that kiln doesn't run itself (its own agents are rows above).
  const elsewhere = found.filter((s) => !s.managed_by && !all.some((r) => r.a.harness_session && r.a.harness_session === s.id));

  const status = [n("needs") && `${n("needs")} need${n("needs") === 1 ? "s" : ""} you`, n("crashed") && `${n("crashed")} stopped`, n("working") && `${n("working")} working`, n("finished") && `${n("finished")} finished`]
    .filter(Boolean)
    .join(", ");
  const lead = !list
    ? "Every coding agent kiln runs, on every machine."
    : all.length === 0
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
    <div className="container-console page ms-page" data-selecting={selected.size > 0 || messaging || undefined}>
      <PageIntro
        index={6}
        label="Agents"
        title="Mission control."
        lead={lead}
        actions={
          <div className="ms-actions-head">
            <Button variant="secondary" onClick={() => setMessaging(true)} disabled={!list?.length}>
              Message agents
            </Button>
            <Button onClick={() => setSpawn(true)}>
              New agents <Plus />
            </Button>
          </div>
        }
      />

      {all.length > 0 && <ListTools view={view} onView={setView} rows={all} stale={stale} onResort={resort} shown={rows.length} />}

      {error && !list ? (
        <ErrorState error={error} onRetry={reload} />
      ) : !list ? (
        <SkeletonRows rows={4} cols={3} />
      ) : all.length === 0 ? (
        <div className="ms-empty">
          <p className="ms-empty-title">No agents yet.</p>
          <p>Claude Code, Codex, or any harness your machines have — one or a dozen. kiln runs them: steer them, answer them, review what they change.</p>
          <Button onClick={() => setSpawn(true)}>
            New agents <Plus />
          </Button>
        </div>
      ) : rows.length === 0 ? (
        <p className="ms-quiet">
          No agent matches.{" "}
          <button type="button" className="ms-link" onClick={() => setView({ ...DEFAULT_VIEW, sort: view.sort, dir: view.dir })}>
            Clear filters
          </button>
        </p>
      ) : live.length > 0 ? (
        <ul className="ms-list">
          {live.map(({ a, stand }) => (
            <Row key={a.id} a={a} stand={stand} selected={selected.has(a.id)} onSelect={select} />
          ))}
        </ul>
      ) : (
        <p className="ms-quiet">Nothing running. Start something new, or pick up an ended agent below.</p>
      )}

      {/* Once kiln's own agents are known, so its own sessions never show here for a moment. */}
      {list && <FoundSessions sessions={elsewhere} quiet={quiet} />}

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
                <Row key={a.id} a={a} stand={stand} selected={selected.has(a.id)} onSelect={select} />
              ))}
            </ul>
          )}
        </section>
      )}

      {(selected.size > 0 || messaging) && (
        <MessageMany
          selected={[...selected]}
          shown={isFiltered(view) ? rows.map((r) => r.a.id) : undefined}
          agents={list ?? []}
          onClose={() => {
            setSelected(new Set());
            setMessaging(false);
          }}
        />
      )}

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
