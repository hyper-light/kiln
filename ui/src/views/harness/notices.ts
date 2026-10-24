/**
 * What you did with kiln's notices, kept for you by the server (`/api/push/notices`), so no device,
 * browser or kiln address shows one again: a request or card put away (`away`) or acted on
 * (`done`), and when you last saw each agent (`seen:<agent>`). A mark made here shows at once and
 * goes to the server; one made elsewhere comes as an event (`notices`), and the whole list again
 * whenever the event stream comes back. Putting a notice away closes its agent's notification on
 * every device with kiln open.
 */
import { useSyncExternalStore } from "react";
import { events } from "@/api/events";
import { pushApi, type MarkHow, type NoticeMark } from "@/api/push";
import { useAuth } from "@/lib/auth";

/** A seen mark goes to the server at most this often for one agent (here, it counts at once). */
const SEEN_EVERY_MS = 30_000;
/** What this browser kept before the server did (seen.ts's): its agents seen, taken in once. */
const OLD_SEEN = "kiln.agents.seen";

let marks: ReadonlyMap<string, NoticeMark> = new Map(oldSeen());
let you: string | null = null;
const listeners = new Set<() => void>();
let stop: (() => void) | null = null;
const sent = new Map<string, number>();

function oldSeen(): [string, NoticeMark][] {
  try {
    const v: unknown = JSON.parse(localStorage.getItem(OLD_SEEN) ?? "{}");
    if (!v || typeof v !== "object") return [];
    return Object.entries(v as Record<string, unknown>).flatMap(([id, at]) => (typeof at === "number" ? [[`seen:${id}`, { key: `seen:${id}`, how: "seen" as const, at }]] : []));
  } catch {
    return [];
  }
}

function emit() {
  listeners.forEach((l) => l());
}

/** `more` taken in: a later mark of a key stands, and an agent's latest seen. */
function take(more: readonly NoticeMark[]) {
  if (more.length === 0) return;
  const next = new Map(marks);
  for (const m of more) {
    const was = next.get(m.key);
    next.set(m.key, was && was.how === "seen" && m.how === "seen" && was.at > m.at ? was : m);
  }
  marks = next;
  emit();
}

/** Whether the person signed in has notices (agents': the owner, or `agents.run` on the server, as `/api/push` needs). */
function mine(): boolean {
  const s = useAuth.getState();
  if (s.kind !== "authed") return false;
  const can = s.me?.can;
  return !can || can["agents.run"] === "server";
}

async function load() {
  if (!mine()) return;
  try {
    const r = await pushApi.notices();
    you = r.you;
    take(r.marks);
  } catch {
    /* an older server: marks stay this page's */
  }
}

/** The notifications of agents `tags` on this device, closed. */
function closeShown(tags: readonly string[]) {
  if (tags.length === 0 || typeof navigator === "undefined" || !navigator.serviceWorker) return;
  void navigator.serviceWorker
    .getRegistration()
    .then(async (r) => {
      for (const tag of r ? tags : []) for (const n of await r!.getNotifications({ tag })) n.close();
    })
    .catch(() => undefined);
}

function start() {
  void load();
  const offEvents = events.on((ev) => {
    if (ev.type !== "notices" || !you || ev.principal !== you) return;
    take(ev.keys.map((key) => ({ key, how: ev.how, at: ev.at })));
    closeShown(ev.tags ?? []);
  });
  // What was marked while the stream was away isn't told: read again once it's back.
  let was = events.getSnapshot().state;
  const offState = events.subscribeState(() => {
    const now = events.getSnapshot().state;
    if (now === "open" && was !== "open") void load();
    was = now;
  });
  const offAuth = useAuth.subscribe((now, before) => {
    if (now.kind === "authed" && before.kind !== "authed") void load();
  });
  return () => {
    offEvents();
    offState();
    offAuth();
  };
}

function subscribe(l: () => void) {
  listeners.add(l);
  stop ??= start();
  return () => {
    listeners.delete(l);
    if (listeners.size === 0) {
      stop?.();
      stop = null;
    }
  };
}

const get = () => marks;

/** Your marks (re-renders as they change). */
export function useMarks(): ReadonlyMap<string, NoticeMark> {
  return useSyncExternalStore(subscribe, get, get);
}

/** Whether notice `key` was put away or acted on (seen is for agents, not notices). */
export function isMarked(m: ReadonlyMap<string, NoticeMark>, key: string): boolean {
  const x = m.get(key);
  return !!x && x.how !== "seen";
}

/** Put away or acted on: these notices go, and `agents`' shown notifications, wherever you are. */
export function markNotices(keys: string[], how: Exclude<MarkHow, "seen">, agents: string[] = []) {
  if (keys.length === 0) return;
  take(keys.map((key) => ({ key, how, at: Date.now() })));
  closeShown(agents);
  if (mine()) void pushApi.mark(keys, how, agents).catch(() => undefined);
}

/** You saw agent `id` now: here at once, the server at most every 30 s, and as you `leave` it. */
export function markSeen(id: string, leave = false) {
  const key = `seen:${id}`;
  const now = Date.now();
  take([{ key, how: "seen", at: now }]);
  if (!mine() || (!leave && now - (sent.get(key) ?? 0) < SEEN_EVERY_MS)) return;
  sent.set(key, now);
  void pushApi.mark([key], "seen").catch(() => undefined);
}
