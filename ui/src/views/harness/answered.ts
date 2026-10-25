/**
 * Requests you've answered here that the agent hasn't taken yet. An answer shows as given at once,
 * wherever its request showed (its row in Mission control, its card anywhere in kiln, the agent's
 * page), however slow the way to the agent; if sending it fails, the request is back as it was. An
 * answer not taken within a minute is let go, so a request the agent still waits on shows again.
 */
import { useSyncExternalStore } from "react";
import { harnessApi, type ApprovalBrief, type Command } from "@/api/harness";
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
 * waited only on them working again (the same object when nothing changes).
 */
export function asAnswered<T extends { approvals: ApprovalBrief[]; status: string }>(a: T, done: ReadonlyMap<string, number>): T {
  const left = stillAsked(a.approvals, done);
  if (left === a.approvals) return a;
  return { ...a, approvals: left, status: left.length === 0 && a.status === "waiting" ? "running" : a.status };
}
