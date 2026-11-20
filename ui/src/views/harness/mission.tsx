/**
 * Attention plan 01a199e1b177752e84dad5dc990e6a15.
 * Mission control: one list of your agents, the ones that need you first: a row says in one line
 * what that agent is doing — or what it wants, or what it finished — and you answer it right there.
 * An agent that comes to need you rises as it does; while you're using the list, rows hold still,
 * then glide to their places.
 */
import * as React from "react";
import { useRouter } from "@tanstack/react-router";
import "./harness.css";
import "./mission.css";
import { Link, useNavigate, useSearchParams } from "@/lib/router";
import { ArrowUpRight, Pin, ChevronDown, ChevronRight, ChevronUp, GitFork, ListChecks, X } from "lucide-react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo, type ApprovalBrief } from "@/api/harness";
import { commandAgents, useAttention, useParkTime, type AttentionCommand, type AttentionMark, type AttentionSettings, type AttentionSnapshot, type AttentionUrgency } from "@/api/attention";
import { ownAgentAction } from "@/lib/own-agent-action";
import { ArchiveAsk, CopyAsk, ParkAsk, UndoLine } from "./attention-controls";
import { attentionRows, isParked, organizedLocally, untilWords, urgencyWords } from "./attention-state";
import { described, entriesOf, inverseOf, pinPlaces, placeOf, withTrees, withUndo, type Anchor, type Entry, type PinPlace, type UndoState } from "./attention-list";
import type { OrganizeItem } from "./actions";
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
import { useAction } from "@/lib/action";
import { invalidate, preload, setData, useSuspenseResource } from "@/lib/resource";
import { heldWords } from "./cards";
import { agentLabel, hasOwnName, plain } from "./derive";
import { pauseLine } from "./pause";
import { NameField, useAgentActions } from "./actions";
import { MessageBar } from "./broadcast";
import { BulkBar, Slide } from "./bulk";
import { DEFAULT_VIEW, intervention, isFiltered, ListBar, OPEN, sortOf, useGlide, useListHold, useShownList, type ListView } from "./list-tools";
import { SaidInConversations } from "./said";
import { byStart, FoundSessions, sessionKey } from "./found";
import { Ago, machineName } from "./parts";
import { LowBattery } from "../battery";
import { useSeen } from "./seen";
import { activityAt, missionAgents, missionFoundSessions, missionRows, subagentsLine, type MissionRow, type MissionStand as Stand } from "./mission-rows";
import { MachinesNote, machinesRes, NewAgent, NewAgentLoading } from "./new-agent";
import { agentfileChoicesRes } from "@/views/agentfiles/choose";
import { agentenvChoicesRes } from "@/api/agentenvs";
import { MISSION_DRAFT } from "./new-agent-draft";
import { useDrafts } from "./state";
import { preloadAgents, useAgentList, useAgents } from "./use-agents";
import { lastWordsOf, useLastWords, useSaid } from "@/api/last-words";
import { firstRows } from "./first-rows";
import { both, touchSelect, type TouchSelect } from "./touch-select";
import { agentsAnswered, answer, heldKey, resumeHeld, tried, tryKey, useAnswered } from "./answered";
import { PushOffer } from "./push-offer";
import { isMarked, markNotices, useMarks } from "./notices";
import { swipeAway, swipeRow } from "./swipe-away";
import { RowChange, RowReview } from "./change-bar";
import { Shimmer } from "./signals";
import { subagentsAtWork, throughSubagents } from "./tasks";
import { Stands } from "./stands";
import { GroupHead, StandCounts, OpenHead, SandboxLine, watchUnseen } from "./definitions";
import { MicroVmRows } from "./engine-rows";
import { agentdefsRes } from "@/api/agentdefs";
import { sandboxesRes } from "@/api/microvms";
import { KindGlyph } from "./kind-glyph";
import { agentIn, keepRows, NODE_WORD, nodeAsk, subagentTree, treePaths, type SubagentNode, type SubagentRow } from "./subagent-tree";
import { PauseButton } from "./subagent-pause";
import { ENDED, RemoveButton, useRemoving } from "./subagent-remove";
import { AnswerHere } from "./mission-answer";
import { TreePath } from "./tree-path";
import { sessionIntent } from "@/lib/session-prefetch";
import { listWindow, useWindowed } from "@/lib/list-window";

const WORD: Record<Stand, string> = {
  needs: "Needs you",
  crashed: "Stopped",
  paused: "Paused",
  working: "Working",
  finished: "Finished",
  ready: "Ready",
  offline: "Offline",
  ended: "Ended",
  archived: "Archived",
};

/** An archived chat's word: its archive or Restore may still be on its way to its machine. */
const archiveWord = (mark: AttentionMark | undefined) => (mark?.archive_status === "archiving" ? "Archiving…" : mark?.archive_status === "restoring" ? "Restoring…" : WORD.archived);

/** An agent waiting for room on its machine to go to work (`room_wait`): what it was sent waits in its queue. */
const ROOM = "Waiting for room: its machine has as many agents at work as its limit lets";

/** W21-R02: mission rows are buttons, so Link's intent preloading cannot discover this chunk.
 * Load only component code; private loaders and session readiness keep their own lifetimes.
 * The router deduplicates the one shared agent chunk across all target sessions.
 */
function useAgentCode() {
  const router = useRouter();
  return React.useCallback(() => router.loadRouteChunk(router.routesById["/_app/agents/$id"]), [router]);
}

/**
 * What an agent last said, as its row's line (W21-R02): none while it isn't known yet (the list told the agent
 * lean, and its row's are on their way: api/last-words.ts), never another text in its place; `none` once it's
 * known to have said nothing.
 */
const saidLine = (said: string | undefined, none: string): string => (said === undefined ? "" : said ? plain(said) : none);

/** The one line that matters for this agent right now. `said`: its last words, for a stand whose line they are. */
function lineOf(a: AgentInfo, stand: Stand, said?: string): string {
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
      return a.frozen != null ? "Paused" : a.pause ? pauseLine(a.pause, Date.now()) : "Paused";
    case "working": {
      if (a.room_wait != null) return ROOM;
      if (a.status === "idle" && a.queued > 0) return `${a.queued} message${a.queued === 1 ? "" : "s"} queued`;
      const plan = (a.todos_total ?? 0) > 0 ? `plan ${a.todos_done ?? 0}/${a.todos_total}` : "";
      // Its turn over, what its subagents do.
      const doing = a.status === "starting" ? (a.harness_session ? "Resuming" : "Starting") : throughSubagents(a) ? subagentsLine(subagentsAtWork(a.subagents)) : a.doing || a.activity || "Working";
      return [doing, plan].filter(Boolean).join(" · ");
    }
    case "finished":
      if (a.room_wait) return ROOM;
      return saidLine(said, a.goal || a.title || "Done");
    case "ready":
      return a.room_wait ? ROOM : a.goal || a.title || "No task yet";
    case "offline":
      return "Its machine is offline";
    case "ended":
      return a.room_wait ? ROOM : a.goal || a.title || "Ended";
    case "archived":
      return saidLine(said, a.goal || a.title || "Archived");
  }
}

/**
 * Answered at once (answered.ts): the row moves on now; if the answer doesn't go, it's back. `to`:
 * where what `a` asks is answered. A question opens under its row (`answering`, `onAnswer`).
 */
