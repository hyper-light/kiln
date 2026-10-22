/** Multi-stack programs (docs/programs.md): a master plan and its tracks, run as goals on a coordinator. */
import { request } from "./client";

/** What a program's usage allows (the master plan's §5): above 80% of a weekly window no new goal starts, above 95% the coordinator's goals pause. */
export type ProgramRule = { rule: "ok" } | { rule: "hold"; why: string } | { rule: "pause"; why: string };

/** The share of a weekly window an account has used. */
export interface ProgramUsage {
  pct: number;
  account?: string;
  /** Unix ms it starts over. */
  resets_at?: number;
}

/** A session's pause: at a usage limit (`limit`) or its threshold (`threshold`). */
export interface ProgramPause {
  reason: string;
  /** The window (`five_hour`, `seven_day`, …) and its label (`Session`, `Weekly`). */
  window?: string;
  label?: string;
  /** Unix ms the window starts over. */
  until?: number;
}

export interface ProgramAgent {
  id: string;
  name: string;
  status: string;
  /** Its session's pause, when it's paused. */
  pause?: ProgramPause;
  /** Its threshold: its session pauses once a window is this full. */
  pause_at?: number;
  /** Its goal stack's state (`running`, `paused`, `done`) and why, when it has one. */
  stack?: string;
  reason?: string;
  usage?: ProgramUsage;
}

export interface ProgramGoal {
  /** Its id in the stack it's kept in. */
  id: string;
  /** `CP-T7.1`. */
  key?: string;
  text: string;
  /** `active` (under way) or `pending` (to come). */
  status: string;
  /** The agent it's under way on, and its name: a goal of the coordinator's, handed to the track's worker. */
  on?: string;
  on_name?: string;
}

/** A track's change request: `built` (open for review), `merged` (landed) or `closed`. */
export interface ProgramChange {
  title: string;
  url: string;
  branch: string;
  change?: string;
  state: string;
  checks?: string;
}

/** A track's slot: at work, next for a free slot, waiting, held for usage, its worker paused, done. */
export type TrackSlot = "running" | "next" | "waiting" | "held" | "paused" | "done";

export interface ProgramTrack {
  /** `T7`. */
  id: string;
  name: string;
  /** Its plan's id. */
  plan: string;
  weight: number;
  /** Its worker, as assigned (`assigned`: its id, whether kiln knows the agent now or not). */
  worker: ProgramAgent | null;
  assigned?: string;
  goal: ProgramGoal | null;
  met: number;
  total: number;
  /** Each goal met adds 1/weight: the lowest pass gets the next free slot. */
  pass: number;
  slot: TrackSlot;
  /** The threshold kiln holds its worker to: 95, or 80 for the lowest-weight track at work. */
  wants?: number;
  changes: ProgramChange[];
}

/** What the coordinator was last told of the settings, and whether kiln queued it for its session (its machine took it). */
export interface ProgramNotice {
  text: string;
  at: number;
  delivered: boolean;
  /** The message's id, once delivered. */
  message?: string;
  /** Why it isn't yet (kiln tries again every 15 s). */
  why?: string;
}

export interface ProgramSettings {
  slots: number | null;
  weights: Record<string, number>;
  /** The weights were seeded from the master plan's tracks table, once: from then on they're only the person's. */
  seeded: boolean;
  updated_at: number;
  by: string;
  notice?: ProgramNotice;
}

export interface Program {
  /** Its master plan's id. */
  id: string;
  title: string;
  tag: string;
  /** The agent its goals run on, as kiln knows it now (null: its session isn't listed any more), and its id. */
  coordinator: ProgramAgent | null;
  coordinator_id?: string;
  slots: number;
  running: number;
  settings: ProgramSettings;
  usage: ProgramUsage | null;
  rule: ProgramRule;
  met: number;
  total: number;
  /** The coordinator's goal under way. */
  goal: ProgramGoal | null;
  tracks: ProgramTrack[];
}

/** What the person changes: the slots, and tracks' weights. */
export interface ProgramChangeSettings {
  slots?: number;
  weights?: Record<string, number>;
}

export const programsApi = {
  list: (signal?: AbortSignal) => request<{ programs: Program[] }>("GET", "/api/programs", undefined, { signal }).then((r) => r.programs),
  set: (id: string, body: ProgramChangeSettings) => request<Program>("PUT", `/api/programs/${encodeURIComponent(id)}/settings`, body),
  /** A track's worker (`agent`: an agent's id, null to unassign). */
  assign: (id: string, track: string, agent: string | null) => request<Program>("PUT", `/api/programs/${encodeURIComponent(id)}/tracks/${encodeURIComponent(track)}`, { agent }),
};
