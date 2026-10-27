/**
 * Mission control: one list of your agents, the ones that need you first: a row says in one line
 * what that agent is doing — or what it wants, or what it finished — and you answer it right there.
 * An agent that comes to need you rises as it does; while you're using the list, rows hold still,
 * then glide to their places.
 */
import * as React from "react";
import "./harness.css";
import "./mission.css";
import { Link, useNavigate } from "@/lib/router";
import { ArrowUpRight, ChevronRight } from "lucide-react";
import { api } from "@/api/client";
import { clientId, harnessApi, type AgentInfo, type ApprovalBrief } from "@/api/harness";
import type { AiSession } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField, Skeleton } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useAgentsLive } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage, useLocalStorage, useMediaQuery, useNow } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { Tries } from "@/lib/tries";
import { heldWords } from "./cards";
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
import { missionFoundSessions, missionRows, subagentsLine, type MissionRow, type MissionStand as Stand } from "./mission-rows";
import { NewAgent } from "./new-agent";
import { MISSION_DRAFT } from "./new-agent-draft";
import { useDrafts } from "./state";
import { useAgentList, useAgents } from "./use-agents";
import { both, touchSelect, type TouchSelect } from "./touch-select";
import { answer, asAnswered, useAnswered } from "./answered";
import { PushOffer } from "./push-offer";
import { RowChange, RowReview } from "./change-bar";
import { Shimmer } from "./signals";
import { subagentsAtWork, throughSubagents } from "./tasks";
import { Stands } from "./stands";

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

/** An agent waiting for room on its machine to go to work (`room_wait`): what it was sent waits in its queue. */
const ROOM = "Waiting for room: its machine has as many agents at work as its limit lets";