function Decide({ a, ap, to, answering, onAnswer }: { a: AgentInfo; ap: ApprovalBrief; to: string; answering?: boolean; onAnswer?: () => void }) {
  const decide = (decision: "allow" | "deny") => void answer(a.id, { type: "approve", id: ap.id, decision }).catch((e: unknown) => toast.error("Couldn't answer", errorMessage(e)));
  if (ap.tool === "question")
    return onAnswer ? (
      <Button size="xs" aria-expanded={answering} onClick={onAnswer}>
        Answer
      </Button>
    ) : (
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

/**
 * Goals it paused for you: a plan they wait on approved here, or reviewed in its Plan tab
 * (views/plans/review.tsx); else Resume. Either moves the row on at once (answered.ts), and back if
 * it doesn't go (W21-R15).
 */
function HeldDecide({ a }: { a: AgentInfo }) {
  // Asked again (its answer lost, or it failed), the same resume (or approval) under its key, its
  // card's own across its going and coming back (CP-T3.2, answered.ts).
  const card = heldKey(a);
  const resume = () =>
    void resumeHeld(a, () => harnessApi.goalAction(a.id, "start", undefined, tryKey(card, a.id))).then(
      () => tried(card),
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
    void resumeHeld(a, () => ownAgentAction(plansApi.review(plan, { answer: "approve", key: tryKey(card, `approve ${plan} ${a.held?.since ?? 0}`) }), [a.id])).then(
      () => tried(card),
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

/** Where the page was as each row's dropdown opened: closing it goes back there (kiln's principle 1). */
const scrolledAt = new Map<string, number>();

/**
 * A list's keys (kiln's principle 1): j and k move between its rows, Enter opens a row's dropdown (every
 * row has one), Esc closes the one open. Not while typing.
 */
function listKeys(e: React.KeyboardEvent<HTMLUListElement>, toggle: (id: string) => void, open: string | null) {
  if (e.altKey || e.ctrlKey || e.metaKey) return;
  const at = e.target instanceof HTMLElement ? e.target : null;
  if (!at || at.closest("input, textarea, select, [contenteditable]")) return;
  const rows = [...e.currentTarget.querySelectorAll<HTMLElement>(":scope > li.ms-row")];
  const row = at.closest<HTMLElement>("li.ms-row");
  const id = row?.dataset.agent;
  if (e.key === "j" || e.key === "k") {
    const i = row ? rows.indexOf(row) : -1;
    const next = rows[Math.max(0, Math.min(rows.length - 1, i + (e.key === "j" ? 1 : -1)))];
    next?.querySelector<HTMLElement>(".ms-main")?.focus();
    e.preventDefault();
  } else if (e.key === "Enter" && id && at.classList.contains("ms-main")) {
    e.preventDefault();
    toggle(id);
  } else if (e.key === "Escape" && open) {
    e.preventDefault();
    toggle(open);
    rows
      .find((r) => r.dataset.agent === open)
      ?.querySelector<HTMLElement>(".ms-main")
      ?.focus();
  }
}

/** The line asked under a row: when to park it, archiving it, or (its Restore not gone through) going on from kiln's saved copy. */
export type RowAsk = { kind: "park" } | { kind: "archive" } | { kind: "copy"; error: string };

/**
 * A row re-renders only when its own agent changes (or is selected); its clock counts by itself.
 * `picking`: agents are being selected by touch (its mark says whether it is). `asking`: a subagent
 * kiln started for it that waits on you: what it asks is said, and answered, here. Where it's kept
 * for you (attention plan 01a199e1b177752e84dad5dc990e6a15) is said on its line (Updated, Stale, a
 * pin, a correction, and what holds it where it is: `reason`) and changed from its ⋯ menu, a
 * stale request's Park and Archive, or a line under it (`ask`).
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
  updated,
  mark,
  reason,
  correction,
  stale,
  urgency,
  parked,
  pin,
  ask,
  failure,
  settings,
  onAsk,
  onOrganize,
  onPinMove,
  onFailure,
  onOpen,
}: {
  a: AgentInfo;
  stand: Stand;
  asking?: AgentInfo;
  selected: boolean;
  picking?: boolean;
  onSelect: (id: string, on: boolean) => void;
  /** Its dropdown is open under it: its subagents' tree, its engine, and the agent it was started from when it's one of yours. */
  tree: boolean;
  onTree?: (id: string) => void;
  /** It came back since you last looked: opening it, or Clear Updated from its menu, takes that off. */
  updated?: boolean;
  /** Where it's kept for you, as the server has it. */
  mark?: AttentionMark;
  /** What holds it where it is. */
  reason?: string;
  /** You brought it back from a park kiln made by itself. */
  correction?: boolean;
  stale?: boolean;
  urgency?: AttentionUrgency;
  /** In the Parked list. */
  parked?: boolean;
  /** Its place among your pins (none: it isn't pinned). */
  pin?: { index: number; count: number };
  ask?: RowAsk;
  /** What it couldn't do (its Remove refused: the server keeps it), said on it until put away. */
  failure?: string;
  settings: AttentionSettings;
  onAsk: (id: string, ask: RowAsk | null) => void;
  onOrganize: (commands: AttentionCommand[]) => void;
  onPinMove: (id: string, by: -1 | 1) => void;
  onFailure: (id: string, why: string | null) => void;
  onOpen: (agent: string) => void;
}) {
  useLabels();
  const navigate = useNavigate();
  const archived = stand === "archived";
  const prepare = useAgentCode();
  // Who waits on you: the agent, or its subagent (answered as that one; opened in the agent's chat, its thread shown).
  const who = asking ?? a;
  const ap = who.approvals[0];
  // Its last words, where they're its line: the list told it without them (W21-R02).
  const said = useLastWords(a, !asking && (stand === "finished" || stand === "archived"));
  // A question it asks, answered under its row: the one open, and the one shown (open, or sliding shut).
  const [answering, setAnswering] = React.useState<string | null>(null);
  const [shownAsk, setShownAsk] = React.useState<ApprovalBrief | null>(null);
  const question = stand === "needs" && ap?.tool === "question" ? ap : undefined;
  const open = question !== undefined && answering === question.id;
  const toggleAnswer = () => {
    if (!question) return;
    setAnswering(open ? null : question.id);
    if (!open) setShownAsk(question);
  };
  // The line asked under it: the one open, and the one shown (open, or sliding shut).
  const [shownLine, setShownLine] = React.useState<RowAsk | null>(null);
  if (ask && ask !== shownLine) setShownLine(ask);
  const lineGone = React.useCallback(() => setShownLine(null), []);
  // A plan of its own waiting on your review: opened where it's reviewed, its Plan tab (a phone's row has no Review).
  const review = stand === "needs" && !asking && (ap ? ap.tool === "plan" : Boolean(a.held?.plan));
  const openAgent = () => {
    onOpen(a.id);
    navigate(review ? `/agents/${a.id}?tab=plan` : `/agents/${a.id}`);
  };
  const mainIntent = React.useMemo(() => (picking ? {} : sessionIntent(a.id, prepare)), [a.id, picking, prepare]);
  const goIntent = React.useMemo(() => sessionIntent(a.id, prepare), [a.id, prepare]);
  // A subagent's thread straight, not by its own address: that one's redirect into its agent's chat never lands.
  const answerAt = asking ? `/agents/${a.id}?task=${encodeURIComponent(asking.id)}` : `/agents/${a.id}`;
  // A Remove the server refuses (it keeps the agent: its archive or restore is under way) says so here, on its row.
  const actions = useAgentActions(a, { onFailed: (why) => onFailure(a.id, `Couldn't remove it: ${why}`) });
  // A name given here holds as it's taken, whatever older news of the agent says meanwhile.
  const label = agentLabel({ ...a, name: actions.name });
  const what = `\u201c${label}\u201d`;
  // Each says it's on its way until the server has it, and a press meanwhile does nothing (W21-R15).
  const [resume, resuming] = useAction(() => harnessApi.command(a.id, a.frozen != null ? { type: "pause", on: false } : { type: "resume" }), "Couldn't resume");
  const [restart, restarting] = useAction(() => harnessApi.command(a.id, { type: "restart" }), "Couldn't restart");
  // The harness, said under a label that isn't its own name.
  const kind = label.toLowerCase() !== a.kind_name.toLowerCase();
  const askFor = (k: "park" | "archive") => onAsk(a.id, ask?.kind === k ? null : { kind: k });
  // Where it's kept, from its menu: each line it opens takes the focus.
  const organize: OrganizeItem[] = [
    ...(updated && stand !== "needs" ? [{ label: "Clear Updated", onSelect: () => onOrganize([{ action: "seen", agent: a.id }]) }] : []),
    pin ? { label: "Unpin", onSelect: () => onOrganize([{ action: "unpin", agent: a.id }]) } : { label: "Pin", onSelect: () => onOrganize([{ action: "pin", agent: a.id }]) },
    ...(pin
      ? [
          { label: "Move pin up", disabled: pin.index === 0, onSelect: () => onPinMove(a.id, -1) },
          { label: "Move pin down", disabled: pin.index === pin.count - 1, onSelect: () => onPinMove(a.id, 1) },
        ]
      : []),
    parked ? { label: "Unpark", onSelect: () => onOrganize([{ action: "unpark", agent: a.id }]) } : { label: "Park until…", opens: true, onSelect: () => onAsk(a.id, { kind: "park" }) },
    { label: "Archive…", opens: true, onSelect: () => onAsk(a.id, { kind: "archive" }) },
  ];
  const line = (
    <span className="ms-line">
      {/* Working: the prism flows through its word, each agent's its own way, so the list moves as one field. */}
      <Shimmer on={stand === "working"} flow={a.id} className="ms-word">
        <i aria-hidden="true" />
        {archived ? archiveWord(mark) : WORD[stand]}
      </Shimmer>
      {stand === "needs" && stale && (
        <span className="ms-flag" data-flag="stale" title={urgency ? urgencyWords(urgency) : undefined}>
          Stale
        </span>
      )}
      {updated && stand !== "needs" && !archived && (
        <span className="ms-flag" data-flag="updated">
          Updated
        </span>
      )}
      {parked && mark?.automatic && (
        <span className="ms-flag" data-flag="automatic" title="kiln parked it by itself">
          Automatic
        </span>
      )}
      {correction && (
        <span className="ms-flag" data-flag="correction" title="You brought back a chat kiln parked by itself">
          Correction
        </span>
      )}
      {pin && <Pin size={11} className="ms-flag-pin" role="img" aria-label="Pinned" />}
      <span className="ms-text">{asking ? `Its subagent \u201c${agentLabel(asking)}\u201d: ${lineOf(asking, "needs")}` : lineOf(a, stand, said)}</span>
      {reason && (
        <span className="ms-reason" title={reason}>
          {reason}
        </span>
      )}
    </span>
  );
  return (
    <li
      ref={tree ? watchUnseen : undefined}
      className="ms-row"
      data-entry={`row:${a.id}`}
      data-stand={stand}
      data-selected={selected || undefined}
      data-parked={parked || undefined}
      data-stale={(stand === "needs" && stale) || undefined}
      data-glide={a.id}
      data-agent={a.id}
      // A link that opens it (Review, Answer) is a pick from the list too.
      onClickCapture={(event) => {
        const link = event.target instanceof Element ? event.target.closest("a") : null;
        if (link instanceof HTMLAnchorElement && link.pathname === `/agents/${encodeURIComponent(a.id)}`) onOpen(a.id);
      }}
    >
      <label className="ms-check">
        <input type="checkbox" className="check" checked={selected} disabled={archived} onChange={(e) => onSelect(a.id, e.target.checked)} aria-label={`Select ${label}`} />
      </label>
      {actions.renaming ? (
        <div className="ms-main">
          <NameField value={hasOwnName(a) ? a.name : ""} placeholder={label} onSave={actions.rename} onDone={actions.stopRenaming} className="ms-name-field" label="Agent name" />
          {line}
        </div>
      ) : archived ? (
        // Archived, it isn't open anywhere: Restore brings it back first.
        <div className="ms-main">
          <span className="ms-name">{label}</span>
          {line}
        </div>
      ) : (
        <button type="button" className="ms-main" {...mainIntent} onClick={openAgent} aria-pressed={picking ? selected : undefined}>
          <span className="ms-name">
            {/* Always there by touch, so it slides in as picking starts rather than knocking the name aside. */}
            <span className="ms-pick" data-on={picking || undefined} aria-hidden="true" />
            {label}
          </span>
          {line}
        </button>
      )}
      <span className="ms-meta">
        {/* Where it runs: its engine, its runner, how it started; then when it was last at work, and its harness. */}
        <span className="ms-where">
          <SandboxLine id={a.id} target={a.target} />
        </span>
        <span className="ms-meta-2">
          {activityAt(a) > 0 && <Ago className="ms-time" ts={Math.floor(activityAt(a) / 1000)} />}
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
          {stand === "needs" && ap && <Decide a={who} ap={ap} to={answerAt} answering={open} onAnswer={toggleAnswer} />}
          {stand === "needs" && !ap && who.held && <HeldDecide a={who} />}
          {/* Waited longer than any you've answered: it can wait parked, or go. */}
          {stand === "needs" && stale && (
            <>
              <Button size="xs" variant="ghost" aria-expanded={ask?.kind === "park"} onClick={() => askFor("park")}>
                Park
              </Button>
              <Button size="xs" variant="ghost" aria-expanded={ask?.kind === "archive"} onClick={() => askFor("archive")}>
                Archive…
              </Button>
            </>
          )}
          {stand === "paused" && (
            <Button size="xs" variant="secondary" pending={resuming} onClick={resume}>
              Resume
            </Button>
          )}
          {stand === "crashed" && (
            <Button size="xs" variant="secondary" pending={restarting} onClick={restart}>
              Restart
            </Button>
          )}
          {parked && (
            <Button size="xs" variant="secondary" onClick={() => onOrganize([{ action: "unpark", agent: a.id }])}>
              Unpark
            </Button>
          )}
          {archived && (
            <Button size="xs" variant="secondary" disabled={mark?.archive_status === "archiving"} onClick={() => onOrganize([{ action: "restore", agent: a.id }])}>
              Restore
            </Button>
          )}
          {(stand === "finished" || stand === "ready" || stand === "ended") && !parked && (a.files_changed ?? 0) > 0 && <RowReview agentId={a.id} />}
        </span>
        <span className="ms-tail">
          <button type="button" className="ms-subs" aria-expanded={tree} aria-label={`${tree ? "Hide" : "Show"} ${label}'s details`} data-on={tree || undefined} onClick={() => onTree?.(a.id)}>
            <KindGlyph kind="agent" />
            {tree ? <ChevronUp size={12} aria-hidden="true" /> : <ChevronDown size={12} aria-hidden="true" />}
          </button>
          {!archived && actions.menu(undefined, [], organize)}
          {!archived && actions.dialogs}
          {!archived && (
            <button type="button" className="ms-go" {...goIntent} onClick={openAgent} aria-label={`Open ${label}`}>
              <ArrowUpRight size={16} />
            </button>
          )}
        </span>
      </span>
      {failure && (
        <div className="ms-row-ask">
          <div className="ms-bulk-ask ms-ask" role="alert">
            <span className="ms-bulk-q ms-ask-error">{failure}</span>
            <Button size="xs" variant="ghost" onClick={() => onFailure(a.id, null)}>
              Dismiss
            </Button>
          </div>
        </div>
      )}
      {shownLine && (
        <div className="ms-row-ask">
          <Slide open={ask === shownLine} onGone={lineGone}>
            {shownLine.kind === "park" ? (
              <ParkAsk what={what} settings={settings} onPark={(until) => onOrganize([{ action: "park", agent: a.id, until }])} onCancel={() => onAsk(a.id, null)} />
            ) : shownLine.kind === "archive" ? (
              <ArchiveAsk what={what} n={1} onArchive={() => onOrganize([{ action: "archive", agent: a.id }])} onCancel={() => onAsk(a.id, null)} />
            ) : (
              <CopyAsk what={what} error={shownLine.error} onContinue={() => onOrganize([{ action: "restore_copy", agent: a.id }])} onCancel={() => onAsk(a.id, null)} />
            )}
          </Slide>
        </div>
      )}
      {shownAsk && (
        <div className="ms-answer">
          <Slide open={open && shownAsk.id === question?.id} onGone={() => setShownAsk(null)}>
            <AnswerHere who={who} ap={shownAsk} />
          </Slide>
        </div>
      )}
    </li>
  );
});

/**
 * An agent's subagents under its row, drawn as the tree they are (subagent-tree.ts), each in the
 * row's own columns: the path where the checkbox is, its name and what it's at, its machine and
 * time, what to answer and its way in. `rowAsk`: the request the row itself answers, not again here.
 */
/**
 * A row's dropdown, every row's, one open a list (in the URL, `?open=ID`), a line of its windowed list
 * (`tree:ID`) under one head that stays in sight: its subagents first, then, for a session in a microVM,
 * its sandboxes and its agent's settings and experiments (engine-rows.tsx). Its subagents' rows are made
 * again at every word from any agent (W26-R15): each row whose subagent says what it said is the object
 * it was, so of a hundred subagents only those with news are drawn again; and as a subagent's last words
 * are told, one that's done shows them (W21-R02).
 */
const MissionTree = React.memo(function MissionTree({ id, list, rowAsk, onClose }: { id: string; list: readonly AgentInfo[]; rowAsk?: string; onClose: (id: string) => void }) {
  const root = agentIn(list, id);
  const { going, remove } = useRemoving();
  const [kept] = React.useState(() => ({ rows: [] as SubagentRow[] }));
  const said = useSaid();
  const rows = React.useMemo(() => {
    void said;
    kept.rows = keepRows(kept.rows, root ? drawn(subagentTree(root, list).filter((n) => !going.has(n.key))) : []);
    return kept.rows;
  }, [root, list, going, kept, said]);
  // Its subagents' group, open, its head one line; every row's, "none" where it has none.
  const [shut, setShut] = React.useState(false);
  if (!root) return null;
  const atWork = rows.filter((r) => r.stand === "working" || r.stand === "needs").length;
  return (
    <li className="ms-tree ms-dropdown" data-entry={`tree:${id}`} data-glide={`${id}:tree`}>
      <OpenHead label={agentLabel(root)} onClose={() => onClose(id)} />
      <ul className="ms-tree-rows" aria-label={`${agentLabel(root)}'s details`}>
        <GroupHead
          label="Subagents"
          open={!shut}
          onOpen={() => setShut(!shut)}
          stand={atWork ? "working" : "done"}
          summary={rows.length ? <StandCounts stands={rows.map((r) => r.stand)} /> : <span className="df-head-v">none</span>}
        />
        {!shut && rows.map((r) => <MissionNode key={r.key} r={r} owner={nodeAsk(r, rowAsk) ? agentIn(list, r.session) : undefined} rowAsk={rowAsk} onRemove={remove} />)}
        {!shut && rows.length === 0 && (
          <li className="ms-node df-body" data-depth={2}>
            <span className="df-note">No subagents.</span>
          </li>
        )}
        <MicroVmRows a={root} list={list} />
      </ul>
    </li>
  );
});

/** A tree's rows as drawn: each with its piece of the path, from where it is among them. */
function drawn(nodes: SubagentNode[]): SubagentRow[] {
  const paths = treePaths(nodes);
  return nodes.map((n, i) => ({ ...n, path: paths[i] ?? { lane: n.depth, pieces: [], curve: null } }));
}

/**
 * One subagent in the tree under its agent's row: at work, its word carries the prism; one that
 * ended is removed by its trash, or by a swipe; a question it asks opens under it. It re-renders
 * only when what it says changes (its row is the same object until then; `onRemove` is its tree's).
 * `owner`: the agent its answer goes to, given while it asks (`nodeAsk`) and only then, so that
 * agent's own news doesn't draw again its subagents that ask nothing.
 */
const MissionNode = React.memo(function MissionNode({
  r,
  owner,
  rowAsk,
  onRemove,
}: {
  r: SubagentRow;
  owner: AgentInfo | undefined;
  rowAsk?: string;
  onRemove: (rows: readonly SubagentRow[], els?: readonly (HTMLElement | null)[]) => void;
}) {
  // Its machine by the name people see (docs/machines.md).
  useLabels();
  const navigate = useNavigate();
  const prepare = useAgentCode();
  const coarse = useMediaQuery("(pointer: coarse)");
  const open = () => navigate(r.href);
  const mainIntent = React.useMemo(() => sessionIntent(r.session, prepare), [r.session, prepare]);
  const goIntent = React.useMemo(() => sessionIntent(r.session, prepare), [r.session, prepare]);
  const ask = nodeAsk(r, rowAsk);
  // The agent it last asked through: its answer, sliding shut once given, is still that one's.
  const [asker] = React.useState(() => ({ last: owner }));
  if (owner) asker.last = owner;
  const who = owner ?? asker.last;
  const ended = ENDED.has(r.stand);
  const removeIt = (el: HTMLElement | null) => onRemove([r], [el]);
  const [answering, setAnswering] = React.useState<string | null>(null);
  const [shownAsk, setShownAsk] = React.useState<ApprovalBrief | null>(null);
  const question = ask?.tool === "question" ? ask : undefined;
  const asked = question !== undefined && answering === question.id;
  const toggleAnswer = () => {
    if (!question) return;
    setAnswering(asked ? null : question.id);
    if (!asked) setShownAsk(question);
  };
  return (
    <li className="ms-node" data-stand={r.stand} data-depth={r.depth} {...(ended ? swipeRow<HTMLLIElement>(() => removeIt(null), ".ms-node-main") : {})}>
      <TreePath path={r.path} stand={r.stand} className="ms-node-path" x0={coarse ? 12 : 17} dx={10} y={19} width={coarse ? 40 : 46} />
      <button type="button" className="ms-node-main" {...mainIntent} onClick={open}>
        <span className="ms-node-name">
          {r.variant && <GitFork size={12} className="tree-fork" role="img" aria-label="Variant" />}
          {r.name}
        </span>
        <span className="ms-node-line">
          <Shimmer on={r.stand === "working"} flow={r.key} className="ms-word">
            <i aria-hidden="true" />
            {NODE_WORD[r.stand]}
          </Shimmer>
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
        <span className="ms-decide">{ask && owner && <Decide a={owner} ap={ask} to={r.href} answering={asked} onAnswer={toggleAnswer} />}</span>
        <span className="ms-tail">
          {ended ? <RemoveButton n={r} className="ms-go ms-remove" size={15} onRemove={removeIt} /> : <PauseButton n={r} className="ms-go" size={15} />}
          <button type="button" className="ms-go" {...goIntent} onClick={open} aria-label={`Open ${r.name}`}>
            <ArrowUpRight size={16} />
          </button>
        </span>
      </span>
      {shownAsk && who && (
        <div className="ms-answer">
          <Slide open={asked && shownAsk.id === question?.id} onGone={() => setShownAsk(null)}>
            <AnswerHere who={who} ap={shownAsk} />
          </Slide>
        </div>
      )}
    </li>
  );
});

/**
 * Where each agent stands (what you answered here counted as taken; the subagents kiln started for one on its row), and how many stand where (the page's lead says so).
 * The page's head and its list both ask, of the same agents: what's made of them is made once (W26-R15: answered.ts `agentsAnswered`, mission-rows.ts `missionRows`).
 */
function standsOf(list: readonly AgentInfo[], seen: Record<string, number>, done: ReadonlyMap<string, number>): MissionRow[] {
  return missionRows(agentsAnswered(list, done), seen);
}

const NO_COUNTS = {};
const NO_IDS: readonly string[] = [];
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
  ask(agentfileChoicesRes);
  ask(agentenvChoicesRes);
  ask(agentsRes);
  ask(agentdefsRes);
  ask(sandboxesRes);
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

/** The ended agents cleared: gone from the list at once (W21-R15); any the server kept are back once it's read again, with why on each (`failed`). */
function clearEnded(ids: string[], reload: () => void, failed: (id: string, why: string) => void) {
  setData<AgentInfo[]>("harness/agents", (l) => l.filter((a) => !ids.includes(a.id)));
  void Promise.allSettled(ids.map((id) => harnessApi.stop(id))).then((done) => {
    done.forEach((d, i) => {
      const id = ids[i];
      if (d.status === "rejected" && id) failed(id, `Couldn't clear it: ${errorMessage(d.reason)}`);
    });
    reload();
  });
}

function RecoverMachine({ machine, count, since, others }: { machine: string; count: number; since: number; others: string[] }) {
  useLabels();
  const [to, setTo] = React.useState("");
  const key = goneKey(machine, since);
  const away = () => markNotices([key], "away");
  const shown = labelOf(machine);
  // Move them on says it's on its way until kiln has moved them (W21-R15).
  const [go, moving] = useAction(async () => {
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
  }, `${shown}'s agents couldn't go on elsewhere`);
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
          <Button size="xs" variant="secondary" pending={moving} onClick={go}>
            Move them on
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

/** The phone layout, as mission.css has it (a pointer's type isn't a layout): where a pick's position is learned for. */
const attentionLayout = () => (window.matchMedia("(max-width: 640px)").matches ? "mission/phone" : "mission/desktop");

/** One of Mission control's lists as it's on the page: its name (`data-list`) and its lines' keys, in order. */
type ShownList = { list: "live" | "parked" | "ended"; keys: readonly string[] };

/**
 * Where the first of `ids`' rows is, and what's around it (the rows `ids` names aside): read as you
 * act, from the lists as they show (`lists`; attention-list.ts `placeOf`). Its height from the page
 * while it's on it, else as its list's window last measured it (W21-R02: a row far from the viewport
 * isn't on the page).
 */
function anchorOf(ids: readonly string[], lists: readonly ShownList[]): Anchor | null {
  const at = placeOf(ids, lists);
  if (!at) return null;
  const first = document.querySelector<HTMLElement>(`ul[data-list="${at.list}"] > li[data-entry="${CSS.escape(at.key)}"]`);
  if (!first) return { list: at.list, prev: at.prev, next: at.next, height: listWindow(`mission/${at.list}`).heightOf(at.key) ?? 0 };
  // Its own height, without a line open under it (Park until…, an answer) and the row's gap before it: what stays when that line closes.
  const gap = parseFloat(getComputedStyle(first).rowGap) || 0;
  const lines = [...first.querySelectorAll(":scope > .ms-row-ask, :scope > .ms-answer")].reduce((h, el) => h + el.getBoundingClientRect().height + gap, 0);
  return { list: at.list, prev: at.prev, next: at.next, height: first.getBoundingClientRect().height - lines };
}

/** Tab stops in every line of a list but a section's head and the space that ends it, which have nothing to stop at. */
const stopsTab = (key: string) => !key.startsWith("sep:");
/**
 * A line stays mounted, wherever the page has gone, while something is open in it or a request of
 * its is on its way: an answer being written, the line asked under it, its name being edited, its
 * menu, a button that says its press is on its way (lib/action.ts `sendingProps`).
 */
const KEEP = '.ms-answer, .ms-row-ask, .ms-name-field, [data-sending], [data-state="open"]';

/**
 * One of Mission control's lists, windowed (W21-R02, lib/list-window.ts): only the lines near the
 * viewport are mounted (a row, a section's head, a tree of subagents, the Undo), gaps keeping the
 * room of the rest, so thousands of agents cost what a screenful does. It re-renders by itself as
 * the page scrolls. `order`: the list glides to its new order (list-tools.tsx `useGlide`).
 */
function Entries({
  list,
  entries,
  pinned,
  order,
  render,
  onKeyDown,
}: {
  list: ShownList["list"];
  entries: readonly Entry[];
  /** Lines mounted wherever they are. */
  pinned: ReadonlySet<string>;
  order?: string;
  render: (e: Entry) => React.ReactNode;
  /** The list's keys (`listKeys`). */
  onKeyDown?: (e: React.KeyboardEvent<HTMLUListElement>) => void;
}) {
  const win = listWindow(`mission/${list}`);
  const { ref, parts } = useWindowed(
    win,
    entries.map((e) => e.key),
    { attr: "data-entry", pinned, tabStop: stopsTab, keep: KEEP, open: OPEN, order },
  );
  const glide = useGlide(order ?? "", win);
  const both = React.useMemo(
    () =>
      order === undefined
        ? ref
        : (el: HTMLElement | null) => {
            const windowed = ref(el);
            const glided = glide(el);
            return () => {
              windowed?.();
              glided?.();
            };
          },
    [ref, glide, order],
  );
  return (
    <ul className="ms-list" data-list={list} ref={both} onKeyDown={onKeyDown}>
      {parts.map((p) => {
        // Never what the browser keeps where it is as the page changes (W21-R02): a gap stands for rows, its height and place change as the window does.
        if ("gap" in p) return <li key={`gap:${p.gap}`} data-gap="" aria-hidden="true" style={{ height: p.room, overflowAnchor: "none" }} />;
        const e = entries[p.row];
        return e ? render(e) : null;
      })}
    </ul>
  );
}

function Agents() {
  const [showParked, setShowParked] = React.useState(false);
  const [showEnded, setShowEnded] = useLocalStorage<boolean>("kiln.agents.showEnded", false);
  const [stored, setView] = useLocalStorage<Partial<ListView>>("kiln.agents.view", DEFAULT_VIEW);
  const [q, setQ] = React.useState("");
  const [selected, setSelected] = React.useState<ReadonlySet<string>>(new Set());
  const [picking, setPicking] = React.useState(false);
  // The one row whose dropdown is open lives in the URL (`?open=ID`): reload, back and a shared link
  // open it again, and Back closes it (kiln's principle 1). One a list.
  const [params, setParams] = useSearchParams();
  const openRow = params.get("open");
  // As a set, the lines the windowed lists open under their rows (attention-list.ts `withTrees`).
  const trees = React.useMemo<ReadonlySet<string>>(() => new Set(openRow ? [openRow] : []), [openRow]);
  // The one line open under a row (Park until…, Archive…, a Restore that didn't go), and the Undo where a row was.
  const [asked, setAsked] = React.useState<{ id: string; ask: RowAsk } | null>(null);
  const [undo, setUndo] = React.useState<UndoState | null>(null);
  // The rows an Undo is putting back, until the server has it.
  const [restoring, setRestoring] = React.useState<readonly string[]>(NO_IDS);
  // Removes the server refused (it keeps those agents), each said on its row until put away.
  const [failures, setFailures] = React.useState<Readonly<Record<string, string>>>({});
  const fail = (id: string, why: string | null) =>
    setFailures((f) => {
      const next = { ...f };
      if (why === null) delete next[id];
      else next[id] = why;
      return next;
    });
  const touchScreen = useMediaQuery("(pointer: coarse)");
  useMediaQuery("(max-width: 640px)");
  const selecting = picking || selected.size > 0;
  const { held, bind } = useListHold(selecting);
  const controls = JSON.stringify({ q, stored, showParked, showEnded, selected: [...selected].sort(), picking, trees: [...trees].sort(), layout: attentionLayout() });
  const attention = useAttention(controls, firstRows());
  const { list, reload } = useAgents();
  const snapshot = attention.data;
  const now = useParkTime(snapshot.agents);
  const seen = React.useMemo(() => {
    const next: Record<string, number> = {};
    for (const [id, mark] of Object.entries(snapshot.agents)) next[id] = Math.max(next[id] ?? 0, mark.updated_seen ?? 0);
    return next;
  }, [snapshot.agents]);
  // Selecting by touch (touch-select.ts): from a row held, or from Select, until Done.
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
  const rows = React.useMemo(
    () =>
      attentionRows(
        // The list itself while none of it is archived, so what the page's head made of it stands for these rows too.
        standsOf(list.some((a) => snapshot.agents[a.id]?.archived) ? list.filter((a) => !snapshot.agents[a.id]?.archived) : list, seen, done),
        snapshot,
      ),
    [list, seen, done, snapshot],
  );
  // Parked: in a folded list of their own, the ones coming back soonest first.
  const parked = React.useMemo(
    () => rows.filter((r) => isParked(r, snapshot, now)).sort((x, y) => (snapshot.agents[x.a.id]?.parked_until ?? 0) - (snapshot.agents[y.a.id]?.parked_until ?? 0)),
    [rows, snapshot, now],
  );
  const all = React.useMemo(() => rows.filter((r) => !isParked(r, snapshot, now)), [rows, snapshot, now]);
  // Archived: out of the list, under the State filter's Archived, the last archived first.
  const archived = React.useMemo(
    () =>
      attentionRows(
        missionAgents(snapshot.archived).map((a): MissionRow => ({ a, stand: "archived" })),
        snapshot,
      ).map((r, i) => ({ ...r, rank: i, pinned: undefined })),
    [snapshot],
  );
  const everyRow = React.useMemo(() => [...all, ...archived], [all, archived]);
  // How the list is shown (the sort and the facets chosen) is kept across visits; the words typed
  // to find a name are not. (A new key: everyone starts on Suggested once.) While you're using
  // the list, or have agents selected, it holds still (see useListHold), but for the rows you act on.
  const view = React.useMemo(
    () => ({
      ...DEFAULT_VIEW,
      ...stored,
      sort: stored.sort === undefined || stored.sort === "priority" || stored.sort === "yours" ? (snapshot.settings.sort === "yours" ? "yours" : "priority") : sortOf(stored.sort),
    }),
    [stored, snapshot.settings.sort],
  );
  const archivedShown = view.facets.state?.includes("archived") ?? false;
  const acted = React.useMemo(() => new Set(snapshot.acted), [snapshot.acted]);
  const { live, ended } = useShownList(everyRow, q, view, held, snapshot.pair?.key ?? "", acted);
  // The rows the list opens on are drawn with their lines (W21-R02): what they last said comes with the snapshot
  // (`said`), and where the list is shown some other way (another sort, a filter kept) it's asked for before they
  // are. Once, for this arrival: a row that comes to the page later asks for its own (api/last-words.ts).
  useSuspenseResource(`harness/last-words/${attention.key}`, () =>
    lastWordsOf(live.slice(0, firstRows()).flatMap((r) => (r.asking || (r.stand !== "finished" && r.stand !== "archived") ? [] : [r.a]))).then(() => true),
  );
  // What's made of every row shown is made as the rows change, not at each render (W26-R15).
  const order = React.useMemo(() => live.map((r) => `${r.a.id}:${r.stand}`).join(), [live]);
  const selectable = React.useMemo(() => [...live, ...ended].filter((r) => r.stand !== "archived"), [live, ended]);
  const searchAgents = React.useMemo(() => new Set(all.map(({ a }) => a.id)), [all]);
  const filtered = isFiltered(q, view.facets);
  const total = archivedShown ? everyRow.length : all.length;
  // The list as filtered and sorted: an Undo stands where its row was in this one, not in another.
  const shownAs = JSON.stringify([q, view]);

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
  const archivedIds = new Set(archived.map((r) => r.a.id));
  // A swipe left offers to park it, in a line under the row (a phone's own way to Park until…).
  touch.now.s = {
    isSelected: (id) => selected.has(id),
    set: setMany,
    selecting: touchScreen && selecting,
    swipeLeft: touchScreen ? (id) => !archivedIds.has(id) && setAsked({ id, ask: { kind: "park" } }) : undefined,
  };
  // Only what the list shows can be checked: a row the filter takes away is unchecked with it.
  const shownIds = React.useMemo(() => selectable.map((r) => r.a.id).join(","), [selectable]);
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
  const liveSelectable = live.filter((r) => r.stand !== "archived");
  const liveChecked = liveSelectable.filter((r) => selected.has(r.a.id)).length;
  const some = liveChecked > 0 && liveChecked < liveSelectable.length;
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
  // A row's dropdown opened (a step in history, so Back closes it) or closed (back where the page was when it opened).
  const toggleTree = React.useCallback(
    (id: string) => {
      const next = new URLSearchParams(params);
      if (openRow === id) {
        next.delete("open");
        setParams(next, { replace: true });
        const y = scrolledAt.get(id);
        if (y !== undefined) requestAnimationFrame(() => window.scrollTo({ top: y }));
        return;
      }
      scrolledAt.set(id, window.scrollY);
      next.set("open", id);
      setParams(next);
    },
    [params, openRow, setParams],
  );
  const answeredList = agentsAnswered(list, done);
  // The lines mounted wherever the page has gone (W21-R02): a section's head and its end (each as
  // tall as where it stands in the list makes it) and the Undo; the row with a line asked under it;
  // an open tree's row; the rows an Undo is putting back.
  const kept = React.useMemo(() => {
    const keys = new Set(["sep:pinned", "sep:rest", "undo"]);
    if (asked) keys.add(`row:${asked.id}`);
    for (const id of trees) keys.add(`row:${id}`);
    for (const id of restoring) keys.add(`row:${id}`);
    return keys;
  }, [asked, trees, restoring]);

  // What shows, noted as it renders (no request): it goes with what you do next, or with the next
  // snapshot request once you've left (api/attention.ts). The learned rest is what the draw orders.
  const shownRows = [...live, ...(showParked ? parked : []), ...(showEnded ? ended : [])];
  const parkedIds = new Set(parked.map((r) => r.a.id));
  const placements = React.useMemo(
    () =>
      Object.entries(snapshot.agents)
        .filter(([, mark]) => mark.pinned !== undefined || mark.parked_until !== undefined)
        .map(([id, mark]) => [id, mark.pinned, mark.parked_until])
        .sort((a, b) => String(a[0]).localeCompare(String(b[0]))),
    [snapshot.agents],
  );
  attention.presentation.note(snapshot.pair, {
    shown: shownRows.map((r) => r.a.id),
    eligible: new Set(shownRows.filter((r) => !parkedIds.has(r.a.id) && r.stand !== "archived" && r.stand !== "ended" && intervention(r) === 2 && r.pinned === undefined).map((r) => r.a.id)),
    context: attentionLayout(),
    controls: JSON.stringify({ controls, sort: view.sort, placements }),
    automatic: view.sort === "priority" && !archivedShown,
    allowed: attention.learningAllowed,
  });

  // Your pins, in your order (Move pin up and down reorder them): those of your marks that are, then those of them with a row.
  const pinnedMarks = React.useMemo(
    () =>
      Object.entries(snapshot.agents)
        .filter(([, mark]) => mark.pinned !== undefined && !mark.archived)
        .sort((x, y) => (x[1].pinned ?? 0) - (y[1].pinned ?? 0))
        .map(([id]) => id),
    [snapshot.agents],
  );
  const pins = React.useMemo(() => {
    if (pinnedMarks.length === 0) return pinnedMarks;
    const here = new Set(rows.map((r) => r.a.id));
    return pinnedMarks.filter((id) => here.has(id));
  }, [pinnedMarks, rows]);
  // Each pin's place, the same object while it's the same place (a row re-renders only when its own changes).
  const [pinned] = React.useState(() => ({ at: new Map<string, PinPlace>() }));
  pinned.at = pinPlaces(pins, pinned.at);
  const nameOf = (id: string) => {
    const row = [...rows, ...archived].find((r) => r.a.id === id);
    return row ? agentLabel(row.a) : id;
  };
  /**
   * Your action on rows: it shows at once, and the Undo stands where the (first) row was until your
   * next action; a Restore that didn't go offers kiln's saved copy in the row instead.
   */
  const organize = (commands: AttentionCommand[], opts: { arrival?: boolean } = {}) => {
    const ids = commandAgents(commands);
    const words = described(commands, nameOf, Date.now());
    const anchor = words ? anchorOf(ids, lists()) : null;
    const before = { order: [...snapshot.order], marks: Object.fromEntries(ids.map((id) => [id, snapshot.agents[id]])) };
    setAsked(null);
    const { seq, done: answered } = attention.organize(commands, (s) => organizedLocally(s, commands, Date.now(), list), opts);
    if (anchor && words) setUndo({ ...anchor, seq, visit: snapshot.visit, shown: shownAs, text: words.done, pending: true });
    void answered.then(({ undo: tokens, failed }) => {
      const first = commands[0];
      if (commands.length === 1 && first?.action === "restore" && failed[0]) {
        setUndo((u) => (u?.seq === seq ? null : u));
        setAsked({ id: first.agent, ask: { kind: "copy", error: failed[0].error } });
        return;
      }
      // No Undo line to say it (Clear Updated, a pin moved, the sort): what showed goes back, and why it didn't go is said.
      if (!words) {
        if (failed[0]) toast.error("That didn't go through", failed[0].error);
        return;
      }
      const didnt = new Set(failed.flatMap((f) => (f.agent ? [f.agent] : [])));
      const inverse = inverseOf(commands, tokens, didnt);
      const none = failed.length === commands.length;
      setUndo((u) =>
        u?.seq !== seq
          ? u
          : {
              ...u,
              pending: false,
              text: none ? `Couldn't ${words.doing}: ${failed[0]?.error ?? ""}` : u.text,
              failed: !none && failed.length > 0 ? `${failed.length} couldn't: ${failed[0]?.error ?? ""}` : undefined,
              height: none ? 0 : u.height,
              undo: inverse.length > 0 ? () => undoIt(inverse, ids, before) : undefined,
            },
      );
    });
  };
  /** Undo: the placements as they were, each row back where it was. */
  const undoIt = (inverse: AttentionCommand[], ids: string[], before: { order: string[]; marks: Record<string, AttentionMark | undefined> }) => {
    const local = (s: AttentionSnapshot): AttentionSnapshot => {
      const back = organizedLocally(
        s,
        inverse.filter((c) => c.action !== "undo"),
        Date.now(),
        list,
      );
      const agents = { ...back.agents };
      for (const id of ids) {
        const mark = before.marks[id];
        if (mark) agents[id] = mark;
        else delete agents[id];
      }
      return { ...back, agents };
    };
    setRestoring(ids);
    void attention.organize(inverse, local, { reference: before.order }).done.then(({ failed }) => {
      setRestoring((now) => (now === ids ? NO_IDS : now));
      if (failed[0]) toast.error("Couldn't undo it", failed[0].error);
    });
  };
  const pinMove = (id: string, by: -1 | 1) => {
    const at = pins.indexOf(id);
    const other = pins[at + by];
    if (at < 0 || other === undefined) return;
    const order = [...pins];
    order[at] = other;
    order[at + by] = id;
    organize([{ action: "reorder", order }]);
  };
  // The rows' callbacks, made once (a row re-renders only when its own agent does): each calls what this render has.
  const [rowActs] = React.useState(() => {
    const now = {
      organize: (_: AttentionCommand[]) => undefined as void,
      pinMove: (_: string, __: -1 | 1) => undefined as void,
      ask: (_: string, __: RowAsk | null) => undefined as void,
      fail: (_: string, __: string | null) => undefined as void,
      open: (_: string) => undefined as void,
    };
    return {
      now,
      organize: (c: AttentionCommand[]) => now.organize(c),
      pinMove: (id: string, by: -1 | 1) => now.pinMove(id, by),
      ask: (id: string, a: RowAsk | null) => now.ask(id, a),
      fail: (id: string, why: string | null) => now.fail(id, why),
      open: (id: string) => now.open(id),
    };
  });
  rowActs.now.organize = (c) => organize(c);
  rowActs.now.pinMove = pinMove;
  rowActs.now.ask = (id, a) => setAsked(a ? { id, ask: a } : null);
  rowActs.now.fail = fail;
  rowActs.now.open = attention.pick;

  const undoShown = undo && undo.seq === snapshot.seq && undo.visit === snapshot.visit && undo.shown === shownAs ? undo : null;
  const renderEntry = (e: Entry, where: "live" | "parked" | "ended") => {
    if (e.key === "undo" && undoShown) return <UndoLine key="undo" text={undoShown.text} failed={undoShown.failed} height={undoShown.height} pending={undoShown.pending} onUndo={undoShown.undo} />;
    if (e.key === "sep:pinned")
      return (
        <li key={e.key} className="ms-sep" data-entry={e.key}>
          <span className="ms-sep-h">Pinned · {e.pinned}</span>
        </li>
      );
    if (e.key === "sep:rest") return <li key={e.key} className="ms-sep" data-entry={e.key} aria-hidden="true" />;
    if (!e.row) return null;
    // Its dropdown, a line of its own under its row.
    if (e.key.startsWith("tree:")) return <MissionTree key={e.key} id={e.row.a.id} list={answeredList} rowAsk={(e.row.asking ?? e.row.a).approvals[0]?.id} onClose={toggleTree} />;
    return renderRow(e.row, where);
  };
  const renderRow = (row: MissionRow, where: "live" | "parked" | "ended") => {
    const { a, stand, asking, updated, stale, urgency, correction } = row;
    const open = openRow === a.id;
    const mark = snapshot.agents[a.id];
    const pin = stand === "archived" ? undefined : pinned.at.get(a.id);
    // What holds it where it is: when a park ends and why it was parked; otherwise why it came back, or
    // where the learned order put it. Needs you and pins say it themselves.
    const reason =
      where === "parked" && mark?.parked_until !== undefined
        ? [`Until ${untilWords(mark.parked_until, now)}`, mark.reason].filter(Boolean).join(" · ")
        : intervention(row) < 2 || pin
          ? undefined
          : row.reason;
    return (
      <Row
        key={`row:${a.id}`}
        a={a}
        stand={stand}
        asking={asking}
        updated={updated}
        mark={mark}
        reason={reason}
        correction={correction}
        stale={stale}
        urgency={urgency}
        parked={where === "parked"}
        pin={pin}
        ask={asked?.id === a.id ? asked.ask : undefined}
        failure={failures[a.id]}
        settings={snapshot.settings}
        onAsk={rowActs.ask}
        onOrganize={rowActs.organize}
        onPinMove={rowActs.pinMove}
        onFailure={rowActs.fail}
        onOpen={rowActs.open}
        selected={selected.has(a.id)}
        picking={touchScreen && selecting}
        onSelect={select}
        tree={open}
        onTree={toggleTree}
      />
    );
  };
  // Every agent kiln runs, the subagents on their agents' rows too: none is listed again as found on its machine.
  const everyone = React.useMemo(() => list.map((a) => ({ a })), [list]);
  const sectioned = view.sort === "priority" || view.sort === "yours";
  // Each list's lines: its rows, the Pinned section's head and end, the Undo where a row was, and each open tree under its row.
  const entries = withTrees(withUndo(entriesOf(live, sectioned), undoShown, "live"), trees);
  const parkedEntries = withTrees(
    withUndo(
      parked.map((row) => ({ key: `row:${row.a.id}`, row })),
      undoShown,
      "parked",
    ),
    trees,
  );
  const endedEntries = withTrees(
    ended.map((row) => ({ key: `row:${row.a.id}`, row })),
    trees,
  );
  const liveShown = everyRow.length > 0 && (live.length > 0 || undoShown?.list === "live");
  const parkedShown = showParked && (parked.length > 0 || undoShown?.list === "parked");
  const endedShown = showEnded && ended.length > 0;
  // The lists on the page, in its order: where an action's Undo stands is read from them, as you act.
  const lists = (): ShownList[] => [
    ...(liveShown ? [{ list: "live" as const, keys: entries.map((e) => e.key) }] : []),
    ...(parkedShown ? [{ list: "parked" as const, keys: parkedEntries.map((e) => e.key) }] : []),
    ...(endedShown ? [{ list: "ended" as const, keys: endedEntries.map((e) => e.key) }] : []),
  ];

  return (
    <>
      {composer}
      {everyRow.length > 0 && (
        <>
          <ListBar
            rows={everyRow}
            q={q}
            onQuery={setQ}
            view={view}
            onView={(v) => {
              setView(v);
              // Your own order, or the learned one: kept for you, and a new order drawn by it at once. A facet chosen saves nothing.
              const sort = v.sort === "yours" ? "yours" : v.sort === "priority" ? "learned" : null;
              if (sort && sort !== snapshot.settings.sort) organize([{ action: "settings", settings: { sort } }], { arrival: true });
            }}
          />
          <React.Suspense fallback={null}>
            <Recover rows={all} />
          </React.Suspense>
          <React.Suspense fallback={null}>
            <PushOffer />
          </React.Suspense>
        </>
      )}

      {/* Read with the agents, under the page's own loading: the list's place is held, nothing moves as it comes. */}

      {everyRow.length === 0 ? (
        <div className="ms-empty">
          <p className="ms-empty-title">{parked.length > 0 ? "Your chats are parked below." : "No agents yet."}</p>
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
                  disabled={!selecting || liveSelectable.length === 0}
                  checked={liveSelectable.length > 0 && liveChecked === liveSelectable.length}
                  onChange={(e) =>
                    setSelected(e.target.checked ? new Set([...selected, ...liveSelectable.map((r) => r.a.id)]) : new Set([...selected].filter((id) => !liveSelectable.some((r) => r.a.id === id))))
                  }
                  aria-label={filtered ? `Select the ${liveSelectable.length} agents shown` : `Select all ${liveSelectable.length} agents`}
                />
                <span className="ms-count" aria-live="polite">
                  <span key={selecting ? "picked" : "all"} className="ms-swap">
                    {selecting ? `${checked.length} selected` : filtered ? `${live.length + ended.length} of ${total} agents` : `${all.length} agent${all.length === 1 ? "" : "s"}`}
                  </span>
                </span>
              </span>
            ) : (
              <label className="ms-all">
                <input
                  ref={allBox}
                  type="checkbox"
                  className="check"
                  disabled={liveSelectable.length === 0}
                  checked={liveSelectable.length > 0 && liveChecked === liveSelectable.length}
                  onChange={(e) =>
                    setSelected(e.target.checked ? new Set([...selected, ...liveSelectable.map((r) => r.a.id)]) : new Set([...selected].filter((id) => !liveSelectable.some((r) => r.a.id === id))))
                  }
                  aria-label={filtered ? `Select the ${liveSelectable.length} agents shown` : `Select all ${liveSelectable.length} agents`}
                />
                <span key={checked.length > 0 ? "picked" : "all"} className="ms-swap">
                  {checked.length > 0
                    ? `${checked.length} of ${selectable.length} selected`
                    : filtered
                      ? `${live.length + ended.length} of ${total} agents`
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
                touchScreen && liveSelectable.length > 0 ? (
                  <Button size="xs" variant="secondary" onClick={() => setPicking(true)}>
                    <ListChecks aria-hidden="true" />
                    Select
                  </Button>
                ) : undefined
              }
              onGone={clear}
              onClear={clear}
              settings={snapshot.settings}
              onPark={(agents, until) => organize(agents.map((a) => ({ action: "park", agent: a.id, until })))}
              onArchive={(agents) => organize(agents.map((a) => ({ action: "archive", agent: a.id })))}
            />
          </div>
          {live.length > 0 || undoShown?.list === "live" ? (
            <div {...touch.props} data-held={held || undefined}>
              <Entries list="live" entries={entries} pinned={kept} order={order} render={(e) => renderEntry(e, "live")} onKeyDown={(e) => listKeys(e, toggleTree, openRow)} />
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

      {(parked.length > 0 || undoShown?.list === "parked") && (
        <section className="ms-ended">
          <div className="ms-fold">
            <button type="button" className="ms-fold-btn" aria-expanded={showParked} onClick={() => setShowParked(!showParked)}>
              <ChevronRight size={13} className="ms-chev" aria-hidden="true" />
              Parked · {parked.length}
            </button>
          </div>
          {showParked && <Entries list="parked" entries={parkedEntries} pinned={kept} render={(e) => renderEntry(e, "parked")} onKeyDown={(e) => listKeys(e, toggleTree, openRow)} />}
        </section>
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
              <button
                type="button"
                className="ms-link"
                onClick={() =>
                  clearEnded(
                    ended.map((r) => r.a.id),
                    reload,
                    (id, why) => {
                      // Those kept are back in the list, open, each saying why.
                      fail(id, why);
                      setShowEnded(true);
                    },
                  )
                }
              >
                Clear
              </button>
            )}
          </div>
          {showEnded && <Entries list="ended" entries={endedEntries} pinned={kept} render={(e) => renderEntry(e, "ended")} onKeyDown={(e) => listKeys(e, toggleTree, openRow)} />}
        </section>
      )}
      {/* How the list is ordered and kept for you: Settings, You, Attention. */}
      <p className="ms-settings-link">
        <Link to="/settings#you.attention" className="ms-link">
          Attention settings
        </Link>
      </p>
    </>
  );
}
