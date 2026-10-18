/**
 * Observe (docs/observe.md §7): kiln's own metrics, logs, events, monitors and dashboards, under
 * /api/observe/v1. Times are unix milliseconds throughout, as the API takes and gives them.
 */
import { request } from "./client";

const BASE = "/api/observe/v1";
const enc = encodeURIComponent;

/** A query's answer: series on one grid of buckets. */
export interface QueryAnswer {
  grid: { from: number; step: number; n: number };
  series: { tags: Record<string, string>; values: (number | null)[] }[];
  scalar: number | null;
  scanned: { series: number; samples: number };
  applied: number;
  took_ms: number;
}

export interface LogRecord {
  ts_ns: number;
  status: string;
  severity: number;
  service: string;
  host: string;
  source: string;
  message: string;
  attributes: Record<string, string>;
  trace_id: string | null;
  span_id: string | null;
}

export interface LogSearch {
  records: LogRecord[];
  blocks: { total: number; pruned: number };
  rows_scanned: number;
  took_ms: number;
}

export interface LogHistogram {
  grid: { from: number; step: number; n: number };
  series: { group: Record<string, string>; values: (number | null)[] }[];
  rows_scanned: number;
  took_ms: number;
}

export interface LogFacets {
  facets: { facet: string; distinct: number; values: { value: string; count: number }[] }[];
  matched: number;
  rows_scanned: number;
  took_ms: number;
}

/** What one replica says of itself (the server's view of each machine's). */
export interface ReplicaStatus {
  member: number;
  applied: number;
  durable: number;
  series: number;
  blocks: number;
  disk_bytes: number;
  fetching: [number, number] | null;
  queued: number;
  dropped: number;
  digest_hex: string;
  verified_hex: [number, string] | null;
  error: string | null;
  at: number;
}

export interface ObserveStatus {
  role: string;
  member: number;
  leader: number;
  term: number;
  applied: number;
  durable: number;
  series: number;
  blocks: number;
  disk_bytes: number;
  log_bytes: number;
  cutoff_ms: number;
  waiting: number;
  dropped: number;
  written: number;
  voters: number[];
  learners: number[];
  machines: { machine: string; member: number; up: boolean; status: ReplicaStatus | null }[];
  digest_hex: string;
  digest_at: number;
  verified_hex: [number, string] | null;
  error: string | null;
}

export type MonitorState = "OK" | "WARN" | "ALERT" | "NO DATA";

export interface MonitorOptions {
  thresholds: { critical?: number | null; warning?: number | null; critical_recovery?: number | null; warning_recovery?: number | null };
  notify_no_data: boolean;
  no_data_timeframe?: number | null;
  renotify_interval?: number | null;
  evaluation_delay?: number | null;
  every?: number | null;
}

export interface MonitorGroup {
  group: [string, string][];
  state: MonitorState;
  value: number | null;
  since: number;
  last_data: number;
  notified: number;
}

export interface Monitor {
  id: string;
  name: string;
  query: string;
  message: string;
  tags: string[];
  priority: number | null;
  options: MonitorOptions;
  muted_until: number | null;
  created: number;
  modified: number;
  state: MonitorState;
  groups: MonitorGroup[];
}

/** A monitor's query checked: its condition, and what it watches (to draw), or why it is not one. */
export interface MonitorCheck {
  ok: boolean;
  op?: string;
  threshold?: number;
  kind?: "metric" | "change" | "logs";
  watched?: { query?: string; search?: string };
  window_ms?: number;
  error?: string;
}

/** What a monitor is made of, as it is sent. */
export type MonitorIn = Pick<Monitor, "name" | "query" | "message" | "tags" | "priority" | "options">;

export type IntegrationKind = "slack" | "discord" | "teams" | "pagerduty" | "webhook";

/** An integration as the API shows it: its secrets left out. */
export interface Integration {
  name: string;
  kind: IntegrationKind;
  url?: string;
  signed?: boolean;
}

/** An integration as it is sent: whole. */
export type IntegrationIn = { kind: "slack" | "discord" | "teams"; url: string } | { kind: "pagerduty"; routing_key: string; url?: string } | { kind: "webhook"; url: string; secret?: string };

export type WidgetKind = "timeseries" | "query_value" | "toplist" | "table" | "logs" | "events" | "monitors" | "note";

export interface Widget {
  id: string;
  title: string;
  kind: WidgetKind;
  queries: string[];
  search: string;
  text: string;
  layout: { x: number; y: number; w: number; h: number };
  options: { unit?: string; display?: "line" | "bars" | "area" } | null;
}

export interface TemplateVariable {
  name: string;
  tag: string;
  default: string;
}

export interface Dashboard {
  id: string;
  title: string;
  description: string;
  template_variables: TemplateVariable[];
  widgets: Widget[];
  builtin: boolean;
  created: number;
  modified: number;
}

export interface DashboardSummary {
  id: string;
  title: string;
  description: string;
  builtin: boolean;
  widgets: number;
  modified: number;
}

