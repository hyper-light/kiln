/** Mission Control attention, plan 01a199e1b177752e84dad5dc990e6a15. Kept per person on the server. */
import * as React from "react";
import { useServerEvents } from "./events";
import { request } from "./client";
import { clientId, type AgentInfo } from "./harness";
import { AttentionControls, AttentionOrder, AttentionVisits } from "@/lib/attention-order";
import type { AttentionClose, AttentionFrame, AttentionPair, AttentionSelection } from "@/lib/attention-exposure";
import { onOwnAgentAction } from "@/lib/own-agent-action";
import { useAuth } from "@/lib/auth";
import { invalidate, preload, setData, useSuspenseResource } from "@/lib/resource";

export interface AttentionUrgency {
  score: number | null;
  stale: boolean;
  known: boolean;
  since: number | null;
  waited: number | null;
  expected_wait: number | null;
  observations: number;
  reason: string;
}
export interface AttentionRequest {
  agent: string;
  kind: "approval" | "held" | "stop";
  id: string;
  at: number;
}
export interface AttentionMark {
  pinned?: number;
  parked_until?: number;
  parked_at?: number;
  deferred_requests?: AttentionRequest[];
  updated_seen?: number;
  archived?: boolean;
  archive_status?: "archiving" | "archived";
  rank?: number | null;
  reason?: string;
  rank_reason?: string;
  wake_reason?: string;
  revision: number;
  urgency?: AttentionUrgency;
}
export interface AttentionSettings {
  sort: "learned" | "yours";
  auto_park_days: number | null;
  archive_after_days: number;
  auto_archive: boolean;
  park_later_minutes?: number;
  park_evening_hour?: number;
  park_morning_hour?: number;
}
export interface AttentionFit {
  observations: number;
  ignored: number;
  position_corrected: number;
  improves_baseline: boolean;
  reason: string;
  parameters: { name: string; value: number; standard_error: number | null }[];
  comparisons: { name: string; choices: number; log_likelihood: number }[];
  unavailable_comparisons?: { name: string; reason: string }[];
}
export interface AttentionReplay {
  at: number;
  order: { agent: string; score: number | null; reason: string }[];
  model: AttentionFit;
  choices: { at: number; chosen: string; candidates: string[] }[];
}
export function attentionReplay(at: number): Promise<AttentionReplay> {
  return request("GET", `/api/harness/attention/replay?at=${encodeURIComponent(at)}`);
}
export interface AttentionSnapshot {
  you: string;
  agents: Record<string, AttentionMark>;
  order: string[];
  request_order?: string[];
  settings: AttentionSettings;
  archived: AgentInfo[];
  uses?: { agent: string; kind: string; at: number }[];
  undo?: { agent: string; revision: number };
  exposure?: string | null;
  pair?: AttentionPair | null;
  examination?: {
    curves: { layout: string; reference: number; positions: { position: number; relative: number; standard_error: number | null }[]; opportunities: number; reason: string }[];
    unavailable: { layout: string; positions: number[]; reason: string }[];
    exposures: number;
    censored: number;
    ignored: number;
    opportunities: number;
  } | null;
  rank_reason?: string;
  model?: AttentionFit;
}
export interface AttentionCommand {
  action: "pin" | "unpin" | "park" | "unpark" | "seen" | "visit" | "archive" | "restore" | "restore_copy" | "undo" | "settings" | "reorder";
  agent?: string;
  until?: number;
  revision?: number;
  settings?: Partial<AttentionSettings>;
  order?: string[];
  exposure?: string;
  key?: string;
  mode?: "arrival" | "refresh";
  visit?: string;
  frame_revision?: number;
  chosen_position?: number;
  source?: "list" | "direct";
}
let pendingVisit: { agent: string; exposure?: string; frame_revision?: number; chosen_position?: number } | undefined;
export function fromAttentionRow(agent: string, selection?: AttentionSelection | string) {
  pendingVisit = { agent, ...(typeof selection === "string" ? { exposure: selection } : selection) };
}
export function attentionVisit(agent: string): Promise<AttentionSnapshot> {
  const selection = pendingVisit?.agent === agent ? pendingVisit : undefined;
  pendingVisit = undefined;
  return attentionAction({ action: "visit", ...selection, agent, source: selection ? "list" : "direct", key: clientId() });
}
export const attentionRender = (frame: AttentionFrame) => request<{ ok: boolean; accepted: boolean }>("POST", "/api/harness/attention", { action: "render", ...frame });
export const attentionClose = (close: AttentionClose) => request<{ ok: boolean; accepted: boolean }>("POST", "/api/harness/attention", { action: "close", ...close }, { keepalive: true });
let session = 0;
const orders = new Map<string, AttentionOrder>();
const activeViews = new Set<string>();
const visits = new AttentionVisits();
interface Scope {
  visit: string;
  controls: AttentionControls;
  needsDraw: boolean;
  departed: boolean;
  epoch: number;
  pairKey?: string;
  snapshot?: AttentionSnapshot;
  pending?: Promise<AttentionSnapshot>;
  onDeparture?: () => void;
}
const scopes = new Map<string, Scope>();
function scopeFor(ownKey: string, controls?: string): Scope {
  let scope = scopes.get(ownKey);
  if (!scope) {
    scope = { visit: clientId(), controls: new AttentionControls(), needsDraw: true, departed: false, epoch: 0 };
    scopes.set(ownKey, scope);
  }
  if (controls !== undefined) scope.controls.bind(controls);
  return scope;
}
function orderFor(key: string): AttentionOrder {
  const order = orders.get(key) ?? new AttentionOrder();
  orders.set(key, order);
  return order;
}
useAuth.subscribe((now, before) => {
  if (now.kind !== before.kind || (now.kind === "authed" && before.kind === "authed" && now.me !== before.me)) {
    session++;
    pendingVisit = undefined;
    orders.clear();
    activeViews.clear();
    scopes.clear();
    visits.returned();
  }
});
const key = () => `harness/attention/${session}/${visits.current()}`;
const arrivalKey = () => {
  visits.arrive();
  return key();
};
const read = (signal: AbortSignal): Promise<AttentionSnapshot> => {
  const ownKey = key();
  const scope = scopeFor(ownKey);
  if (scope.pending) return scope.pending;
  const mode = scope.needsDraw ? "arrival" : "refresh";
  const controls = scope.controls.value;
  const epoch = scope.epoch;
  const params = new URLSearchParams({ mode, visit: scope.visit });
  const pending = request<AttentionSnapshot>("GET", `/api/harness/attention?${params}`, undefined, { signal })
    .then((next) => {
      if (ownKey !== key() || scope.epoch !== epoch) return scope.snapshot ?? next;
      const held = orderFor(ownKey).snapshot(next);
      if (mode === "arrival") {
        scope.needsDraw = false;
        scope.controls.sampled(controls);
        scope.pairKey = held.pair?.key;
      }
      scope.snapshot = held;
      return held;
    })
    .finally(() => {
      if (scope.pending === pending) scope.pending = undefined;
    });
  scope.pending = pending;
  return pending;
};

