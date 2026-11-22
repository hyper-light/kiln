/**
 * The agents' list, apart from the rest of harness.ts: every signed-in page reads it (the shell's
 * agents, the palette), while the session store and the rest of harness.ts load only with the pages
 * that show a conversation (goal 8: what a page doesn't show stays out of its first load).
 */
import { SESSIONS_READ, WAITING_READ } from "./launch-reads";
import { fromColumns, isColumns, type Columns } from "./columns";
import { listAsked, listedAgain } from "./last-words";
import { ApiError } from "./client";
import type { AgentInfo } from "./wire";

export async function call<T>(method: string, path: string, body?: unknown, signal?: AbortSignal, headers?: Record<string, string>): Promise<T> {
  const res = await fetch(path, {
    method,
    credentials: "same-origin",
    headers: body === undefined ? headers : { "content-type": "application/json", ...headers },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal,
  });
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`;
    try {
      const j = (await res.json()) as { error?: string };
      if (j.error) msg = j.error;
    } catch {
      /* not json */
    }
    // With its status, for what tells one answer from another (a start asked again, refused as there already: starting.ts).
    throw new ApiError(res.status, msg);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

/** Whether an agent is one of the agents kiln lists: a card's helper (`helper`, `agents.about`) is the card's, no row of them. */
export const listed = (a: AgentInfo) => !a.helper;

/**
 * The agents, as the list tells them lean and as columns (W21-R02; a server from before answers the list itself).
 * What was known of their last words is as old as the list before: the rows that show them ask again.
 */
export function listAgents(signal?: AbortSignal): Promise<AgentInfo[]> {
  const since = listAsked();
  return call<AgentInfo[] | Columns>("GET", SESSIONS_READ, undefined, signal).then((read) => {
    listedAgain(since);
    return (isColumns(read) ? fromColumns<AgentInfo>(read) : read).filter(listed);
  });
}

/** Compact waiting-cards list (~300 B even at 10k sessions), falling back to full list on older servers. */
export function listWaiting(signal?: AbortSignal): Promise<AgentInfo[]> {
  return call<unknown>("GET", WAITING_READ, undefined, signal)
    .then((read) => (Array.isArray(read) ? (read as AgentInfo[]).filter(listed) : listAgents(signal)))
    .catch((error: unknown) => {
      if (signal?.aborted) throw error;
      if (error instanceof ApiError && error.status === 404) return listAgents(signal);
      return [];
    });
}

/** Only one session, its parent, and its descendants (~400 B even at 10k sessions). */
export function listAgentFamily(id: string, signal?: AbortSignal): Promise<AgentInfo[]> {
  return call<AgentInfo[]>("GET", `/api/harness/sessions?family=${encodeURIComponent(id)}`, undefined, signal)
    .then((read) => read.filter(listed))
    .catch((error: unknown) => {
      if (signal?.aborted) throw error;
      return [];
    });
}
