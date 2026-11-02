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
import { ArrowUpRight, ChevronDown, ChevronRight, ChevronUp, GitFork, ListChecks, X } from "lucide-react";
import { api } from "@/api/client";
import { clientId, harnessApi, type AgentInfo, type ApprovalBrief } from "@/api/harness";
import { plansApi } from "@/api/plans";
import type { AiSession } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField, Skeleton } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { pushDevicesRes, pushStateRes, thisDeviceRes } from "@/api/push";
import { agentsRes, labelOf, useAgentsLive, useLabels, useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage, useLocalStorage, useMediaQuery, useNow } from "@/lib/hooks";
import { invalidate, preload, useSuspenseResource } from "@/lib/resource";
import { Tries } from "@/lib/tries";
import { heldWords } from "./cards";
import { agentLabel, hasOwnName, plain } from "./derive";
import { pauseLine } from "./pause";
import { NameField, useAgentActions } from "./actions";
import { MessageBar } from "./broadcast";
import { BulkBar } from "./bulk";
import { DEFAULT_VIEW, isFiltered, ListBar, sortOf, useGlide, useListHold, useShownList, type ListView } from "./list-tools";
import { SaidInConversations } from "./said";
import { byStart, FoundSessions, sessionKey } from "./found";
import { Ago, machineName, machineShown } from "./parts";
import { LowBattery } from "../battery";
import { useSeen } from "./seen";
import { missionFoundSessions, missionRows, subagentsLine, type MissionRow, type MissionStand as Stand } from "./mission-rows";
import { MachinesNote, machinesRes, NewAgent, NewAgentLoading } from "./new-agent";
import { MISSION_DRAFT } from "./new-agent-draft";
import { useDrafts } from "./state";
import { preloadAgents, useAgentList, useAgents } from "./use-agents";
import { both, touchSelect, type TouchSelect } from "./touch-select";
import { answer, asAnswered, useAnswered } from "./answered";
import { PushOffer } from "./push-offer";
import { isMarked, markNotices, useMarks } from "./notices";
import { swipeAway } from "./swipe-away";
import { RowChange, RowReview } from "./change-bar";
import { Shimmer } from "./signals";
import { subagentsAtWork, throughSubagents } from "./tasks";
import { Stands } from "./stands";
import { KindGlyph } from "./kind-glyph";
import { NODE_WORD, subagentRows, type SubagentRow } from "./subagent-tree";
import { PauseButton } from "./subagent-pause";
import { TreePath } from "./tree-path";

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

/** Answered at once (answered.ts): the row moves on now; if the answer doesn't go, it's back. `to`: where what `a` asks is answered. */
function Decide({ a, ap, to }: { a: AgentInfo; ap: ApprovalBrief; to: string }) {
  const decide = (decision: "allow" | "deny") => void answer(a.id, { type: "approve", id: ap.id, decision }).catch((e: unknown) => toast.error("Couldn't answer", errorMessage(e)));
  if (ap.tool === "question")
    return (
      <Button size="xs" asChild>
        <Link to={to}>Answer</Link>
      </Button>
    );
  // A plan it proposes is approved here; read, rejected or answered in your own words in its Plan tab (views/plans/review.tsx).
  const review = ap.tool === "plan" && to === `/agents/${a.id}` ? `${to}?tab=plan` : to;
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
      <Link to={review} className="ms-link">
        Review
      </Link>
    </>
  );
}

