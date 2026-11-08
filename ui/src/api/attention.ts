/** Mission Control attention, plan 01a199e1b177752e84dad5dc990e6a15. Kept per person on the server. */
import * as React from "react";
import { useServerEvents } from "./events";
import { request } from "./client";
import { clientId, type AgentInfo } from "./harness";
import { AttentionControls, AttentionOrder, AttentionVisits } from "@/lib/attention-order";
import { AttentionPresentation, type AttentionClose, type AttentionFrame, type AttentionPair } from "@/lib/attention-exposure";
import { onOwnAgentAction } from "@/lib/own-agent-action";
import { useAuth } from "@/lib/auth";
import { errorMessage } from "@/lib/hooks";
import { invalidate, preload, setData, useSuspenseResource } from "@/lib/resource";
import type { Res } from "@/lib/data";

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
  /** The requests a park was asked to wait through (none: left out). */
  deferred_requests?: AttentionRequest[];
  updated_seen?: number;
  archived?: boolean;
  archive_status?: "archiving" | "archived" | "restoring";
  rank?: number | null;
  /** Why its placement is what it is: why it was parked, or why it came back. */
  reason?: string;
  /** Where the learned order put it, and why. */
  rank_reason?: string;
  /** kiln parked it by itself (its reason says why): Parked marks it so, for review. */
  automatic?: boolean;
  /** You brought back a chat kiln parked by itself: a correction of the automatic park, for review. */
  correction?: boolean;
  /** When its last park ended (it came back, saying why). */
  woke_at?: number;
  revision: number;
  urgency?: AttentionUrgency;
}
export interface AttentionSettings {
  sort: "learned" | "yours";
  auto_park_days: number | null;
  archive_after_days: number;
  auto_archive: boolean;
  park_later_minutes: number;
  park_evening_hour: number;
  park_morning_hour: number;
}
export interface AttentionFit {
  observations: number;
  ignored: number;
  position_corrected: number;
  improves_baseline: boolean;
  reason: string;
  parameters: { name: string; value: number; standard_error: number | null }[];
  comparisons: AttentionComparison[];
}
/** An order's record on the picks it scored before learning from them: how many, how many it gave no chance, and its log score on the rest. */
export interface AttentionScore {
  choices: number;
  refuted: number;
  log_likelihood: number;
}
/** One order the learned one is compared with, beside the learned order's record on the same picks (none on the learned order's own row). */
export interface AttentionComparison extends AttentionScore {
  name: string;
  /** Learned enough to score a pick yet. */
  fitted: boolean;
  learned: AttentionScore | null;
}
export interface AttentionReplay {
  at: number;
  order: { agent: string; score: number | null; reason: string }[];
  model: AttentionFit;
  /** Whether the learned order made it (else your latest interaction did). */
  learned: boolean;
  /** Where your last pick at or before `at` sat in that order, when there was one. */
  pick: { agent: string; at: number; position: number; of: number } | null;
  /** What a replay can't know. */
  limitation: string;
}
export function attentionReplay(at: number, signal?: AbortSignal): Promise<AttentionReplay> {
  return request("GET", `/api/harness/attention/replay?at=${encodeURIComponent(at)}`, undefined, { signal });
}
export interface AttentionUndo {
  agent: string;
  revision: number;
}
export interface AttentionSnapshot {
  you: string;
  agents: Record<string, AttentionMark>;
  order: string[];
  request_order: string[];
  settings: AttentionSettings;
  /** What a setting starts at when it's turned on (the server's): automatic parking's days. */
  settings_defaults?: { auto_park_days: number };
  archived: AgentInfo[];
  /** A placement change's Undo (pin, unpin, park, unpark). */
  undo?: AttentionUndo | null;
  exposure: string | null;
  pair: AttentionPair | null;
  model: AttentionFit | null;
}
/** The snapshot as the list shows it: the visit's order held, what you're doing applied at once, the rows you acted on (`acted`) and how many actions you've taken (`seq`) in this visit. */
export interface AttentionView extends AttentionSnapshot {
  acted: readonly string[];
  seq: number;
  visit: string;
}
export type AttentionCommand =
  | { action: "pin" | "unpin" | "unpark" | "seen" | "archive" | "restore" | "restore_copy"; agent: string }
  | { action: "park"; agent: string; until: number }
  | { action: "undo"; agent: string; revision: number }
  | { action: "reorder"; order: string[] }
  | { action: "settings"; settings: Partial<AttentionSettings> };
