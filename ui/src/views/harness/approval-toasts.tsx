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
import { clientId, harnessApi, type AgentInfo } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { Link, useLocation, useNavigate } from "@/lib/router";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { Tries } from "@/lib/tries";
import { cardsOf, heldWords, type Card } from "./cards";
import { agentLabel } from "./derive";
import { useClock } from "./dom";
import { machineName } from "./parts";
import { pauseLine } from "./pause";
import { getNoneWaiting, getWaiting, subscribeWaiting } from "./waiting";
import { answer, useAnswered } from "./answered";
import { isMarked, markNotices, useMarks } from "./notices";

/** Cards at once (one on a phone); past these, one line says how many more agents wait. */
const MAX = 2;
const PHONE = "(max-width: 640px)";
/** How far a finger drags a card sideways to put it away. */
const SWIPE = 64;

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

/** A finger dragging a card sideways: it follows, and past SWIPE it's put away. */
function swipe(onAway: () => void): Pick<React.HTMLAttributes<HTMLLIElement>, "onPointerDown" | "onPointerMove" | "onPointerUp" | "onPointerCancel"> {
  const end = (e: React.PointerEvent<HTMLLIElement>, cancel: boolean) => {
    const el = e.currentTarget;
    const dx = Number(el.dataset.dx ?? 0);
    delete el.dataset.from;
    delete el.dataset.dx;
    el.style.transition = "transform 0.2s ease, opacity 0.2s ease";
    if (!cancel && Math.abs(dx) > SWIPE) {
      el.style.transform = `translateX(${dx > 0 ? 110 : -110}%)`;
      el.style.opacity = "0";
      window.setTimeout(onAway, 180);
    } else el.style.transform = "";
  };
  return {
    onPointerDown: (e) => {
      if (e.pointerType !== "touch" || (e.target as Element).closest("button, a")) return;
      e.currentTarget.dataset.from = String(e.clientX);
      e.currentTarget.style.transition = "";
    },
    onPointerMove: (e) => {
      const el = e.currentTarget;
      if (el.dataset.from === undefined) return;
      const dx = e.clientX - Number(el.dataset.from);
      el.dataset.dx = String(dx);
      el.style.transform = dx !== 0 ? `translateX(${dx}px)` : "";
    },
    onPointerUp: (e) => end(e, false),
    onPointerCancel: (e) => end(e, true),
  };
}

/** A card's frame: its agent and where it runs (and how many more it asks), what it says, what to do about it, and × to put it away. */
function Shell({ card, line, busy, children }: { card: Card; line?: React.ReactNode; busy?: boolean; children?: React.ReactNode }) {
  const { a, more } = card;
  const label = agentLabel(a);
  // A request puts away all its agent asks now; the rest, this one.
  const away = () => putAway(card.kind === "request" ? a.approvals.map((p) => p.id) : [card.key], a.id);
  return (
    <li className="toast rq" data-kind={card.kind} aria-busy={busy || undefined} {...swipe(away)}>
      <span className="rq-dot" aria-hidden="true" />
      <div className="rq-body">
        <p className="rq-head">
          <Link to={`/agents/${a.id}`} className="rq-name">
            {label}
          </Link>
          <span className="rq-where">
            {machineName(a.target)}
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
  const [busy, setBusy] = React.useState(false);
  const navigate = useNavigate();
  const { a, ap } = card;
  if (!ap) return null;
  const href = `/agents/${a.id}`;
  const plan = ap.tool === "plan";
  // More to see than the card has room for: a plan to read, an edit's diff.
  const review = plan || ap.tool === "edit";
  // Answered at once (answered.ts): the card goes now, and is back if the answer doesn't go.
  const decide = (decision: "allow" | "deny") => {
    setBusy(true);
    void answer(a.id, { type: "approve", id: ap.id, decision }).catch((e: unknown) => {
      toast.error(`Couldn't answer ${agentLabel(a)}`, errorMessage(e));
      setBusy(false);
    });
  };
  return (
    <Shell card={card} line={asks(ap)} busy={busy}>
      {ap.tool === "question" ? (
        <Button size="xs" onClick={() => navigate(href)}>
          Answer
        </Button>
      ) : (
        <>
          {/* A plan is approved here; to keep it planning (with what to change), review it on its page. */}
          <Button size="xs" disabled={busy} onClick={() => void decide("allow")}>
            {plan ? "Approve" : "Allow"}
          </Button>
          {!plan && (
            <Button size="xs" variant="secondary" disabled={busy} onClick={() => void decide("deny")}>
              Deny
            </Button>
          )}
          {review && (
            <Link to={href} className="rq-open">
              Review <ArrowUpRight size={12} aria-hidden="true" />
            </Link>
          )}
        </>
      )}
    </Shell>
  );
}

/** Goals it paused for you: the plan they wait on to look at, and Resume. */
function HeldCard({ card }: { card: Card }) {
  const [busy, setBusy] = React.useState(false);
  // Asked again (its answer lost, or it failed), the same resume under its key (CP-T3.2).
  const [tries] = React.useState(() => new Tries(clientId));
  const navigate = useNavigate();
  const { a } = card;
  const held = a.held;
  if (!held) return null;
  // Resumed: the card goes now (the server lets go of the pause within moments).
  const resume = () => {
    setBusy(true);
    void harnessApi.goalAction(a.id, "start", undefined, tries.keyFor(a.id)).then(
      () => {
        tries.done();
        putAway([card.key], a.id, "done");
      },
      (e: unknown) => {
        toast.error(`Couldn't resume ${agentLabel(a)}'s goals`, errorMessage(e));
        setBusy(false);
      },
    );
  };
  return (
    <Shell card={card} line={`Paused its goals${heldWords(held.why) ? `: ${heldWords(held.why)}` : ""}`} busy={busy}>
      {held.plan ? (
        <>
          <Button size="xs" onClick={() => navigate(`/agents/plans/${held.plan}`)}>
            Review the plan
          </Button>
          <Button size="xs" variant="secondary" disabled={busy} onClick={resume}>
            Resume
          </Button>
        </>
      ) : (
        <>
          <Button size="xs" disabled={busy} onClick={resume}>
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
  const [busy, setBusy] = React.useState(false);
  const { a } = card;
  const restart = () => {
    setBusy(true);
    void harnessApi.command(a.id, { type: "restart" }).then(
      () => putAway([card.key], a.id, "done"),
      (e: unknown) => {
        toast.error(`Couldn't restart ${agentLabel(a)}`, errorMessage(e));
        setBusy(false);
      },
    );
  };
  const code = a.exit?.code;
  return (
    <Shell card={card} line={`Stopped by itself: ${a.exit?.error ?? "its harness ended"}${typeof code === "number" ? ` (exit ${code})` : ""}`} busy={busy}>
      <Button size="xs" disabled={busy} onClick={restart}>
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