/** Goals it paused for you: a plan they wait on approved here, or reviewed in its Plan tab (views/plans/review.tsx); else Resume. */
function HeldDecide({ a }: { a: AgentInfo }) {
  // Asked again (its answer lost, or it failed), the same resume (or approval) under its key (CP-T3.2).
  const [tries] = React.useState(() => new Tries(clientId));
  const resume = () =>
    void harnessApi.goalAction(a.id, "start", undefined, tries.keyFor(a.id)).then(
      () => tries.done(),
      (e: unknown) => toast.error("Couldn't resume its goals", errorMessage(e)),
    );
  const plan = a.held?.plan;
  if (!plan)
    return (
      <Button size="xs" onClick={resume}>
        Resume
      </Button>
    );
  const approve = () =>
    void plansApi.review(plan, { answer: "approve", key: tries.keyFor(`approve ${plan} ${a.held?.since ?? 0}`) }).then(
      () => tries.done(),
      (e: unknown) => toast.error("Couldn't approve its plan", errorMessage(e)),
    );
  return (
    <>
      <Button size="xs" onClick={approve}>
        Approve
      </Button>
      <Link to={`/agents/${a.id}?tab=plan`} className="ms-link">
        Review
      </Link>
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
  tree,
  onTree,
}: {
  a: AgentInfo;
  stand: Stand;
  asking?: AgentInfo;
  selected: boolean;
  picking?: boolean;
  onSelect: (id: string, on: boolean) => void;
  /** Its subagents' tree is open under it (undefined: it has none). */
  tree?: boolean;
  onTree?: (id: string) => void;
}) {
  useLabels();
  const navigate = useNavigate();
  // Who waits on you: the agent, or its subagent (answered as that one; opened in the agent's chat, its thread shown).
  const who = asking ?? a;
  const ap = who.approvals[0];
  // A plan of its own waiting on your review: opened where it's reviewed, its Plan tab (a phone's row has no Review).
  const review = stand === "needs" && !asking && (ap ? ap.tool === "plan" : Boolean(a.held?.plan));
  const open = () => navigate(review ? `/agents/${a.id}?tab=plan` : `/agents/${a.id}`);
  // A subagent's thread straight, not by its own address: that one's redirect into its agent's chat never lands.
  const answerAt = asking ? `/agents/${a.id}?task=${encodeURIComponent(asking.id)}` : `/agents/${a.id}`;
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
            {/* Always there by touch, so it slides in as picking starts rather than knocking the name aside. */}
            <span className="ms-pick" data-on={picking || undefined} aria-hidden="true" />
            {label}
          </span>
          {line}
        </button>
      )}
      <span className="ms-meta">
        <span>
          {machineShown(a.target)} · <Ago className="ms-time" ts={Math.floor(a.last_activity / 1000)} />
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
          {stand === "needs" && ap && <Decide a={who} ap={ap} to={answerAt} />}
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
          {tree !== undefined && (
            <button
              type="button"
              className="ms-subs"
              aria-expanded={tree}
              aria-label={tree ? `Hide ${label}'s subagents` : `Show ${label}'s subagents`}
              data-on={tree || undefined}
              onClick={() => onTree?.(a.id)}
            >
              <KindGlyph kind="agent" />
              {tree ? <ChevronUp size={12} aria-hidden="true" /> : <ChevronDown size={12} aria-hidden="true" />}
            </button>
          )}
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

/**
 * An agent's subagents under its row, drawn as the tree they are (subagent-tree.ts), each in the
 * row's own columns: the path where the checkbox is, its name and what it's at, its machine and
 * time, what to answer and its way in. `rowAsk`: the request the row itself answers, not again here.
 */
function MissionTree({ id, list, rowAsk }: { id: string; list: readonly AgentInfo[]; rowAsk?: string }) {
  const root = list.find((x) => x.id === id);
  const rows = React.useMemo(() => (root ? subagentRows(root, list) : []), [root, list]);
  if (!root || rows.length === 0) return null;
  return (
    <li className="ms-tree" data-glide={`${id}:tree`}>
      <ul className="ms-tree-rows" aria-label={`${agentLabel(root)}'s subagents`}>
        {rows.map((r) => (
          <MissionNode key={r.key} r={r} owner={list.find((x) => x.id === r.session)} rowAsk={rowAsk} />
        ))}
      </ul>
    </li>
  );
}

