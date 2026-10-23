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

/** A log index's exclusion filter (docs/observe.md §8.10), Datadog's: of the records it matches, `sample_rate` excluded (1, every one), by `sample_attribute`'s values where it names one. */
export interface ExclusionFilter {
  name: string;
  is_enabled: boolean;
  filter: { query: string; sample_rate: number; sample_attribute?: string };
}

/** A log index as made or changed (Datadog's `LogsIndex`). */
export interface LogsIndexIn {
  name: string;
  filter: { query: string };
  exclusion_filters: ExclusionFilter[];
  /** Records kept a day at the most; none, no quota. */
  daily_limit: number | null;
  daily_limit_reset?: { reset_time: string; reset_utc_offset: string };
  daily_limit_warning_threshold_percentage?: number;
  /** Days its records are kept for search; none, the store's own. */
  num_retention_days: number | null;
  tags: string[];
}

/** A log index as kiln gives one: Datadog's, its quota's word, and its part of the store and its day. */
export interface LogsIndex extends LogsIndexIn {
  is_rate_limited: boolean;
  kiln: { part: number; created: number; modified: number; today?: { count: number; limit: number; start: number; next_reset: number; warned: boolean } };
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
  /** A service check monitor's critical, warning, ok and unknown: runs of that status in a row. */
  thresholds: { critical?: number | null; warning?: number | null; critical_recovery?: number | null; warning_recovery?: number | null; ok?: number | null; unknown?: number | null };
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
  kind?: "metric" | "change" | "logs" | "spans" | "composite" | "forecast" | "slo" | "error-tracking" | "service-check";
  /** A service check monitor's: its check, the tags of the runs it reads, its groups, and how many
   * of a group's latest runs it reads. */
  watched?: { query?: string | null; search?: string; check?: string; over?: string[]; by?: string[]; last?: number };
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

/** What replaces what a scanning rule matches (docs/observe.md §8.9): nothing (its record tagged alone), a keyed hash, a string, or its first or last characters hidden. */
export type ScanReplacement = "none" | "hash" | "replacement_string" | "partial_replacement_from_beginning" | "partial_replacement_from_end";

/** A sensitive data scanning rule (Datadog's): a standard pattern or one of its own, where it looks, what replaces a match, the tags a match puts on its record. */
export interface ScanningRule {
  id?: string;
  name: string;
  description?: string;
  standard_pattern_id?: string;
  pattern?: string;
  namespaces: string[];
  excluded_namespaces: string[];
  tags: string[];
  is_enabled: boolean;
  text_replacement: { type: ScanReplacement; replacement_string?: string; number_of_chars?: number };
  included_keyword_configuration?: { keywords: string[]; character_count: number; use_recommended_keywords: boolean };
  priority: number;
}

/** A scanning group as it is sent: the records it scans (a log search) and its rules. */
export interface ScanningGroupIn {
  name: string;
  description?: string;
  filter: { query: string };
  is_enabled: boolean;
  rules: ScanningRule[];
}

export type ScanningGroup = ScanningGroupIn & { id: string; created: number; modified: number };

/** A standard pattern a rule may name: what it finds, its tags, and the keywords it suggests (or needs). */
export interface StandardPattern {
  id: string;
  name: string;
  category: string;
  description: string;
  pattern: string;
  tags: string[];
  keywords: string[];
  keywords_required: boolean;
  priority: number;
}

/** The kiln cloud integration whose credentials write an archive's objects (`cloud`, its name); Datadog's own fields beside it kept as given. */
export type ArchiveIntegration = { cloud: string } & Record<string, unknown>;

/** S3's server-side encryption of an archive's objects: the bucket's own, S3's keys, or a KMS key's. */
export interface ArchiveEncryption {
  type: "NO_OVERRIDE" | "SSE_S3" | "SSE_KMS";
  key?: string;
}

/** Where a log archive's objects go (docs/observe.md §10.5): an S3 bucket (or an S3-compatible store's, at `endpoint`), a Cloud Storage bucket, an Azure Storage container. */
export type ArchiveDestination =
  | { type: "s3"; bucket: string; path?: string; integration: ArchiveIntegration; region?: string; storage_class?: string; encryption?: ArchiveEncryption; endpoint?: string }
  | { type: "gcs"; bucket: string; path?: string; integration: ArchiveIntegration; endpoint?: string }
  | { type: "azure"; storage_account: string; container: string; path?: string; integration: ArchiveIntegration; endpoint?: string };

/** A log archive as it is sent (Datadog's): the records it takes (a log search), where it writes, whether its events carry tags. */
export interface ArchiveIn {
  name: string;
  query?: string;
  destination: ArchiveDestination;
  include_tags?: boolean;
  rehydration_tags?: string[];
  rehydration_max_scan_size_in_gb?: number;
}

/** Datadog's state of an archive: unknown until it writes or fails. */
export type ArchiveState = "UNKNOWN" | "WORKING" | "FAILING";

/** How an archive fares: what it wrote since the server started (bytes gzip'd), why it fails, and the log files it has yet to write. */
export interface ArchiveStatus {
  state: ArchiveState;
  error: string | null;
  events: number;
  objects: number;
  bytes: number;
  last_written: number | null;
  pending: number;
}

/** An archive as the API shows it. */
export type Archive = ArchiveIn & { id: string; created: number; modified: number; state: ArchiveState; status: ArchiveStatus };

/** A rehydration as it is asked (docs/observe.md §10.6): an archive's records of a range (ms) that a search takes, read back into a view of their own, kept `retention_days`; its scan bounded in GB (10^9 bytes). */
export interface RehydrationIn {
  name: string;
  archive_id: string;
  query?: string;
  from: number;
  to: number;
  retention_days?: number;
  max_scan_size_in_gb?: number;
}

export type RehydrationState = "queued" | "reading" | "done" | "failed";

/** A rehydration as the API shows it: what it was asked (its archive's name then), how its reading goes, whether its bound stopped it, when its view goes. */
export interface Rehydration {
  id: string;
  name: string;
  archive_id: string;
  archive: string;
  query: string;
  from: number;
  to: number;
  retention_days: number;
  max_scan_bytes?: number;
  created: number;
  expires: number;
  state: RehydrationState;
  error: string | null;
  objects: number;
  objects_read: number;
  bytes_read: number;
  events_read: number;
  records: number;
  malformed: number;
  limited: boolean;
  started: number | null;
  finished: number | null;
}

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

export type WidgetKind = "timeseries" | "query_value" | "toplist" | "table" | "heatmap" | "distribution" | "pie" | "logs" | "events" | "monitors" | "check" | "note" | "group";

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
  options: {
    unit?: string;
    display?: "line" | "bars" | "area";
    aggregator?: string;
    limit?: number;
    order?: "asc" | "desc";
    /** A check widget's: its service check, the tags of its runs, what a group is, and whether it
     * shows every group's status (`cluster`) or one group's (`check`, `group` its tags). */
    check?: string;
    tags?: string[];
    group_by?: string[];
    grouping?: "cluster" | "check";
    group?: string;
  } | null;
  /** A group's widgets, on a grid of its own. */
  widgets?: Widget[];
}

