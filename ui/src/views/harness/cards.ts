/**
 * What shows as a card wherever you are in kiln (approval-toasts.tsx), one an agent, the most
 * pressing first: a request (a tool call, a question, a plan), goals it paused for you, a stop of
 * its own, then a usage limit it reached (only to say so). Never where the page shows it already
 * (Mission control, the agent's own pages), nor once put away.
 */
import type { AgentInfo, ApprovalBrief } from "@/api/harness";

export type CardKind = "request" | "held" | "stopped" | "limit";

export interface Card {
  kind: CardKind;
  a: AgentInfo;
  /** What puts it away: a request's id; else its kind, its agent and since when. */
  key: string;
  /** Since when: a request's, a pause's, a stop's. */
  since: number;
  /** The request, for one. */
  ap?: ApprovalBrief;
  /** Its agent's other requests. */
  more: number;
}

const ORDER: Record<CardKind, number> = { request: 0, held: 1, stopped: 2, limit: 3 };

/** The page shows this agent's requests and how it stands itself. */
export const showsItsOwn = (path: string, id: string) => path === "/agents" || path === "/agents/" || path === `/agents/${id}` || path.startsWith(`/agents/${id}/`);

/** Stopped by itself: its harness ended with an error. */
export const stoppedItself = (a: AgentInfo) => a.status === "exited" && !!a.exit?.error;

/** Waiting at a usage limit, as Mission control's Paused (a turn under way finishes first). */
export const atLimit = (a: AgentInfo) => !!a.pause && a.status !== "running" && a.status !== "exited" && a.status !== "disconnected";

/** Whether an agent has anything to show as a card. */
export const notable = (a: AgentInfo) => a.approvals.length > 0 || !!a.held || stoppedItself(a) || atLimit(a);

/** The cards for page `path`: requests answered here (`answered`) or put away (`away`) aren't. */
export function cardsOf(agents: readonly AgentInfo[], path: string, away: ReadonlySet<string>, answered: { has(id: string): boolean }): Card[] {
  const cards = agents.flatMap((a): Card[] => {
    if (showsItsOwn(path, a.id)) return [];
    const open = a.approvals.filter((p) => !away.has(p.id) && !answered.has(p.id));
    if (open[0]) return [{ kind: "request", a, key: open[0].id, since: open[0].at || a.last_activity, ap: open[0], more: open.length - 1 }];
    const card = (kind: CardKind, since: number): Card[] => {
      const key = `${kind}:${a.id}:${since}`;
      return away.has(key) ? [] : [{ kind, a, key, since, more: 0 }];
    };
    if (a.held) return card("held", a.held.since);
    if (stoppedItself(a)) return card("stopped", a.last_activity);
    if (a.pause && atLimit(a)) return card("limit", a.pause.since);
    return [];
  });
  return cards.sort((x, y) => ORDER[x.kind] - ORDER[y.kind] || x.since - y.since);
}

/** A held stack's why without who paused it (the card says that): "plan … is ready for you to look at". */
export const heldWords = (why: string) => why.replace(/^Paused by [^:]+:\s*/, "").replace(/^Paused by [^.]+\.$/, "");