export const commandAgents = (commands: readonly AttentionCommand[]): string[] => [...new Set(commands.flatMap((c) => ("agent" in c ? [c.agent] : "order" in c ? c.order : [])))];

const post = <T>(body: object) => request<T>("POST", "/api/harness/attention", body);
const sendFrame = (frame: AttentionFrame) => void post({ action: "render", ...frame }).catch(() => undefined);
const sendClose = (close: AttentionClose) => void post({ action: "close", ...close }).catch(() => undefined);

/* -------------------------------------------------- a visit left: closed with the next snapshot request */
const CLOSING = "kiln.attention.closing";
type Closing = { frame?: AttentionFrame; close: AttentionClose };
let closing: Closing[] = [];
function stored(): Closing[] {
  try {
    const raw = localStorage.getItem(CLOSING);
    const all: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(all) ? (all as Closing[]) : [];
  } catch {
    return [];
  }
}
/** Visits left (their list gone from the page, or the page hidden) whose close hasn't gone yet; one that comes back (React subscribing it again) is taken off. */
const gone = new Set<{ presentation: AttentionPresentation }>();
/** Each visit left: its last frame and its close, ready to send. */
function settle() {
  for (const scope of gone) {
    const left = scope.presentation.close(clientId());
    if (left.close) closing.push({ frame: left.frame, close: left.close });
  }
  gone.clear();
}
/** The page goes (or hides): what's left to close is kept, for the next snapshot request whenever it comes. */
function persist() {
  settle();
  if (closing.length === 0) return;
  try {
    localStorage.setItem(CLOSING, JSON.stringify([...stored(), ...closing]));
    closing = [];
  } catch {
    /* Kept in memory: this page's next arrival sends it. */
  }
}
/** Sent with an arrival's snapshot request: each visit left since, its last frame and its close. */
function sendClosing() {
  settle();
  const all = [...stored(), ...closing];
  closing = [];
  try {
    localStorage.removeItem(CLOSING);
  } catch {
    /* Nothing kept there. */
  }
  for (const c of all) {
    if (c.frame) sendFrame(c.frame);
    sendClose(c.close);
  }
}
if (typeof window !== "undefined") window.addEventListener("pagehide", persist);

/* -------------------------------------------------- the list's visits */
let session = 0;
const visits = new AttentionVisits();
interface Scope {
  key: string;
  /** The server's key for this visit's draw. */
  visit: string;
  controls: AttentionControls;
  needsDraw: boolean;
  departed: boolean;
  epoch: number;
  pairKey?: string;
  snapshot?: AttentionSnapshot;
  view?: AttentionView;
  pending?: Promise<AttentionView>;
  order: AttentionOrder;
  presentation: AttentionPresentation;
  acted: Set<string>;
  seq: number;
  /** What actions on their way show at once, applied over every snapshot until each is answered. */
  local: Map<object, (s: AttentionSnapshot) => AttentionSnapshot>;
}
const scopes = new Map<string, Scope>();
function scopeFor(ownKey: string, controls?: string): Scope {
  let scope = scopes.get(ownKey);
  if (!scope) {
    // An earlier visit's list is gone from the page: only its close (kept apart) is left of it.
    scopes.clear();
    scope = {
      key: ownKey,
      visit: clientId(),
      controls: new AttentionControls(),
      needsDraw: true,
      departed: false,
      epoch: 0,
      order: new AttentionOrder(),
      presentation: new AttentionPresentation(),
      acted: new Set(),
      seq: 0,
      local: new Map(),
    };
    scopes.set(ownKey, scope);
  }
  if (controls !== undefined) scope.controls.bind(controls);
  return scope;
}
useAuth.subscribe((now, before) => {
  if (now.kind !== before.kind || (now.kind === "authed" && before.kind === "authed" && now.me !== before.me)) {
    session++;
    picked = undefined;
    closing = [];
    gone.clear();
    scopes.clear();
    visits.returned();
  }
});
const key = () => `harness/attention/${session}/${visits.current()}`;
const arrivalKey = () => {
  visits.arrive();
  return key();
};