/** A service check run's status, as Datadog names it. */
export type CheckState = "OK" | "WARNING" | "CRITICAL" | "UNKNOWN";

/** A service check's status (`POST checks/status`): the worst group's, how many groups are in
 * each status, and each group's (the worst of its series' latest runs). */
export interface CheckStatus {
  check: string;
  status: CheckState | null;
  counts: Record<CheckState, number>;
  groups: { tags: Record<string, string>; status: CheckState; at: number }[];
  total: number;
}

/** What a check status asks: its check, the tags of its runs, what a group is, or one group. */
export interface CheckStatusAsk {
  check: string;
  tags?: string[];
  group_by?: string[];
  group?: string;
}

/** What became of a Datadog dashboard's widgets as kiln took it: those drawn as Datadog draws
 * them, those drawn otherwise, and those kept as notes (or left out), each with why. */
export interface DashboardImport {
  widgets: number;
  drawn: number;
  otherwise: { title: string; type: string; why: string }[];
  not_drawn: { title: string; type: string; why: string }[];
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

/** Error Tracking's: an issue's status, Datadog's. */
export type IssueStatus = "for_review" | "reviewed" | "resolved" | "ignored";

/** An issue: errors of a service of one type, alike in their words, thrown at one place. */
export interface Issue {
  id: string;
  source: "span" | "log" | "";
  service: string;
  type: string;
  message: string;
  file: string;
  function: string;
  first_seen: number;
  last_seen: number;
  versions: string[];
  envs: string[];
  status: IssueStatus;
  status_at: number;
  regression: boolean;
  activity: { at: number; status: IssueStatus; by: "kiln" | "user"; why: string }[];
  /** Its errors over the range asked. */
  errors: number | null;
}

export interface IssueList {
  from: number;
  to: number;
  total: number;
  issues: Issue[];
}

/** An error's record, whole: a span's or a log's. */
export interface ErrorRecord {
  ts_ns: number;
  status: string;
  service: string;
  host: string;
  source: string;
  message: string;
  attributes: Record<string, string>;
  trace_id: string | null;
  span_id: string | null;
}

export interface IssueDetail {
  issue: Issue;
  from: number;
  to: number;
  timeline: { from: number; step: number; values: number[] };
  versions: { version: string; errors: number }[];
  envs: { env: string; errors: number }[];
  latest: ErrorRecord | null;
}

const rangeQuery = (from: number, to: number, env?: string) => `from=${Math.floor(from)}&to=${Math.ceil(to)}${env && env !== "*" ? `&env=${enc(env)}` : ""}`;

/** A service's scores: its rules passed of all, and each scorecard's (docs/observe.md §10.8). */
export interface CatalogScore {
  passed: number;
  total: number;
  scorecards: { name: string; passed: number; total: number }[];
}

/** A container (docs/observe.md §10.10), as kiln's container check, Datadog's Agent or OpenTelemetry report it. */
export interface Container {
  id: string;
  name: string | null;
  state: "running" | "stopped";
  host: string | null;
  runtime: string | null;
  image: { name: string | null; tag: string | null; short: string | null };
  kube: { namespace: string | null; pod: string | null; container: string | null; qos: string | null };
  compose: { service: string | null; project: string | null };
  /** ms since the epoch: its newest sample. */
  last_reported: number;
  series: number;
  /** Each its newest of the last ten minutes: CPU in nanocores, memory in bytes, I/O and throttling a second. */
  figures: {
    cpu_nanocores: number | null;
    cpu_limit_nanocores: number | null;
    cpu_throttled_ns: number | null;
    memory_bytes: number | null;
    working_set_bytes: number | null;
    memory_limit_bytes: number | null;
    pids: number | null;
    io_read_bytes: number | null;
    io_write_bytes: number | null;
    uptime_seconds: number | null;
  };
  tags: string[];
}

/** A line of a container's chart: its name and the query that draws it. */
export interface ChartLine {
  name: string;
  query: string;
}

/** A container as its page reads it: its charts' lines, each of the first of its figure's sources that reported it (kiln's check's, or an OpenTelemetry collector's in its own names). */
export interface ContainerDetail extends Container {
  charts: { cpu: ChartLine[]; memory: ChartLine[]; throttled: ChartLine[]; io: ChartLine[] };
}

/** A profiler's upload (docs/observe.md §8.11): its times, its profiler's language and tags (`key:value`), and each profile's sample types, their values summed. */
export interface ProfileUpload {
  id: string;
  start_ms: number;
  end_ms: number;
  family: string;
  tags: string[];
  profiles: { name: string; types: { type: string; unit: string; total: number }[] }[];
}

export interface ProfileUploadDetail extends ProfileUpload {
  /** Requests by endpoint while it was collected. */
  endpoints: { endpoint: string; count: number }[];
}

/** A sample type the uploads hold: its profile (`cpu.pprof`), its type (`cpu`) and unit, and how many uploads hold it. */
export interface ProfileType {
  profile: string;
  type: string;
  unit: string;
  uploads: number;
}

export interface ProfileFacet {
  key: string;
  values: { value: string; uploads: number }[];
}

/**
 * A type summed into a flame graph: its functions' names and files; its nodes root first, each node's children after it by name, as
 * `[function, parent, total, self]` (the root's function -1, a parent a place in `nodes`); its functions by self, as `[function, self, total]`.
 */
export interface FlameGraph {
  profile: string;
  type: string;
  unit: string;
  /** Whether the graph is the uploads' average over time (a snapshot's type: the heap live, goroutines), each process's summed; else their sum. */
  averaged: boolean;
  /** Uploads that held the type, were read (evenly across the time, past the limit), and failed to read. */
  matched: number;
  read: number;
  failed: number;
  samples: number;
  total: number;
  names: string[];
  files: string[];
  nodes: [number, number, number, number][];
  top: [number, number, number][];
}

/** One side of a comparison: its uploads (held the type, read, failed), its whole and samples. */
export interface FlameSide {
  matched: number;
  read: number;
  failed: number;
  total: number;
  samples: number;
}

/**
 * Two graphs compared path by path: `after` (what is compared) and `before` (what it is compared with); each node
 * `[function, parent, total after, self after, total before, self before]`, sized by one side; functions by how much their self's
 * share of its whole changed, as `[function, self after, self before, total after, total before]`.
 */
export interface FlameComparison {
  profile: string;
  type: string;
  unit: string;
  averaged: boolean;
  after: FlameSide;
  before: FlameSide;
  names: string[];
  files: string[];
  nodes: [number, number, number, number, number, number][];
  top: [number, number, number, number, number][];
}

/** Uploads chosen: their time (ms) and a search of their tags in log search's syntax. */
export interface ProfileQuery {
  from: number;
  to: number;
  query?: string;
}

const profileParams = (q: object) => {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== "") p.set(k, String(v));
  return p;
};

