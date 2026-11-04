/**
 * Requests you've answered here that the agent hasn't taken yet, and paused goals you resumed here
 * (or whose plan you approved). An answer shows as given at once, wherever its request showed (its
 * row in Mission control, its card anywhere in kiln, the agent's page), however slow the way to the
 * agent; if sending it fails, the request is back as it was. An answer not taken within a minute is
 * let go, so a request the agent still waits on shows again.
 */
import { useSyncExternalStore } from "react";
import { clientId, commandId, harnessApi, type ApprovalBrief, type Command, type SessionStore } from "@/api/harness";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { Tries } from "@/lib/tries";
import { markNotices } from "./notices";

/** How long an answer given here stands in for the agent's own word. */
const STANDS_MS = 60_000;

let answered: ReadonlyMap<string, number> = new Map();
const listeners = new Set<() => void>();
let timer = 0;

function emit() {
  listeners.forEach((l) => l());
}

/** Let go of answers past their time, and wake again for the next one to pass. */
function sweep() {
  window.clearTimeout(timer);
  timer = 0;
  const now = Date.now();
  const kept = [...answered].filter(([, at]) => now - at < STANDS_MS);
  if (kept.length !== answered.size) {
    answered = new Map(kept);
    emit();
  }
  const next = Math.min(...kept.map(([, at]) => at + STANDS_MS - now));
  if (Number.isFinite(next)) timer = window.setTimeout(sweep, next + 50);
}

/** These requests are answered (they go from sight now). */
export function markAnswered(ids: string[]) {
  if (ids.length === 0) return;
  const next = new Map(answered);
  for (const id of ids) next.set(id, Date.now());
  answered = next;
  emit();
  if (!timer) sweep();
}

/** These requests weren't answered after all (the answer didn't go). */
export function unmark(ids: string[]) {
  if (!ids.some((id) => answered.has(id))) return;
  const next = new Map(answered);
  for (const id of ids) next.delete(id);
  answered = next;
  emit();
}

/** Answer one request of agent `agent`: shown as answered at once, back if it fails (and thrown). */
export async function answer(agent: string, c: Extract<Command, { type: "approve" }>) {
  markAnswered([c.id]);
  try {
    await harnessApi.command(agent, c);
  } catch (e) {
    unmark([c.id]);
    throw e;
  }
  // Dealt with: gone from your other devices and pages too.
  markNotices([c.id], "done", [agent]);
}

/**
 * Send `c` to agent `agent` from its page: over its socket, else (the socket reconnecting) over
 * HTTP under the same id, which its session takes once however it comes; never "try again in a
 * moment" (W21-R15). An answer shows as given at once, and is back, with why, if it doesn't go.
 * Settles once it's gone (at once over the socket, which says itself what it waits for): a
 * control can say it's on its way until then (`useAction`); a failure is said here.
 */
export function sendCommand(store: SessionStore, agent: string, c: Command): Promise<void> {
  if (c.type === "approve") markAnswered([c.id]);
  if (store.send(c)) return Promise.resolve();
  return harnessApi.command(agent, c, commandId(c)).then(
    () => {
      if (c.type === "approve") markNotices([c.id], "done", [agent]);
    },
    (e: unknown) => {
      if (c.type === "approve") unmark([c.id]);
      toast.error("Couldn't reach the agent", errorMessage(e));
    },
  );
}

/** What says an agent's paused goals were resumed here, or the plan they wait on approved: its card's key (cards.ts). */
export const heldKey = (a: { id: string; held?: { since: number } }) => (a.held ? `held:${a.id}:${a.held.since}` : "");

/**
 * What waits on you of agent `agent` (its card's `key`, cards.ts), dealt with here as `go` asks:
 * shown so at once, back if it doesn't go (and thrown), and gone from your other devices once it has.
 */
export async function dealWith(key: string, agent: string, go: () => Promise<unknown>) {
  markAnswered([key]);
  try {
    await go();
  } catch (e) {
    unmark([key]);
    throw e;
  }
  markNotices([key], "done", [agent]);
}

/**
 * The retries of what's asked of each card (its key, cards.ts), kept while its card is out of sight:
 * dealt with here, a card goes at once and comes back new if it didn't go, and what it asks then is
 * the same request under the same key (CP-T3.2), never a second one.
 */
const tries = new Map<string, Tries>();

/** The key to ask `said` of the card under `card` with: the last try's, while it's the same. */
export function tryKey(card: string, said: string): string {
  let t = tries.get(card);
  if (!t) {
    t = new Tries(clientId);
    tries.set(card, t);
  }
  return t.keyFor(said);
}

/** What was asked of the card under `card` went through: what's asked next is new. */
export function tried(card: string) {
  tries.delete(card);
}

/**
 * Resume agent `a`'s paused goals, or approve the plan they wait on (`go` asks): its card gone and
 * its row no longer waiting on you at once, wherever they show (`dealWith`).
 */
export async function resumeHeld(a: { id: string; held?: { since: number } }, go: () => Promise<unknown>) {
  const key = heldKey(a);
  if (key) await dealWith(key, a.id, go);
}

const subscribe = (l: () => void) => {
  listeners.add(l);
  return () => listeners.delete(l);
};
const get = () => answered;

/** The requests answered here and not yet taken (re-renders as that changes). */
export function useAnswered(): ReadonlyMap<string, number> {
  return useSyncExternalStore(subscribe, get, get);
}

/** What still waits of `approvals`, the answered ones left out (the same array when none are). */
export function stillAsked<T extends { id: string }>(approvals: T[], done: ReadonlyMap<string, number>): T[] {
  return done.size > 0 && approvals.some((p) => done.has(p.id)) ? approvals.filter((p) => !done.has(p.id)) : approvals;
}

/**
 * An agent as it stands once what was answered here is taken: those requests gone, and one that
 * waited only on them working again; its goals no longer paused once resumed here (`resumeHeld`).
 * The same object when nothing changes.
 */
export function asAnswered<T extends { id: string; approvals: ApprovalBrief[]; status: string; held?: { since: number } }>(a: T, done: ReadonlyMap<string, number>): T {
  const left = stillAsked(a.approvals, done);
  const resumed = done.size > 0 && done.has(heldKey(a));
  if (left === a.approvals && !resumed) return a;
  // Working again: answered, it had only those to wait on; resumed, its goals go on.
  const working = (left.length === 0 && a.status === "waiting") || (resumed && a.status === "idle");
  const next = { ...a, approvals: left, status: working ? "running" : a.status };
  if (resumed) delete next.held;
  return next;
}
