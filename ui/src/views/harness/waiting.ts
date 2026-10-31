/**
 * U13: waiting cards project the shared harness roster. The resource owns fetching, live events
 * and reconnects; this projection only notifies when the visible cards or their order change.
 * Only a person with server-wide agents.run may subscribe to that roster here.
 */
import type { AgentInfo } from "@/api/harness";
import { useAuth } from "@/lib/auth";
import { harnessSessionsRes, listedHarnessSessions } from "@/lib/data";
import { observeResource } from "@/lib/resource";
import { atLimit, notable, stoppedItself } from "./cards";

let waiting: AgentInfo[] = [];
let signature = "[]";
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

function project(list: AgentInfo[]) {
  const next = listedHarnessSessions(list).filter(notable).sort((x, y) => since(x) - since(y));
  const key = JSON.stringify(next.map((a) => [a.id, shownOf(a), since(a)]));
  if (signature === key) return;
  signature = key;
  waiting = next;
  listeners.forEach((l) => l());
}

/** Whether the person signed in may see agents: the owner may (no capabilities listed), anyone else with `agents.run` on the server. */
function mayRead(): boolean {
  const s = useAuth.getState();
  if (s.kind !== "authed") return false;
  const can = s.me?.can;
  return !can || can["agents.run"] === "server";
}

function start() {
  let offRoster: (() => void) | undefined;
  const syncAuth = () => {
    if (mayRead()) {
      offRoster ??= observeResource(harnessSessionsRes.key, harnessSessionsRes.fetch, harnessSessionsRes.opts, project);
    } else {
      offRoster?.();
      offRoster = undefined;
      project([]);
    }
  };
  const offAuth = useAuth.subscribe(syncAuth);
  syncAuth();
  return () => {
    offRoster?.();
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