export interface ContainerQuery {
  state?: "running" | "stopped" | "all";
  filter?: string;
  sort_field?: "name" | "cpu" | "memory" | "cpu_limit" | "memory_limit" | "throttled" | "io" | "uptime" | "last_reported";
  sort_dir?: "asc" | "desc";
  start?: number;
  count?: number;
}

/** A notebook's time, Datadog's: the last of a span, or from one time to another. */
export type NotebookTime = { live_span: string } | { start: string; end: string; live?: boolean };
export type NotebookType = "postmortem" | "runbook" | "investigation" | "documentation" | "report";
export type GraphSize = "xs" | "s" | "m" | "l" | "xl";

/** A cell's definition: Markdown's text, or one of Datadog's widgets' (its requests, a log stream's query). */
export interface CellDefinition {
  type: "markdown" | "timeseries" | "toplist" | "heatmap" | "distribution" | "log_stream";
  text?: string;
  title?: string;
  requests?: { q?: string; display_type?: string; [k: string]: unknown }[];
  query?: string;
  [k: string]: unknown;
}

export interface NotebookCellAttributes {
  definition: CellDefinition;
  graph_size?: GraphSize;
  split_by?: unknown;
  time?: NotebookTime | null;
}

/** A notebook's cell (docs/observe.md §11.1), kiln's drawing of a graph beside. */
export interface NotebookCell {
  id: string;
  type: "notebook_cells";
  attributes: NotebookCellAttributes;
  kiln?: { widget: Widget | null; said?: { title: string; type: string; why: string }[] };
}

/** A notebook as Datadog's API gives one (a list's without its cells and time). */
export interface Notebook {
  id: number;
  type: "notebooks";
  attributes: {
    author: { handle: string; name: string };
    cells?: NotebookCell[];
    created: string;
    modified: string;
    name: string;
    status: "published";
    time?: NotebookTime;
    metadata: { is_template: boolean; take_snapshots: boolean; type: NotebookType | null };
    template_variables: unknown[] | null;
    kiln: { incident: string | null };
  };
}

/** What a notebook made or changed says: Datadog's attributes, a kept cell its id. */
export interface NotebookBody {
  name: string;
  cells: { id?: string; type: "notebook_cells"; attributes: NotebookCellAttributes }[];
  time: NotebookTime;
  metadata?: { is_template?: boolean; take_snapshots?: boolean; type?: NotebookType | null };
}

export interface NotebookQuery {
  query?: string;
  type?: NotebookType;
  is_template?: boolean;
  start?: number;
  count?: number;
  sort_field?: "modified" | "name" | "created";
  sort_dir?: "asc" | "desc";
}

/** An incident's field: Datadog's, a text or a list of texts (docs/observe.md §9.5). */
export interface IncidentField {
  type: string;
  value: string | string[] | null;
}

export interface IncidentHandle {
  display_name: string;
  handle: string;
}

export type IncidentState = "active" | "stable" | "resolved";