/** The one line that matters for this agent right now. */
function lineOf(a: AgentInfo, stand: Stand): string {
  switch (stand) {
    case "needs": {
      const ap = a.approvals[0];
      if (!ap && a.held) return heldWords(a.held.why) ? `Paused its goals for you: ${heldWords(a.held.why)}` : "Paused its goals for you";
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
      if (a.room_wait) return ROOM;
      return a.summary ? plain(a.summary) : a.goal || a.title || "Done";
    case "ready":
      return a.room_wait ? ROOM : a.goal || a.title || "No task yet";
    case "offline":
      return "Its machine is offline";
    case "ended":
      return a.room_wait ? ROOM : a.goal || a.title || "Ended";
  }
}

/** Answered at once (answered.ts): the row moves on now; if the answer doesn't go, it's back. */
function Decide({ a, ap }: { a: AgentInfo; ap: ApprovalBrief }) {
  const decide = (decision: "allow" | "deny") => void answer(a.id, { type: "approve", id: ap.id, decision }).catch((e: unknown) => toast.error("Couldn't answer", errorMessage(e)));
  if (ap.tool === "question")
    return (
      <Button size="xs" asChild>
        <Link to={`/agents/${a.id}`}>Answer</Link>
      </Button>
    );
  // A plan it proposes is approved here; to keep it planning (with what to change), review it on its page.
  return (
    <>
      <Button size="xs" onClick={() => decide("allow")}>
        {ap.tool === "plan" ? "Approve" : "Allow"}
      </Button>
      {ap.tool !== "plan" && (
        <Button size="xs" variant="secondary" onClick={() => decide("deny")}>
          Deny
        </Button>
      )}
      <Link to={`/agents/${a.id}`} className="ms-link">
        Review
      </Link>
    </>
  );
}

/** Goals it paused for you: the plan they wait on to look at, and Resume. */
function HeldDecide({ a }: { a: AgentInfo }) {
  // Asked again (its answer lost, or it failed), the same resume under its key (CP-T3.2).
  const [tries] = React.useState(() => new Tries(clientId));
  const resume = () =>
    void harnessApi.goalAction(a.id, "start", undefined, tries.keyFor(a.id)).then(
      () => tries.done(),
      (e: unknown) => toast.error("Couldn't resume its goals", errorMessage(e)),
    );
  if (!a.held?.plan)
    return (
      <Button size="xs" onClick={resume}>
        Resume
      </Button>
    );
  return (
    <>
      <Button size="xs" asChild>
        <Link to={`/agents/plans/${a.held.plan}`}>Review the plan</Link>
      </Button>
      <Button size="xs" variant="secondary" onClick={resume}>
        Resume
      </Button>
    </>
  );
}

/**
 * A row re-renders only when its own agent changes (or is selected); its clock counts by itself.
 * `picking`: agents are being selected by touch (its mark says whether it is). `asking`: a subagent
 * kiln started for it that waits on you: what it asks is said, and answered, here.
 */
const Row = React.memo(function Row({
  a,
  stand,
  asking,
  selected,
  picking,
  onSelect,
}: {
  a: AgentInfo;
  stand: Stand;
  asking?: AgentInfo;
  selected: boolean;
  picking?: boolean;
  onSelect: (id: string, on: boolean) => void;
}) {
  const navigate = useNavigate();
  const open = () => navigate(`/agents/${a.id}`);
  // Who waits on you: the agent, or its subagent (answered as that one; opened in the agent's chat, its thread shown).
  const who = asking ?? a;
  const ap = who.approvals[0];
  const actions = useAgentActions(a);
  const label = agentLabel(a);
  // The harness, said under a label that isn't its own name.
  const kind = label.toLowerCase() !== a.kind_name.toLowerCase();
  const line = (
    <span className="ms-line">
      {/* Working: the prism flows through its word, each agent's its own way, so the list moves as one field. */}
      <Shimmer on={stand === "working"} flow={a.id} className="ms-word">
        <i aria-hidden="true" />
        {WORD[stand]}
      </Shimmer>
      <span className="ms-text">{asking ? `Its subagent \u201c${agentLabel(asking)}\u201d: ${lineOf(asking, "needs")}` : lineOf(a, stand)}</span>
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
          {stand === "needs" && ap && <Decide a={who} ap={ap} />}
          {stand === "needs" && !ap && who.held && <HeldDecide a={who} />}
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

/** Where each agent stands (what you answered here counted as taken; the subagents kiln started for one on its row), and how many stand where (the page's lead says so). */
function standsOf(list: AgentInfo[], seen: Record<string, number>, done: ReadonlyMap<string, number>): MissionRow[] {
  return missionRows(
    list.map((agent) => asAnswered(agent, done)),
    seen,
  );
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

/** How your agents stand, in a row of its own under the page's head (stands.tsx). */
function StandsNow() {
  const list = useAgentList();
  const seen = useSeen();
  const done = useAnswered();
  const rows = React.useMemo(() => standsOf(list, seen, done), [list, seen, done]);
  return <Stands rows={rows} />;
}

export default function Mission() {
  return (
    <div className="container-console page ms-page">
      <title>Agents · kiln</title>
      <PageIntro index={6} label="Agents" title="Mission control." compact />
      <React.Suspense fallback={<div className="st" aria-hidden="true" />}>
        <StandsNow />
      </React.Suspense>
      <Loadable label="Loading agents" fallback={<RowsLoading />}>
        <Agents />
      </Loadable>
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
      if (n > 0)
        toast({
          title: `${n} agent${n === 1 ? "" : "s"} of ${machine} going on elsewhere`,
          description: failed.length > 0 ? `${failed.length} couldn't: ${failed[0]?.error ?? ""}` : undefined,
          tone: "running",
        });
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

/**
 * The composer at the top of the page (no dialog, no New agents button): with agents selected it
 * messages them (broadcast.tsx); with none it starts one (new-agent.tsx). What's written stays as it
 * turns from one to the other, and the turn is a quick crossfade, the same lines in the same places.
 */
function MissionComposer({
  picked,
  all,
  onPick,
  onClear,
  focusFor,
}: {
  picked: AgentInfo[];
  all: AgentInfo[];
  onPick: (ids: string[], on: boolean) => void;
  onClear: () => void;
  focusFor: "new" | "message" | null;
}) {
  const text = useDrafts((s) => s.drafts[MISSION_DRAFT] ?? "");
  const setDraft = useDrafts((s) => s.setDraft);
  const onText = React.useCallback((t: string | ((was: string) => string)) => setDraft(MISSION_DRAFT, typeof t === "function" ? t(useDrafts.getState().drafts[MISSION_DRAFT] ?? "") : t), [setDraft]);
  return picked.length > 0 ? (
    <div className="mc-mode" key="message">
      <MessageBar agents={picked} reach="selected" onEnded={onClear} all={all} onPick={onPick} text={text} onText={onText} onClear={onClear} autoFocus={focusFor === "message"} />
    </div>
  ) : (
    <div className="mc-mode" key="new">
      <NewAgent text={text} onText={onText} all={all} onPick={onPick} autoFocus={focusFor === "new"} />
    </div>
  );
}

function Agents() {
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
  // Where the composer's field takes the focus as it turns: to messaging, from an agent picked with @ in it; back to starting one, from Clear.
  const [focusFor, setFocusFor] = React.useState<"new" | "message" | null>(null);

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
  const select = React.useCallback(
    (id: string, on: boolean) => {
      setFocusFor(null);
      setMany([id], on);
    },
    [setMany],
  );
  // The rows' gestures ask what's selected now; their props are made once, with the list's hold.
  const [touch] = React.useState(() => {
    const now: { s: TouchSelect } = { s: { isSelected: () => false, set: () => undefined, selecting: false } };
    return {
      now,
      props: both(
        bind,
        touchSelect(() => now.s),
      ),
    };
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
  const findable = React.useMemo(() => all.map((r) => r.a), [all]);
  // Nothing selected, it starts an agent; agents selected, it messages those (and nothing else: "every agent shown" is Select all).
  const composer = (
    <MissionComposer
      picked={checked.map((r) => r.a)}
      all={findable}
      onPick={(ids, on) => {
        setMany(ids, on);
        setFocusFor("message");
      }}
      onClear={() => {
        clear();
        setFocusFor("new");
      }}
      focusFor={focusFor}
    />
  );
  const renderRow = ({ a, stand, asking }: MissionRow) => <Row key={a.id} a={a} stand={stand} asking={asking} selected={selected.has(a.id)} picking={touchScreen && selecting} onSelect={select} />;
  // Every agent kiln runs, the subagents on their agents' rows too: none is listed again as found on its machine.
  const everyone = React.useMemo(() => list.map((a) => ({ a })), [list]);

  return (
    <>
      {composer}
      {all.length > 0 && (
        <>
          <ListBar rows={all} q={q} onQuery={setQ} view={view} onView={setView} />
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
          <p>Claude Code, Codex, or any harness your machines have — one or a dozen: tell the box above what to do, and it starts. kiln runs them: steer them, answer them, review what they change.</p>
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
                    <button
                      type="button"
                      className="ms-link"
                      disabled={live.length === 0 || liveChecked === live.length}
                      onClick={() =>
                        setMany(
                          live.map((r) => r.a.id),
                          true,
                        )
                      }
                    >
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
          </div>
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
        <Found all={everyone} />
      </React.Suspense>

      {ended.length > 0 && (
        <section className="ms-ended">
          <div className="ms-fold">
            <button type="button" className="ms-fold-btn" aria-expanded={showEnded} onClick={() => setShowEnded(!showEnded)}>
              <ChevronRight size={13} className="ms-chev" aria-hidden="true" />
              Ended · {ended.length}
            </button>
            {ended.length > 0 && (
              <button type="button" className="ms-link" onClick={() => void Promise.allSettled(ended.map((r) => harnessApi.stop(r.a.id))).then(() => reload())}>
                Clear
              </button>
            )}
          </div>
          {showEnded && <ul className="ms-list">{ended.map(renderRow)}</ul>}
        </section>
      )}
    </>
  );
}