function viewOf(scope: Scope, snapshot: AttentionSnapshot): AttentionView {
  let shown = snapshot;
  for (const f of scope.local.values()) shown = f(shown);
  return { ...shown, acted: [...scope.acted], seq: scope.seq, visit: scope.visit };
}
function publish(scope: Scope) {
  if (!scope.snapshot) return;
  const view = viewOf(scope, scope.snapshot);
  scope.view = view;
  setData<AttentionView>(scope.key, () => view);
}

const read = (signal: AbortSignal): Promise<AttentionView> => {
  const ownKey = key();
  const scope = scopeFor(ownKey);
  if (scope.pending) return scope.pending;
  const mode = scope.needsDraw ? "arrival" : "refresh";
  if (mode === "arrival") {
    sendClosing();
    picked = undefined;
  }
  const controls = scope.controls.value;
  const epoch = scope.epoch;
  const params = new URLSearchParams({ mode, visit: scope.visit });
  const pending = request<AttentionSnapshot>("GET", `/api/harness/attention?${params}`, undefined, { signal })
    .then((next) => {
      if (ownKey !== key() || scope.epoch !== epoch) return scope.view ?? viewOf(scope, next);
      if (mode === "arrival") {
        scope.needsDraw = false;
        scope.controls.sampled(controls);
        scope.order.returned();
        scope.acted.clear();
        scope.pairKey = next.pair?.key;
      }
      scope.snapshot = scope.order.snapshot(next);
      scope.view = viewOf(scope, scope.snapshot);
      return scope.view;
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

/** How an organize action went: each Undo the server gave, and each command that failed (with why). */
export interface Organized {
  undo: AttentionUndo[];
  failed: { agent?: string; error: string }[];
}

/**
 * Your own action on rows of the list: it shows at once (`local`), goes with the frame the list shows,
 * and refreshes this visit (its key: no new draw). Only the rows it acted on move, to their places in
 * `reference` (the server's new order, or for Undo the order before), every other row staying where it
 * is. `arrival`: a new order asked for (the sort changed): this visit closes and the next is drawn.
 */
function organize(
  scope: Scope,
  commands: readonly AttentionCommand[],
  local: (s: AttentionSnapshot) => AttentionSnapshot,
  opts: { reference?: readonly string[]; arrival?: boolean } = {},
): { seq: number; done: Promise<Organized> } {
  const ids = commandAgents(commands);
  for (const id of ids) scope.acted.add(id);
  scope.seq++;
  const token = {};
  scope.local.set(token, local);
  publish(scope);
  const frame = scope.presentation.frame();
  if (frame) sendFrame(frame);
  const arrival = opts.arrival ? clientId() : undefined;
  if (arrival) {
    gone.add(scope);
    sendClosing();
  }
  const visit = scope.visit;
  const controls = scope.controls.value;
  return { seq: scope.seq, done: settleOrganized(scope, commands, ids, token, visit, controls, arrival, opts.reference) };
}
async function settleOrganized(
  scope: Scope,
  commands: readonly AttentionCommand[],
  ids: readonly string[],
  token: object,
  visit: string,
  controls: string | undefined,
  arrival: string | undefined,
  reference: readonly string[] | undefined,
): Promise<Organized> {
  const result: Organized = { undo: [], failed: [] };
  let last: AttentionSnapshot | undefined;
  for (const command of commands) {
    try {
      const next = await post<AttentionSnapshot>({ ...command, key: clientId(), mode: arrival ? "arrival" : "refresh", visit: arrival ?? visit });
      if (next.undo) result.undo.push(next.undo);
      last = next;
    } catch (e) {
      result.failed.push({ agent: "agent" in command ? command.agent : undefined, error: errorMessage(e) });
    }
  }
  scope.local.delete(token);
  if (commands.some((c) => c.action === "archive" || c.action === "restore" || c.action === "restore_copy")) invalidate("harness/agents");
  if (commands.some((c) => c.action === "settings")) invalidate("attention/");
  if (last && arrival) {
    scope.visit = arrival;
    scope.epoch++;
    scope.needsDraw = false;
    scope.controls.sampled(controls);
    scope.order.returned();
    scope.acted.clear();
    scope.pairKey = last.pair?.key;
    scope.snapshot = scope.order.snapshot(last);
  } else if (last && scope.visit === visit) {
    scope.order.place(ids, reference ?? last.order);
    scope.snapshot = scope.order.snapshot(last);
  } else {
    // Nothing came back for this visit: the server's word, read again, stands over what showed.
    invalidate(scope.key);
  }
  publish(scope);
  return result;
}

/** After your command to agents (an answer, a message…): this visit refreshed, those rows placed by their new rank. */
async function refreshActed(scope: Scope, ids: readonly string[]) {
  const visit = scope.visit;
  const params = new URLSearchParams({ mode: "refresh", visit });
  const next = await request<AttentionSnapshot>("GET", `/api/harness/attention?${params}`);
  if (scope.visit !== visit || scopes.get(scope.key) !== scope) return;
  scope.order.place(ids, next.order);
  scope.snapshot = scope.order.snapshot(next);
  publish(scope);
}

/** The list's own row picked (opened from it): the frame it was picked from and where, sent now. */
let picked: string | undefined;
function pick(scope: Scope, agent: string) {
  const { frame, selection } = scope.presentation.choose(agent);
  if (frame) sendFrame(frame);
  picked = agent;
  void post({ action: "visit", agent, source: "list", key: clientId(), mode: "refresh", ...selection }).catch(() => undefined);
}

/** An agent's page opened: a visit of its own unless the list's pick already said so (however often its page subscribes again), until another is opened or the list comes back. */
export function attentionVisit(agent: string) {
  if (picked === agent) return;
  picked = undefined;
  void post({ action: "visit", agent, source: "direct", key: clientId(), mode: "refresh" }).catch(() => undefined);
}
/** What came back while its page was open is seen. */
export function attentionSeen(agent: string) {
  void post({ action: "seen", agent, key: clientId(), mode: "refresh" }).catch(() => undefined);
}

/** No activity polling: an arrival draws the order, your own actions move what they acted on. */
export function useAttention(controls = "") {
  useAuth();
  const ownKey = arrivalKey();
  const scope = scopeFor(ownKey, controls);
  const resource = useSuspenseResource(ownKey, read);
  useServerEvents(["attention"], (event) => {
    if (event.principal !== resource.data.you) return;
    void read(new AbortController().signal)
      .then((next) => {
        if (ownKey === key()) setData<AttentionView>(ownKey, () => next);
      })
      .catch(() => undefined);
  });
  const subscribe = React.useCallback(() => {
    visits.subscribed();
    gone.delete(scope);
    const stopOwnAction = onOwnAgentAction(
      (ids) => void refreshActed(scope, ids).catch(() => undefined),
      (ids) => {
        for (const id of ids) scope.acted.add(id);
        scope.seq++;
        publish(scope);
      },
    );
    const left = () => {
      if (scope.departed) return;
      scope.departed = true;
      gone.add(scope);
      persist();
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
        }
        invalidate(scope.key);
      }
    };
    document.addEventListener("visibilitychange", returned);
    window.addEventListener("focus", returned);
    window.addEventListener("pagehide", left);
    window.addEventListener("pageshow", returned);
    return () => {
      stopOwnAction();
      document.removeEventListener("visibilitychange", returned);
      window.removeEventListener("focus", returned);
      window.removeEventListener("pagehide", left);
      window.removeEventListener("pageshow", returned);
      // Gone from the page: this visit closes with the next snapshot request.
      if (!scope.departed) gone.add(scope);
      visits.returned();
    };
  }, [scope]);
  React.useSyncExternalStore(
    subscribe,
    () => false,
    () => false,
  );
  return {
    data: resource.data,
    learningAllowed: scope.pairKey === resource.data.pair?.key && scope.controls.allowed,
    presentation: scope.presentation,
    organize: (commands: readonly AttentionCommand[], local: (s: AttentionSnapshot) => AttentionSnapshot, opts?: { reference?: readonly string[]; arrival?: boolean }) =>
      organize(scope, commands, local, opts),
    pick: (agent: string) => pick(scope, agent),
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

/* -------------------------------------------------- the Settings page's Attention group */
/** Your settings and the placements, without drawing an order (no visit). */
export const attentionSettingsRes: Res<AttentionSnapshot> = { key: "attention/settings", fetch: (signal) => request("GET", "/api/harness/attention?mode=refresh", undefined, { signal }) };
/** The learned order as it stands now: how well it predicts your picks against simpler orders. */
export const attentionModelRes: Res<AttentionReplay> = { key: "attention/model", fetch: (signal) => attentionReplay(Date.now(), signal) };
export const saveAttentionSettings = (settings: Partial<AttentionSettings>) => post<AttentionSnapshot>({ action: "settings", settings, key: clientId(), mode: "refresh" });
