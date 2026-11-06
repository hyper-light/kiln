/**
 * What waits on you, wherever you are in kiln: a card for each agent with something to show
 * (cards.ts), acted on right there: a tool call allowed or denied, or its agent opened to answer
 * its question or review its plan; the plan its goals wait on reviewed, and its goals resumed; an
 * agent that stopped by itself restarted. One at a usage limit is only said. Where kiln's other
 * notifications are, on every screen: at the bottom, above any passing ones. Not on Mission
 * control, nor on the agent's own page (where the message box is): those show it themselves.
 * × (or a swipe, on a touch screen) puts one away until there's something new: kept for you by the
 * server (notices.ts), so no refresh, restart, device or kiln address brings it back.
 */
import * as React from "react";
import "./approval-toasts.css";
import { ArrowUpRight, X } from "lucide-react";
import { harnessApi, type AgentInfo } from "@/api/harness";
import { plansApi } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { Link, useLocation, useNavigate } from "@/lib/router";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { cardsOf, heldWords, type Card } from "./cards";
import { agentLabel } from "./derive";
import { useClock } from "./dom";
import { machineShown } from "./parts";
import { useLabels } from "@/lib/data";
import { pauseLine } from "./pause";
import { getNoneWaiting, getWaiting, subscribeWaiting } from "./waiting";
import { answer, dealWith, resumeHeld, tried, tryKey, useAnswered } from "./answered";
import { isMarked, markNotices, useMarks } from "./notices";
import { swipeAway } from "./swipe-away";

/** Cards at once (one on a phone); past these, one line says how many more agents wait. */
const MAX = 2;
const PHONE = "(max-width: 640px)";

/** Card `keys` of agent `agent` put away (`away`) or dealt with (`done`), wherever you are. */
function putAway(keys: string[], agent: string, how: "away" | "done" = "away") {
  markNotices(keys, how, [agent]);
}

/** What it asks, in a few words, whatever its harness calls the tool: a command as you'd type it, else what it would do to what. */
function asks(ap: NonNullable<Card["ap"]>): React.ReactNode {
  const code = (s: string) => <span className="rq-code">{s}</span>;
  switch (ap.tool) {
    case "shell":
      return code(`$ ${ap.title}`);
    case "edit":
      return <>Edit {code(ap.title)}</>;
    case "read":
      return <>Read {code(ap.title)}</>;
    case "web":
      return /^https?:\/\//.test(ap.title) ? <>Open {code(ap.title)}</> : <>Search the web for {ap.title}</>;
    case "mcp":
      return <>Use {code(ap.title)}</>;
    case "skill":
      return <>Run the {code(ap.title)} skill</>;
    case "agent":
      return <>Start a subagent: {ap.title}</>;
    case "plan":
      return ap.title || "A plan to look at";
    default:
      return ap.title;
  }
}

/** A card's frame: its agent and where it runs (and how many more it asks), what it says, what to do about it, and × to put it away. */
function Shell({ card, line, children }: { card: Card; line?: React.ReactNode; children?: React.ReactNode }) {
  const { a, more } = card;
  // Its machine by the name people see, as labels come (docs/machines.md).
  useLabels();
  const label = agentLabel(a);
  // A request puts away all its agent asks now; the rest, this one.
  const away = () => putAway(card.kind === "request" ? a.approvals.map((p) => p.id) : [card.key], a.id);
  return (
    <li className="toast rq" data-kind={card.kind} {...swipeAway<HTMLLIElement>(away)}>
      <span className="rq-dot" aria-hidden="true" />
      <div className="rq-body">
        <p className="rq-head">
          <Link to={`/agents/${a.id}`} className="rq-name">
            {label}
          </Link>
          <span className="rq-where">
            {machineShown(a.target)}
            {more > 0 && ` · +${more} more`}
          </span>
        </p>
        {line && <p className="rq-line">{line}</p>}
        {children && <div className="rq-acts">{children}</div>}
      </div>
      <button type="button" className="icon-btn rq-close" data-variant="bare" data-size="sm" aria-label={`Put away ${label}'s ${card.kind === "request" ? "request" : "card"}`} onClick={away}>
        <X size={14} />
      </button>
    </li>
  );
}

function Open({ a }: { a: AgentInfo }) {
  return (
    <Link to={`/agents/${a.id}`} className="rq-open">
      Open <ArrowUpRight size={12} aria-hidden="true" />
    </Link>
  );
}

function RequestCard({ card }: { card: Card }) {
  const navigate = useNavigate();
  const { a, ap } = card;
  if (!ap) return null;
  const href = `/agents/${a.id}`;
  const plan = ap.tool === "plan";
  // More to see than the card has room for: a plan to read, an edit's diff.
  const review = plan || ap.tool === "edit";
  // Answered at once (answered.ts): the card goes now, and is back if the answer doesn't go.
  const decide = (decision: "allow" | "deny") => void answer(a.id, { type: "approve", id: ap.id, decision }).catch((e: unknown) => toast.error(`Couldn't answer ${agentLabel(a)}`, errorMessage(e)));
  return (
    <Shell card={card} line={asks(ap)}>
      {ap.tool === "question" ? (
        <Button size="xs" onClick={() => navigate(href)}>
          Answer
        </Button>
      ) : (
        <>
          {/* A plan is approved here; read, rejected or answered in your own words in its agent's Plan tab. */}
          <Button size="xs" onClick={() => decide("allow")}>
            {plan ? "Approve" : "Allow"}
          </Button>
          {!plan && (
            <Button size="xs" variant="secondary" onClick={() => decide("deny")}>
              Deny
            </Button>
          )}
          {review && (
            <Link to={plan ? `${href}?tab=plan` : href} className="rq-open">
              Review <ArrowUpRight size={12} aria-hidden="true" />
            </Link>
          )}
        </>
      )}
    </Shell>
  );
}

