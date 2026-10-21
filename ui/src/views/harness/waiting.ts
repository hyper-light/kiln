/**
 * The agents waiting on you, kept for the whole app (the requests shown wherever you are in kiln):
 * read once when first asked for, then kept from the server's events, and read again whenever the
 * event stream comes back (what changed while it was away isn't told). Only a change in what waits
 * (a request, its agent's name) tells the page; an agent's other news doesn't.
 */
import { events } from "@/api/events";
import { harnessApi, type AgentInfo } from "@/api/harness";

const agents = new Map<string, AgentInfo>();
/** What each waiting agent shows, so news that changes none of it says nothing. */
const shown = new Map<string, string>();
let waiting: AgentInfo[] = [];
const listeners = new Set<() => void>();
let stop: (() => void) | null = null;

const shownOf = (a: AgentInfo) => [a.name, a.title, a.target, ...a.approvals.map((p) => `${p.id}\u0000${p.tool}\u0000${p.tool_name}\u0000${p.title}`)].join("\u0001");
/** Since when it has waited: its oldest request. */
const since = (a: AgentInfo) => Math.min(...a.approvals.map((p) => p.at || a.last_activity));

function changed(a: AgentInfo | undefined, id: string): boolean {
  const now = a && a.approvals.length > 0 ? shownOf(a) : undefined;
  if (shown.get(id) === now) return false;
  if (now === undefined) shown.delete(id);
  else shown.set(id, now);
  return true;
}

function publish() {
  waiting = [...agents.values()].filter((a) => a.approvals.length > 0).sort((x, y) => since(x) - since(y));
  listeners.forEach((l) => l());
}

async function read() {
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
  void read();
  return () => {
    offEvents();
    offState();
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