/** An incident as Datadog's API gives it (times RFC 3339, figures in seconds), kiln's own beside under `kiln`. */
export interface Incident {
  id: string;
  type: "incidents";
  attributes: {
    public_id: number;
    title: string;
    customer_impacted: boolean;
    customer_impact_scope: string | null;
    customer_impact_start: string | null;
    customer_impact_end: string | null;
    customer_impact_duration: number | null;
    time_to_detect: number | null;
    time_to_internal_response: number | null;
    time_to_repair: number | null;
    time_to_resolve: number | null;
    created: string;
    modified: string;
    detected: string;
    declared: string;
    resolved: string | null;
    severity: string;
    state: IncidentState;
    fields: Record<string, IncidentField>;
    notification_handles: IncidentHandle[];
    is_test: boolean;
    declared_by: { name: string };
    kiln: {
      monitor: { id: string; name: string; group: [string, string][] } | null;
      todos: number;
      todos_open: number;
      postmortem: { url: string; title: string } | null;
    };
  };
  relationships: {
    commander_user: { data: { type: "users"; id: string } | null };
    created_by_user: { data: { type: "users"; id: string } };
  };
}

export interface IncidentTodo {
  id: string;
  type: "incident_todos";
  attributes: { content: string; assignees: string[]; due_date: string | null; completed: string | null; created: string; modified: string; incident_id: string };
}

export interface IncidentAttachment {
  id: string;
  type: "incident_attachments";
  attributes: { attachment_type: "postmortem" | "link"; attachment: { documentUrl: string; title: string }; modified: string };
}

/** A cell of an incident's timeline: a note, a change, its monitor's state. */
export type TimelineCell = { id: string; created: string; author: string } & (
  | { cell_type: "markdown"; content: string; important: boolean }
  | { cell_type: "changed"; what: string; from: string; to: string }
  | { cell_type: "monitor"; monitor_id: string; monitor: string; state: string; value: number | null; message: string }
);

export interface FacetCount {
  name: string;
  count: number;
}

/** What a search matched, counted by facet (Datadog's). */
export interface IncidentFacets {
  state: FacetCount[];
  severity: FacetCount[];
  commander: FacetCount[];
  created_by: FacetCount[];
  postmortem: FacetCount[];
  impact: FacetCount[];
  fields: { name: string; facets: FacetCount[] }[];
}

export interface IncidentSearch {
  incidents: Incident[];
  total: number;
  facets: IncidentFacets;
}

/** What declaring or changing an incident says: Datadog's attributes. */
export interface IncidentChange {
  title?: string;
  severity?: string;
  state?: IncidentState;
  customer_impacted?: boolean;
  customer_impact_scope?: string | null;
  customer_impact_start?: string | null;
  customer_impact_end?: string | null;
  fields?: Record<string, IncidentField>;
  notification_handles?: IncidentHandle[];
  is_test?: boolean;
  initial_cells?: { cell_type: "markdown"; content: { content: string }; important?: boolean }[];
}

/** A host's figures, its newest of the last ten minutes: CPU in use, I/O wait, memory in use and its fullest disk's use (%), the load over 15 minutes (and a core's). */
export interface HostFigures {
  cpu: number | null;
  iowait: number | null;
  load: number | null;
  load_norm: number | null;
  memory: number | null;
  disk: number | null;
}

/** A host's platform, from the best of what says it: its runner, the Agent, its resource's attributes, its metrics. */
export interface HostPlatform {
  os: string | null;
  kernel: string | null;
  arch: string | null;
  cpu_model: string | null;
  cores: number | null;
  threads: number | null;
  memory_bytes: number | null;
  cloud: string | null;
  instance_type: string | null;
  region: string | null;
  agent_version: string | null;
  kiln_version: string | null;
}

/** A host as Datadog's host list gives it (times in seconds), kiln's own beside (docs/observe.md §10.9). */
export interface Host {
  name: string;
  host_name: string;
  id: number;
  aliases: string[];
  apps: string[];
  sources: string[];
  up: boolean;
  is_muted: boolean;
  mute_timeout: number | null;
  last_reported_time: number | null;
  metrics: { cpu: number | null; iowait: number | null; load: number | null };
  tags_by_source: Record<string, string[]>;
  meta?: Record<string, unknown>;
  kiln: {
    status: "reporting" | "late" | "gone";
    seen: number | null;
    series: number;
    figures: HostFigures;
    platform: HostPlatform;
    runner: { name: string; connected: boolean; last_seen: number; version: string; labels: string[] } | null;
    mute: { downtime: string; end: number | null; message: string } | null;
    inventory?: Record<string, string | number>;
    agent_updated?: number;
    gpus?: { vendor: string; model: string; memory: number }[];
  };
}

/** A container on a host: its name, image and pod, when it last reported, its CPU (nanocores) and memory (bytes). */
export interface HostDetail extends Host {
  /** Its containers as the containers' API gives them (docs/observe.md §10.10): running, then stopped within the hour. */
  containers: Container[];
}

export interface HostQuery {
  filter?: string;
  sort_field?: string;
  sort_dir?: "asc" | "desc";
  count?: number;
}

/** A service in the catalog: where its definition came from, who owns it, how it is doing. */
export interface CatalogService {
  service: string;
  source: "api" | "repository" | "seen";
  repo: string | null;
  team: string | null;
  tier: string | null;
  lifecycle: string | null;
  type: string | null;
  description: string | null;
  /** The last hour's, from the trace metrics. */
  requests: number;
  errors: number;
  error_rate: number | null;
  p95_s: number | null;
  open_issues: number;
  monitors: number;
  alerting: number;
  slos: number;
  deployments_30d: number;
  failed_30d: number;
  last_deployment: number | null;
  score: CatalogScore;
}

/** A scorecard rule of a service, passed or not, and why. */
export interface CatalogRule {
  scorecard: string;
  id: string;
  name: string;
  passed: boolean;
  why: string;
}

