/**
 * The agents' last words, for the rows that show them (W21-R02). The list of the agents is read lean
 * (`SESSIONS_READ`): an agent that has said something comes without its `summary`, a quarter of its row, and only
 * a row on the page shows that line. Such a row reads it here: told by the attention snapshot for the rows the
 * list opens on (`said`, api/attention.ts), by an agent's own news (an event carries its whole entry), and
 * otherwise asked for, every row of one render in one request (`GET /api/harness/sessions?summaries=`).
 *
 * Until an agent's are known its row shows no line (never another text in their place, which would then change);
 * once the list is read again (the stream came back without what it missed), what's known stays on the page while
 * the rows shown ask again.
 */
import * as React from "react";
import { request } from "./client";
import { events } from "./events";
import type { AgentInfo } from "./harness";

/** An agent, as far as its last words go: one the list told lean says none (`summary` left out). */
type Speaker = Pick<AgentInfo, "id" | "summary">;

/** What each agent last said, as last told (empty: nothing). */
const known = new Map<string, string>();
/** Those told since the list was last asked for, each with when (`now`): another's is asked for again by the row that shows it. */
const fresh = new Map<string, number>();
/** Counts what's told and each asking for the list: what was told before the list was asked for is older than it. */
let now = 0;
/** Those asked for and not answered yet, each with its request's end. */
const asked = new Map<string, Promise<void>>();
/** Those that couldn't be read: left until the list is read again. */
const failed = new Set<string>();
/** The request being gathered: every agent asked for before it goes, at the end of this turn of the page's code. */
let gathering: { ids: string[]; done: Promise<void> } | null = null;
const SETTLED: Promise<void> = Promise.resolve();

let version = 0;
const listeners = new Set<() => void>();
const subscribe = (changed: () => void) => {
  listeners.add(changed);
  return () => {
    listeners.delete(changed);
  };
};
function changed() {
  version++;
  for (const l of [...listeners]) l();
}

/** `said`: agents' last words, by id, as new as the list. */
export function told(said: Readonly<Record<string, string>>) {
  let news = false;
  for (const [id, words] of Object.entries(said)) {
    if (known.get(id) !== words) news = true;
    known.set(id, words);
    fresh.set(id, now);
    failed.delete(id);
  }
  if (news) changed();
}

async function read(ids: readonly string[]) {
  try {
    const said = await request<Record<string, string>>("GET", `/api/harness/sessions?summaries=${ids.map(encodeURIComponent).join(",")}`);
    // One the server says nothing of is gone, or was never this caller's to see: it has no line to wait for.
    told(Object.fromEntries(ids.map((id) => [id, said[id] ?? ""])));
  } catch {
    for (const id of ids) failed.add(id);
  } finally {
    for (const id of ids) asked.delete(id);
  }
}

/** Asks for `id`'s, with whatever else is asked for now: settled once they're told, or couldn't be. */
function ask(id: string): Promise<void> {
  const under = asked.get(id);
  if (under) return under;
  if (fresh.has(id) || failed.has(id)) return SETTLED;
  if (!gathering) {
    const ids: string[] = [];
    gathering = {
      ids,
      done: SETTLED.then(() => {
        gathering = null;
        return read(ids);
      }),
    };
  }
  gathering.ids.push(id);
  asked.set(id, gathering.done);
  return gathering.done;
}

/**
 * `a`'s last words: its own when it says them (empty: it has said none), else what's known of them, asked for
 * when that's older than the list. Undefined: not known yet (its row shows no line until they are).
 */
export function lastWords(a: Speaker): string | undefined {
  if (a.summary !== undefined) return a.summary;
  if (typeof window !== "undefined") void ask(a.id);
  return known.get(a.id);
}

/** `a`'s last words (`lastWords`), followed: the row that shows them (`shows`) draws again as they're told. */
export function useLastWords(a: Speaker, shows = true): string | undefined {
  const current = () => (shows ? lastWords(a) : undefined);
  return React.useSyncExternalStore(subscribe, current, () => (shows ? a.summary : undefined));
}

/** Changes whenever any agent's last words are told: what something made of several agents' (`lastWords`) is made again by. */
export function useSaid(): number {
  return React.useSyncExternalStore(
    subscribe,
    () => version,
    () => 0,
  );
}

/**
 * Settled once the last words of each of `agents` are known, or couldn't be read: what a list's first rows wait
 * on, so they're drawn with their lines (words known before, however old, don't wait: they show while asked again).
 */
export function lastWordsOf(agents: readonly Speaker[]): Promise<void> {
  const waits = agents.filter((a) => a.summary === undefined && !known.has(a.id)).map((a) => ask(a.id));
  return waits.length === 0 ? SETTLED : Promise.all(waits).then(() => undefined);
}

/** The list is being asked for: what's told from now on is as new as it. Its answer says since when (`listedAgain`). */
export function listAsked(): number {
  return ++now;
}

/**
 * The list asked for at `since` (`listAsked`) is in: what was known of the agents' last words before it was asked
 * for is as old as the list before, and what couldn't be read then can be asked for again.
 */
export function listedAgain(since: number) {
  let older = failed.size > 0;
  failed.clear();
  for (const [id, at] of fresh) {
    if (at >= since) continue;
    fresh.delete(id);
    older = true;
  }
  if (older) changed();
}

// An agent's news carries its whole entry: its last words are then as new as can be. One that closed has none to keep.
events.on(["harness", "harness_closed"], (ev) => {
  if (ev.type === "harness_closed") {
    known.delete(ev.id);
    fresh.delete(ev.id);
  } else if (ev.info.summary !== undefined) {
    known.set(ev.info.id, ev.info.summary);
    fresh.set(ev.info.id, now);
  }
});

/** For tests: nothing known, nothing asked. */
export function forgetLastWords() {
  known.clear();
  fresh.clear();
  asked.clear();
  failed.clear();
  gathering = null;
  version = 0;
  now = 0;
}