export const preloadAttention = (controls?: string) => {
  const ownKey = arrivalKey();
  scopeFor(ownKey, controls);
  preload(ownKey, read);
};

export async function attentionAction(command: AttentionCommand): Promise<AttentionSnapshot> {
  const ownKey = key();
  const scope = scopeFor(ownKey);
  const own = activeViews.has(ownKey) && command.action !== "visit";
  const visit = own ? clientId() : scope.visit;
  const controls = scope.controls.value;
  const next = await request<AttentionSnapshot>("POST", "/api/harness/attention", { ...command, mode: own ? "arrival" : "refresh", visit });
  if (ownKey === key()) {
    if (own) {
      scope.epoch++;
      scope.visit = visit;
      scope.needsDraw = false;
      scope.controls.sampled(controls);
      scope.pairKey = next.pair?.key;
    }
    const held = activeViews.has(ownKey) ? orderFor(ownKey).snapshot(next, own) : next;
    if (!activeViews.has(ownKey)) orderFor(ownKey).returned();
    scope.snapshot = held;
    setData<AttentionSnapshot>(ownKey, () => held);
  }
  return next;
}

/** No activity polling: a visit/return samples the order, own actions update it immediately. */
export function useAttention(controls = "") {
  useAuth();
  const ownKey = arrivalKey();
  const scope = scopeFor(ownKey, controls);
  const resource = useSuspenseResource(ownKey, read);
  useServerEvents((event) => {
    if (String(event.type) !== "attention" || !("principal" in event) || event.principal !== resource.data.you) return;
    void read(new AbortController().signal)
      .then((next) => {
        if (ownKey === key()) setData<AttentionSnapshot>(ownKey, () => next);
      })
      .catch(() => undefined);
  });
  const subscribe = React.useCallback(() => {
    visits.subscribed();
    activeViews.add(ownKey);
    let sampling = false;
    let again = false;
    const ownAction = async () => {
      if (sampling) {
        again = true;
        return;
      }
      sampling = true;
      try {
        do {
          again = false;
          const visit = clientId();
          const epoch = scope.epoch;
          const controls = scope.controls.value;
          const params = new URLSearchParams({ mode: "arrival", visit });
          const next = await request<AttentionSnapshot>("GET", `/api/harness/attention?${params}`);
          if (ownKey !== key() || !activeViews.has(ownKey) || epoch !== scope.epoch) continue;
          scope.epoch++;
          scope.visit = visit;
          scope.needsDraw = false;
          scope.controls.sampled(controls);
          scope.pairKey = next.pair?.key;
          scope.snapshot = orderFor(ownKey).snapshot(next, true);
          setData<AttentionSnapshot>(ownKey, () => scope.snapshot ?? next);
        } while (again && activeViews.has(ownKey));
      } finally {
        sampling = false;
      }
    };
    const stopOwnAction = onOwnAgentAction(() => {
      void ownAction().catch(() => undefined);
    });
    const left = () => {
      scope.departed = true;
      scope.onDeparture?.();
    };
    const returned = () => {
      if (document.hidden) {
        left();
      } else {
        if (scope.departed) {
          scope.departed = false;
          scope.visit = clientId();
          scope.needsDraw = true;
          scope.epoch++;
          scope.pending = undefined;
          scope.controls.invalidate();
          orderFor(ownKey).returned();
        }
        invalidate(ownKey);
      }
    };
    document.addEventListener("visibilitychange", returned);
    window.addEventListener("focus", returned);
    window.addEventListener("pagehide", left);
    window.addEventListener("pageshow", returned);
    return () => {
      stopOwnAction();
      activeViews.delete(ownKey);
      document.removeEventListener("visibilitychange", returned);
      window.removeEventListener("focus", returned);
      window.removeEventListener("pagehide", left);
      window.removeEventListener("pageshow", returned);
      orderFor(ownKey).returned();
      visits.returned();
      orders.delete(ownKey);
    };
  }, [ownKey]);
  React.useSyncExternalStore(
    subscribe,
    () => false,
    () => false,
  );
  const departure = React.useCallback(
    (fn: () => void) => {
      scope.onDeparture = fn;
      return () => {
        if (scope.onDeparture === fn) scope.onDeparture = undefined;
      };
    },
    [scope],
  );
  return {
    ...resource,
    learningAllowed: scope.pairKey === resource.data.pair?.key && scope.controls.allowed,
    departure,
  };
}

/** Re-render only when a park expires, rather than ticking every row once a second. */
export function useParkTime(agents: AttentionSnapshot["agents"]): number {
  const deadlines = Object.values(agents)
    .flatMap((m) => (m.parked_until === undefined ? [] : [m.parked_until]))
    .sort((a, b) => a - b);
  const identity = deadlines.join(",");
  const store = React.useMemo(() => ({ now: Date.now(), identity }), [identity]);
  const subscribe = React.useCallback(
    (changed: () => void) => {
      let timer: number | undefined;
      const schedule = () => {
        const next = identity
          .split(",")
          .map(Number)
          .find((at) => at > store.now);
        if (next === undefined) return;
        // The DOM's signed 32-bit timer range; longer parks are scheduled in pieces.
        timer = window.setTimeout(
          () => {
            store.now = Date.now();
            changed();
            schedule();
          },
          Math.min(next - Date.now(), 2 ** 31 - 1),
        );
      };
      schedule();
      return () => window.clearTimeout(timer);
    },
    [identity, store],
  );
  return React.useSyncExternalStore(
    subscribe,
    () => store.now,
    () => store.now,
  );
}
