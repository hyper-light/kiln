/**
 * Mission control: one list of your agents, the ones that need you first: a row says in one line
 * what that agent is doing — or what it wants, or what it finished — and you answer it right there.
 * An agent that comes to need you rises as it does; while you're using the list, rows hold still,
 * then glide to their places.
 */
import * as React from "react";
import "./harness.css";
import "./mission.css";
import { Link, useNavigate, useSearchParams } from "@/lib/router";
import { ArrowUpRight, ChevronRight, Plus } from "lucide-react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo, type ApprovalBrief } from "@/api/harness";
import type { AiSession } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField, Skeleton } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useAgentsLive } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage, useLocalStorage, useMediaQuery, useNow } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { agentLabel, hasOwnName, plain } from "./derive";
import { pauseLine } from "./pause";
import { NameField, useAgentActions } from "./actions";
import { MessageBar } from "./broadcast";
import { DEFAULT_VIEW, isFiltered, ListBar, sortOf, useGlide, useListHold, useShownList, type ListView } from "./list-tools";
import { SaidInConversations } from "./said";
import { byStart, FoundSessions, sessionKey } from "./found";
import { Ago, machineName } from "./parts";
import { LowBattery } from "../battery";
import { useSeen } from "./seen";
import { missionFoundSessions, standOf, subagentsLine, type MissionRow, type MissionStand as Stand } from "./mission-rows";
import { SpawnDialog } from "./spawn";
import { useAgentList, useAgents } from "./use-agents";
import { both, touchSelect, type TouchSelect } from "./touch-select";
import { answer, asAnswered, useAnswered } from "./answered";
import { PushOffer } from "./push-offer";
import { RowChange, RowReview } from "./change-bar";
import { Shimmer } from "./signals";
import { subagentRunning, subagentsAtWork, throughSubagents } from "./tasks";

/** A phone: the page's head is one line, and the message box folds. */
const PHONE = "(max-width: 640px)";

