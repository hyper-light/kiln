/**
 * What waits on you, wherever you are in kiln: a card for each agent with something to show
 * (cards.ts), acted on right there: a tool call allowed or denied, or its agent opened to answer
 * its question or review its plan; the plan its goals wait on reviewed, and its goals resumed; an
 * agent that stopped by itself restarted. One at a usage limit is only said. Where kiln's other
 * notifications are, on every screen: at the bottom, above any passing ones. Not on Mission
 * control, nor on the agent's own page (where the message box is): those show it themselves.
 * × (or a swipe, on a touch screen) puts one away until there's something new.
 */
import * as React from "react";
import "./approval-toasts.css";
import { ArrowUpRight, X } from "lucide-react";
import { harnessApi, type AgentInfo } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { Link, useLocation, useNavigate } from "@/lib/router";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { cardsOf, heldWords, type Card } from "./cards";
import { agentLabel } from "./derive";
import { useClock } from "./dom";
import { machineName } from "./parts";
import { pauseLine } from "./pause";
import { getNoneWaiting, getWaiting, subscribeWaiting } from "./waiting";
import { answer, useAnswered } from "./answered";

/** Cards at once (one on a phone); past these, one line says how many more agents wait. */
const MAX = 2;
const PHONE = "(max-width: 640px)";
/** Requests put away, for as long as the tab is open (by request: a new one shows again). */
const AWAY_KEY = "kiln.requests.away";
/** How far a finger drags a card sideways to put it away. */
const SWIPE = 64;

let away: ReadonlySet<string> = new Set(readAway());
const awayListeners = new Set<() => void>();
function readAway(): string[] {
  try {
    const v: unknown = JSON.parse(sessionStorage.getItem(AWAY_KEY) ?? "[]");
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
  } catch {
    return [];
  }
}
function putAway(ids: string[]) {
  away = new Set([...away, ...ids]);
  try {
    // The latest few hundred are plenty: a request's id is never asked again.
    sessionStorage.setItem(AWAY_KEY, JSON.stringify([...away].slice(-300)));
  } catch {
    /* private mode: kept for this page only */
  }
  awayListeners.forEach((l) => l());
}
const subscribeAway = (l: () => void) => {
  awayListeners.add(l);
  return () => awayListeners.delete(l);
};
const getAway = () => away;

/** What a tool call would do, as an agent's page says it. */
const VERB: Record<string, string> = {
  shell: "run a command",
  edit: "change files",
  web: "go online",
  mcp: "use a connected tool",
  agent: "start a subagent",
};

/** What it asks, in a few words. */
function what(ap: NonNullable<Card["ap"]>): { lead: string; line?: string } {
  if (ap.tool === "question") return { lead: "asks you", line: ap.title };
  if (ap.tool === "plan") return { lead: "proposes a plan", line: ap.title || undefined };
  return { lead: `wants to ${VERB[ap.tool] ?? "go ahead"}`, line: `${ap.tool_name || ap.tool}: ${ap.title}` };
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

/** A card's frame: its agent and what it says, what to do about it, and × to put it away. */
function Shell({ card, lead, line, busy, children }: { card: Card; lead: string; line?: string; busy?: boolean; children?: React.ReactNode }) {
  const { a } = card;
  const label = agentLabel(a);
  // A request puts away all its agent asks now; the rest, this one.
  const away = () => putAway(card.kind === "request" ? a.approvals.map((p) => p.id) : [card.key]);
  return (
    <li className="toast rq" data-kind={card.kind} aria-busy={busy || undefined} {...swipe(away)}>
      <span className="rq-dot" aria-hidden="true" />
      <div className="rq-body">
        <p className="rq-head">
          <Link to={`/agents/${a.id}`} className="rq-name">
            {label}
          </Link>{" "}
          <span className="rq-lead">{lead}</span>
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

/** Its machine, and how many more it asks. */
function Where({ a, more = 0 }: { a: AgentInfo; more?: number }) {
  return (
    <span className="rq-where">
      {machineName(a.target)}
      {more > 0 && ` · +${more} more`}
    </span>
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
  const { a, ap, more } = card;
  if (!ap) return null;
  const href = `/agents/${a.id}`;
  const { lead, line } = what(ap);
  const asks = ap.tool === "question" || ap.tool === "plan";
  // Answered at once (answered.ts): the card goes now, and is back if the answer doesn't go.
  const decide = (decision: "allow" | "deny") => {
    setBusy(true);
    void answer(a.id, { type: "approve", id: ap.id, decision }).catch((e: unknown) => {
      toast.error(`Couldn't answer ${agentLabel(a)}`, errorMessage(e));
      setBusy(false);
    });
  };
  return (
    <Shell card={card} lead={lead} line={line} busy={busy}>
      {asks ? (
        <Button size="xs" onClick={() => navigate(href)}>
          {ap.tool === "plan" ? "Review the plan" : "Answer"}
        </Button>
      ) : (
        <>
          <Button size="xs" disabled={busy} onClick={() => void decide("allow")}>
            Allow
          </Button>
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => void decide("deny")}>
            Deny
          </Button>
          <Link to={href} className="rq-open">
            Review <ArrowUpRight size={12} aria-hidden="true" />
          </Link>
        </>
      )}
      <Where a={a} more={more} />
    </Shell>
  );
}

/** Its first letter a capital: a line that stands on its own. */
const capital = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/** Goals it paused for you: the plan they wait on to look at, and Resume. */
function HeldCard({ card }: { card: Card }) {
  const [busy, setBusy] = React.useState(false);
  const navigate = useNavigate();
  const { a } = card;
  const held = a.held;
  if (!held) return null;
  // Resumed: the card goes now (the server lets go of the pause within moments).
  const resume = () => {
    setBusy(true);
    void harnessApi.goalAction(a.id, "start").then(
      () => putAway([card.key]),
      (e: unknown) => {
        toast.error(`Couldn't resume ${agentLabel(a)}'s goals`, errorMessage(e));
        setBusy(false);
      },
    );
  };
  return (
    <Shell card={card} lead="paused its goals for you" line={capital(heldWords(held.why)) || undefined} busy={busy}>
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
      <Where a={a} />
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
      () => putAway([card.key]),
      (e: unknown) => {
        toast.error(`Couldn't restart ${agentLabel(a)}`, errorMessage(e));
        setBusy(false);
      },
    );
  };
  const code = a.exit?.code;
  return (
    <Shell card={card} lead="stopped by itself" line={`${a.exit?.error ?? "Its harness ended"}${typeof code === "number" ? ` (exit ${code})` : ""}`} busy={busy}>
      <Button size="xs" disabled={busy} onClick={restart}>
        Restart
      </Button>
      <Open a={a} />
      <Where a={a} />
    </Shell>
  );
}

/** At a usage limit: only said (when it goes on), nothing to do. */
function LimitCard({ card }: { card: Card }) {
  const now = useClock(30_000);
  const { a } = card;
  if (!a.pause) return null;
  return <Shell card={card} lead="is paused at its usage limit" line={pauseLine(a.pause, now)} />;
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
  const gone = React.useSyncExternalStore(subscribeAway, getAway, getAway);
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