export const observeApi = {
  query: (body: { query: string; from: number; to: number; step?: number; points?: number }, signal?: AbortSignal) => request<QueryAnswer>("POST", `${BASE}/query`, body, { signal }),
  metrics: (match: string | undefined, signal?: AbortSignal) =>
    request<{ metrics: string[] }>("GET", `${BASE}/metrics${match ? `?match=${enc(match)}&limit=200` : "?limit=200"}`, undefined, { signal }),
  tagValues: (key: string, signal?: AbortSignal) => request<{ key: string; values: string[] }>("GET", `${BASE}/tags?key=${enc(key)}&limit=500`, undefined, { signal }),
  logs: (body: { query: string; from: number; to: number; limit?: number }, signal?: AbortSignal) => request<LogSearch>("POST", `${BASE}/logs/search`, body, { signal }),
  histogram: (body: { query: string; from: number; to: number; step: number; by?: string[] }, signal?: AbortSignal) => request<LogHistogram>("POST", `${BASE}/logs/aggregate`, body, { signal }),
  facets: (body: { query: string; from: number; to: number; facets: string[]; top?: number }, signal?: AbortSignal) => request<LogFacets>("POST", `${BASE}/logs/facets`, body, { signal }),
  events: (body: { query: string; from: number; to: number; limit?: number }, signal?: AbortSignal) => request<LogSearch>("POST", `${BASE}/events/search`, body, { signal }),
  status: (signal?: AbortSignal) => request<ObserveStatus>("GET", `${BASE}/status`, undefined, { signal }),
  verify: () => request<unknown>("POST", `${BASE}/verify`, {}),
  monitors: (signal?: AbortSignal) => request<{ monitors: Monitor[] }>("GET", `${BASE}/monitors`, undefined, { signal }).then((r) => r.monitors),
  monitor: (id: string, signal?: AbortSignal) => request<Monitor>("GET", `${BASE}/monitors/${enc(id)}`, undefined, { signal }),
  createMonitor: (m: MonitorIn) => request<Monitor>("POST", `${BASE}/monitors`, m),
  updateMonitor: (id: string, m: MonitorIn) => request<Monitor>("PUT", `${BASE}/monitors/${enc(id)}`, m),
  deleteMonitor: (id: string) => request<void>("DELETE", `${BASE}/monitors/${enc(id)}`),
  validateMonitor: (query: string, signal?: AbortSignal) => request<MonitorCheck>("POST", `${BASE}/monitors/validate`, { query }, { signal }),
  mute: (id: string, until?: number) => request<Monitor>("POST", `${BASE}/monitors/${enc(id)}/mute`, until ? { until } : {}),
  unmute: (id: string) => request<Monitor>("POST", `${BASE}/monitors/${enc(id)}/unmute`, {}),
  integrations: (signal?: AbortSignal) => request<{ integrations: Integration[] }>("GET", `${BASE}/integrations`, undefined, { signal }).then((r) => r.integrations),
  putIntegration: (name: string, i: IntegrationIn) => request<void>("PUT", `${BASE}/integrations/${enc(name)}`, i),
  deleteIntegration: (name: string) => request<void>("DELETE", `${BASE}/integrations/${enc(name)}`),
  testIntegration: (name: string) => request<unknown>("POST", `${BASE}/integrations/${enc(name)}/test`, {}),
  dashboards: (signal?: AbortSignal) => request<{ dashboards: DashboardSummary[] }>("GET", `${BASE}/dashboards`, undefined, { signal }).then((r) => r.dashboards),
  dashboard: (id: string, signal?: AbortSignal) => request<Dashboard>("GET", `${BASE}/dashboards/${enc(id)}`, undefined, { signal }),
  createDashboard: (d: Omit<Dashboard, "id" | "builtin" | "created" | "modified">) => request<Dashboard>("POST", `${BASE}/dashboards`, d),
  updateDashboard: (d: Dashboard) => request<Dashboard>("PUT", `${BASE}/dashboards/${enc(d.id)}`, d),
  deleteDashboard: (id: string) => request<void>("DELETE", `${BASE}/dashboards/${enc(id)}`),
};

/**
 * A template variable's value in a query: `$host` stands for `host:<value>`, or for `any` (`*` in a
 * metric query's filter; nothing in a log search) when it is `*`.
 */
export function fillVariables(query: string, vars: TemplateVariable[], values: Record<string, string>, any = "*"): string {
  let q = query;
  // Longest names first, so `$hostname` is not read as `$host` and `name`.
  for (const v of [...vars].sort((a, b) => b.name.length - a.name.length)) {
    const value = values[v.name] ?? v.default;
    q = q.split(`$${v.name}`).join(!value || value === "*" ? any : `${v.tag}:${/^[\w.\-/:]+$/.test(value) ? value : `"${value.replace(/["\\]/g, "\\$&")}"`}`);
  }
  return q;
}
