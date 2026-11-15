/** Agent definitions (`agentdefs.*`, /api/agentdefs; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P5.1, P7.1). */
import type { Res } from "@/lib/data";
import { request } from "./client";

/** A measure of one variant: a 95% interval of its median and the median, [low, mid, high]. */
export type Interval = [number, number, number];

/** A variant: a deployment new sessions start in. Its id is never shown: it reads as its swatch and its name. */
export interface Variant {
  id: string;
  /** The name a person gave it; none: what sets it apart from the others (definitions.tsx variantName). */
  name: string | null;
  agentfile: string;
  engine: "microvm" | "host";
  cpus: number;
  memory: number;
  /** Percent of new sessions it takes; the variants' weights sum to 100. */
  weight: number;
  sessions: number;
  /** What the target runs on now. */
  current?: boolean;
  measures: { warm_ms: Interval; peak_gib: Interval; failed_pct: Interval };
}

export interface Experiment {
  id: string;
  state: "live" | "paused" | "promoted";
  version: number;
  sessions: number;
  variants: Variant[];
}

export interface AgentDef {
  id: string;
  name: string;
  /** Yours; kiln's are read only. */
  own: boolean;
  /** kiln's: one per harness, one per CI skill, or one its UI starts. */
  kind?: "harness" | "skill" | "ui";
  harness: string;
  model: string;
  agentfile: string;
  version: number;
  instructions: string;
  tools: number;
  mcp: number;
  skills: number;
  limit: string;
  /** Where its sessions run and how fast they start: its engine, warm hosts kept of the target, start p50 by kind (ms). */
  runs: { engine: "microvm" | "host"; warm: number; target: number; p50_ms: { warm?: number; template?: number; cold?: number } };
  /** Sessions started from it: each a harness session's id, the version it started on and its variant's id. */
  sessions: { id: string; version: number; variant?: string }[];
  experiment?: Experiment;
  /** Its Agentfile's AGENT lines and it changed apart: shown, never settled for you. */
  conflict?: { field: string; here: string; there: string; from: string };
  /** kiln's UI agent yours stands in for. */
  replaces?: string;
  /** Yours that stands in for this one of kiln's. */
  replaced_by?: string;
}

export const agentdefsApi = {
  list: (signal?: AbortSignal) => request<{ agentdefs: AgentDef[] }>("GET", "/api/agentdefs", undefined, { signal }).then((r) => r.agentdefs),
};

export const agentdefsRes: Res<AgentDef[]> = { key: "agentdefs/list", fetch: (s) => agentdefsApi.list(s) };
