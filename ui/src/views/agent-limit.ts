import type { AgentLimitView, ServerEvent } from "@/api/types";

const agents = (n: number) => `${n} agent${n === 1 ? "" : "s"}`;

/** How many of its agents are at work now (what the limit counts) out of all it has, and how many wait for room; only how many in all while that isn't known (offline). */
function now(v: AgentLimitView): string {
  if (v.sessions === 0) return "No agents now";
  if (v.working === null) return agents(v.sessions);
  const waiting = v.waiting ? `, ${v.waiting} waiting for room` : "";
  return `${v.working} of ${agents(v.sessions)} at work now${waiting}`;
}

/**
 * How many of a runner's agents are at work now, the most it has at work at once, and where that
 * comes from (docs/agents.md, "How many agents a machine runs"): set here, its config's `[agent]
 * max_agent_sessions`, or kiln's default. Only agents at work count: finished, ready, paused and
 * ended ones don't.
 */
export function limitLine(v: AgentLimitView): string {
  const h = v.held;
  if (h) {
    const why = h.from === "server" ? "set here" : h.from === "config" ? "from its config" : "kiln's default";
    const config = h.from === "server" && h.config != null ? ` (its config says ${h.config})` : "";
    return `${now(v)} · at most ${h.max_sessions} at once, ${why}${config}`;
  }
  return v.max_sessions === null ? now(v) : `${now(v)} · at most ${v.max_sessions} at once, set here`;
}

/** What became of what's set here when it isn't simply in use (offline, too old, not said yet); empty when there's nothing to say. */
export function standing(v: AgentLimitView): string {
  if (v.own) return "The server runs this machine's agents itself, held to this.";
  if (!v.connected) return v.max_sessions === null ? `${v.runner} is offline.` : `${v.runner} is offline: it holds ${v.max_sessions} when it connects.`;
  if (v.runs_agents === false) return `${v.runner} runs no agents: its terminals are off.`;
  if (v.supported === false)
    return `${v.runner}'s kiln is too old to take a limit from here: until it's upgraded (System → Updates), it holds ${v.default} and counts every agent it has, finished ones too.`;
  if (v.supported === null || !v.held) return `Waiting for ${v.runner} to say what it holds…`;
  const holds = v.held.from === "server" ? v.held.max_sessions : null;
  return v.max_sessions === holds ? "" : `${v.runner} is taking it…`;
}

/** Whether a limit is set from here: not for a runner that runs no agents. */
export function settable(v: AgentLimitView): boolean {
  return v.own || v.runs_agents !== false;
}

/** The number typed, as a limit (from 1 to `max`), or why it can't be one. */
export function parseLimit(text: string, max: number): number | string {
  const t = text.trim();
  const n = /^\d+$/.test(t) ? Number(t) : NaN;
  return Number.isSafeInteger(n) && n >= 1 && n <= max ? n : `A whole number of agents, from 1 to ${max}.`;
}

/**
 * A runner's agent limit after a server event (its resource's `apply`): as it was when the event
 * isn't about it; undefined, to load it again, when its limit or its connection changed, one of its
 * agents did (starting or finishing a turn changes how many are at work), or an agent was closed
 * (whose isn't said).
 */
export function withEvent(v: AgentLimitView, ev: ServerEvent): AgentLimitView | undefined {
  switch (ev.type) {
    case "agent_limit":
      return ev.runner === v.runner ? undefined : v;
    case "agent":
      return ev.agent.name === v.runner && ev.agent.connected !== v.connected ? undefined : v;
    case "harness":
      return ev.info.target === (v.own ? "local" : `agent:${v.runner}`) ? undefined : v;
    default:
      return undefined;
  }
}