/** One subagent in the tree under its agent's row. */
function MissionNode({ r, owner, rowAsk }: { r: SubagentRow; owner: AgentInfo | undefined; rowAsk?: string }) {
  // Its machine by the name people see (docs/machines.md).
  useLabels();
  const navigate = useNavigate();
  const coarse = useMediaQuery("(pointer: coarse)");
  const open = () => navigate(r.href);
  const ask = r.stand === "needs" && r.ask && r.ask.id !== rowAsk ? r.ask : undefined;
  return (
    <li className="ms-node" data-stand={r.stand} data-depth={r.depth}>
      <TreePath path={r.path} stand={r.stand} className="ms-node-path" x0={coarse ? 12 : 17} dx={10} y={19} width={coarse ? 40 : 46} />
      <button type="button" className="ms-node-main" onClick={open}>
        <span className="ms-node-name">
          {r.variant && <GitFork size={12} className="tree-fork" role="img" aria-label="Variant" />}
          {r.name}
        </span>
        <span className="ms-node-line">
          <span className="ms-word">
            <i aria-hidden="true" />
            {NODE_WORD[r.stand]}
          </span>
          <span className="ms-node-text" data-mono={r.mono || undefined}>
            {r.line}
          </span>
          {r.stand !== "done" && r.started > 0 && <Ago className="ms-node-time" ts={Math.floor(r.started / 1000)} />}
        </span>
      </button>
      <span className="ms-node-meta">
        <span>
          {labelOf(r.machine)}
          {r.started > 0 && (
            <>
              {" · "}
              <Ago className="ms-time" ts={Math.floor((r.ended ?? r.started) / 1000)} />
            </>
          )}
        </span>
        <span>{r.harness}</span>
      </span>
      <span className="ms-node-actions">
        <span className="ms-decide">{ask && owner && <Decide a={owner} ap={ask} to={r.href} />}</span>
        <span className="ms-tail">
          <PauseButton n={r} className="ms-go" size={15} />
          <button type="button" className="ms-go" onClick={open} aria-label={`Open ${r.name}`}>
            <ArrowUpRight size={16} />
          </button>
        </span>
      </span>
    </li>
  );
}

/** Where each agent stands (what you answered here counted as taken; the subagents kiln started for one on its row), and how many stand where (the page's lead says so). */
function standsOf(list: AgentInfo[], seen: Record<string, number>, done: ReadonlyMap<string, number>): MissionRow[] {
  return missionRows(
    list.map((agent) => asAnswered(agent, done)),
    seen,
  );
}

const NO_COUNTS = {};
const ONE_STAND = ["working"] as const;

/**
 * Before the agents are known: the page's shape (the stands' line, the composer with what's written
 * in it, the filter and the list's head over a few rows), so nothing moves when they come. What only
 * their answer can tell (a note under the composer, a notice over the list) comes with them, in one go.
 */
