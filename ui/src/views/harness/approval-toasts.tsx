/**
 * What waits on you, wherever you are in kiln: each agent's oldest request as a card at the top of
 * the window (kiln's passing words keep the bottom), answered right there: allow or deny a tool
 * call, or open the agent to answer its question or review its plan. Not on Mission control, nor on
 * the page of the agent that asks: those show its requests themselves. × (or a swipe, on a touch
 * screen) puts one away until that agent asks for something else.
 */
import * as React from "react";
import "./approval-toasts.css";
import { ArrowUpRight, X } from "lucide-react";
import { harnessApi, type AgentInfo, type ApprovalBrief } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { Link, useLocation, useNavigate } from "@/lib/router";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { agentLabel } from "./derive";
import { machineName } from "./parts";
import { getNoneWaiting, getWaiting, subscribeWaiting } from "./waiting";

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

/** The page shows this agent's requests itself. */
const showsItsOwn = (path: string, id: string) => path === "/agents" || path === "/agents/" || path === `/agents/${id}` || path.startsWith(`/agents/${id}/`);

/** What a tool call would do, as an agent's page says it. */
const VERB: Record<string, string> = {
  shell: "run a command",
  edit: "change files",
  web: "go online",
  mcp: "use a connected tool",
  agent: "start a subagent",
};

/** What it asks, in a few words. */
function what(ap: ApprovalBrief): { lead: string; line?: string } {
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

function Card({ a, ap, more }: { a: AgentInfo; ap: ApprovalBrief; more: number }) {
  const [busy, setBusy] = React.useState(false);
  const navigate = useNavigate();
  const href = `/agents/${a.id}`;
  const label = agentLabel(a);
  const { lead, line } = what(ap);
  const asks = ap.tool === "question" || ap.tool === "plan";
  const decide = async (decision: "allow" | "deny") => {
    setBusy(true);
    try {
      await harnessApi.command(a.id, { type: "approve", id: ap.id, decision });
    } catch (e) {
      toast.error(`Couldn't answer ${label}`, errorMessage(e));
      setBusy(false);
    }
  };
  return (
    <li className="toast rq" aria-busy={busy || undefined} {...swipe(() => putAway(a.approvals.map((p) => p.id)))}>
      <span className="rq-dot" aria-hidden="true" />
      <div className="rq-body">
        <p className="rq-head">
          <Link to={href} className="rq-name">
            {label}
          </Link>{" "}
          <span className="rq-lead">{lead}</span>
        </p>
        {line && <p className="rq-line">{line}</p>}
        <div className="rq-acts">
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
          <span className="rq-where">
            {machineName(a.target)}
            {more > 0 && ` · +${more} more`}
          </span>
        </div>
      </div>
      <button type="button" className="icon-btn rq-close" data-variant="bare" data-size="sm" aria-label={`Put away ${label}'s request`} onClick={() => putAway(a.approvals.map((p) => p.id))}>
        <X size={14} />
      </button>
    </li>
  );
}

export default function RequestToasts() {
  const waiting = React.useSyncExternalStore(subscribeWaiting, getWaiting, getNoneWaiting);
  const gone = React.useSyncExternalStore(subscribeAway, getAway, getAway);
  const path = useLocation().pathname;
  const max = useMediaQuery(PHONE) ? 1 : MAX;
  const asking = waiting.flatMap((a) => {
    if (showsItsOwn(path, a.id)) return [];
    const open = a.approvals.filter((p) => !gone.has(p.id));
    return open[0] ? [{ a, ap: open[0], more: open.length - 1 }] : [];
  });
  if (asking.length === 0) return null;
  const rest = asking.length - max;
  return (
    <ol className="rq-viewport" aria-label="Waiting on you" aria-live="polite">
      {asking.slice(0, max).map(({ a, ap, more }) => (
        <Card key={`${a.id}:${ap.id}`} a={a} ap={ap} more={more} />
      ))}
      {rest > 0 && (
        <li className="toast rq rq-rest">
          <Link to="/agents" className="rq-name">
            {rest} more agent{rest === 1 ? "" : "s"} waiting on you
          </Link>
          <ArrowUpRight size={13} aria-hidden="true" />
        </li>
      )}
    </ol>
  );
}