/** Goals it paused for you: a plan they wait on approved here, or reviewed in its agent's Plan tab; else Resume. */
function HeldCard({ card }: { card: Card }) {
  // Asked again (its answer lost, or it failed), the same resume (or approval) under its key, its
  // card's own across its going and coming back (CP-T3.2, answered.ts).
  const { a } = card;
  const held = a.held;
  if (!held) return null;
  const plan = held.plan;
  // Approved, or resumed: the card goes now (answered.ts), and is back if it doesn't go (the server
  // lets go of the pause within moments, as it sends the plan's steps).
  const approve = (id: string) =>
    void resumeHeld(a, () => plansApi.review(id, { answer: "approve", key: tryKey(card.key, `approve ${id} ${held.since}`) })).then(
      () => tried(card.key),
      (e: unknown) => toast.error(`Couldn't approve ${agentLabel(a)}'s plan`, errorMessage(e)),
    );
  const resume = () =>
    void resumeHeld(a, () => harnessApi.goalAction(a.id, "start", undefined, tryKey(card.key, a.id))).then(
      () => tried(card.key),
      (e: unknown) => toast.error(`Couldn't resume ${agentLabel(a)}'s goals`, errorMessage(e)),
    );
  return (
    <Shell card={card} line={`Paused its goals${heldWords(held.why) ? `: ${heldWords(held.why)}` : ""}`}>
      {plan ? (
        <>
          <Button size="xs" onClick={() => approve(plan)}>
            Approve
          </Button>
          <Link to={`/agents/${a.id}?tab=plan`} className="rq-open">
            Review <ArrowUpRight size={12} aria-hidden="true" />
          </Link>
        </>
      ) : (
        <>
          <Button size="xs" onClick={resume}>
            Resume
          </Button>
          <Open a={a} />
        </>
      )}
    </Shell>
  );
}

/** It stopped by itself: why, and Restart. */
function StoppedCard({ card }: { card: Card }) {
  const { a } = card;
  // Restarted: the card goes now (answered.ts), and is back if the restart doesn't go.
  const restart = () => void dealWith(card.key, a.id, () => harnessApi.command(a.id, { type: "restart" })).catch((e: unknown) => toast.error(`Couldn't restart ${agentLabel(a)}`, errorMessage(e)));
  const code = a.exit?.code;
  return (
    <Shell card={card} line={`Stopped by itself: ${a.exit?.error ?? "its harness ended"}${typeof code === "number" ? ` (exit ${code})` : ""}`}>
      <Button size="xs" onClick={restart}>
        Restart
      </Button>
      <Open a={a} />
    </Shell>
  );
}

/** At a usage limit: only said (when it goes on), nothing to do. */
function LimitCard({ card }: { card: Card }) {
  const now = useClock(30_000);
  const { a } = card;
  if (!a.pause) return null;
  return <Shell card={card} line={pauseLine(a.pause, now)} />;
}

function CardOf({ card }: { card: Card }) {
  switch (card.kind) {
    case "request":
      return <RequestCard card={card} />;
    case "held":
      return <HeldCard card={card} />;
    case "stopped":
      return <StoppedCard card={card} />;
    case "limit":
      return <LimitCard card={card} />;
  }
}

export default function RequestToasts() {
  const waiting = React.useSyncExternalStore(subscribeWaiting, getWaiting, getNoneWaiting);
  const marks = useMarks();
  const gone = React.useMemo(() => ({ has: (key: string) => isMarked(marks, key) }), [marks]);
  const done = useAnswered();
  const path = useLocation().pathname;
  const max = useMediaQuery(PHONE) ? 1 : MAX;
  const cards = cardsOf(waiting, path, gone, done);
  if (cards.length === 0) return null;
  const rest = cards.slice(max);
  const limits = rest.filter((c) => c.kind === "limit").length;
  const needs = rest.length - limits;
  const restWords = [
    needs > 0 && `${needs} more agent${needs === 1 ? "" : "s"} waiting on you`,
    limits > 0 && (needs > 0 ? `${limits} at a usage limit` : `${limits} more agent${limits === 1 ? "" : "s"} at a usage limit`),
  ]
    .filter(Boolean)
    .join(" · ");
  return (
    <ol className="rq-viewport" aria-label="Waiting on you" aria-live="polite">
      {cards.slice(0, max).map((c) => (
        <CardOf key={`${c.a.id}:${c.key}`} card={c} />
      ))}
      {rest.length > 0 && (
        <li className="toast rq rq-rest">
          <Link to="/agents" className="rq-name">
            {restWords}
          </Link>
          <ArrowUpRight size={13} aria-hidden="true" />
        </li>
      )}
    </ol>
  );
}
