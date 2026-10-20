/**
 * Observe (docs/observe.md §7): kiln's own metrics, logs, events, monitors and dashboards, under
 * /api/observe/v1. Times are unix milliseconds throughout, as the API takes and gives them.
 */
import { request } from "./client";

const BASE = "/api/observe/v1";
const enc = encodeURIComponent;

/** A query's answer: series on one grid of buckets. */
/** An `anomalies()` series' band: each bucket's bounds and score (its error in calibrated deviations). */
/** A series' band: an `anomalies()` one's (each bucket's score) or a `forecast()` one's (from
 * `forecast_from` on, the values are forecast, not data). */
export interface Bounds {
  lower: (number | null)[];
  upper: (number | null)[];
  score: (number | null)[];
  k: number;
  direction: "both" | "above" | "below";
  algorithm: "basic" | "agile" | "robust" | "linear" | "seasonal";
  forecast_from?: number | null;
}

export interface QueryAnswer {
  /** `times`, `calendar`: a calendar rollup's, each bucket's start (ms) and the period. */
  grid: { from: number; step: number; n: number; times?: number[]; calendar?: "daily" | "weekly" | "monthly" };
  /** `outlier`: an `outliers()` series', whether it is one of its group over the range. */
  series: { tags: Record<string, string>; values: (number | null)[]; bounds?: Bounds; outlier?: boolean }[];
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

/** A search's records grouped by their template (Drain): the most common first. */
export interface LogPattern {
  /** The template, its variables `<*>`. */
  template: string;
  count: number;
  first_ns: number;
  last_ns: number;
  /** Its newest record's message. */
  sample: string;
  statuses: Record<string, number>;
}

export interface LogPatterns {
  patterns: LogPattern[];
  /** The newest records they were found in. */
  records: number;
  rows_scanned: number;
  took_ms: number;
}

/** What a live tail says each tick (200 ms), or once to refuse: the tick's records (a uniform sample
 * past the rate asked), and how many rows it read, matched and missed (the tail fell behind). */
export type LogTailMessage = { type: "records"; records: LogRecord[]; read: number; matched: number; missed: number; ms: number } | { type: "error"; message: string };

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

/** What a composite monitor reads: its monitors, each with its letter, and its condition. */
export interface CompositeOf {
  monitors: { letter: string; id: string; name: string | null; state: MonitorState | null }[];
  /** The condition in letters (`a && b`). */
  expression: string;
  /** The condition with the monitors' names. */
  named: string;
}

export interface Monitor {
  id: string;
  name: string;
  /** `composite` for a composite (its query an expression of monitors' ids); none for the rest. */
  type?: string | null;
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
  composite?: CompositeOf;
  /** The downtimes that hold it now, or will. */
  downtimes?: Downtime[];
}

/** One recurrence of a downtime: an RFC 5545 rule, its first wall-clock start in the schedule's timezone, how long each lasts. */
export interface DowntimeRecurrence {
  rrule: string;
  start: string;
  duration: string;
}

/** A downtime as it is sent (Datadog's v2): the monitors it holds (one, or every one with these tags), the groups (a scope), when. */
export interface DowntimeIn {
  message?: string;
  monitor_identifier: { monitor_id?: string; monitor_tags?: string[] };
  scope?: string;
  schedule: { start?: number; end?: number } | { recurrences: DowntimeRecurrence[]; timezone?: string };
  mute_first_recovery_notification?: boolean;
  notify_end_states?: ("alert" | "no data" | "warn")[];
  notify_end_types?: ("canceled" | "expired")[];
}

export type DowntimeStatus = "active" | "scheduled" | "ended" | "canceled";

/** A downtime as the API shows it: its status, and its window now or next. */
export type Downtime = DowntimeIn & { id: string; canceled?: number; created: number; modified: number; status: DowntimeStatus; window: { start: number; end: number | null } | null };

/** A monitor's query checked: its condition, and what it watches (to draw), or why it is not one. */
export interface MonitorCheck {
  ok: boolean;
  op?: string;
  threshold?: number;
  kind?: "metric" | "change" | "logs" | "composite" | "forecast";
  watched?: { query?: string; search?: string };
  composite?: CompositeOf;
  window_ms?: number;
  error?: string;
}

/** What a monitor is made of, as it is sent. */
export type MonitorIn = Pick<Monitor, "name" | "query" | "message" | "tags" | "priority" | "options"> & { type?: string };

export type IntegrationKind = "slack" | "discord" | "teams" | "pagerduty" | "webhook" | "opsgenie" | "email";

/** How an SMTP relay is reached: STARTTLS (587), TLS from the first byte (465), or neither (25). */
export type SmtpSecurity = "starttls" | "tls" | "none";

/** An integration as the API shows it: its secrets left out. */
export interface Integration {
  name: string;
  kind: IntegrationKind;
  url?: string;
  signed?: boolean;
  /** An email's recipients, sender and relay. */
  to?: string[];
  from?: string;
  host?: string;
  port?: number;
  security?: SmtpSecurity;
  username?: string;
}

/** An integration as it is sent: whole. */
export type IntegrationIn =
  | { kind: "slack" | "discord" | "teams"; url: string }
  | { kind: "pagerduty"; routing_key: string; url?: string }
  | { kind: "webhook"; url: string; secret?: string }
  | { kind: "opsgenie"; api_key: string; url?: string }
  | { kind: "email"; to: string[]; from: string; host: string; port?: number; security?: SmtpSecurity; username?: string; password?: string; ca?: string };

/** What a metric is and costs: its series (its cardinality) by kind, each tag's values, its samples. */
export interface MetricSummary {
  name: string;
  series: number;
  kinds: { kind: "gauge" | "count" | "counter"; series: number }[];
  /** The tag of most values first; a metric's own summary has each tag's values of most series. */
  tags: { key: string; values: number; series: number; top?: { value: string; series: number }[] }[];
  /** Samples applied since the server started; of late a second (none until it can say). */
  samples: number;
  samples_per_s: number | null;
  last_ms: number | null;
}

/** A synthetic test's kind (docs/observe.md §10.4). */
export type SyntheticType = "http" | "tcp" | "dns" | "tls";
export type SyntheticAssertionType = "status_code" | "response_time" | "header" | "body" | "certificate_days" | "address";
export type SyntheticOperator = "is" | "is_not" | "less_than" | "more_than" | "contains" | "does_not_contain" | "matches" | "does_not_match";

export interface SyntheticAssertion {
  type: SyntheticAssertionType;
  operator: SyntheticOperator;
  /** A header assertion's header. */
  property?: string;
  /** A number (status code, ms, days) or text. */
  target: number | string;
}

/** A test that alerts owns a monitor: at least `min_locations` failing for `for_s`. */
export interface SyntheticAlert {
  message?: string;
  min_locations?: number;
  for_s?: number;
  priority?: number | null;
}

/** A synthetic test as it is sent. A bearer token left out (or "…") is the one kept. */
export interface SyntheticIn {
  type: SyntheticType;
  url?: string;
  method?: string;
  headers?: [string, string][];
  body?: string;
  follow_redirects?: boolean;
  bearer?: string;
  ca?: string;
  host?: string;
  port?: number;
  assertions?: SyntheticAssertion[];
  /** `server`, runners by name, or `*` (every machine). */
  locations?: string[];
  every_s?: number;
  timeout_s?: number;
  retries?: number;
  retry_ms?: number;
  tags?: string[];
  paused?: boolean;
  alert?: SyntheticAlert;
}

/** How a location's runs of a test go. Times are unix ms; the last run's took_us in µs. */
export interface SyntheticState {
  passed: boolean | null;
  last_ms?: number | null;
  took_us: number;
  status?: number | null;
  certificate_minutes?: number | null;
  attempts: number;
  failed: string[];
  runs: number;
  failures: number;
}

export type Synthetic = SyntheticIn & { name: string; monitor_id?: string; machines: { machine: string; state: SyntheticState }[] };

/** One run, its parts in ms. */
export interface SyntheticOutcome {
  passed: boolean;
  failures: string[];
  total_ms: number;
  dns_ms: number | null;
  connect_ms: number | null;
  tls_ms: number | null;
  firstbyte_ms: number | null;
  download_ms: number | null;
  redirect_ms: number | null;
  status: number | null;
  size: number | null;
  certificate_days: number | null;
  addresses: string[];
  attempts: number;
}

export type CloudKind = "aws" | "gcp" | "azure";

/** What a cloud integration is doing (docs/observe.md §10.1). Times are unix ms. */
export interface CloudStatus {
  state: "starting" | "ok" | "failing" | "paused";
  error?: string;
  warnings?: string[];
  last_pull: number | null;
  last_ok: number | null;
  next_pull: number | null;
  series: number;
  over: number;
  points: number;
  deferred: number;
  calls: number;
  took_ms: number;
  pulls: number;
  failures: number;
  /** The last pull's log records written (each once), those left to the next pull, and every one since the server started. */
  records: number;
  records_deferred: number;
  records_total: number;
  /** What is read as it comes: an Azure integration's event hubs, each one's. */
  streams?: CloudStream[];
}

/** An event hub read as its events come (docs/observe.md §10.3). */
export interface CloudStream {
  hub: string;
  state: "starting" | "reading" | "failing";
  error?: string;
  warnings?: string[];
  partitions: number;
  /** Events not yet read, every partition's. */
  lag: number;
  events: number;
  records: number;
  /** Metric records passed over (Azure Monitor's metrics are pulled). */
  skipped: number;
  last_ms?: number | null;
}

/** An Azure integration's event hubs. */
export interface EventHubs {
  /** Its name, or its host. */
  namespace: string;
  hubs: string[];
  /** With Listen rights; "…" when kept; empty removes it (the integration's credentials then). */
  connection_string?: string;
  endpoint?: string;
  tls?: boolean;
}

interface CloudCommon {
  interval_s?: number;
  lookback_s?: number;
  tags?: string[];
  max_series?: number;
  paused?: boolean;
}

/**
 * A cloud integration as it is sent. A secret left out (or "…", as shown) is the one kept; an empty
 * one removes it.
 */
export type CloudIn = (
  | {
      kind: "aws";
      regions?: string[];
      namespaces?: string[];
      stats?: string[];
      period_s?: number;
      access_key_id?: string;
      secret_access_key?: string;
      role_arn?: string;
      external_id?: string;
      endpoint?: string;
      sts_endpoint?: string;
      /** Log groups whose events are read: names, or prefixes ending in `*`. */
      log_groups?: string[];
      log_filter?: string;
      logs_endpoint?: string;
    }
  | {
      kind: "gcp";
      projects?: string[];
      services?: string[];
      period_s?: number;
      service_account?: string;
      endpoint?: string;
      /** Log entries read: a filter in Cloud Logging's query language; empty, every entry; left out, no logs. */
      logs?: string;
      logs_endpoint?: string;
    }
  | {
      kind: "azure";
      tenant_id?: string;
      client_id?: string;
      client_secret?: string;
      subscriptions?: string[];
      resource_types?: string[];
      aggregations?: string[];
      period_s?: number;
      authority?: string;
      endpoint?: string;
      /** Log Analytics workspaces whose rows are read (their ids), each `log_query`'s (a table or KQL; every table unless said). */
      workspaces?: string[];
      log_query?: string;
      logs_endpoint?: string;
      /** Azure Monitor's metrics read (unless false: only logs). */
      metrics?: boolean;
      event_hubs?: EventHubs;
    }
) &
  CloudCommon;

/** A cloud integration as the API shows it: its secrets "…", its status. */
export type Cloud = CloudIn & { name: string; status: CloudStatus };

/** How a machine's scrapes of a target go (`up`: none before its first). */
export interface ScrapeState {
  up: boolean | null;
  last_ms: number | null;
  took_ms: number;
  samples: number;
  skipped: number;
  deferred: number;
  error: string | null;
  scrapes: number;
  failures: number;
}

/** A Prometheus scrape target as it is given: `on` is `server`, a runner's name, or `*` (every machine, each its own). */
export interface ScrapeIn {
  url: string;
  every_s?: number;
  timeout_s?: number;
  on?: string;
  tags?: string[];
  /** Left out: the one kept. */
  bearer?: string;
}

/** A scrape target as the API shows it: whether it has a token, and how each machine that reads it goes. */
export interface Scrape {
  name: string;
  url: string;
  every_s: number;
  timeout_s: number;
  on: string;
  tags: string[];
  bearer: boolean;
  machines: { machine: string; state: ScrapeState }[];
}

/** What a target would give, read once from the server and nothing written. */
export interface ScrapeTest {
  ok: boolean;
  error?: string;
  format?: "openmetrics" | "text";
  samples?: number;
  skipped?: number;
  series?: number;
  metrics?: number;
  sample?: string[];
  took_ms?: number;
}

/** How a machine's DogStatsD listener goes. */
export interface StatsdState {
  listening: string[];
  rcvbuf: number;
  datagrams: number;
  metrics: number;
  events: number;
  checks: number;
  invalid: number;
  refused: number;
  conflicts: number;
  contexts: number;
  points: number;
  dropped: number;
  held: number;
  last_ms: number | null;
  error: string | null;
}

export type HistogramAggregate = "max" | "median" | "avg" | "count" | "min" | "sum";

/** A DogStatsD listener as it is given: where it listens (`udp://127.0.0.1:8125`, `unix:///…`), who listens. */
export interface StatsdIn {
  listen: string[];
  tags?: string[];
  histogram_aggregates?: HistogramAggregate[];
  histogram_percentiles?: number[];
  on?: string;
}

/** A DogStatsD listener as the API shows it, and how each machine that opens it goes. */
export type Statsd = StatsdIn & { name: string; on: string; machines: { machine: string; state: StatsdState }[] };

export type LogSourceKind = "file" | "kubernetes" | "docker" | "journald" | "syslog" | "unified_log" | "windows_event";
export type LogRuleKind = "exclude_at_match" | "include_at_match" | "mask_sequences" | "multi_line";

/** Datadog's processing rules, in their order (a multi_line rule first). */
export interface LogRule {
  type: LogRuleKind;
  name?: string;
  pattern: string;
  replace_placeholder?: string;
}

/** Logs machines collect (docs/observe.md §8.1): files by their globs, a node's pods' or Docker's containers' logs, or the systemd journal. `on` as a scrape target's. */
export interface LogSourceIn {
  type: LogSourceKind;
  paths?: string[];
  exclude?: string[];
  format?: "auto" | "text" | "json" | "cri" | "docker";
  start?: "end" | "beginning";
  encoding?: "utf-8" | "utf-16le" | "utf-16be";
  units?: string[];
  /** A syslog source's: where it listens (`udp://0.0.0.0:514`), the zone of RFC 3164's times. */
  listen?: string[];
  timezone?: string;
  /** macOS's unified log's: an NSPredicate of its events, and the level read. */
  predicate?: string;
  level?: "default" | "info" | "debug";
  /** The Windows Event Log's: its channels, and an XPath of their events (`*[...]`). */
  channels?: string[];
  query?: string;
  service?: string;
  source?: string;
  tags?: string[];
  rules?: LogRule[];
  on?: string;
}

/** How a machine's reading of a source goes, its counts since `since_ms` (when its process began reading). */
export interface LogSourceState {
  since_ms: number | null;
  files: number;
  over_limit: number;
  bytes: number;
  records: number;
  in_flight: number;
  excluded: number;
  truncated: number;
  lag_bytes: number;
  /** A syslog source's TCP connections open, and datagrams dropped for want of room. */
  connections: number;
  dropped: number;
  last_ms: number | null;
  error: string | null;
  errors: number;
}

export type LogSource = LogSourceIn & {
  name: string;
  format: NonNullable<LogSourceIn["format"]>;
  start: NonNullable<LogSourceIn["start"]>;
  on: string;
  machines: { machine: string; state: LogSourceState }[];
};

/** What a source reads on the server: the files it matches and the records of the newest one's last lines, nothing written. */
export interface LogSourceTest {
  ok: boolean;
  files: { path: string; bytes: number }[];
  matched: number;
  records: { ts_ns: number; status: string; service: string; host: string; source: string; message: string; attributes: Record<string, string> }[];
  excluded: number;
  error?: string | null;
  took_ms: number;
}

export type ProcessorType =
  | "grok-parser"
  | "date-remapper"
  | "status-remapper"
  | "service-remapper"
  | "message-remapper"
  | "trace-id-remapper"
  | "attribute-remapper"
  | "url-parser"
  | "user-agent-parser"
  | "category-processor"
  | "arithmetic-processor"
  | "string-builder-processor"
  | "lookup-processor"
  | "pipeline";

/** A pipeline's processor, Datadog's shape: its type and that type's fields (docs/observe.md §8.2). */
export interface Processor {
  type: ProcessorType;
  name?: string;
  is_enabled?: boolean;
  source?: string;
  samples?: string[];
  grok?: { match_rules: string; support_rules?: string };
  sources?: string[];
  target?: string;
  target_format?: "auto" | "string" | "integer" | "double";
  preserve_source?: boolean;
  override_on_conflict?: boolean;
  normalize_ending_slashes?: boolean;
  is_encoded?: boolean;
  categories?: { filter: { query: string }; name: string }[];
  expression?: string;
  template?: string;
  is_replace_missing?: boolean;
  lookup_table?: string[];
  default_lookup?: string | null;
  filter?: { query: string };
  processors?: Processor[];
}

/** A log pipeline as it is sent: its filter (a search; empty, every record) and its processors in order. */
export interface PipelineIn {
  name: string;
  is_enabled: boolean;
  filter: { query: string };
  processors: Processor[];
}

/** A pipeline as the API shows it; kiln's own (`builtin`) read-only but for whether it is on. */
export type Pipeline = PipelineIn & { id: string; builtin: boolean; created?: number; modified?: number };

/** A metric from logs as it is sent (Datadog's definition): what it computes, of which records, grouped by what. */
export interface LogMetricIn {
  name: string;
  compute: { aggregation_type: "count" | "distribution"; path?: string; include_percentiles?: boolean };
  filter: { query: string };
  group_by: { path: string; tag_name?: string }[];
}

/** What a metric's latest window with records of it held. */
export interface LogMetricStatus {
  window: number;
  records: number;
  groups: number;
  overflowed: number;
}

export type LogMetric = LogMetricIn & { created: number; modified: number; status: LogMetricStatus | null };

/** How the server's counting goes: when a window is first counted (learned), and what came late. */
export interface LogMetricCounting {
  settle_ms: number;
  windows_counted: number;
  windows_recounted: number;
  windows_expired: number;
  windows_waiting: number;
  late_batches: number;
  rows_scanned: number;
  count_ms: number;
  error: string | null;
}

/** What a definition counts of the records held, window by window. */
export interface LogMetricTry {
  series: { metric: string; tags: Record<string, string>; points: [number, number][] }[];
  records: number;
  rows_scanned: number;
}

/** A record to try pipelines on, and what one makes of it. */
export interface PipelineSample {
  message: string;
  service?: string;
  host?: string;
  source?: string;
  status?: string;
  attributes?: Record<string, string>;
}

export interface PipelineResult {
  ts_ns: number;
  status: string;
  service: string;
  host: string;
  source: string;
  message: string;
  attributes: Record<string, string>;
  trace_id: string | null;
}

/** An SLO as it is sent (docs/observe.md §9.2): a ratio of good events to total, or time slices meeting a condition. */
export interface SloIn {
  name: string;
  description?: string;
  type: "ratio" | "slices";
  /** A ratio's queries, of counts. */
  good?: string;
  total?: string;
  /** A time slice SLO's query, its condition (`< 0.3`) and its slice (60 or 300 s). */
  query?: string;
  condition?: string;
  slice_s?: 60 | 300;
  /** Percent: 99.9. */
  target: number;
  warning?: number;
  timeframe: "7d" | "30d" | "90d";
  tags?: string[];
}

export type SloState = "OK" | "WARN" | "BREACHED" | "NO DATA" | "INVALID";

/** An SLO's standing, read from the store when asked. */
export interface SloStanding {
  state: SloState;
  sli?: number | null;
  good?: number;
  total?: number;
  /** Percent of the error budget left (below 0 once spent). */
  budget_left?: number | null;
  /** Over the last 1h, 6h, 1d and 3d: the error rate over the budget's. */
  burn_rates?: Record<string, number | null>;
  /** Each UTC day the timeframe touches (the SLO's own page). */
  days?: { day: number; sli: number | null; budget_left: number | null }[];
  error?: string;
}

export type Slo = SloIn & {
  id: string;
  created: number;
  modified: number;
  standing: SloStanding;
  /** The monitors that alert on it (its own page). */
  alerts?: { id: string; name: string; query: string; state: MonitorState }[];
};

/** What a configuration would read, pulled once and nothing written. */
export interface CloudTest {
  ok: boolean;
  error?: string;
  series?: number;
  over?: number;
  points?: number;
  /** The log records the lookback holds, each once, and a few of the newest. */
  records?: number;
  records_sample?: string[];
  calls?: number;
  took_ms?: number;
  metrics?: number;
  sample?: string[];
  warnings?: string[];
}

export type WidgetKind = "timeseries" | "query_value" | "toplist" | "table" | "heatmap" | "distribution" | "logs" | "events" | "monitors" | "note";

/** A query's heatmap: each row's `[lo, hi]`, and each bucket of time's count in each row (a
 * distribution's rows its buckets). */
export interface HeatmapAnswer {
  grid: { from: number; step: number; n: number; times?: number[] };
  heatmap: { rows: [number, number][]; counts: number[][]; distribution: boolean };
  applied: number;
  took_ms: number;
}

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

/** A span as a search gives one (docs/observe.md §8.7): its message is its name (Datadog's resource). */
export interface Span {
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

export interface SpanSearch {
  spans: Span[];
  blocks: { total: number; pruned: number };
  rows_scanned: number;
  took_ms: number;
}

/** A trace's span, placed as its waterfall draws it. */
export interface PlacedSpan {
  ts_ns: number;
  offset_ns: number;
  duration_ns: number;
  depth: number;
  parent: number | null;
  status: string;
  service: string;
  host: string;
  source: string;
  resource_name: string;
  attributes: Record<string, string>;
  trace_id: string | null;
  span_id: string | null;
}

export interface Trace {
  trace_id: string;
  start_ns: number;
  end_ns: number;
  duration_ns: number;
  root: { service: string; resource_name: string } | null;
  spans_count: number;
  truncated: boolean;
  errors: number;
  roots: number;
  services: { service: string; spans: number }[];
  spans: PlacedSpan[];
  took_ms: number;
}

/** What a group's requests (or calls) came to over a range, from the trace metrics. */
export interface Red {
  hits: number;
  errors: number;
  error_rate: number | null;
  avg_s: number | null;
  p50_s: number | null;
  p95_s: number | null;
  p99_s: number | null;
  max_s: number | null;
}

export type ServiceRow = Red & { service: string };

export interface Services {
  from: number;
  to: number;
  env: string | null;
  envs: string[];
  services: ServiceRow[];
}

export interface ServiceDetail {
  service: string;
  from: number;
  to: number;
  env: string | null;
  summary: ServiceRow | null;
  kinds: (Red & { "span.kind": string })[];
  resources: (Red & { resource_name: string })[];
  calls: (Red & { "peer.service": string })[];
}

export interface ServiceMap {
  from: number;
  to: number;
  nodes: { service: string; hits: number; errors: number; inferred: boolean }[];
  edges: { from: string; to: string; calls: number; errors: number; avg_s: number | null; inferred: boolean }[];
  spans_read: number;
  sampled_one_in: number;
  took_ms: number;
}

const rangeQuery = (from: number, to: number, env?: string) => `from=${Math.floor(from)}&to=${Math.ceil(to)}${env && env !== "*" ? `&env=${enc(env)}` : ""}`;

export const observeApi = {
  spans: (body: { query: string; from: number; to: number; limit?: number }, signal?: AbortSignal) => request<SpanSearch>("POST", `${BASE}/spans/search`, body, { signal }),
  spanHistogram: (body: { query: string; from: number; to: number; step: number; by?: string[] }, signal?: AbortSignal) => request<LogHistogram>("POST", `${BASE}/spans/aggregate`, body, { signal }),
  spanFacets: (body: { query: string; from: number; to: number; facets: string[]; top?: number }, signal?: AbortSignal) => request<LogFacets>("POST", `${BASE}/spans/facets`, body, { signal }),
  trace: (id: string, signal?: AbortSignal) => request<Trace>("GET", `${BASE}/traces/${enc(id)}`, undefined, { signal }),
  services: (from: number, to: number, env: string | undefined, signal?: AbortSignal) => request<Services>("GET", `${BASE}/apm/services?${rangeQuery(from, to, env)}`, undefined, { signal }),
  service: (name: string, from: number, to: number, env: string | undefined, signal?: AbortSignal) =>
    request<ServiceDetail>("GET", `${BASE}/apm/services/${enc(name)}?${rangeQuery(from, to, env)}`, undefined, { signal }),
  serviceMap: (from: number, to: number, env: string | undefined, signal?: AbortSignal) => request<ServiceMap>("GET", `${BASE}/apm/map?${rangeQuery(from, to, env)}`, undefined, { signal }),
  query: (body: { query: string; from: number; to: number; step?: number; points?: number }, signal?: AbortSignal) => request<QueryAnswer>("POST", `${BASE}/query`, body, { signal }),
  heatmap: (body: { query: string; from: number; to: number; points?: number; heatmap: number }, signal?: AbortSignal) => request<HeatmapAnswer>("POST", `${BASE}/query`, body, { signal }),
  metrics: (match: string | undefined, signal?: AbortSignal) =>
    request<{ metrics: string[] }>("GET", `${BASE}/metrics${match ? `?match=${enc(match)}&limit=200` : "?limit=200"}`, undefined, { signal }),
  tagValues: (key: string, signal?: AbortSignal) => request<{ key: string; values: string[] }>("GET", `${BASE}/tags?key=${enc(key)}&limit=500`, undefined, { signal }),
  logs: (body: { query: string; from: number; to: number; limit?: number }, signal?: AbortSignal) => request<LogSearch>("POST", `${BASE}/logs/search`, body, { signal }),
  histogram: (body: { query: string; from: number; to: number; step: number; by?: string[] }, signal?: AbortSignal) => request<LogHistogram>("POST", `${BASE}/logs/aggregate`, body, { signal }),
  facets: (body: { query: string; from: number; to: number; facets: string[]; top?: number }, signal?: AbortSignal) => request<LogFacets>("POST", `${BASE}/logs/facets`, body, { signal }),
  events: (body: { query: string; from: number; to: number; limit?: number }, signal?: AbortSignal) => request<LogSearch>("POST", `${BASE}/events/search`, body, { signal }),
  patterns: (body: { query: string; from: number; to: number }, signal?: AbortSignal) => request<LogPatterns>("POST", `${BASE}/logs/patterns`, body, { signal }),
  /** The live tail's WebSocket path: the search's records as they are applied. */
  tailPath: (query: string) => `${BASE}/logs/tail?query=${enc(query)}`,
  status: (signal?: AbortSignal) => request<ObserveStatus>("GET", `${BASE}/status`, undefined, { signal }),
  verify: () => request<unknown>("POST", `${BASE}/verify`, {}),
  slos: (signal?: AbortSignal) => request<{ slos: Slo[] }>("GET", `${BASE}/slos`, undefined, { signal }).then((r) => r.slos),
  slo: (id: string, signal?: AbortSignal) => request<Slo>("GET", `${BASE}/slos/${enc(id)}`, undefined, { signal }),
  createSlo: (s: SloIn) => request<Slo>("POST", `${BASE}/slos`, s),
  updateSlo: (id: string, s: SloIn) => request<Slo>("PUT", `${BASE}/slos/${enc(id)}`, s),
  deleteSlo: (id: string) => request<void>("DELETE", `${BASE}/slos/${enc(id)}`),
  monitors: (signal?: AbortSignal) => request<{ monitors: Monitor[] }>("GET", `${BASE}/monitors`, undefined, { signal }).then((r) => r.monitors),
  monitor: (id: string, signal?: AbortSignal) => request<Monitor>("GET", `${BASE}/monitors/${enc(id)}`, undefined, { signal }),
  createMonitor: (m: MonitorIn) => request<Monitor>("POST", `${BASE}/monitors`, m),
  downtimes: (signal?: AbortSignal) => request<{ downtimes: Downtime[] }>("GET", `${BASE}/downtimes`, undefined, { signal }).then((r) => r.downtimes),
  createDowntime: (d: DowntimeIn) => request<Downtime>("POST", `${BASE}/downtimes`, d),
  updateDowntime: (id: string, d: DowntimeIn) => request<Downtime>("PUT", `${BASE}/downtimes/${enc(id)}`, d),
  cancelDowntime: (id: string) => request<void>("DELETE", `${BASE}/downtimes/${enc(id)}`),
  updateMonitor: (id: string, m: MonitorIn) => request<Monitor>("PUT", `${BASE}/monitors/${enc(id)}`, m),
  deleteMonitor: (id: string) => request<void>("DELETE", `${BASE}/monitors/${enc(id)}`),
  validateMonitor: (query: string, signal?: AbortSignal, type?: string) => request<MonitorCheck>("POST", `${BASE}/monitors/validate`, { query, type }, { signal }),
  mute: (id: string, until?: number) => request<Monitor>("POST", `${BASE}/monitors/${enc(id)}/mute`, until ? { until } : {}),
  unmute: (id: string) => request<Monitor>("POST", `${BASE}/monitors/${enc(id)}/unmute`, {}),
  integrations: (signal?: AbortSignal) => request<{ integrations: Integration[] }>("GET", `${BASE}/integrations`, undefined, { signal }).then((r) => r.integrations),
  putIntegration: (name: string, i: IntegrationIn) => request<void>("PUT", `${BASE}/integrations/${enc(name)}`, i),
  deleteIntegration: (name: string) => request<void>("DELETE", `${BASE}/integrations/${enc(name)}`),
  testIntegration: (name: string) => request<unknown>("POST", `${BASE}/integrations/${enc(name)}/test`, {}),
  clouds: (signal?: AbortSignal) => request<{ clouds: Cloud[] }>("GET", `${BASE}/clouds`, undefined, { signal }).then((r) => r.clouds),
  putCloud: (name: string, c: CloudIn) => request<void>("PUT", `${BASE}/clouds/${enc(name)}`, c),
  deleteCloud: (name: string) => request<void>("DELETE", `${BASE}/clouds/${enc(name)}`),
  pullCloud: (name: string) => request<unknown>("POST", `${BASE}/clouds/${enc(name)}/pull`, {}),
  testCloud: (name: string, c: CloudIn) => request<CloudTest>("POST", `${BASE}/clouds/${enc(name)}/test`, c),
  scrapes: (signal?: AbortSignal) => request<{ scrapes: Scrape[] }>("GET", `${BASE}/scrapes`, undefined, { signal }).then((r) => r.scrapes),
  putScrape: (name: string, t: ScrapeIn) => request<void>("PUT", `${BASE}/scrapes/${enc(name)}`, t),
  metricsSummary: (match: string | undefined, limit: number, signal?: AbortSignal) =>
    request<{ matched: number; metrics: MetricSummary[] }>("GET", `${BASE}/metrics/summary?limit=${limit}${match ? `&match=${encodeURIComponent(match)}` : ""}`, undefined, { signal }),
  metricSummary: (name: string, signal?: AbortSignal) => request<MetricSummary>("GET", `${BASE}/metrics/summary/${enc(name)}`, undefined, { signal }),
  synthetics: (signal?: AbortSignal) => request<{ synthetics: Synthetic[] }>("GET", `${BASE}/synthetics`, undefined, { signal }).then((r) => r.synthetics),
  putSynthetic: (name: string, t: SyntheticIn) => request<void>("PUT", `${BASE}/synthetics/${enc(name)}`, t),
  deleteSynthetic: (name: string) => request<void>("DELETE", `${BASE}/synthetics/${enc(name)}`),
  testSynthetic: (name: string, t: SyntheticIn) => request<SyntheticOutcome>("POST", `${BASE}/synthetics/${enc(name)}/test`, t),
  runSynthetic: (name: string) => request<{ run: boolean }>("POST", `${BASE}/synthetics/${enc(name)}/run`),
  deleteScrape: (name: string) => request<void>("DELETE", `${BASE}/scrapes/${enc(name)}`),
  statsd: (signal?: AbortSignal) => request<{ statsd: Statsd[] }>("GET", `${BASE}/statsd`, undefined, { signal }).then((r) => r.statsd),
  putStatsd: (name: string, l: StatsdIn) => request<void>("PUT", `${BASE}/statsd/${enc(name)}`, l),
  deleteStatsd: (name: string) => request<void>("DELETE", `${BASE}/statsd/${enc(name)}`),
  testScrape: (name: string, t: ScrapeIn) => request<ScrapeTest>("POST", `${BASE}/scrapes/${enc(name)}/test`, t),
  pipelines: (signal?: AbortSignal) => request<{ pipelines: Pipeline[] }>("GET", `${BASE}/pipelines`, undefined, { signal }).then((r) => r.pipelines),
  pipeline: (id: string, signal?: AbortSignal) => request<Pipeline>("GET", `${BASE}/pipelines/${enc(id)}`, undefined, { signal }),
  createPipeline: (p: PipelineIn) => request<Pipeline>("POST", `${BASE}/pipelines`, p),
  updatePipeline: (id: string, p: PipelineIn) => request<Pipeline>("PUT", `${BASE}/pipelines/${enc(id)}`, p),
  deletePipeline: (id: string) => request<void>("DELETE", `${BASE}/pipelines/${enc(id)}`),
  orderPipelines: (ids: string[]) => request<void>("PUT", `${BASE}/pipeline_order`, { pipeline_ids: ids }),
  tryPipeline: (pipeline: PipelineIn | null, records: PipelineSample[]) =>
    request<{ records: PipelineResult[] }>("POST", `${BASE}/pipeline_test`, { ...(pipeline ? { pipeline } : {}), records }).then((r) => r.records),
  logMetrics: (signal?: AbortSignal) => request<{ log_metrics: LogMetric[]; counting: LogMetricCounting }>("GET", `${BASE}/log_metrics`, undefined, { signal }),
  createLogMetric: (m: LogMetricIn) => request<LogMetric>("POST", `${BASE}/log_metrics`, m),
  updateLogMetric: (name: string, m: LogMetricIn) => request<LogMetric>("PUT", `${BASE}/log_metrics/${enc(name)}`, m),
  deleteLogMetric: (name: string) => request<void>("DELETE", `${BASE}/log_metrics/${enc(name)}`),
  tryLogMetric: (metric: LogMetricIn) => request<LogMetricTry>("POST", `${BASE}/log_metric_test`, { metric }),
  logSources: (signal?: AbortSignal) => request<{ log_sources: LogSource[] }>("GET", `${BASE}/log_sources`, undefined, { signal }).then((r) => r.log_sources),
  putLogSource: (name: string, s: LogSourceIn) => request<void>("PUT", `${BASE}/log_sources/${enc(name)}`, s),
  deleteLogSource: (name: string) => request<void>("DELETE", `${BASE}/log_sources/${enc(name)}`),
  testLogSource: (name: string, s: LogSourceIn) => request<LogSourceTest>("POST", `${BASE}/log_sources/${enc(name)}/test`, s),
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