/** Datadog's service definition v2.2. */
export interface ServiceDefinition {
  "schema-version": string;
  "dd-service": string;
  team?: string;
  application?: string;
  description?: string;
  tier?: string;
  lifecycle?: string;
  type?: string;
  languages?: string[];
  contacts?: { name?: string; type: string; contact: string }[];
  links?: { name: string; type: string; url: string; provider?: string }[];
  tags?: string[];
  integrations?: { pagerduty?: { "service-url": string }; opsgenie?: { "service-url": string; region?: string } };
}

/** A service's page: its definition, every rule, what reads it, its latest deployments. */
export interface CatalogServiceDetail extends CatalogService {
  definition: { id: string; attributes: { schema: ServiceDefinition; meta: { "ingestion-source": string; "origin-detail": string; "last-modified-time": string } } } | null;
  rules: CatalogRule[];
  monitor_list: { id: string; name: string; state: string }[];
  slo_list: { id: string; name: string }[];
  deployments: { id: string; env: string; version: string; finished_at: number; failed: boolean }[];
  traced: boolean;
  logs_traced: number;
}

/** DORA's levels: the 2024 report's clusters (docs/observe.md §10.7). */
export type DoraLevel = "elite" | "high" | "medium" | "low";

/** The four keys over a range, week by week (`GET dora/summary`). */
export interface DoraSummary {
  from: number;
  to: number;
  deployments: number;
  per_day: number;
  frequency_level: DoraLevel | null;
  lead_time_s: number | null;
  lead_time_level: DoraLevel | null;
  lead_deployments: number;
  change_failures: number;
  failure_rate: number | null;
  failure_rates: { level: DoraLevel; rate: number }[];
  recovery_s: number | null;
  recovery_level: DoraLevel | null;
  recovered: number;
  failures: number;
  open_failures: number;
  restore_s: number | null;
  weeks: { start: number; deployments: number; change_failures: number; lead_time_s: number | null; recovery_s: number | null }[];
  services: string[];
  envs: string[];
  teams: string[];
}

export type DoraFound = "rollback" | "revert" | "hotfix" | "manual";
export type DoraRemedyKind = "rollback" | "rollforward";

/** A deployment as kiln keeps it: its lead time, whether it failed and what put it right. Times in ms. */
export interface DoraDeployment {
  id: string;
  type: "dora_deployment";
  attributes: {
    service: string;
    env: string;
    team: string;
    version: string;
    started_at: number;
    finished_at: number;
    git: { commit_sha: string; repository_url: string } | null;
    custom_tags: string[];
    source: "api" | "kiln.cd";
    lead: { commits: number; mean_s: number; min_s: number; max_s: number } | null;
    change_failure: { found: DoraFound; remediation: { id: string; type: DoraRemedyKind; finished_at: number } | null } | null;
    recovery_s: number | null;
    remedies: string[];
  };
}

/** A failure (an incident). Times in ms. */
export interface DoraFailure {
  id: string;
  type: "dora_failure";
  attributes: { services: string[]; env: string; team: string; name: string; severity: string; started_at: number; finished_at: number | null; version: string; custom_tags: string[] };
}

/** What a list of deployments or failures asks: a range (ms), a search of their fields, an order. */
export interface DoraList {
  from: number;
  to: number;
  query?: string;
  limit?: number;
  sort?: "-finished_at" | "finished_at" | "-started_at" | "started_at";
}

