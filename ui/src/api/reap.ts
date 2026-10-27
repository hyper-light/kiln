/**
 * The reaper's policies (docs/ci.md W24-R31): what each of kiln's caches keeps, set for the system, a
 * runner, a repository or a session, and the policy in force for them together, each value with the
 * scope it came from.
 */
import { request } from "./client";

export type ReapScope = "system" | "runner" | "repo" | "session";
export type ReapKey = "floor" | "ceiling" | "target" | "keep_last" | "keep_within" | "max_age" | "schedule";
/** A policy as people write it: sizes as `20GB` or `10%`, durations as `6h` or `14d`, the schedule as an interval or a cron expression. */
export type ReapPolicy = Partial<Record<ReapKey, string | number>>;
/** One value in force: bytes for the hardware limits, a value for the rest, and the scope it came from (`null`: kiln's default). */
export interface ReapValue {
  bytes?: number;
  value?: string | number;
  from: ReapScope | null;
}
export type ReapEffective = { volume: number } & Record<ReapKey, ReapValue>;
export interface ReapSet {
  scope: ReapScope;
  target: string;
  policy: ReapPolicy;
  updated_at: number;
  updated_by: string;
}
export interface ReapView {
  policies: ReapSet[];
  effective: ReapEffective;
}
/** Which scopes a view takes together: the system's always, and a runner's, a repository's and a session's when named. */
export interface ReapAt {
  runner?: string;
  repo?: string;
  session?: string;
  volume?: number;
}

export const REAP_KEYS: ReapKey[] = ["floor", "ceiling", "target", "keep_last", "keep_within", "max_age", "schedule"];

export const REAP_LABEL: Record<ReapKey, string> = {
  floor: "Floor",
  ceiling: "Ceiling",
  target: "Target",
  keep_last: "Keep the newest",
  keep_within: "Keep what was used within",
  max_age: "Drop what's unused for",
  schedule: "Schedule",
};

export const REAP_HINT: Record<ReapKey, string> = {
  floor: "Free space the caches never push below: 20GB or 10%.",
  ceiling: "The most the caches hold: 300GiB or 25%.",
  target: "Free space a reap restores: 15%.",
  keep_last: "Per lane or key, whatever else happens, unless the floor needs them.",
  keep_within: "A duration: 1d, 6h.",
  max_age: "A duration: 30d.",
  schedule: "An interval (6h) or a cron expression (30 3 * * *), UTC.",
};

function query(at: ReapAt): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(at)) if (v !== undefined && v !== "") p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
}

export const reapPolicies = (at: ReapAt, signal?: AbortSignal) => request<ReapView>("GET", `/api/ci/work/reap/policies${query(at)}`, undefined, { signal });

export const setReapPolicy = (scope: string, policy: ReapPolicy) =>
  request<{ scope: string; policy: ReapPolicy; effective: ReapEffective }>("PUT", `/api/ci/work/reap/policies?scope=${encodeURIComponent(scope)}`, { policy });

/** The scope a change names: `server`, `runner:<name>`, `repo:<owner/name>` or `session:<id>`. */
export function scopeParam(scope: ReapScope, target: string): string {
  return scope === "system" ? "server" : `${scope}:${target}`;
}

/** The keys a scope may set: the hardware limits only for the system and a runner (W24-R31). */
export function keysOf(scope: ReapScope): ReapKey[] {
  return scope === "system" || scope === "runner" ? REAP_KEYS : REAP_KEYS.filter((k) => k !== "floor" && k !== "ceiling" && k !== "target");
}

/** The policy a form's fields make: empty ones left to other scopes, `keep_last` a number. */
export function policyOf(fields: Partial<Record<ReapKey, string>>, scope: ReapScope): ReapPolicy {
  const out: ReapPolicy = {};
  for (const k of keysOf(scope)) {
    const v = (fields[k] ?? "").trim();
    if (!v) continue;
    out[k] = k === "keep_last" && /^\d+$/.test(v) ? Number(v) : v;
  }
  return out;
}

/** The fields a scope's stored policy fills. */
export function fieldsOf(policy: ReapPolicy | undefined, scope: ReapScope): Partial<Record<ReapKey, string>> {
  return Object.fromEntries(keysOf(scope).map((k) => [k, policy?.[k] === undefined ? "" : String(policy[k])]));
}

/** A size in bytes, as people read it. */
export function bytesText(n: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${Number.isInteger(v) ? v : v.toFixed(1)} ${units[i]}`;
}

/** A value in force, as people read it. */
export function valueText(key: ReapKey, v: ReapValue | undefined): string {
  if (!v) return "—";
  if (v.bytes !== undefined) return bytesText(v.bytes);
  if (key === "keep_last" && v.value !== undefined) return `${v.value} per lane`;
  return v.value === undefined ? "—" : String(v.value);
}

/** Where a value in force came from. */
export function fromText(from: ReapScope | null): string {
  switch (from) {
    case "system":
      return "the system";
    case "runner":
      return "the runner";
    case "repo":
      return "the repository";
    case "session":
      return "the session";
    default:
      return "kiln's default";
  }
}