const WORD: Record<Stand, string> = {
  needs: "Needs you",
  crashed: "Stopped",
  paused: "Paused",
  working: "Working",
  finished: "Finished",
  ready: "Ready",
  offline: "Offline",
  ended: "Ended",
};

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
    case "paused":
      return a.pause ? pauseLine(a.pause, Date.now()) : "Paused";
    case "working": {
      const plan = (a.todos_total ?? 0) > 0 ? `plan ${a.todos_done ?? 0}/${a.todos_total}` : "";
      // Its turn over, what its subagents do.
      const doing = a.status === "starting" ? (a.harness_session ? "Resuming" : "Starting") : throughSubagents(a) ? subagentsLine(subagentsAtWork(a.subagents)) : a.doing || a.activity || "Working";
      return [doing, plan].filter(Boolean).join(" · ");
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

/** Answered at once (answered.ts): the row moves on now; if the answer doesn't go, it's back. */
function Decide({ a, ap }: { a: AgentInfo; ap: ApprovalBrief }) {
  const decide = (decision: "allow" | "deny") => void answer(a.id, { type: "approve", id: ap.id, decision }).catch((e: unknown) => toast.error("Couldn't answer", errorMessage(e)));
  if (ap.tool === "question" || ap.tool === "plan")
    return (
      <Button size="xs" asChild>
        <Link to={`/agents/${a.id}`}>{ap.tool === "plan" ? "Review the plan" : "Answer"}</Link>
      </Button>
    );
  return (
    <>
      <Button size="xs" onClick={() => decide("allow")}>
        Allow
      </Button>
      <Button size="xs" variant="secondary" onClick={() => decide("deny")}>
        Deny
      </Button>
      <Link to={`/agents/${a.id}`} className="ms-link">
        Review
      </Link>
    </>
  );
}

/**
 * A row re-renders only when its own agent changes (or is selected); its clock counts by itself.
 * `picking`: agents are being selected by touch (its mark says whether it is).
 */
const Row = React.memo(function Row({ a, stand, selected, picking, onSelect }: { a: AgentInfo; stand: Stand; selected: boolean; picking?: boolean; onSelect: (id: string, on: boolean) => void }) {
  const navigate = useNavigate();
  const open = () => navigate(`/agents/${a.id}`);
  const ap = a.approvals[0];
  const actions = useAgentActions(a);
  const label = agentLabel(a);
  // The harness, said under a label that isn't its own name.
  const kind = label.toLowerCase() !== a.kind_name.toLowerCase();
  const line = (
    <span className="ms-line">
      {/* A subagent of its own running: the prism sweeps through its word. */}
      <Shimmer on={stand === "working" && subagentRunning(a.subagents)} className="ms-word">
        <i aria-hidden="true" />
        {WORD[stand]}
      </Shimmer>
      <span className="ms-text">{lineOf(a, stand)}</span>
    </span>
  );
  return (
    <li className="ms-row" data-stand={stand} data-selected={selected || undefined} data-glide={a.id} data-agent={a.id}>
      <label className="ms-check">
        <input type="checkbox" className="check" checked={selected} onChange={(e) => onSelect(a.id, e.target.checked)} aria-label={`Select ${label}`} />
      </label>
      {actions.renaming ? (
        <div className="ms-main">
          <NameField value={hasOwnName(a) ? a.name : ""} placeholder={label} onSave={actions.rename} onDone={actions.stopRenaming} className="ms-name-field" label="Agent name" />
          {line}
        </div>
      ) : (
        <button type="button" className="ms-main" onClick={open} aria-pressed={picking ? selected : undefined}>
          <span className="ms-name">
            {picking && <span className="ms-pick" aria-hidden="true" />}
            {label}
          </span>
          {line}
        </button>
      )}
      <span className="ms-meta">
        <span>
          {machineName(a.target)} · <Ago className="ms-time" ts={Math.floor(a.last_activity / 1000)} />
        </span>
        <span className="ms-meta-2">
          {kind && <span className="ms-kind">{a.kind_name}</span>}
          {(a.files_changed ?? 0) > 0 && (
            <span className="ms-changes">
              {a.files_changed} file{a.files_changed === 1 ? "" : "s"} <b>+{a.lines_added ?? 0}</b> <i>−{a.lines_removed ?? 0}</i>
            </span>
          )}
          {(a.left_behind ?? 0) > 0 && <span className="ms-left">{a.left_behind} left running</span>}
          {(a.files_changed ?? 0) > 0 && <RowChange agentId={a.id} />}
        </span>
      </span>
      <span className="ms-actions">
        {/* What to answer; on a phone it takes the meta line's place, so the row's corner keeps one width. */}
        <span className="ms-decide">
          {stand === "needs" && ap && <Decide a={a} ap={ap} />}
          {stand === "paused" && (
            <Button size="xs" variant="secondary" onClick={() => void harnessApi.command(a.id, { type: "resume" }).catch((e: unknown) => toast.error("Couldn't resume", errorMessage(e)))}>
              Resume
            </Button>
          )}
          {stand === "crashed" && (
            <Button size="xs" variant="secondary" onClick={() => void harnessApi.command(a.id, { type: "restart" }).catch((e: unknown) => toast.error("Couldn't restart", errorMessage(e)))}>
              Restart
            </Button>
          )}
          {(stand === "finished" || stand === "ready" || stand === "ended") && (a.files_changed ?? 0) > 0 && <RowReview agentId={a.id} />}
        </span>
        <span className="ms-tail">
          {actions.menu()}
          {actions.dialogs}
          <button type="button" className="ms-go" onClick={open} aria-label={`Open ${label}`}>
            <ArrowUpRight size={16} />
          </button>
        </span>
      </span>
    </li>
  );
});

/** Where each agent stands (what you answered here counted as taken), and how many stand where (the page's lead says so). */
function standsOf(list: AgentInfo[], seen: Record<string, number>, done: ReadonlyMap<string, number>) {
  return list.map((agent) => {
    const a = asAnswered(agent, done);
    return { a, stand: standOf(a, seen[a.id]) };
  });
}

/** Before the agents are known: the list's shape, so nothing moves when they come. */
function RowsLoading() {
  return (
    <div className="ms-loading" role="status" aria-label="Loading agents">
      <Skeleton className="ms-loading-bar" />
      <ul className="ms-list" aria-hidden="true">
        {[62, 48, 70, 54, 40].map((w, i) => (
          <li key={i} className="ms-row ms-loading-row" style={{ opacity: 1 - i * 0.15 }}>
            <span className="ms-main">
              <Skeleton className="ms-loading-name" style={{ width: `${w}%` }} />
              <Skeleton className="ms-loading-line" style={{ width: `${w + 16}%` }} />
            </span>
            <span className="ms-meta">
              <Skeleton className="ms-loading-meta" />
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** What the page's lead says before the agents are known. */
const LEAD = "Every coding agent kiln runs, on every machine.";

/** The lead: how your agents stand, and where (on a phone, how they stand). */
function Lead({ short }: { short?: boolean }) {
  const list = useAgentList();
  const seen = useSeen();
  const done = useAnswered();
  const all = React.useMemo(() => standsOf(list, seen, done), [list, seen, done]);
  const n = (s: Stand) => all.filter((r) => r.stand === s).length;
  const machines = [...new Set(all.filter((r) => r.stand !== "ended").map((r) => machineName(r.a.target)))];
  const status = [
    n("needs") && `${n("needs")} need${n("needs") === 1 ? "s" : ""} you`,
    n("crashed") && `${n("crashed")} stopped`,
    n("paused") && `${n("paused")} paused`,
    n("working") && `${n("working")} working`,
    n("finished") && `${n("finished")} finished`,
  ]
    .filter(Boolean)
    .join(", ");
  if (short && all.length > 0) return <>{status || "All quiet"}</>;
  return <>{all.length === 0 ? "Start coding agents on any of your machines and run them all from here." : `${status || "All quiet"}${machines.length ? ` — on ${machines.join(" and ")}` : ""}.`}</>;
}

export default function Mission() {
  const [spawn, setSpawn] = React.useState(false);
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  // ?new=1 (links from elsewhere) opens New agents: open, it stays so by itself, and the link's
  // mark comes off the address.
  const fromLink = params.get("new") === "1";
  const phone = useMediaQuery(PHONE);
  const opened = () => {
    if (!fromLink) return;
    setSpawn(true);
    const n = new URLSearchParams(params);
    n.delete("new");
    setParams(n, { replace: true });
  };
  return (
    <div className="container-console page ms-page">
      <title>Agents · kiln</title>
      <PageIntro
        index={6}
        label="Agents"
        title="Mission control."
        compact
        lead={
          <React.Suspense fallback={LEAD}>
            <Lead short={phone} />
          </React.Suspense>
        }
        actions={
          phone ? (
            <>
              <Button asChild variant="ghost" size="xs">
                <Link to="/agents/plans">Plans</Link>
              </Button>
              <Button asChild variant="ghost" size="xs">
                <Link to="/agents/programs">Programs</Link>
              </Button>
              <Button asChild variant="ghost" size="xs">
                <Link to="/agents/attached">Attached</Link>
              </Button>
              <Button size="icon" aria-label="New agents" title="New agents" onClick={() => setSpawn(true)}>
                <Plus />
              </Button>
            </>
          ) : (
            <>
              <Button asChild variant="secondary">
                <Link to="/agents/plans">Plans</Link>
              </Button>
              <Button asChild variant="secondary">
                <Link to="/agents/programs">Programs</Link>
              </Button>
              <Button asChild variant="secondary">
                <Link to="/agents/attached">Attached</Link>
              </Button>
              <Button onClick={() => setSpawn(true)}>
                New agents <Plus />
              </Button>
            </>
          )
        }
      />
      <Loadable label="Loading agents" fallback={<RowsLoading />}>
        <Agents onSpawn={() => setSpawn(true)} />
      </Loadable>
      <SpawnDialog
        open={spawn || fromLink}
        onOpenChange={setSpawn}
        onOpened={opened}
        onStarted={(started) => {
          setSpawn(false);
          if (started.length === 1 && started[0]) navigate(`/agents/${started[0].id}`);
          else invalidate("harness/agents");
        }}
      />
    </div>
  );
}

/** Sessions found on your machines, as last read (each kept a read longer than it was missing, so rows don't blink). */
const kept = new Map<string, { s: AiSession; missed: number }>();
async function readFound(signal: AbortSignal): Promise<AiSession[]> {
  const list = await api.aiSessions(signal);
  const here = new Set(list.map(sessionKey));
  // An unchanged session keeps its object, so its row doesn't re-render.
  for (const s of list) {
    const k = sessionKey(s);
    const was = kept.get(k)?.s;
    kept.set(k, { s: was && JSON.stringify(was) === JSON.stringify(s) ? was : s, missed: 0 });
  }
  for (const [k, v] of kept) if (!here.has(k) && ++v.missed >= 2) kept.delete(k);
  return [...kept.values()].map((v) => v.s).sort(byStart);
}
/**
 * The found sessions and the machines that can't say what runs on them, read together. A read that
 * fails leaves what was found (a server without these says nothing here).
 */
const readAll = (signal: AbortSignal) =>
  Promise.all([
    readFound(signal).catch(() => [...kept.values()].map((v) => v.s).sort(byStart)),
    api.aiSilent(signal).then(
      (r) => r.machines,
      (): string[] => [],
    ),
  ]);
const FOUND = { on: ["ai_sessions"], pollMs: 20_000 } as const;

/**
 * Sessions found on your machines that kiln doesn't run itself (its own agents are rows above):
 * told when they change, and read again now and then for their state and last activity.
 */
function Found({ all }: { all: { a: AgentInfo }[] }) {
  // Its own key: "ai-sessions" is the list alone (`aiSessionsRes`), and whichever read it first
  // would hand the other the wrong shape.
  const [found, quiet] = useSuspenseResource("ai-sessions-found", readAll, FOUND).data;
  const elsewhere = missionFoundSessions(found, all);
  return <FoundSessions sessions={elsewhere} quiet={quiet} />;
}

/** A machine offline this long, with agents on it, may be gone: they can go on elsewhere. */
const GONE_AFTER_S = 120;

/**
 * Machines that have been offline a while with agents on them: if one is gone, its agents go on
 * elsewhere from kiln's copy of their conversations, each on the machine that suits it, or all on
 * one. A machine only restarting doesn't show here.
 */
function Recover({ rows }: { rows: { a: AgentInfo; stand: Stand }[] }) {
  const runners = useAgentsLive().data;
  const nowS = useNow() / 1000;
  const offline = new Map<string, number>();
  for (const { a, stand } of rows) {
    if (stand !== "offline" || a.target === "local") continue;
    const m = machineName(a.target);
    offline.set(m, (offline.get(m) ?? 0) + 1);
  }
  const gone = [...offline].flatMap(([machine, count]) => {
    const r = runners.find((x) => x.name === machine);
    return r && !r.connected && nowS - r.last_seen > GONE_AFTER_S ? [{ machine, count, since: r.last_seen }] : [];
  });
  const others = runners.filter((r) => r.connected && !r.ephemeral && !r.outpost).map((r) => r.name);
  // A machine on battery that is low: its agents stop when it sleeps.
  const agentsOn = (m: string) => {
    const n = rows.filter(({ a, stand }) => stand !== "ended" && a.target !== "local" && machineName(a.target) === m).length;
    return n > 0 ? `its ${n} agent${n === 1 ? "" : "s"}` : "";
  };
  if (gone.length === 0) return <LowBattery agents={runners} what={agentsOn} />;
  return (
    <>
      <LowBattery agents={runners} what={agentsOn} />
      {gone.map((g) => (
        <RecoverMachine key={g.machine} {...g} others={others} />
      ))}
    </>
  );
}

function RecoverMachine({ machine, count, since, others }: { machine: string; count: number; since: number; others: string[] }) {
  const [to, setTo] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const go = async () => {
    setBusy(true);
    try {
      const { moved } = await harnessApi.recover(machine, to || undefined);
      const failed = moved.filter((m) => m.error);
      const n = moved.length - failed.length;
      if (n > 0) toast({ title: `${n} agent${n === 1 ? "" : "s"} of ${machine} going on elsewhere`, description: failed.length > 0 ? `${failed.length} couldn't: ${failed[0]?.error ?? ""}` : undefined, tone: "running" });
      else toast.error(`${machine}'s agents couldn't go on elsewhere`, failed[0]?.error ?? "");
      invalidate("harness/agents");
    } catch (e) {
      toast.error(`${machine}'s agents couldn't go on elsewhere`, errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="ms-recover" role="status">
      <span>
        {machine} has been offline since {relativeTime(since)}. If it&rsquo;s gone, its {count} agent{count === 1 ? "" : "s"} can go on elsewhere from kiln&rsquo;s copy of
        {count === 1 ? " its conversation" : " their conversations"}.
      </span>
      {others.length > 0 ? (
        <span className="ms-recover-go">
          <SelectField
            label={`Where ${machine}'s agents go on`}
            hideLabel
            value={to}
            onChange={setTo}
            options={[{ value: "", label: "Each where it suits best" }, ...others.map((m) => ({ value: m, label: `All on ${m}` }))]}
          />
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => void go()}>
            {busy ? "Moving…" : "Move them on"}
          </Button>
        </span>
      ) : (
        <span className="ms-recover-none">No other machine is connected to take them.</span>
      )}
    </div>
  );
}

function Agents({ onSpawn }: { onSpawn: () => void }) {
  const { list, reload } = useAgents();
  const seen = useSeen();
  const [showEnded, setShowEnded] = useLocalStorage<boolean>("kiln.agents.showEnded", false);
  const [selected, setSelected] = React.useState<ReadonlySet<string>>(new Set());
  // Selecting by touch (touch-select.ts): from a row held, or from Select, until Done.
  const touchScreen = useMediaQuery("(pointer: coarse)");
  const [picking, setPicking] = React.useState(false);
  const selecting = picking || selected.size > 0;
  const clear = () => {
    setSelected(new Set());
    setPicking(false);
  };
  // On a phone the message box stays folded until agents are selected or it's opened.
  const phone = useMediaQuery(PHONE);
  const [messaging, setMessaging] = React.useState(false);

  const done = useAnswered();
  const all = React.useMemo(() => standsOf(list, seen, done), [list, seen, done]);
  // How the list is shown (the sort and the facets chosen) is kept across visits; the words typed
  // to find a name are not. (A new key: everyone starts on Needs you first once.) While you're using
  // the list, or have agents selected, it holds still: see useListHold.
  const [stored, setView] = useLocalStorage<Partial<ListView>>("kiln.agents.view", DEFAULT_VIEW);
  const view = React.useMemo(() => ({ ...DEFAULT_VIEW, ...stored, sort: sortOf(stored.sort ?? DEFAULT_VIEW.sort) }), [stored]);
  const [q, setQ] = React.useState("");
  const { held, bind } = useListHold(selecting);
  const { live, ended } = useShownList(all, q, view, held);
  const glide = useGlide(live.map((r) => `${r.a.id}:${r.stand}`).join());
  const selectable = [...live, ...ended];
  const searchAgents = React.useMemo(() => new Set(all.map(({ a }) => a.id)), [all]);
  const filtered = isFiltered(q, view.facets);

  const setMany = React.useCallback((ids: string[], on: boolean) => {
    setSelected((s) => {
      const next = new Set(s);
      for (const id of ids) {
        if (on) next.add(id);
        else next.delete(id);
      }
      return next;
    });
  }, []);
  const select = React.useCallback((id: string, on: boolean) => setMany([id], on), [setMany]);
  // The rows' gestures ask what's selected now; their props are made once, with the list's hold.
  const [touch] = React.useState(() => {
    const now: { s: TouchSelect } = { s: { isSelected: () => false, set: () => undefined, selecting: false } };
    return { now, props: both(bind, touchSelect(() => now.s)) };
  });
  touch.now.s = { isSelected: (id) => selected.has(id), set: setMany, selecting: touchScreen && selecting };
  // Only what the list shows can be checked: a row the filter takes away is unchecked with it.
  const shownIds = selectable.map((r) => r.a.id).join(",");
  const [wasShown, setWasShown] = React.useState(shownIds);
  if (wasShown !== shownIds) {
    setWasShown(shownIds);
    const here = new Set(shownIds.split(","));
    if (![...selected].every((id) => here.has(id))) setSelected(new Set([...selected].filter((id) => here.has(id))));
  }
  const checked = selectable.filter((r) => selected.has(r.a.id));
  const liveChecked = live.filter((r) => selected.has(r.a.id)).length;
  const some = liveChecked > 0 && liveChecked < live.length;
  const allBox = React.useCallback(
    (el: HTMLInputElement | null) => {
      if (el) el.indeterminate = some;
    },
    [some],
  );
  const targets = checked.length > 0 ? checked.map((r) => r.a) : live.map((r) => r.a);
  const reach = checked.length > 0 ? "selected" : filtered ? "shown" : "all";
  const findable = React.useMemo(() => all.map((r) => r.a), [all]);
  const message = <MessageBar agents={targets} reach={reach} onEnded={clear} all={findable} onPick={setMany} />;
  const folds = phone && checked.length === 0;
  const renderRow = ({ a, stand }: MissionRow) => <Row key={a.id} a={a} stand={stand} selected={selected.has(a.id)} picking={touchScreen && selecting} onSelect={select} />;

  return (
    <>
      {all.length > 0 && (
        <>
          <ListBar rows={all} q={q} onQuery={setQ} view={view} onView={setView} />
          {!phone && message}
          <React.Suspense fallback={null}>
            <Recover rows={all} />
          </React.Suspense>
          <React.Suspense fallback={null}>
            <PushOffer />
          </React.Suspense>
        </>
      )}

      {all.length === 0 ? (
        <div className="ms-empty">
          <p className="ms-empty-title">No agents yet.</p>
          <p>Claude Code, Codex, or any harness your machines have — one or a dozen. kiln runs them: steer them, answer them, review what they change.</p>
          <Button onClick={onSpawn}>
            New agents <Plus />
          </Button>
        </div>
      ) : (
        <>
          <div className="ms-listhead" data-selecting={(touchScreen && selecting) || undefined}>
            {touchScreen ? (
              // By touch: Select (or hold a row), then tap rows; All, and Done.
              <>
                <span className="ms-count" aria-live="polite">
                  {selecting ? `${checked.length} selected` : filtered ? `${live.length + ended.length} of ${all.length} agents` : `${all.length} agent${all.length === 1 ? "" : "s"}`}
                </span>
                {selecting ? (
                  <span className="ms-picks">
                    <button type="button" className="ms-link" disabled={live.length === 0 || liveChecked === live.length} onClick={() => setMany(live.map((r) => r.a.id), true)}>
                      Select all
                    </button>
                    <button type="button" className="ms-link ms-done" onClick={clear}>
                      Done
                    </button>
                  </span>
                ) : (
                  live.length > 0 && (
                    <button type="button" className="ms-link ms-select" onClick={() => setPicking(true)}>
                      Select
                    </button>
                  )
                )}
              </>
            ) : (
              <>
                <label className="ms-all">
                  <input
                    ref={allBox}
                    type="checkbox"
                    className="check"
                    disabled={live.length === 0}
                    checked={live.length > 0 && liveChecked === live.length}
                    onChange={(e) => setSelected(e.target.checked ? new Set([...selected, ...live.map((r) => r.a.id)]) : new Set([...selected].filter((id) => !live.some((r) => r.a.id === id))))}
                    aria-label={filtered ? `Select the ${live.length} agents shown` : `Select all ${live.length} agents`}
                  />
                  <span>
                    {checked.length > 0
                      ? `${checked.length} of ${selectable.length} selected`
                      : filtered
                        ? `${live.length + ended.length} of ${all.length} agents`
                        : `${all.length} agent${all.length === 1 ? "" : "s"}`}
                  </span>
                </label>
                {checked.length > 0 && (
                  <button type="button" className="ms-link" onClick={clear}>
                    Clear selection
                  </button>
                )}
              </>
            )}
            {folds && (
              <button type="button" className="ms-link ms-msg-open" aria-expanded={messaging} onClick={() => setMessaging(!messaging)}>
                <ChevronRight size={13} className="ms-chev" aria-hidden="true" />
                Message agents
              </button>
            )}
          </div>
          {phone && (!folds || messaging) && message}
          {live.length > 0 ? (
            <div {...touch.props} data-held={held || undefined}>
              <ul className="ms-list" ref={glide}>
                {live.map(renderRow)}
              </ul>
            </div>
          ) : filtered && ended.length === 0 ? (
            <p className="ms-quiet">
              No agent matches.{" "}
              <button
                type="button"
                className="ms-link"
                onClick={() => {
                  setQ("");
                  setView({ ...view, facets: {} });
                }}
              >
                Clear the filter
              </button>
            </p>
          ) : (
            <p className="ms-quiet">Nothing running. Start something new, or pick up an ended agent below.</p>
          )}
        </>
      )}

      {/* Search the standalone agents' conversations, without their child threads. */}
      {all.length > 0 && <SaidInConversations q={q} agents={searchAgents} />}

      {/* Once kiln's own agents are known, so its own sessions never show here for a moment. */}
      <React.Suspense fallback={null}>
        <Found all={all} />
      </React.Suspense>

      {ended.length > 0 && (
        <section className="ms-ended">
          <div className="ms-fold">
            <button type="button" className="ms-fold-btn" aria-expanded={showEnded} onClick={() => setShowEnded(!showEnded)}>
              <ChevronRight size={13} className="ms-chev" aria-hidden="true" />
              Ended · {ended.length}
            </button>
            {ended.length > 0 && <button type="button" className="ms-link" onClick={() => void Promise.allSettled(ended.map((r) => harnessApi.stop(r.a.id))).then(() => reload())}>
              Clear
            </button>}
          </div>
          {showEnded && (
            <ul className="ms-list">
              {ended.map(renderRow)}
            </ul>
          )}
        </section>
      )}
    </>
  );
}
