/**
 * The agents with something to show wherever you are in kiln (cards.ts: a request, goals paused for
 * you, a stop of their own, a usage limit), kept for the whole app: read once when first asked for,
 * then kept from the server's events, and read again whenever the event stream comes back (what
 * changed while it was away isn't told). Only a change in what the cards show tells the page; an
 * agent's other news doesn't. Only for a person who may see agents (`agents.run` on the server, as
 * the agents' routes need): one who may not gets no cards, and kiln isn't asked (it would refuse).
 */
import { events } from "@/api/events";
import { harnessApi, type AgentInfo } from "@/api/harness";
import { useAuth } from "@/lib/auth";
import { atLimit, notable, stoppedItself } from "./cards";

const agents = new Map<string, AgentInfo>();
/** What each waiting agent shows, so news that changes none of it says nothing. */
const shown = new Map<string, string>();
let waiting: AgentInfo[] = [];
const listeners = new Set<() => void>();
let stop: (() => void) | null = null;

const shownOf = (a: AgentInfo) =>
  [
    a.name,
    a.title,
    a.target,
    ...a.approvals.map((p) => `${p.id}\u0000${p.tool}\u0000${p.tool_name}\u0000${p.title}`),
    a.held ? `held\u0000${a.held.since}\u0000${a.held.why}\u0000${a.held.plan ?? ""}` : "",
    stoppedItself(a) ? `stopped\u0000${a.last_activity}\u0000${a.exit?.error ?? ""}` : "",
    a.pause && atLimit(a) ? `limit\u0000${a.pause.since}\u0000${a.pause.reason}\u0000${a.pause.label ?? ""}\u0000${a.pause.until ?? ""}\u0000${a.pause.auto}\u0000${a.pause.by}` : "",
  ].join("\u0001");
/** Since when it has had something to show: its oldest request, else when it paused or stopped. */
const since = (a: AgentInfo) => (a.approvals.length > 0 ? Math.min(...a.approvals.map((p) => p.at || a.last_activity)) : (a.held?.since ?? a.pause?.since ?? a.last_activity));

function changed(a: AgentInfo | undefined, id: string): boolean {
  const now = a && notable(a) ? shownOf(a) : undefined;
  if (shown.get(id) === now) return false;
  if (now === undefined) shown.delete(id);
  else shown.set(id, now);
  return true;
}

function publish() {
  waiting = [...agents.values()].filter(notable).sort((x, y) => since(x) - since(y));
  listeners.forEach((l) => l());
}

/** Whether the person signed in may see agents: the owner may (no capabilities listed), anyone else with `agents.run` on the server. */
function mayRead(): boolean {
  const s = useAuth.getState();
  if (s.kind !== "authed") return false;
  const can = s.me?.can;
  return !can || can["agents.run"] === "server";
}

async function read() {
  if (!mayRead()) return;
  let list: AgentInfo[];
  try {
    list = await harnessApi.list();
  } catch {
    return;
  }
  const gone = new Set(agents.keys());
  let any = false;
  for (const a of list) {
    gone.delete(a.id);
    agents.set(a.id, a);
    any = changed(a, a.id) || any;
  }
  for (const id of gone) {
    agents.delete(id);
    any = changed(undefined, id) || any;
  }
  if (any) publish();
}

function start() {
  const offEvents = events.on((ev) => {
    if (ev.type === "harness") {
      agents.set(ev.info.id, ev.info);
      if (changed(ev.info, ev.info.id)) publish();
    } else if (ev.type === "harness_closed") {
      agents.delete(ev.id);
      if (changed(undefined, ev.id)) publish();
    }
  });
  let was = events.getSnapshot().state;
  const offState = events.subscribeState(() => {
    const now = events.getSnapshot().state;
    if (now === "open" && was !== "open") void read();
    was = now;
  });
  // Read once the person is known, if that's after the cards were first asked for.
  const offAuth = useAuth.subscribe((now, before) => {
    if (now.kind === "authed" && before.kind !== "authed") void read();
  });
  void read();
  return () => {
    offEvents();
    offState();
    offAuth();
  };
}

export function subscribeWaiting(l: () => void) {
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

export const getWaiting = () => waiting;
const NONE: AgentInfo[] = [];
export const getNoneWaiting = () => NONE;