export const observeApi = {
  spans: (body: { query: string; from: number; to: number; limit?: number }, signal?: AbortSignal) => request<SpanSearch>("POST", `${BASE}/spans/search`, body, { signal }),
  spanHistogram: (body: { query: string; from: number; to: number; step: number; by?: string[] }, signal?: AbortSignal) => request<LogHistogram>("POST", `${BASE}/spans/aggregate`, body, { signal }),
  spanFacets: (body: { query: string; from: number; to: number; facets: string[]; top?: number }, signal?: AbortSignal) => request<LogFacets>("POST", `${BASE}/spans/facets`, body, { signal }),
  trace: (id: string, signal?: AbortSignal) => request<Trace>("GET", `${BASE}/traces/${enc(id)}`, undefined, { signal }),
  services: (from: number, to: number, env: string | undefined, signal?: AbortSignal) => request<Services>("GET", `${BASE}/apm/services?${rangeQuery(from, to, env)}`, undefined, { signal }),
  service: (name: string, from: number, to: number, env: string | undefined, signal?: AbortSignal) =>
    request<ServiceDetail>("GET", `${BASE}/apm/services/${enc(name)}?${rangeQuery(from, to, env)}`, undefined, { signal }),
  serviceMap: (from: number, to: number, env: string | undefined, signal?: AbortSignal) => request<ServiceMap>("GET", `${BASE}/apm/map?${rangeQuery(from, to, env)}`, undefined, { signal }),
  issues: (from: number, to: number, q: { status?: string; query?: string; env?: string }, signal?: AbortSignal) =>
    request<IssueList>(
      "GET",
      `${BASE}/errors/issues?${rangeQuery(from, to, q.env)}&all=true${q.status ? `&status=${enc(q.status)}` : ""}${q.query ? `&query=${enc(q.query)}` : ""}&limit=200`,
      undefined,
      { signal },
    ),
  issue: (id: string, from: number, to: number, signal?: AbortSignal) => request<IssueDetail>("GET", `${BASE}/errors/issues/${enc(id)}?${rangeQuery(from, to)}`, undefined, { signal }),
  setIssueStatus: (id: string, status: IssueStatus) => request<Issue>("PUT", `${BASE}/errors/issues/${enc(id)}`, { status }),
  query: (body: { query: string; from: number; to: number; step?: number; points?: number }, signal?: AbortSignal) => request<QueryAnswer>("POST", `${BASE}/query`, body, { signal }),
  heatmap: (body: { query: string; from: number; to: number; points?: number; heatmap: number }, signal?: AbortSignal) => request<HeatmapAnswer>("POST", `${BASE}/query`, body, { signal }),
  checkStatus: (body: CheckStatusAsk & { from: number; to: number }, signal?: AbortSignal) => request<CheckStatus>("POST", `${BASE}/checks/status`, body, { signal }),
  metrics: (match: string | undefined, signal?: AbortSignal) =>
    request<{ metrics: string[] }>("GET", `${BASE}/metrics${match ? `?match=${enc(match)}&limit=200` : "?limit=200"}`, undefined, { signal }),
  tagValues: (key: string, signal?: AbortSignal) => request<{ key: string; values: string[] }>("GET", `${BASE}/tags?key=${enc(key)}&limit=500`, undefined, { signal }),
  logs: (body: { query: string; from: number; to: number; limit?: number; view?: string; indexes?: string[] }, signal?: AbortSignal) =>
    request<LogSearch>("POST", `${BASE}/logs/search`, body, { signal }),
  histogram: (body: { query: string; from: number; to: number; step: number; by?: string[]; view?: string; indexes?: string[] }, signal?: AbortSignal) =>
    request<LogHistogram>("POST", `${BASE}/logs/aggregate`, body, { signal }),
  facets: (body: { query: string; from: number; to: number; facets: string[]; top?: number; view?: string; indexes?: string[] }, signal?: AbortSignal) =>
    request<LogFacets>("POST", `${BASE}/logs/facets`, body, { signal }),
  events: (body: { query: string; from: number; to: number; limit?: number }, signal?: AbortSignal) => request<LogSearch>("POST", `${BASE}/events/search`, body, { signal }),
  patterns: (body: { query: string; from: number; to: number; view?: string; indexes?: string[] }, signal?: AbortSignal) => request<LogPatterns>("POST", `${BASE}/logs/patterns`, body, { signal }),
  indexes: (signal?: AbortSignal) => request<{ indexes: LogsIndex[] }>("GET", `${BASE}/logs/config/indexes`, undefined, { signal }).then((r) => r.indexes),
  createIndex: (i: LogsIndexIn) => request<LogsIndex>("POST", `${BASE}/logs/config/indexes`, i),
  updateIndex: (name: string, i: Omit<LogsIndexIn, "name"> & { disable_daily_limit?: boolean }) => request<LogsIndex>("PUT", `${BASE}/logs/config/indexes/${enc(name)}`, i),
  deleteIndex: (name: string) => request<unknown>("DELETE", `${BASE}/logs/config/indexes/${enc(name)}`),
  orderIndexes: (names: string[]) => request<{ index_names: string[] }>("PUT", `${BASE}/logs/config/index-order`, { index_names: names }),
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
  scanningGroups: (signal?: AbortSignal) => request<{ groups: ScanningGroup[] }>("GET", `${BASE}/scanner/groups`, undefined, { signal }).then((r) => r.groups),
  scanningGroup: (id: string, signal?: AbortSignal) => request<ScanningGroup>("GET", `${BASE}/scanner/groups/${enc(id)}`, undefined, { signal }),
  createScanningGroup: (g: ScanningGroupIn) => request<ScanningGroup>("POST", `${BASE}/scanner/groups`, g),
  updateScanningGroup: (id: string, g: ScanningGroupIn) => request<ScanningGroup>("PUT", `${BASE}/scanner/groups/${enc(id)}`, g),
  deleteScanningGroup: (id: string) => request<void>("DELETE", `${BASE}/scanner/groups/${enc(id)}`),
  containers: (q: ContainerQuery, signal?: AbortSignal) => {
    const p = new URLSearchParams();
    for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== "") p.set(k, String(v));
    return request<{ containers: Container[]; total_matching: number; total_returned: number; totals: { running: number; stopped: number } }>("GET", `${BASE}/containers?${p}`, undefined, { signal });
  },
  container: (id: string, signal?: AbortSignal) => request<ContainerDetail>("GET", `${BASE}/containers/${enc(id)}`, undefined, { signal }),
  profiles: (q: ProfileQuery & { limit?: number }, signal?: AbortSignal) =>
    request<{ uploads: ProfileUpload[]; matched: number }>("GET", `${BASE}/profiles?${profileParams(q)}`, undefined, { signal }),
  profileUpload: (id: string, signal?: AbortSignal) => request<ProfileUploadDetail>("GET", `${BASE}/profiles/${enc(id)}`, undefined, { signal }),
  profileFacets: (q: ProfileQuery, signal?: AbortSignal) => request<{ facets: ProfileFacet[]; matched: number }>("GET", `${BASE}/profiles/facets?${profileParams(q)}`, undefined, { signal }),
  profileTypes: (q: ProfileQuery, signal?: AbortSignal) => request<{ types: ProfileType[] }>("GET", `${BASE}/profiles/types?${profileParams(q)}`, undefined, { signal }).then((r) => r.types),
  flame: (q: ProfileQuery & { profile: string; type: string; id?: string; limit?: number }, signal?: AbortSignal) =>
    request<FlameGraph>("GET", `${BASE}/profiles/flame?${profileParams(q)}`, undefined, { signal }),
  compareFlames: (q: ProfileQuery & { profile: string; type: string; id?: string; base_from?: number; base_to?: number; base_query?: string; by?: "after" | "before" }, signal?: AbortSignal) =>
    request<FlameComparison>("GET", `${BASE}/profiles/compare?${profileParams(q)}`, undefined, { signal }),
  /** Where an upload's profile is read as pprof, inflated (what `go tool pprof` reads). */
  profileDownload: (id: string, name: string) => `${BASE}/profiles/${enc(id)}/${enc(name)}`,
  /** Notebooks, as Datadog's list gives them, without their cells. */
  notebooks: (q: NotebookQuery, signal?: AbortSignal) => {
    const p = new URLSearchParams({ include_cells: "false" });
    for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== "") p.set(k, String(v));
    return request<{ data: Notebook[]; meta: { page: { total_count: number; total_filtered_count: number } } }>("GET", `${BASE}/notebooks?${p}`, undefined, { signal });
  },
  notebook: (id: string | number, signal?: AbortSignal) => request<{ data: Notebook }>("GET", `${BASE}/notebooks/${enc(String(id))}`, undefined, { signal }).then((r) => r.data),
  createNotebook: (body: NotebookBody) => request<{ data: Notebook }>("POST", `${BASE}/notebooks`, { data: { type: "notebooks", attributes: body } }).then((r) => r.data),
  updateNotebook: (id: number, body: NotebookBody) => request<{ data: Notebook }>("PUT", `${BASE}/notebooks/${id}`, { data: { type: "notebooks", attributes: body } }).then((r) => r.data),
  removeNotebook: (id: number) => request<void>("DELETE", `${BASE}/notebooks/${id}`),
  /** An incident's postmortem: a notebook of `template` (a template's id; kiln's own unless said), attached to it. */
  writePostmortem: (incident: string, template?: number) =>
    request<{ data: Notebook }>("POST", `${BASE}/incidents/${enc(incident)}/postmortem`, template ? { template_id: template } : {}).then((r) => r.data),
  /** Incidents `query` matches (Datadog's search syntax), the newest first: a page of `size` from `offset`, with what matched counted by facet. */
  incidentSearch: (query: string, offset: number, size: number, signal?: AbortSignal) =>
    request<{ data: { attributes: { incidents: { data: Incident }[]; total: number; facets: IncidentFacets } } }>(
      "GET",
      `${BASE}/incidents/search?${new URLSearchParams({ query, "page[size]": String(size), "page[offset]": String(offset) })}`,
      undefined,
      { signal },
    ).then((r): IncidentSearch => ({ incidents: r.data.attributes.incidents.map((x) => x.data), total: r.data.attributes.total, facets: r.data.attributes.facets })),
  incident: (id: string, signal?: AbortSignal) => request<{ data: Incident }>("GET", `${BASE}/incidents/${enc(id)}`, undefined, { signal }).then((r) => r.data),
  /** Declares an incident; `commander` a user's id or name. */
  declareIncident: (attributes: IncidentChange, commander?: string) =>
    request<{ data: Incident }>("POST", `${BASE}/incidents`, {
      data: { type: "incidents", attributes, ...(commander ? { relationships: { commander_user: { data: { type: "users", id: commander } } } } : {}) },
    }).then((r) => r.data),
  /** Changes an incident; `commander` set (a user) or cleared (`null`) when given. */
  updateIncident: (id: string, attributes: IncidentChange, commander?: string | null) =>
    request<{ data: Incident }>("PATCH", `${BASE}/incidents/${enc(id)}`, {
      data: { type: "incidents", attributes, ...(commander !== undefined ? { relationships: { commander_user: { data: commander ? { type: "users", id: commander } : null } } } : {}) },
    }).then((r) => r.data),
  removeIncident: (id: string) => request<void>("DELETE", `${BASE}/incidents/${enc(id)}`),
  incidentTimeline: (id: string, signal?: AbortSignal) => request<{ data: TimelineCell[] }>("GET", `${BASE}/incidents/${enc(id)}/timeline`, undefined, { signal }).then((r) => r.data),
  noteIncident: (id: string, content: string, important: boolean) => request<{ data: TimelineCell }>("POST", `${BASE}/incidents/${enc(id)}/timeline`, { content, important }),
  incidentTodos: (id: string, signal?: AbortSignal) => request<{ data: IncidentTodo[] }>("GET", `${BASE}/incidents/${enc(id)}/relationships/todos`, undefined, { signal }).then((r) => r.data),
  addIncidentTodo: (id: string, content: string, assignees: string[]) =>
    request<{ data: IncidentTodo }>("POST", `${BASE}/incidents/${enc(id)}/relationships/todos`, { data: { type: "incident_todos", attributes: { content, assignees } } }),
  setIncidentTodo: (id: string, todo: string, attributes: { content?: string; completed?: string | null }) =>
    request<{ data: IncidentTodo }>("PATCH", `${BASE}/incidents/${enc(id)}/relationships/todos/${enc(todo)}`, { data: { type: "incident_todos", attributes } }),
  removeIncidentTodo: (id: string, todo: string) => request<void>("DELETE", `${BASE}/incidents/${enc(id)}/relationships/todos/${enc(todo)}`),
  incidentAttachments: (id: string, signal?: AbortSignal) => request<{ data: IncidentAttachment[] }>("GET", `${BASE}/incidents/${enc(id)}/attachments`, undefined, { signal }).then((r) => r.data),
  attachToIncident: (id: string, kind: "postmortem" | "link", url: string, title: string) =>
    request<{ data: IncidentAttachment }>("POST", `${BASE}/incidents/${enc(id)}/attachments`, {
      data: { type: "incident_attachments", attributes: { attachment_type: kind, attachment: { documentUrl: url, title } } },
    }),
  removeIncidentAttachment: (id: string, attachment: string) => request<void>("DELETE", `${BASE}/incidents/${enc(id)}/attachments/${enc(attachment)}`),
  hosts: (q: HostQuery, signal?: AbortSignal) => {
    const p = new URLSearchParams();
    for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== "") p.set(k, String(v));
    return request<{ host_list: Host[]; total_matching: number; total_returned: number }>("GET", `${BASE}/hosts?${p}`, undefined, { signal });
  },
  hostTotals: (signal?: AbortSignal) => request<{ total_up: number; total_active: number }>("GET", `${BASE}/hosts/totals`, undefined, { signal }),
  host: (name: string, signal?: AbortSignal) => request<HostDetail>("GET", `${BASE}/hosts/${enc(name)}`, undefined, { signal }),
  /** Mutes a host until `end` (seconds since the epoch; until unmuted when none): a downtime of every monitor's groups of it. */
  muteHost: (name: string, body: { end?: number; message?: string; override?: boolean }) => request<{ action: string; hostname: string }>("POST", `${BASE}/hosts/${enc(name)}/mute`, body),
  unmuteHost: (name: string) => request<{ action: string; hostname: string }>("POST", `${BASE}/hosts/${enc(name)}/unmute`),
  catalog: (signal?: AbortSignal) => request<{ services: CatalogService[] }>("GET", `${BASE}/catalog`, undefined, { signal }).then((r) => r.services),
  catalogService: (name: string, signal?: AbortSignal) => request<CatalogServiceDetail>("GET", `${BASE}/catalog/${enc(name)}`, undefined, { signal }),
  /** Definitions as Datadog's API takes them: YAML (several `---` apart) or JSON. */
  defineServices: (text: string) => request<{ data: unknown[] }>("POST", `${BASE}/catalog/definitions`, undefined, { file: new Blob([text]), type: "application/x-yaml" }),
  undefineService: (name: string) => request<void>("DELETE", `${BASE}/catalog/definitions/${enc(name)}`),
  doraSummary: (q: { from: number; to: number; service?: string; env?: string; team?: string }, signal?: AbortSignal) => {
    const p = new URLSearchParams({ from: String(q.from), to: String(q.to) });
    for (const k of ["service", "env", "team"] as const) if (q[k]) p.set(k, q[k] as string);
    return request<DoraSummary>("GET", `${BASE}/dora/summary?${p}`, undefined, { signal });
  },
  doraDeployments: (body: DoraList, signal?: AbortSignal) => request<{ data: DoraDeployment[] }>("POST", `${BASE}/dora/deployments`, body, { signal }).then((r) => r.data),
  doraFailures: (body: DoraList, signal?: AbortSignal) => request<{ data: DoraFailure[] }>("POST", `${BASE}/dora/failures`, body, { signal }).then((r) => r.data),
  markDeployment: (id: string, body: { change_failure: boolean; remediation?: { id: string; type: DoraRemedyKind } }) =>
    request<{ data: DoraDeployment }>("PATCH", `${BASE}/dora/deployments/${enc(id)}`, body).then((r) => r.data),
  orderScanningGroups: (ids: string[]) => request<void>("PUT", `${BASE}/scanner/group_order`, { group_ids: ids }),
  standardPatterns: (signal?: AbortSignal) => request<{ standard_patterns: StandardPattern[] }>("GET", `${BASE}/scanner/standard_patterns`, undefined, { signal }).then((r) => r.standard_patterns),
  tryScanner: (group: ScanningGroupIn | null, records: PipelineSample[]) =>
    request<{ records: PipelineResult[] }>("POST", `${BASE}/scanner/test`, { ...(group ? { group } : {}), records }).then((r) => r.records),
  archives: (signal?: AbortSignal) => request<{ archives: Archive[] }>("GET", `${BASE}/archives`, undefined, { signal }).then((r) => r.archives),
  createArchive: (a: ArchiveIn) => request<Archive>("POST", `${BASE}/archives`, a),
  updateArchive: (id: string, a: ArchiveIn) => request<Archive>("PUT", `${BASE}/archives/${enc(id)}`, a),
  deleteArchive: (id: string) => request<void>("DELETE", `${BASE}/archives/${enc(id)}`),
  orderArchives: (ids: string[]) => request<{ archive_ids: string[] }>("PUT", `${BASE}/archive_order`, { archive_ids: ids }),
  rehydrations: (signal?: AbortSignal) => request<{ rehydrations: Rehydration[] }>("GET", `${BASE}/rehydrations`, undefined, { signal }).then((r) => r.rehydrations),
  rehydration: (id: string, signal?: AbortSignal) => request<Rehydration>("GET", `${BASE}/rehydrations/${enc(id)}`, undefined, { signal }),
  createRehydration: (r: RehydrationIn) => request<Rehydration>("POST", `${BASE}/rehydrations`, r),
  deleteRehydration: (id: string) => request<void>("DELETE", `${BASE}/rehydrations/${enc(id)}`),
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
  /** Datadog's dashboard JSON, as its API gives one, made a dashboard of kiln's. */
  importDashboard: (datadog: unknown) => request<{ dashboard: Dashboard; report: DashboardImport }>("POST", `${BASE}/dashboards/import`, datadog),
  /** Where a dashboard is given as Datadog's dashboard JSON. */
  datadogUrl: (id: string) => `${BASE}/dashboards/${enc(id)}/datadog`,
};

/**
 * A template variable's value in a query: `$host` stands for `host:<value>`, or for `any` (`*` in a
 * metric query's filter; nothing in a log search) when it is `*`; `$host.value` (Datadog's) for the
 * value alone, `*` for any.
 */
export function fillVariables(query: string, vars: TemplateVariable[], values: Record<string, string>, any = "*"): string {
  let q = query;
  // Longest names first, so `$hostname` is not read as `$host` and `name`.
  for (const v of [...vars].sort((a, b) => b.name.length - a.name.length)) {
    const value = values[v.name] ?? v.default;
    const all = !value || value === "*";
    const plain = /^[\w.\-/:]+$/.test(value) ? value : `"${value.replace(/["\\]/g, "\\$&")}"`;
    q = q.split(`$${v.name}.value`).join(all ? "*" : plain);
    q = q.split(`$${v.name}`).join(all ? any : `${v.tag}:${plain}`);
  }
  return q;
}