function PageLoading() {
  const text = useDrafts((s) => s.drafts[MISSION_DRAFT] ?? "");
  return (
    <div className="ms-loading" role="status" aria-label="Loading agents">
      <div className="ms-loading-sd" aria-hidden="true">
        <Stands counts={NO_COUNTS} every={ONE_STAND} />
      </div>
      <div className="mc-mode" data-first="">
        <NewAgentLoading text={text} />
      </div>
      <div className="filter-bar ms-bar" aria-hidden="true">
        <Skeleton className="ms-loading-bar" />
      </div>
      <div className="ms-listhead" aria-hidden="true" />
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

const ask = <T,>(r: Res<T>) => preload(r.key, r.fetch, r.opts);

/**
 * What shows over the rows, read with them so the page comes whole: More options and the
 * composer's notes (the machines' answer), a machine gone or low (the runners), and the
 * notifications offer. It draws nothing: once the rows show, nothing over them moves as these come.
 */
function OverTheRows() {
  useRes(machinesRes);
  useRes(agentsRes);
  useRes(pushStateRes);
  useRes(thisDeviceRes);
  useRes(pushDevicesRes);
  return null;
}

export default function Mission() {
  // All of it asked for at once: each read would otherwise wait for the one before it.
  preloadAgents();
  ask(machinesRes);
  ask(agentsRes);
  ask(pushStateRes);
  ask(thisDeviceRes);
  ask(pushDevicesRes);
  return (
    <div className="container-console page ms-page">
      <title>Agents · kiln</title>
      <PageIntro index={5} label="Agents" title="Mission control." compact />
      <Loadable label="Loading agents" fallback={<PageLoading />}>
        <OverTheRows />
        <StandsNow />
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
 * The notice that machine `machine`, offline since `since` (s), may be gone: put away (× or a swipe)
 * or acted on, it stays so for you everywhere (notices.ts) until the machine goes offline again.
 */
const goneKey = (machine: string, since: number) => `machine-gone:${machine}:${Math.round(since)}`;

/**
 * Machines that have been offline a while with agents on them: if one is gone, its agents go on
 * elsewhere from kiln's copy of their conversations, each on the machine that suits it, or all on
 * one. A machine only restarting doesn't show here.
 */
function Recover({ rows }: { rows: { a: AgentInfo; stand: Stand }[] }) {
  const runners = useAgentsLive().data;
  const nowS = useNow() / 1000;
  const marks = useMarks();
  const offline = new Map<string, number>();
  for (const { a, stand } of rows) {
    if (stand !== "offline" || a.target === "local") continue;
    const m = machineName(a.target);
    offline.set(m, (offline.get(m) ?? 0) + 1);
  }
  const gone = [...offline].flatMap(([machine, count]) => {
    const r = runners.find((x) => x.name === machine);
    return r && !r.connected && nowS - r.last_seen > GONE_AFTER_S && !isMarked(marks, goneKey(machine, r.last_seen)) ? [{ machine, count, since: r.last_seen }] : [];
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
  useLabels();
  const [to, setTo] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const key = goneKey(machine, since);
  const away = () => markNotices([key], "away");
  const shown = labelOf(machine);
  const go = async () => {
    setBusy(true);
    try {
      const { moved } = await harnessApi.recover(machine, to || undefined);
      const failed = moved.filter((m) => m.error);
      const n = moved.length - failed.length;
      // All of them on elsewhere: done with, here and everywhere. Some left: the card stays for those.
      if (n > 0 && failed.length === 0) markNotices([key], "done");
      if (n > 0)
        toast({
          title: `${n} agent${n === 1 ? "" : "s"} of ${shown} going on elsewhere`,
          description: failed.length > 0 ? `${failed.length} couldn't: ${failed[0]?.error ?? ""}` : undefined,
          tone: "running",
        });
      else toast.error(`${shown}'s agents couldn't go on elsewhere`, failed[0]?.error ?? "");
      invalidate("harness/agents");
    } catch (e) {
      toast.error(`${shown}'s agents couldn't go on elsewhere`, errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="ms-recover" role="status" {...swipeAway<HTMLDivElement>(away)}>
      <span>
        {shown} has been offline since {relativeTime(since)}. If it&rsquo;s gone, its {count} agent{count === 1 ? "" : "s"} can go on elsewhere from kiln&rsquo;s copy of
        {count === 1 ? " its conversation" : " their conversations"}.
      </span>
      {others.length > 0 ? (
        <span className="ms-recover-go">
          <SelectField
            label={`Where ${shown}'s agents go on`}
            hideLabel
            value={to}
            onChange={setTo}
            options={[{ value: "", label: "Each where it suits best" }, ...others.map((m) => ({ value: m, label: `All on ${labelOf(m)}` }))]}
          />
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => void go()}>
            {busy ? "Moving…" : "Move them on"}
          </Button>
        </span>
      ) : (
        <span className="ms-recover-none">No other machine is connected to take them.</span>
      )}
      <button type="button" className="icon-btn ms-recover-close" data-variant="bare" data-size="sm" aria-label={`Put away ${shown}'s notice`} title="Put away" onClick={away}>
        <X size={14} />
      </button>
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
  // Both have the same rows (the line of settings, the box, a line under it, the word about the machines),
  // so they're the same height and the list under them stays where it is as the first is picked and the
  // last put back. Its height as it starts an agent is held while it messages all the same, for what only
  // starting one shows (More options open, the line a start leaves).
  const [held] = React.useState(() => ({ height: 0 }));
  // Only a turn crossfades: the composer the page opens with shows as it is, where its shape stood while the agents loaded.
  const mode = picked.length > 0 ? "message" : "new";
  const [opened] = React.useState(mode);
  const [turned, setTurned] = React.useState(false);
  if (!turned && mode !== opened) setTurned(true);
  const first = turned ? undefined : "";
  const measured = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el) return;
      // To the fraction of a pixel (offsetHeight rounds), or the list would still move by it.
      const seen = new ResizeObserver(([e]) => {
        held.height = e?.borderBoxSize[0]?.blockSize ?? el.getBoundingClientRect().height;
      });
      seen.observe(el);
      return () => seen.disconnect();
    },
    [held],
  );
  return picked.length > 0 ? (
    <div className="mc-mode" key="message" data-first={first} style={held.height > 0 ? { minHeight: held.height } : undefined}>
      <MessageBar
        agents={picked}
        reach="selected"
        onEnded={onClear}
        all={all}
        onPick={onPick}
        text={text}
        onText={onText}
        onClear={onClear}
        autoFocus={focusFor === "message"}
        under={
          <React.Suspense fallback={null}>
            <MachinesNote />
          </React.Suspense>
        }
      />
    </div>
  ) : (
    <div className="mc-mode" key="new" data-first={first} ref={measured}>
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
  // Putting back the last one picked (a row, its chip, the ring that picks them all) ends selecting, as Clear does.
  const [hadPicked, setHadPicked] = React.useState(false);
  if (selected.size > 0 && !hadPicked) setHadPicked(true);
  if (selected.size === 0 && hadPicked) {
    setHadPicked(false);
    setPicking(false);
  }
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
  // In the order they were picked: the composer's To line and the bulk bar add each one picked at the
  // end, and nothing picked before it moves (the list's own order changes as they work).
  const shownById = new Map(checked.map((r) => [r.a.id, r.a]));
  const picked = [...selected].flatMap((id) => {
    const a = shownById.get(id);
    return a ? [a] : [];
  });
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
      picked={picked}
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
  // The agents whose subagents' trees are open under their rows, and every agent as answered here (each tree's subagents and theirs).
  const [trees, setTrees] = React.useState<ReadonlySet<string>>(() => new Set());
  const toggleTree = React.useCallback(
    (id: string) =>
      setTrees((s) => {
        const next = new Set(s);
        if (!next.delete(id)) next.add(id);
        return next;
      }),
    [],
  );
  const answeredList = React.useMemo(() => list.map((agent) => asAnswered(agent, done)), [list, done]);
  const renderRow = ({ a, stand, asking }: MissionRow) => {
    const subs = (a.subagents?.length ?? 0) > 0;
    const open = subs && trees.has(a.id);
    return (
      <React.Fragment key={a.id}>
        <Row a={a} stand={stand} asking={asking} selected={selected.has(a.id)} picking={touchScreen && selecting} onSelect={select} tree={subs ? open : undefined} onTree={toggleTree} />
        {open && <MissionTree id={a.id} list={answeredList} rowAsk={(asking ?? a).approvals[0]?.id} />}
      </React.Fragment>
    );
  };
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
              // By touch: Select (or hold a row), then tap rows. Selecting, a ring where the rows' rings are
              // selects them all (or none, which ends it, as putting back the last one does); Clear ends it.
              <span className="ms-all">
                <input
                  ref={allBox}
                  type="checkbox"
                  className="ms-pick-all"
                  data-on={selecting || undefined}
                  tabIndex={selecting ? undefined : -1}
                  aria-hidden={selecting ? undefined : true}
                  disabled={!selecting || live.length === 0}
                  checked={live.length > 0 && liveChecked === live.length}
                  onChange={(e) => setSelected(e.target.checked ? new Set([...selected, ...live.map((r) => r.a.id)]) : new Set([...selected].filter((id) => !live.some((r) => r.a.id === id))))}
                  aria-label={filtered ? `Select the ${live.length} agents shown` : `Select all ${live.length} agents`}
                />
                <span className="ms-count" aria-live="polite">
                  <span key={selecting ? "picked" : "all"} className="ms-swap">
                    {selecting ? `${checked.length} selected` : filtered ? `${live.length + ended.length} of ${all.length} agents` : `${all.length} agent${all.length === 1 ? "" : "s"}`}
                  </span>
                </span>
              </span>
            ) : (
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
                <span key={checked.length > 0 ? "picked" : "all"} className="ms-swap">
                  {checked.length > 0
                    ? `${checked.length} of ${selectable.length} selected`
                    : filtered
                      ? `${live.length + ended.length} of ${all.length} agents`
                      : `${all.length} agent${all.length === 1 ? "" : "s"}`}
                </span>
              </label>
            )}
            {/* What to do with those selected, on the head's one line (held in view by touch while selecting, there from the start; by touch Select in its place before); what they started, from every agent kiln runs. */}
            <BulkBar
              agents={picked}
              all={list}
              open={selecting}
              idle={
                touchScreen && live.length > 0 ? (
                  <Button size="xs" variant="secondary" onClick={() => setPicking(true)}>
                    <ListChecks aria-hidden="true" />
                    Select
                  </Button>
                ) : undefined
              }
              onGone={clear}
              onClear={clear}
            />
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
