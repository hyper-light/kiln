/**
 * The agents with something to show wherever you are in kiln (cards.ts: a request, goals paused for
 * you, a stop of their own, a usage limit), kept for the whole app from the one list of agents every
 * page reads (lib/data.ts's `harnessSessionsRes`): it's read once for them all and kept from the
 * server's events, and read again when the event stream comes back without what it missed
 * (lib/resource.ts). This kept a list of its own once, read as the shell mounted and again each time
 * the stream opened, resumed or not: every page asked the server for all its agents two or three
 * times as it opened, and again at each reconnect. Only a change in what the cards show tells the
 * page; an agent's other news doesn't. Only for a person who may see agents (`agents.run` on the
 * server, as the agents' routes need): one who may not gets no cards, and kiln isn't asked (it
 * would refuse).
 */
import type { AgentInfo } from "@/api/harness";
import { useAuth } from "@/lib/auth";
import { harnessSessionsRes, harnessWaitingRes } from "@/lib/data";
import { resourceData, watch } from "@/lib/resource";
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

/** Mission Control already reads `harnessSessionsRes`, so share that one list there; other pages read compact `harnessWaitingRes`. */
function source() {
  if (resourceData(harnessSessionsRes) !== undefined) return harnessSessionsRes;
  if (typeof location !== "undefined") {
    const p = location.pathname.replace(/^\/\.kiln-peer\/[^/]+/, "").replace(/\/+$/, "") || "/";
    if (p === "/agents") return harnessSessionsRes;
  }
  return harnessWaitingRes;
}

/** The cards as the active source (waiting list on non-MC pages, full list on MC) now says them. */
function sync() {
  const list = mayRead() ? (resourceData(harnessSessionsRes) ?? resourceData(harnessWaitingRes) ?? []) : [];
  const gone = new Set(agents.keys());
  let any = false;
  for (const a of list) {
    gone.delete(a.id);
    if (agents.get(a.id) === a) continue;
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
  let off = () => {};
  const follow = () => {
    off();
    const res = source();
    off = mayRead() ? watch(res.key, res.fetch, res.opts ?? {}, sync) : () => {};
    sync();
  };
  follow();
  // The list is its login's own (lib/resource.ts lets go of it as that ends): followed afresh once the person is known.
  const offAuth = useAuth.subscribe((now, before) => {
    if (now.kind === "authed" && before.kind !== "authed") follow();
    else if (now.kind !== "authed" && before.kind === "authed") sync();
  });
  return () => {
    off();
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
