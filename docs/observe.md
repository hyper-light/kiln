# Observe: kiln's own metrics, logs, events, monitors and dashboards

kiln watches what it runs without Grafana, Prometheus or a SaaS: one engine, inside the kiln binary,
that stores metrics, logs and events in a columnar store replicated across runners with hyper-raft,
answers a query language in milliseconds on any replica, evaluates monitors and notifies Slack,
Discord, Teams, PagerDuty, Opsgenie, email or a webhook, and draws dashboards in kiln's UI. It takes OpenTelemetry
(OTLP) from anything, and kiln CI, kiln CD, GitHub and the clouds (AWS, GCP, Azure) feed it too.

This document is the design and the record: what is built, how it is measured, and why each number
is what it is. A change in behaviour updates it in the same commit.

## 0. Status

| Milestone | What | State |
| --- | --- | --- |
| O-1 | Engine core: Gorilla chunks, symbols, series index and selection, head with late writes, write batches; measured (§12.1–12.3) | done |
| O-2 | Columnar blocks on disk, compaction, retention; restart from disk; measured (§12.4) | done |
| O-3 | Replication: the store as a hyper-durable state machine (§5); kiln carries Raft on its own lane, snapshots' files over HTTP; deterministic replicas (1,000-seed scenario), a group on real logs (crashes, partitions, images), live processes killed and restarted (§13); measured (§12.9) | done |
| O-4 | Query language: metrics (parser without recursion, evaluator, §12.5; answers agree to the bit on every replica), log search and analytics, events; the API (§7), described in OpenAPI 3.1 and held to what its routes answer; Datadog's whole function catalog, calendar rollups in any timezone (the tz database in the binary), `weighted()` (§6) | done |
| O-5 | Ingestion (§8): OTLP/HTTP and OTLP/gRPC metrics and logs (protobuf, JSON, gzip; gRPC over HTTP/2, an official SDK's exporters checked against it) on the server and every runner; every node's host, container and process; kiln's registry; kiln CI's jobs and runs, kiln CD's deploys and status; GitHub's webhooks; Datadog-style series, logs and events; DogStatsD on every machine (§8.4: Datadog's protocol over UDP and Unix sockets, aggregated as its Agent aggregates, checked live with Datadog's own client; 5 M messages a second a listener, §12.20); Prometheus's remote write 1.0 and 2.0 on the server and every runner, native histograms as distributions (§8.5, checked live against Prometheus 3.7, sample for sample; §12.21); Datadog's Agent at Datadog's own paths (§8.6: its metrics, sketches, service checks, events and logs, zstd read by kiln's own decoder; checked live against Agent 7.84); OTLP's logs decoded straight into rows, as spans are, but those a pipeline reads (a record 3× faster, no allocation a record, §12.10) | done |
| O-6 | Logs: LZ4 (§12.6), blocks with summaries and token filters (§12.7), Datadog's search syntax, the store's log head and files, OTLP logs, analytics (count, cardinality, sum, avg, min, max by facets, in time buckets; every facet's counts in one pass), allocating a group, not a row, a scan split among the queries' helper threads where its measure does not hang on the records' order (three times faster on five, §12.8); patterns (Drain, 0.887 on Loghub, §12.13); live tail (each record once as the replica applies it, a uniform sample past the rate asked, §6, §12.14) | done |
| O-7 | Monitors (§9): metric, change, log, anomaly; thresholds with recovery, warnings, NO DATA, renotify, mute, evaluation delay; Datadog's templates and mentions; anomaly detection (§9.1): Datadog's three algorithms, seasons learned, deviations calibrated per series, scored on NAB 52.6 (§12.12), `anomalies()` in queries with its band drawn; composites (Datadog's algebra and group matching, its table a test); forecasts (`forecast()` and forecast monitors; chosen and scored on M4, §12.15); outliers (`outliers()` and outlier monitors; DBSCAN's constant from a simulation, §9.1) | done |
| O-8 | The UI (§11): Observe's overview, dashboards (kiln's own and yours, template variables, edited in place; heatmap and distribution widgets), the metrics explorer, the logs explorer and event stream, monitors (composites among them) and integrations; checked by scenarios in Chromium and WebKit (§13); distributions (§4.4: percentiles merged across hosts and time) | done |
| O-11 | Service level objectives (§9.2): ratio and time-slice SLIs over rolling timeframes, the standing (SLI, budget left, day by day, burn rates), burn-rate and budget alerts as monitors; checked live (§13); their pages (§11): the list, each one's standing and days, the workbook's alerts for its timeframe in a click, made and edited in a form; checked in Chromium and WebKit (§13) | done |
| O-12 | Logs collected on every machine (§8.1): files by their globs (renamed, truncated, copied, deleted while read), Kubernetes' pods' and Docker's containers' logs with what they name, the systemd journal by its cursor, syslog received over UDP and TCP (RFC 5424 and 3164, RFC 6587's framings); Datadog's processing rules; macOS's unified log (`log stream`, and `log show` from its place after a restart) and the Windows Event Log's channels (`wevtutil`, each channel's record id its place); each line's place kept only once its record is written, so a stop repeats nothing and a kill at most the last second; checked live (a Mac's unified log; the Event Log against a stand-in for `wevtutil`, and on a Windows machine) and in browsers (§13), measured (§12.17); a JSON line read in place, no tree of it made (36 allocations a line to 18, 40% more lines a second); a line no pipeline reads, text or JSON, handed over as its row, in the order read beside records, a write's rows one item (a JSON line 18 allocations to 0.01, a text line 9 to 0.01; 50–100% more lines a second through the batcher, §12.17) | done |
| O-13 | Log pipelines (§8.2): Datadog's, its JSON read as it is: a filter and processors in order (the grok parser with Datadog's matchers, filters and date patterns; the date, status, service, message, trace id and attribute remappers; the URL and user-agent parsers; category, arithmetic, string builder and lookup processors; a pipeline nested in one), applied where records are taken on every machine; kiln's own for nginx and Apache (Datadog's rules); kept by the server and told to runners; their page with a record tried before a pipeline is kept; checked live and in browsers (§13), measured (§12.17). Allocations a record through kiln's nginx pipeline 75 to 49, a date read without a second expression, a date past 2262 no longer overflowing (§12.17). grok's own matcher, its groups in one pass (held to `regex` by differential tests), a line a pipeline reads written as its row where it is read with its strings' room kept: through kiln's nginx pipeline 282–292 k to 585–615 k lines a second, 48 allocations a line to 0.02 (§12.17). A record of one buffer (§8.2): a line a pipeline reads costs no string of its own, every processor held to the code before it by tests; what it saves measured against that code next (§12.17) | in progress |
| O-14 | Metrics from logs (§8.3): Datadog's definitions (a count, or a distribution with or without percentiles, by facets), counted on the server, the one writer, each window whole once its records have come (the 99th percentile of how late they come) and again when more come within twenty minutes; OpenTelemetry's cardinality limit; their page, a definition tried on the last fifteen minutes before it is kept; checked live and in browsers (§13). Next: counting measured at a fleet's rates | in progress |
| O-15 | Downtimes (§9.3): Datadog's, once or recurring (RFC 5545's rules in a timezone, their offsets' changes as RFC 5545 has them), for monitors by id or tags and the groups a scope holds; held groups' changes still events; as one ends, what is still wrong told once, a first recovery kept quiet when asked; their page and a monitor's line; checked live and in browsers (§13) | done |
| O-9 | Integrations (§10): Slack, Discord, Teams, PagerDuty, Opsgenie, email through an SMTP relay (checked live against Mailpit: STARTTLS, a login, a CA of its own), signed webhooks, GitHub; AWS CloudWatch, Google Cloud Monitoring and Azure Monitor pulled with given or ambient credentials (§10.1), checked live against services that verify as the clouds do (§13); Prometheus's scrapes by the server and every runner (§8); the clouds' logs (CloudWatch Logs, Cloud Logging, Log Analytics), each record once though every pull reads its lookback (§10.1); their pushes: Firehose (CloudWatch Metric Streams in JSON and in OpenTelemetry 1.0.0 and 0.7.0, Logs' subscriptions) and Pub/Sub (Cloud Logging's sinks), each delivery taken once (§10.2); Azure's event hubs read as they come over Kafka's protocol (a consumer of its own, offsets kept by kiln, each record once through restarts and a broker's), checked live against Apache Kafka's broker (§10.3) | in progress |
| O-16 | Synthetic tests (§10.4): Datadog's API tests, HTTP (its time cut into DNS, connection, TLS, first byte, download and redirects), TCP, DNS and TLS (a certificate's days left), from the server and runners as locations; Datadog's assertions and series; a location failing or recovering an event once; alerting by a monitor each test owns (at least N locations failing for a while); tried once from the server, run now everywhere; their page; checked live (a server and a runner, an endpoint switched to 500 and back, a TLS endpoint with a CA of its own) and in browsers (§13) | done |
| O-17 | Metrics summary (§7, §11): Datadog's, each metric's series by kind, each tag's values and its values of most series, the samples it brings (counted as they are applied, by metric; carried over when retention numbers the symbols anew) and of late a second, its newest sample; from the index, nothing read from disk; its page; checked by the API's conformance test, a unit test of the counts through retention, and in browsers (§13) | done |
| O-18 | Traces (APM, §8.7): OTLP traces over HTTP (protobuf, JSON) and gRPC on the server and every runner, each span decoded straight into its row (614 ns, no allocation a span past the request's, §12.23) in a span store of its own beside the logs (its own files and retention, replicated as logs are; formats a build that knows no spans still reads until the first span comes); found by their trace in the whole retention, their blocks' filters holding their ids (salted by the block: a trace reads its own block and 1% of the rest, whichever trace); the trace explorer's search syntax; a trace assembled as a waterfall draws it; Datadog's trace metrics (hits, errors, latency distributions, of entry spans and calls out by peer) counted on the server as metrics from logs are, and metrics from spans; services, resources and calls from them over any range; the service map from the spans, sampled by trace past 200,000; the API, described in OpenAPI; APM's pages (services, a service, the trace explorer, a trace's waterfall and its logs, the map); checked live against OpenTelemetry's Python SDK across two services by way of a runner (§13). Datadog's Agent's traces (§8.7: its indexed and inline forms, 532 ns a span, §12.23) and its stats as the trace metrics (every span counted before it sampled, its DDSketches' four mappings read within their accuracy; 6 µs and 0.2 allocations a grouped stat), checked live against Agent 7.84 and ddtrace 4.16 with traces kept and dropped (§13). Datadog's tracers straight to kiln on every runner (kiln in the Agent's place on its port: `/v0.4/traces`, `/v0.5/traces`, `/info`; 490 ns a span), every span counted by the runner's concentrator before a dropped trace is let go, as the Agent counts (133 ns a span); checked live with ddtrace 4.16, no Agent (§13). The Agent's priority sampler on every runner (each tracer told its services' rates, `rate_by_service`: ten traces a second a node, each service its share by water-filling), checked live: a busy service's rate falling to its share, every trace counted, kept or not (§13). A runner's last spans and buckets written as it stops, a bucket two of its lives write added and not written over, checked live across a stop (§13). The map's figures from the trace metrics, each edge its share of the groups that count its calls (the ratio estimator), so a sampled service's calls are every call: checked live through the Agent, straight to a runner under the sampler, and with OpenTelemetry's SDK (§13). CI runs as traces (a run its root, its jobs and steps under it, failures marked) and trace analytics monitors, checked live (§13) | done |
| O-19 | Error Tracking (§8.8): errors grouped into issues as Datadog groups them (within a service: their type, their message's words, the application's innermost frame of their stack; a user's `error.fingerprint`), of the errors requests failed with (a service's entry span's, or a Datadog tracer's top-most error under its erring entry) and of error logs; eight languages' stacks read as their runtimes write them (recorded from them); an `issue.id` on each error as it is taken, a stack kept whole to 25,000 bytes (both its ends past it); each issue's errors counted on the server (`error_tracking.errors` by issue, service, env and version); issues kept by the server with Datadog's statuses, fourteen quiet days resolving one, a new version's error a regression; the API (OpenAPI) and its pages; checked live with OpenTelemetry's Python SDK (spans and logs) and ddtrace through a resolve and a regression (§13); 1.1 to 2.8 µs an error (§12.24); Datadog's Error Tracking monitors (new issues and regressions, impact; §9) telling integrations, an ignored issue muted, checked live and in browsers (§13); reading an error without allocating, 0.6 to 1.1 µs (§12.24) | done |
| O-20 | Datadog's dashboards (§7, §11): their JSON imported as kiln's and any dashboard given back as Datadog's, every widget kept (drawn as Datadog draws it, drawn otherwise and said, or a note saying what it was); groups, pies; logs, spans and events as operands of formulas, as Datadog's formulas mix them (§6); Datadog's boolean filters, a search's value lists, `max` and `min`, arithmetic without spaces; Datadog's 678 integration dashboards taken and given back the same, 90.8% of their widgets drawn as Datadog draws them (with service checks', O-21), 105 µs an import (§12.25); checked in browsers (§13) | done |
| O-21 | Service checks (§9.4): each run, the Agent's (`/api/v1/check_run`) or DogStatsD's (`_sc`), a record and a sample of its status, read from its check's series; Datadog's service check monitor (`"check".over(...).by(...).last(n).count_by_status()`, its thresholds runs in a row, `{{check_message}}`); the check status widget, `POST checks/status`, Datadog's `check_status` imported and given back; the monitor form and page; checked live (a host alerting on its second failure in a row, not its first, its notice the run's message, recovering on its first OK) and in browsers (§13) | done |
| O-22 | Log archives (§10.5): Datadog's, to S3 (and S3-compatible stores), Cloud Storage and Azure Blob Storage with the cloud integrations' credentials (S3's requests signed with SigV4, the path once and the payload's hash), each record to the first archive whose search takes it; Datadog's objects (an hour's events a folder) and events; each record once through failures, restarts, installs and changes (each archive its place, a file kept to the archives it was begun with, a part written not written again); the store cut within 15 minutes when records come slowly; checked live against RustFS and stand-ins for Google's and Microsoft's stores and token services (§13) | done |
| O-23 | Rehydration from archives (§10.6): an archive's records of a range that a search takes read back from S3, Cloud Storage or Blob Storage (Datadog's format: what Datadog wrote reads alike) into a view of their own, searched, counted, faceted and patterned as the live logs are (`view`); objects read as they come off the wire (none whole in memory), a read cut short taken up where it stopped; each event read into its record in place (400 ns, nothing allocated; 1.8 million events a second a core as objects come, §12.27); a scan's bound; a view kept for its retention, read again whole when the server stopped in it; checked live against RustFS and the stand-ins, every read cut short (§13) | done |
| O-24 | Sensitive Data Scanner (§8.9): Datadog's scanning groups and rules, run on every machine where records are taken, after the pipelines, before anything is kept; the standard library (cards with their Luhn digits, IBANs with their checksums, SSNs in their ranges, personal data, network addresses, credentials of a dozen services, private keys, passwords in URLs and assignments), patterns of one's own; what a match becomes (a string, partly hidden, a keyed hash, or tags alone), keywords near, namespaces; the fast path kept for lines no rule may touch; the API, OpenAPI, its pages | done |
| O-25 | Delivery: DORA metrics (§10.7), Datadog's: deployment frequency, change lead time (each commit's, from kiln's mirrors), change failure rate (rollbacks, reverts and hotfixes found as Datadog finds them, or marked by hand) and failed deployment recovery time, from kiln CD's deploys and Datadog's DORA API; failures; levels from DORA's 2024 report; metrics and events of each; the API, OpenAPI, its page | done |
| O-26 | Service Catalog (§10.8), Datadog's: service definitions (v2.2) from Datadog's API and from `service.datadog.yaml` in the repositories kiln mirrors, services seen by their traces and deployments, each one's facts (requests, errors, latency, issues, monitors, SLOs, deployments, logs correlated) and Datadog's three scorecards; teams for DORA; the API, OpenAPI, its pages | done |
| O-27 | Infrastructure (§10.9), Datadog's host list: every host that reports (kiln's runners, Datadog's Agent, OpenTelemetry's resources, any series tagged `host:<name>`), whether it is up, its CPU, I/O wait, load, memory and disk, its apps, its platform and tags by source, its containers; what the Agent says of its host (its host metadata, checks and inventory) kept; a host muted (a downtime of every monitor's groups of it); the API (Datadog's list and totals), its pages; checked live and in browsers (§13), measured (§12.31) | done |
| O-28 | Incident management (§9.5), Datadog's: incidents declared (from kiln's pages, Datadog's v2 API, or a monitor naming `@incident`), their severity and state, commander, customer impact and fields; their timeline (notes, each change, their monitor's alerts and recoveries), todos and attachments (a postmortem, links); Datadog's figures of each (time to detect, respond, repair, resolve); told to the integrations they name (PagerDuty's incident and Opsgenie's alert closed as one is resolved); searched with Datadog's syntax, counted by facet; kept a file each within bounds; the API, OpenAPI, its pages; checked live and in browsers (§13), measured (§12.32) | done |
| O-29 | Notebooks (§11.1), Datadog's: Markdown and graph cells (Datadog's widget definitions, drawn as the dashboards draw them) over a time, fixed or the last of a span, cells' own times and sizes, types and templates; kept a file each, their heads alone read at the start; Datadog's v1 API (made, listed with its filters, order and pages, read, changed whole, removed); postmortems made of a template and an incident (Datadog's incident variables, kiln's template after the Google SRE book's) and attached to it; the charts on fixed times; their pages; checked live and in browsers (§13) | done |
| O-30 | Containers (§10.10): every container each node runs, read from Linux's cgroups (v2 and v1; Docker, containerd, CRI-O, Podman, Kubernetes' pods, nested nodes'), named by Kubernetes' links and the engine's API (its socket, std alone), the engine's stats where cgroups are not seen (Docker Desktop on macOS and Windows); Datadog's container check's names (CPU, throttling, memory and working set, OOM kills, I/O, processes, uptime); listed by the server from the store's index; the API, OpenAPI, its pages; checked live (real Docker containers: a quota's CPU and throttling, OOM kills) and in browsers (§13), measured (§12.34) | done |
| O-31 | Log indexes (§8.10): Datadog's indexes, exclusion filters (a rate, by an attribute's values or a trace), daily quotas (a warning, the quota reached, each an event; past it, records not kept for search until the day turns) and retention a part of the store each (§4.6: a file a part, each part's cutoff its own, replicated); routed where records are taken, every node told; what is not kept still seen live, archived and counted by metrics from logs; Datadog's estimated usage; the API, OpenAPI, the Indexes page and the explorer's pick of an index; checked live (§13) and measured (§12.35) | done |
| O-32 | OpenTelemetry's containers (§10.11): Docker's, the kubelet's and the cluster's receivers' container metrics and the semantic conventions', read in their own names and units (cumulative time a rate, a working set for what Docker calls usage, limits in cores and bytes, devices and modes summed), a container kiln's check also sees one container, the kubelet's series joined by pod and name; each chart of its page drawn by the source that reported it; checked live against a real Collector and in browsers (§13), measured against O-30 (§12.34) | done |
| O-33 | Continuous profiles (§8.11), Datadog's Continuous Profiler: what tracers' profilers upload (pprof in `multipart/form-data`, gzip'd, zstd'd or in LZ4's frames) taken where they send it (a runner's tracer port, sent on over the observe lane; the server's intake), kept by the server (segments of blobs and headers, an index of 40 bytes an upload), searched by tags, summed into flame graphs over a time (a snapshot's type averaged), compared path by path; its page (canvas flame graph, functions by self or by change, uploads); checked against Go's own profiles and live against dd-trace-go and ddtrace through a runner and Datadog's Agent; measured (§12.36) | done |
| O-34 | Java's profiles (§8.11): JFR recordings (dd-trace-java's `main.jfr`: the JDK's chunks and ddprof's, zstd'd, gzip'd, LZ4'd or not) read where they are taken into pprof profiles in Go's names: CPU (ddprof's or the JDK's), wall time, allocations, the live heap, lock waits, exceptions, file and socket I/O; a runner sends the profiles on, not the recording; frames named as Datadog's pages name them; checked against a reader written apart on dd-trace-java's own recordings (to the nanosecond), live against dd-trace-java 1.67.1 through a runner and in a browser; measured and profiled, rewritten (§12.37) | done |
| O-35 | LLM observability (§8.12), Datadog's (its "Agent Observability"): LLM spans recognized in OpenTelemetry's GenAI conventions (every version), OpenInference's, OpenLLMetry's and Claude Code's, given Datadog's fields; costed from genai-prices' catalog (Datadog's), held to its own package; Datadog's LLM intake and evaluations; `ml_obs.*` metrics; its page; checked live through a runner (Datadog's tracer with APM on and off, its evaluations, OpenTelemetry's GenAI instrumentation, OpenInference's: §13) and in browsers (Chromium and WebKit, a desktop's width and a phone's), 2026-10-24. Next: what recognizing them costs the trace decoders, measured against the decoders before (§12) | under way |
| O-10 | Load, soak and fault scenarios end to end; allocation and fault passes. Done: the flood and the server-away scenarios (§13), OTLP decoding and the sample path (§12.10), blocks and merges (§4.5); measured in §12.11. Full disks and clock skew done (§13). The soak (§13: memory holds its level). Sampling profiles (perf): decoding a third faster (§12.1, §12.5), OTLP a fifth (§12.10), requests' buffers handed back. Rollups (§4.7): five-minute and hour tiers in blocks, every replica's answers the same to the bit, read rolled up or not; a month 17 times faster; part of a long region read without the rest (§4.5, §12.16). Two hours' soak passing (§13): every point and record stored once, the replicas agreeing, memory level. A dashboard's widest query (§12.19): its time its own reading, not a wait; a run of repeated samples decoded a word at a time, series read on helper threads in rounds, added in their fingerprints' order (the same bits): 1,527 ms to 74 ms. The store's files written beside queries (§4.5). Merges join their inputs' rollups rather than make them anew: 38% less merging time (§4.7, §12.16). A query's series copied under one hold and read with the store let go (snapshot reads), and a store lock fair to writers on every platform (std's on macOS and Windows let readers that come back starve a writer, §4): measured against the build before them next, and the snapshot's bytes derived from that measure (§12.19) | continuous |

## 1. Rules

The engine is held to hyper-raft's law (`hyper-raft/CLAUDE.md` §1, §1a) within kiln's lint wall:

- **No arbitrary numbers.** Every constant cites its source or states the measurement it was derived
  from; a tunable is learned from data or exposed as configuration.
- **Bounded growth.** Every buffer, cache, queue and table has a derived bound and a typed refusal.
- **Measured costs.** Allocations, reallocations and page faults are counted per operation on every
  hot path (hyper-measure's counting allocator and `getrusage`), recorded with each benchmark in §12,
  and a change that raises them does not land without a measured reason.
- **Behaviour, not implementation.** Unit tests guard codecs and parsers; scenarios prove the system:
  real processes, real sockets, real disks, faults injected (§13).
- **No panics, no `unsafe`, no unchecked indexing** (the workspace lints). Every failure is a typed
  error the caller handles.
- **Few dependencies.** The engine adds none beyond what kiln has; formats (Gorilla, OTLP's
  protobuf, the query language) are implemented here against their specifications.

## 2. Pieces

```
            OTLP/HTTP  kiln CI/CD  runners' hosts  GitHub  AWS/GCP/Azure  Prometheus text
                 \         |            |            |          |            /
                  ------------------ ingest (any node) ------------------------
                                        |  a write: an entry of the group's log
                                        v
          observe group: the server leads (its one voter), runners learn
          (Raft log = the write-ahead log; hyper-durable on hyper-log)
                                        |  apply
                                        v
              per replica: symbols, series index, head (Gorilla), blocks on disk,
                           log blocks, events            (crates/kiln-obs)
                                        |
                         query engine (local, any replica) — monitors — API — UI
```

- `crates/kiln-obs`: the engine. Codecs, the store, the query language, the evaluator, monitors'
  rules, OTLP decoding. No runtime, no sockets: the store is driven like hyper-raft's crates, fed
  write batches and asked queries.
- `crates/kiln/src/observe/`: kiln's side. Ingest endpoints, the replica's driving and its Raft
  transport over kiln's QUIC links, the HTTP API, monitors' scheduling and notifications, the UI's
  data.

## 3. Data model

**Metrics.** A point is `(metric, tags, ts, value)`: `metric` a dotted name (`kiln.ci.job.duration`,
`system.cpu.user`), `tags` a set of `key:value` strings (Datadog's convention; OTel resource and
point attributes map to tags), `ts` milliseconds since the epoch, `value` an `f64`. A series is a
metric with one tag set. Kinds:

| kind | stored as | rollup default | OTel source |
| --- | --- | --- | --- |
| gauge | the value | avg | Gauge; Sum with non-monotonic cumulative |
| count | delta per interval | sum | Sum, delta temporality |
| counter | cumulative value, resets detected at query time | rate | Sum, monotonic cumulative |
| distribution | its exponential buckets, each a count series (§4.4), and its count, sum, least and most | percentiles (`p99:`), avg, sum, count, min, max | ExponentialHistogram; Datadog's distribution points |

**Logs.** `ts`, `severity` (OTel's 1–24), `service`, `host`, `source`, `message`, `attributes` (a
flat map of `@`-addressable fields), `trace_id`, `span_id`.

**Events.** `ts`, `title`, `text`, `source` (kiln, kiln-ci, kiln-cd, github, aws, ...), `tags`,
`priority`, `alert_type` (info, success, warning, error), `aggregation_key`.

**Spans.** Records of a store of their own (§8.7): `ts` the span's start, `message` its name,
`service`, `host`, `source` (its instrumentation scope), severity (error or ok), `trace_id`,
`span_id`, and attributes: the span's and its resource's, and kiln's own (`span.kind`,
`duration` in ns, `parent_id`, the error, `peer.service`, its events and links).

## 4. The store (one replica)

### 4.1 Symbols and series

Every string (metric names, tag keys and values) is interned once into a symbol table: `u32` ids,
bytes in one arena. A series is `(metric id, sorted tag ids)`; its id is a dense `u32` assigned in
order of first appearance, so series ids index arrays directly. Lookup by content goes through a
hash table keyed by a 64-bit hash of the canonical form with full comparison on collision.

### 4.2 Index

Postings: for the metric and for each tag (`key:value`) a sorted list of series ids. A query's tag
filter is an intersection (and unions for `OR`, wildcards, `IN`) evaluated smallest list first with
galloping search (Bentley and Yao 1976; Demaine, López-Ortiz and Munro, SODA 2000): `O(m log(n/m))`
for lists of sizes `m ≤ n`. Negations subtract. Wildcard values (`host:web-*`) expand through the
symbol table's sorted value index per key. A boolean filter (`OR`, `NOT` of more than one condition,
§6) is evaluated within the series the metric and its ANDed conditions leave: each node's set from
its children's (intersection, union, the universe less it), the nodes in the order kept, so no
recursion however deep its brackets.

### 4.3 Samples: Gorilla chunks

Samples are encoded as in Gorilla (Pelkonen et al., VLDB 2015): timestamps as delta-of-delta with
the paper's variable-length buckets, values as XOR with the previous value with leading/trailing
zero reuse. The paper measures 1.37 bytes a sample on Facebook's production data; §12 records what
kiln's own data gives. A chunk is closed after `CHUNK_SAMPLES` samples or when its time span reaches
`CHUNK_SPAN`; both are derived in §12.2 from the compression curve measured on recorded data (the
point past which longer chunks no longer pay for the extra decode work a query does to reach a
sample inside them).

Out-of-order samples: a point older than its series' open chunk's last timestamp goes to the series'
small out-of-order buffer (bounded, sorted on flush) and is merged when the head is cut into a block.

### 4.4 Distributions

A distribution is a base-2 exponential histogram's buckets, OTel's (`kiln_obs::dist`): a value `v`
falls in bucket `i` at scale `s` when `base^i < |v| ≤ base^(i+1)`, `base = 2^(2^-s)`, negatives in
mirrored buckets, zero in its own; DDSketch's logarithmic mapping (Masson, Rim and Lee, VLDB 2019).
A bucket at scale `s` is bucket `i >> k` at scale `s − k`, so histograms of different scales merge
exactly at the coarser one: percentiles across hosts and time are mergeable, as averages of
percentiles are not.

Each bucket is a series of its own, `<metric>.dist{…, dist_bucket:<scale>:<bucket>}` (`5:p37`,
`5:n4`, `5:z`), its value a count of its interval, beside `<metric>.count` and `.sum` (counts of the
same interval) and `.min` and `.max` (gauges): the store holds nothing it did not hold before, so
replicas agree as they always have, and a distribution is summed across series and time as any
count is. Buckets are kept at scale 5 at the finest (32 an octave): a percentile is read at its
bucket's geometric middle, within `√base − 1` of any value in it, 1.09% (Datadog's distributions
promise 1%; scale 6 would halve it and double the buckets). OTel SDKs begin at scale 20 and shrink to
fit 160 buckets, most often to 3 to 5 for a latency's range, so a finer one's buckets are made
coarser on the way in, exactly.

A percentile (`p99:<metric>{…} by {…}`) merges each group's buckets for each bucket of time at the
coarsest scale among them and takes the rank `q (n − 1)`'s bucket (DDSketch's). `avg` is the sums
over the counts, `sum`, `count`, `min` and `max` the distribution's own; a heatmap (§7) its merged
buckets in each bucket of time. Measured (`a_merged_percentile_is_within_its_relative_error`): over
log-normal, Pareto and two-sided shapes, 40,000 values merged from parts at scales 5, 7 and 8,
every percentile from p0 to p100 within the bound.

A cumulative histogram (OTel's default temporality) comes to a receiver as its counts since it
began; a bucket that holds values for the first time would be a new series whose first point has no
increase, and the tail's counts would be lost. So a receiver turns them into deltas, as the
OpenTelemetry Collector's `cumulativetodelta` and the Datadog Agent do: it keeps each series' last
buckets, count and sum (32 MiB at the most), and gives each point's increase over them; a fall, or a
new start time, is a reset, all of it anew. A series' first point is all new if it began after the
receiver did; else what came before it is not known, and it gives nothing (the next does). That
holds while an exporter's series come to one receiver, as an app's to its machine's runner.

### 4.5 Head and blocks

The head holds recent samples in memory: each series' open chunk and its closed chunks. The head is
cut into an immutable **block** file when the group compacts its log (§5: the log is the
write-ahead log, so a cut is what lets it be trimmed): once it holds half a cadence of entries
(32 MiB), and when entries come slowly, 15 minutes after the first of them not yet cut
(`kiln_obs::group::CUT_AFTER`; Datadog's archives' own batch): what waits on a cut, a log archive's
objects (§10.5), waits that long at the most.

```
block   = MAGIC | FORMAT 3 | data | index | footer
data    = each series' region and its rollups, by fingerprint
region  = table length, table crc32c | table (chunks | per chunk: first, last-first, count, length,
          crc32c) | chunks
rollups = crc32c | its rollups (§4.7), right after its region
index   = per series, by fingerprint: fp | kind | key | region offset, length, crc32c | first, last
          | rollups length
footer  = index offset, length | min ts | max ts | crc32c(index, footer) | TAIL
```

An open block holds, of each series, its fingerprint and where its region is (some 48 bytes):
nothing a chunk or a key, so what a store holds in memory grows with its series and its blocks, not
its samples (Prometheus's blocks and ClickHouse's parts read their marks from disk the same way).
A read of a series' whole region, or of a region of a page or less (`block::READ_WHOLE`), is one
positioned read checked against the region's CRC-32C. A read of part of a longer one (a day of a
month's block) reads the table, checked against its own CRC-32C, and then the run of chunks the
range overlaps, each checked against its own: a day of a month's block reads a thirtieth of it,
not all of it, and damage in a chunk is found by a read that wants that chunk (a page is the
measured break-even, §12.16; the checksums cost 4 bytes a chunk, 1.1% of a block). Keys are read
from the index when the store opens (to rebuild its index of series) and when blocks merge. Blocks
of format 2 (a table after its length as a varint, without its checksum or its chunks', no
rollups) are still read, whole, until a merge rewrites them; a build before format 3 cannot open
format 3's, and a replica that fetches one (§5) refuses it until it is upgraded too.

Blocks merge by the logarithmic method (Bentley and Saxe 1980): the oldest neighbouring pair whose
newer block holds at least half the older's is merged, in its place, until none is, so a store keeps
O(log n) blocks and a sample is rewritten O(log n) times. (Merging only the newest pair stalled: a
busy group cuts a big block and then a small one, a compaction's and a snapshot's, and a small cut
always sat at the newest end.) A merge is written as it is read, a series at a time: a series one
input holds is copied whole, its rollups with it; inputs whose spans of a series follow one another
have their chunks appended (a Gorilla chunk stands alone; their rollups joined from the inputs',
§4.7); only overlap (late writes), retention, chunks under half full and format 2's regions are
decoded and packed anew. Maintenance runs on a thread of its own, told of every cut, and
takes a merge in only if its inputs are still there. A manifest (rewritten by rename) lists the live
blocks and the log point the head was cut at; files wholly past retention are deleted, a block more
than half past it rewritten.

**Files changed beside queries.** A query holds the store shared; applying holds it alone, and a
writer waiting holds every new reader back (Rust's lock prefers writers, as most do). So nothing
writes a file under the exclusive hold: a cut writes its block and logs file, syncs them and saves
the manifest under a shared hold (the group's thread is the one cutting, so nothing applies
meanwhile, and the head it reads stays as it read it), then takes them in under an exclusive hold
that only moves the head's chunks out and the files in; a merge saves its manifest with no hold at
all and deletes its inputs after; so do expired files. Each change of the files holds the manifest
lock throughout, taken before the store's own and never while holding it: one change at a time,
and none waiting for the store's exclusive hold while another reads it to write (`cut_shared`,
`commit_shared`, `drop_expired_shared`; a test runs them against a reader, a maintenance thread and
a cutting one, and opens the files again after).

### 4.6 Logs and events

Log blocks are column chunks of `LOG_BLOCK_ROWS` rows: `ts` delta-encoded varints; `severity`
run-length encoded; `service`, `host`, `source` dictionary-encoded and run-length encoded; `message`
and `attributes` compressed together per block; trace ids raw. Each block carries the minimum and
maximum of `ts`, the dictionaries, and a Bloom filter over the message's tokens sized for
`LOG_BLOOM_FPR` (Bloom 1970; `k = (m/n) ln 2` hashes with Kirsch and Mitzenmacher's double hashing,
2006), so a search reads only blocks that may match and the time range they cover.

Spans (§8.7) are records of a store of their own beside it, the same engine: their own head,
blocks and files (`.spans`), their own cutoff, never in a log search, nor a log monitor's, nor the
live tail. Spans come ten to a hundred times as many as logs, so a store of their own keeps a log
search from reading their blocks, and lets them be kept for their own time (Datadog keeps indexed
spans 15 days: `spans_retention_days`). A block of spans' filter holds its trace ids (§8.7).

A store's logs have **parts** (`kiln_obs::logs::store::Part`): an index's records (§8.10) are a
part of their own, with its own head, blocks and files (a cut writes a file a part, named by it:
`0000000000000042.p00003.logs`) and its own cutoff, so an index kept for a week drops whole files a
week on, whatever the others keep. The main part is every record before indexes (and Datadog's
`main` index's); kiln's events are a part of their own; records not kept for search (excluded, of
no index, past a quota) are one more, kept an hour. A batch's rows name their part (`LOGS_IN`, the
main part's `LOGS` as before); a part's cutoff moves by a command of the leader's, `RETAIN_PART`,
stated only for a part kept for its own time, so a group that never had one never sends an entry a
build that knows no parts would stop at (the manifest is version 4 and the image `KOIMAGE3` only
then, as spans made them 3 and `KOIMAGE2`). A read takes the parts it asks for, each from its own
cutoff: a search, those kept for search (`Store::logs_at`); metrics from logs, archives and the live
tail, every part.

Events are records too, of source `kiln.event`: one store, one search. But they are not logs: a
monitor's own alerts are events at error's severity, and a monitor of `status:error` that counted
them would count its own alerting. A log monitor, and the logs explorer, read logs only
(`kiln_obs::monitor::logs_only`: the search and `-source:kiln.event`) unless the search names
`kiln.event`; the event stream reads events only.

A replica publishes the log records it applies to a **feed** (`kiln_obs::logs::tail`) while anyone
follows them (live tail, §6): each batch's rows copied once into a ring bounded at 32 MiB, kept until
every follower has read them or the bound passes them by (a follower that fell that far behind is
told how many rows it missed; none holds the replica up). With no one following, applying costs a
load. What a replica applies again is not published again: opening, it applies the entries after its
last cut once more, and the feed skips every entry its log held as it opened.

### 4.7 Rollups

A query over a month would read every sample of it if nothing stood in for them. Each series in a
merged block keeps **rollups** (`kiln_obs::rollup`): for each tier, each window of the tier's width
it has samples in, their count and what its kind's rollup functions read of them (a gauge's or a
count's sum, least, most and last; a counter's first, last and its increases after its first). The
tiers are five minutes and an hour, Thanos's downsampling resolutions (Mimir's and Cortex's too):
every step of five minutes or more on the grid's ladder (§6) is a multiple of five minutes, every
one of an hour or more of an hour, and a grid begins at a multiple of its step, so the buckets of a
long range are whole windows. So are a calendar's days in every timezone in use (offsets are whole
quarter hours): New York's are whole hours, Kathmandu's (+5:45) whole five minutes.

**The same answer, rolled up or not.** Replicas cut and merge their blocks at their own times, so
which windows a replica holds rolled up is its own. Floating-point addition does not associate:
a bucket summed from one window's aggregate on one replica and from that window's samples on
another would differ in its last bits, and every replica answers to the bit (§5). So a query whose
buckets are whole windows of a tier is *defined* window by window: each window's samples folded in
time order, then each bucket's windows folded in time order (§6). A window's rollup is that fold,
made once; read from it or from the samples, the window is the same. The tier is a query's, not a
store's: the coarsest whose width divides every bucket edge. A grid of finer steps, `.weighted()`
(its interval is learned from the samples' gaps) and a shift by other than a multiple of the tier
read samples as before. A scenario holds it (`rollups_answer_to_the_bit_as_samples_do_whatever_the_blocks`):
three replicas given the same writes, two cutting and merging on schedules of their own and one
holding all in its head, answer every such query alike to the bit, rollups read or not (§13).

**What a read takes from rollups** (`Store::read_rolled`): a window wholly in the range that one
block alone holds of the series (no other block's span, nor the head's, reaches into it), so all
its samples are there. A window a cut fell inside is read from both blocks' samples, as is one
with a sample that is not finite (kept as a count of 0), a window at the range's edge, the head,
and a series whose blocks overlap (late writes not yet merged). With O(log n) blocks, that is a few
windows a series, read in part (§4.5).

**Which tiers a block keeps.** A tier is kept for a series when it takes at most a quarter of the
bytes of the samples it stands for, weighed at each merge from the encoded sizes
(`rollup::KEEP_RATIO`). On kiln's corpus (§12.16) the hour's tier is kept for 98% of the samples and
the five minutes' for 6% (a 30 s scrape's five minutes are ten samples, which Gorilla packs nearly
as tightly as their aggregates), adding 7.4% to the samples' bytes. A ratio of 2 would add 26% for
11 more points of the five minutes'; 8 would drop the hour's to 88%: 4 is the knee.

**Encoded.** A tier is its windows (each one's index and count, varints) and a column of each
aggregate, Gorilla's XOR of each window's value with the one before (`gorilla::write_xor`, the
chunks' own coding): a series that holds its level writes a bit a window. Each tier and its window
list lead with their byte lengths, so a reader finds its tier and the columns it wants (an average
its sums, a counter its first, last and increases) without reading the rest.

**Made at merges, joined at later ones, checked when fetched.** A cut writes none (it decodes
nothing, under the store's hold); the first merge to take its block makes them. A merge that copies
a series' region whole copies its rollups; one that appends joins its inputs' (`rollup::Joiner`):
each input's windows read and appended, and a window a cut fell inside carried on with the next
input's samples in it, its fold continued in time order, so the window is what its samples make of
it to the bit, as made from the samples (only those samples are decoded: the rest of the input is
copied as it is). Inputs that keep other tiers than each other, or none (a cut's block), have them
made instead, as the chunks are read, every tier in one pass, holding a series' windows and not
its samples (a week's are 2,016 and 168); one that packs anew, from the samples it holds to pack.
Making them cost merges 17% more time (measured); joining them takes it back (§12.16). A test cuts
200 series of each kind at random times (inside windows, on their edges, parts inside one window;
samples that are not finite; counters that reset) and holds the joined rollups to the bytes made
from the whole; the replicas' scenario (§13) checks every block its merges wrote, rollups recomputed
from its samples. A block fetched from another replica (§5) is verified with every series'
rollups recomputed from its samples, window by window, to the bit (which tiers it keeps is its
writer's to choose): a replica's rollups are what keeps its answers its peers', so they are
recomputed, not trusted. Measured on 30 days of kiln's cluster (§12.16): a month at a chart's 240
points 17 times faster (634 ms to 36 ms for every node series by instance), a week 10 times.

## 5. Replication

The store on each member is a `hyper_durable::StateMachine` (`kiln_obs::machine`). The Raft log is
its write-ahead log; an entry holds one command (`kiln_obs::command`): a write (a batch, its sender
and its place in the sender's order), a retention (the cutoffs for samples and for log records), or
a bound on series.

**Members.** The server's member (1) is the group's one voter, and so its leader: it commits alone,
so no runner holds a write up and a runner that sleeps costs nothing. Every runner that replicates
(all but job pods, the server's in-process agent, and those started with `--no-observe`) is a
learner, which takes the log, or an image and its files, and answers queries on its own machine.
Runners reach each other only through the server, so a quorum of runners could never stand without
it: a voter on a runner would add a wait to every write and no availability. The newest
replicating runner process of a machine replicates; another of the machine lets go of the folder
(a file lock, as sync's).

**Determinism.** Replicas that apply the same entries answer every query alike, whenever each cut,
compacted, retained or restarted (§13):
- a series is defined with the first of its samples kept, never alone, so every series is in a
  head or a block and a replica that opens from its blocks holds the series the others hold;
- retention is an entry: the cutoffs move at one point of the log everywhere, a series goes once
  its newest sample is older, and files are deleted only behind the cutoffs that are durable;
- nothing older than a cutoff is kept by an entry or read by a query;
- each sender's last place is kept (client sessions, Ongaro's thesis §6.3), so a write sent twice
  (its answer lost, its leader gone) is applied once; a session goes once its last write is older
  than the later cutoff, or past `MAX_SESSIONS`, the oldest first;
- queries visit series in fingerprint order (floating-point addition does not associate) and order
  groups by their tags; log search breaks time ties by the record's contents.

An entry of a command or batch format this build does not know stops the replica (a newer build
applies it; skipping it would leave another state), and the owner says why. Spans came after:
their rows ride a write as an item of their own (`SPANS`), their retention is a command of its own
(`RETAIN_SPANS`, proposed only once the group holds spans), and a manifest or an image that holds
spans says so in its format (`kiln-obs manifest 3`, `KOIMAGE2`); a group that never took a span
writes nothing a build that knows none would stop at, so runners of either build replicate it until
the first span comes.

**Persist, image, install.** `persist` cuts the store at the last entry applied and records the
point with the replicated state there (the manifest: cutoffs, bound, sessions, configuration).
An image is small whatever the store holds: that state and the store's files by name, size and
footer checksum; the files themselves are hard-linked into a snapshot of their own (copied where
links are refused), so compaction and retention cannot take them from a member fetching them, and
the newest two snapshots are kept. A runner that is sent a snapshot fetches its files over HTTP
(`GET /api/observe/v1/snapshot/{point}/{file}`, the machine's token), checks each whole (every
series' and text's checksum, every sample decoded) into a staging folder, and only then steps the
snapshot, as TiKV sends a region's SST files beside the Raft message that names them. `install`
moves the staged files in under names of the store's own, saves the manifest (the point the
install turns on: a crash before it leaves the old store and orphans that are deleted at open), and
opens the store again.

**Maintenance** (merges, retention's rewrites) runs on a thread of its own: a rewrite is planned
under the store's lock with its file's name reserved, built from the immutable files with no lock
held, and taken in under the lock only if the store still holds every input (an install bumps an
epoch). The thread is joined before a store is opened again on its folder. Queries share the store
under a read lock that prefers a waiting writer on every platform (`kiln_obs::lock`), so a query
holds applying off for no longer than one query takes. std's lock does so on Linux alone: on macOS
(its queue) and Windows (an SRW lock) a reader taking it again the moment it let it go held a write
off for good (2026-10-24: a test of writes beside queries ran 20 minutes on a Mac before it was
stopped, its writer still waiting); `parking_lot`'s, task-fair, has a reader wait while a writer
does, everywhere. The pipelines, scanning groups and indexes each node applies, and the profile
store's index, are held by it too: each is read over and over and must take a change.

**Writes.** A node gathers its points by series (`crate::observe::ingest`) and sends one write at a
time, as the sessions assume, again and again until it is answered: a runner's go on the lane to the
server, which proposes them and routes their answers back. A series is defined in a write until a
write of the sender defining it is applied (and again an hour after); one the group does not know
is sent again with its definition. Gathering follows the OpenTelemetry Collector's batch processor
(200 ms); a write is never larger than the group's entry (1 MiB).

**The lane.** `LANE_OBSERVE` (5) carries the group's messages and writes as JSON beside their bytes,
payloads past 512 KiB in parts put back together under bounds (16 MiB a payload, sixteen at once
from a side, a minute for its parts).
Snapshot files go over HTTP so that a big image never waits ahead of a heartbeat.

## 6. Query language

One language for metrics, logs and events. Metric queries follow Datadog's syntax, which kiln's
users know:

```
query      := expr ( "," expr )*
expr       := term (("+" | "-") term)*
term       := factor (("*" | "/") factor)*
factor     := number | "(" expr ")" | function "(" args ")" | metric_q | records_q
metric_q   := [agg ":"] metric ["{" filters "}"] ["by" "{" tags "}"] modifier*
records_q  := ("logs" | "spans" | "trace-analytics" | "events") "(" '"' search '"' ")"
              [".rollup(" '"' measure '"' ["," '"' facet '"'] ")"] [".by(" '"' facets '"' ")"] [".index(" '"' * '"' ")"]
agg        := avg | sum | min | max | count | p50 | p75 | p90 | p95 | p99 | pNN.N
            | "count(v: v" ("<" | "<=" | ">" | ">=") number ")"   // a distribution's values that pass
filters    := cond (("," | "AND" | " ") cond | "OR" cond)*   // NOT binds tightest, then AND, then OR
cond       := ("!" | "NOT")* (filter | "(" filters ")" | "*" | "$" variable)   // "*", a variable: all
filter     := key ":" value | key ":" | key ["NOT"] "IN" "(" value ("," value)* ")"
                                              // value: exact, wildcard "web-*", (a OR b), or
                                              // "quoted" (any characters, exactly; \" and \\ within);
                                              // key ":" alone: any value of it
modifier   := ".rollup(" fn ["," seconds | "," calendar] ")" | ".rollup(" seconds ")" | ".as_count()" | ".as_rate()"
            | ".fill(" how ["," seconds] ")" | ".weighted()"
calendar   := ("daily" | "weekly" | "monthly") ["," alignment] ["," timezone]
function   := name "(" expr ("," arg)* ")"        // Datadog's catalog, below
```

Examples:

```
avg:system.cpu.user{host:web-*} by {host}
sum:kiln.ci.job.finished{status:failure}.as_count().rollup(sum, 3600)
p99:kiln.http.request.duration{route:/api/*} by {route}
100 * sum:kiln.ci.job.finished{status:failure}.as_count() / sum:kiln.ci.job.finished{*}.as_count()
anomalies(avg:system.load.1{host:web-1}, 'robust', 3)
sum:kiln.ci.job.finished{*}.as_count().rollup(sum, weekly, monday, 'America/New_York')
```

The functions are Datadog's, every one its pages list, by its categories:

| Category | Functions |
| --- | --- |
| Algorithmic | `anomalies`, `forecast`, `outliers` (below; §9.1) |
| Arithmetic | `abs`, `round(q[, places])`, `ceil`, `floor`, `pow(a, b)` (each a query or a number), `log2`, `log10`, `cumsum`, `integral`; `+ - * /` between queries and numbers, spaces or none (`a*100/(b+c)`); `max(a, b, ...)`, `min(...)` of queries and numbers: at each point the largest (least) value present, a group only some arguments have keeping theirs (Datadog's own dashboards take `max(default_zero(otel), default_zero(agent))` for one figure under either name) |
| Count | `count_nonzero`, `count_not_null` |
| Exclusion | `exclude_null`, `clamp_min(q, n)`, `clamp_max(q, n)`, `cutoff_min(q, n)`, `cutoff_max(q, n)`; kiln's `clamp(q, lo, hi)` |
| Interpolation | `.fill(null\|zero\|linear\|last[, seconds])` (`.fill(0)` is zero), `default_zero` |
| Rank | `top(q, n, 'mean'\|'min'\|'max'\|'sum'\|'last'\|'area'\|'l2norm', 'desc'\|'asc')`, `bottom`; the shortcuts `[top\|bottom][5\|10\|15\|20][_mean\|_min\|_max\|_last\|_area\|_l2norm]` |
| Rate | `per_second`, `per_minute`, `per_hour`, `dt`, `diff`, `monotonic_diff`, `derivative`, `rate`, `throughput` |
| Regression | `robust_trend`, `trend_line`, `piecewise_constant` |
| Rollup | `.rollup(…)`, `moving_rollup(q, seconds[, 'avg'\|'sum'\|'min'\|'max'\|'count'])` |
| Smoothing | `autosmooth`, `ewma(q[, span])` and `ewma_3\|5\|10\|20` (and 1, 7), `median_3\|5\|7\|9`, `rollingavg_5\|13\|21\|29` (Datadog's beta) |
| Timeshift | `hour_before`, `day_before`, `week_before`, `month_before`, `timeshift(q, seconds)`, `calendar_shift(q, '-1d'\|'-2w'\|'-1mo'[, timezone])` |

Where a page says what a function does, it does that, each such sentence a test
(`datadog_s_functions_do_what_their_pages_say`): a clamp makes a missing point its bound, a cutoff
makes a point past its bound missing; the counts are across a group's series at each point, one
series; `exclude_null` drops a group whose tag value is missing (`N/A`); `median_N` and
`rollingavg_N` are of the last N points; `ewma`'s weight is 2 / (span + 1), span 20 unless said;
`month_before` is 28 days; `area` is signed. Where a page names a method and not its workings, the
method is the literature's: `robust_trend` fits Huber's line (k = 1.345, iteratively reweighted,
the scale the residuals' MAD); `piecewise_constant` finds its steps by PELT (Killick et al. 2012),
the penalty BIC's 2σ² ln n, σ from the MAD of first differences over √2; `autosmooth` is ASAP (Rong
and Bailis, VLDB 2017): the moving average's span that is smoothest while its kurtosis stays at
least the series' own, so noise goes and what stands out stays (a wave without spikes is left as
it is: smoothed, it would read flatter than it is).

**Rollups.** `.rollup(fn, seconds)` sets a bucket's width (never finer than asked); a query's grid is
otherwise the smallest nice step that fits the range in the points asked. `.fill(how, seconds)`
fills a gap (from the point before it to the point after) of at most `seconds`, 300 unless said
(Datadog's default; wider is allowed), and leaves a wider one a gap whatever the way: `zero`,
`linear` between its ends, or `last`, the value before it; `default_zero()` fills every gap.
**Calendar rollups**, `.rollup(sum, daily|weekly|monthly[, alignment][, 'timezone'])`, are
Datadog's: a day begins at a time of day (`12am` unless said; `9:30am`, `21:30`), a week on a day
(`sunday` unless said; `monday`), a month on a day of it (the `1st` unless said; a month without the
day begins on its last), in an IANA timezone (UTC unless said). The grid is then the calendar's
periods, each from its own start to the next: a month its days, a local day 23 or 25 hours where
the clocks change (a local start a clock skips is the instant after the gap; one it repeats, the
first); the answer says each bucket's start (`grid.times`) and the period. Rates divide by each
period's own length. Every rollup of a query names the same calendar. The timezones are the tz
database's, carried in kiln's binary (jiff's copy, already kiln's), never the machine's: every
machine answers alike, Windows and containers without a zoneinfo too.

**Window by window.** When every bucket of a query's grid is whole windows of a rollup tier (§4.7:
a step of five minutes or more, a calendar's periods), a bucket is folded window by window: each
window's samples in time order, then the bucket's windows in time order (a sum is the sum of its
windows' sums; an average that over their counts; a counter's increase into a window from the
sample before it, then its increases after its first sample). It is what lets a window be read
from its rollup or from its samples alike; it differs from folding the bucket's samples in one run
only in the last bits of a sum.

`calendar_shift(q, '-1d', 'America/New_York')` reads the same local time a day (or weeks, or
months) before: on a calendar grid of its timezone shifted by whole periods (`-1d` of days, `-1w` of
days or weeks, `-1mo` of months) the periods that many before, a 23-hour day included; otherwise
each bucket's edges, each moved to its own local time the days before (an hour that day did not
have reads nothing). A month back from a month's last days would land on a shorter month's last more
than once, so a shift of months that would is the range's end's, throughout.

**`weighted()`** (Datadog's, for gauges whose tag values come and go, a pod's or a container's): a
series counts in a bucket for the share of it its samples cover, their count times its submission
interval over the bucket's width (a pod that lived 10 s of a minute counts a sixth). The interval is
learned: the median gap between the series' own samples (a series with too few to tell takes the
median of the others'). A sum adds each series' value times its share, an average divides that by
the shares (each pod weighed by its time), a count is the shares (pods alive on average); a minimum,
a maximum and a percentile take the values as they are. It weighs averages: with `rollup(avg)` or
none. Datadog's own worked example (six pods, two versions) is a test: 36 and 46 plain, 12 and 16
weighted.

A **distribution** (§4.4) answers its own: `p99:http.server.duration{service:web} by {route}` is the
99th percentile of every value of the group's series in each bucket of time, its buckets merged;
`avg` its sums over its counts; `sum`, `count`, `min`, `max` its own. Of any other metric `pNN:` is
the percentile across the group's series' values.

`outliers(query, 'dbscan'|'mad'|'scaleddbscan'|'scaledmad', tolerance, pct)` (§9.1): which of each
group's series behave unlike their peers over the range, each series marked (`outlier`), its values
as they are; tolerance 3 and pct 20 unless said; a group of under three members has none.

`anomalies(query, algorithm, bounds, direction)` (§9.1): each series of the query and its band of
`bounds` deviations (2 unless said) about what `algorithm` (`basic`, `agile`, `robust`; `basic` unless
said) expected of each bucket before it saw it; the answer's series carry `bounds` (each bucket's
`lower`, `upper` and `score`, its error in deviations, signed). The query beneath reads two weeks
before the range on the range's grid (11,000 buckets in all at the most): the history the season
and the calibration are learned from, its gaps left out, its points' spacing their mean gap.
`direction` (`both`, `above`, `below`) says which side of the band is anomalous.

`forecast(query, 'linear'|'seasonal', deviations, model='default'|'simple'|'reactive',
seasonality='hourly'|'daily'|'weekly', history='1w', interval=seconds)` forecasts each series past
its last point to the range's end (Datadog's arguments, named as Datadog names them; `interval` the
step, `history` how far back a linear one learns, `timezone` only UTC's): up to it the series, from
it the forecast, its band `deviations` either side (§9.1). The query beneath reads ten seasons before
the range (seasonal; a daily season unless said) or the history asked (linear: 30 times the buckets
ahead unless said), at most the 11,000 buckets a query reads. A season under two buckets at the step
is refused.

A search's records group into **patterns** (`kiln_obs::logs::patterns`): Drain (He et al., ICWS
2017) over their messages, a number (its sign with it), `0x` hex and long hex ids masked where they
stand apart, one setting for every system (depth 5, similarity 0.7, a masked variable alike where
the template has a wildcard): on Loghub's 16 systems, 0.887 of lines grouped right, the setting
chosen by leaving each system out (§12.13).

A search can be **followed live** (`GET logs/tail`, a WebSocket): its records as this node's replica
applies them, each once, in the order applied, a message each tick (200 ms; a record posted is on
the tail within 0.41 s at the most, §13). Past the rate asked (50 records a second unless asked,
1,000 at the most) they come faster than they can be read, and the tail shows a uniformly random
sample of them, as Datadog's Live Tail does ("the sampling applied is uniformly random"), saying how
many matched besides: each tick's matched rows each kept with probability `max / matched` (Vitter's
Algorithm R, 1985), a tick's unused share carried to the next (up to a second's worth), so over a
second the rate asked is what is shown. The default rate: of the 25 or so rows a list shows, each
in view for two fixations (200–250 ms each in reading; Rayner 1998), 25 / 0.5 s.

Log search follows Datadog's log syntax: `service:web status:error @http.status_code:>=500
"connection reset" -host:old-*`, with `AND`, `OR`, `NOT`, parentheses, wildcards and ranges, and
log analytics `count() by {service}` over a search. Logs, spans and events are operands of a query
as metrics are (`records_q`, Datadog's log monitor syntax without its window, as its formulas mix
`data_source: logs` with metrics): each bucket's count, or the cardinality, sum, mean, least or most
of a facet, by facets, `100 * logs("status:error").by("service") / logs("*").by("service")`, under
any function (`default_zero`, `top`, `week_before`, ...); a count's bucket nothing fell in 0, another
measure's none. A log search reads logs, not events, unless it names them; `events("…")` reads events
alone. They are read in even buckets (no calendar rollup), the bucketed analytics' 1,500 at the
most. A dashboard's template variable left unfilled in a filter (`{$env}`) is Datadog's `*`. A
metric's filter is Datadog's boolean one: conditions joined by `,`, `AND` or set side by side, `OR`,
negated by `!` or `NOT`, in brackets, the words in any case (`{(type:vsmart OR type:vbond) AND
$host}`, `{not status:done and env:prod}`). Conditions ANDed alone, the common case, are postings
intersected (§4.2); any other filter is an expression of them evaluated within the metric's series.
A value list in a search, `service:(web OR api)` or `@evt.name:(a OR "b c")`, is the key's condition
on each value, `(service:web OR service:api)`. A threshold query of a distribution, Datadog's
`count(v: v>=0.5):latency{…}`, counts each bucket of time's values that pass from its buckets merged
as a percentile's are (§4.4): a bucket whole where all of it passes, in part where the bound falls
within it (its values spread evenly over the logarithm of their magnitude), so within one bucket of
the truth and exactly at a bucket's edge; none where there is no value, a count a second with
`.as_rate()`. `.rollup(10)`, its seconds alone, is the kind's own function at those seconds. A `-` that starts a term is NOT wherever it
stands (`(a OR b) -env:dev`); one inside a term is the term's (`web-1`). A quoted value holds any
characters (`@note:"say \"hi\""`), so a facet's every value can be searched for. Events take the
same search syntax. `trace_id:<32 hex digits>` and `span_id:<16>` are a record's ids (logs' and
spans' alike). Spans are searched as Datadog's trace explorer searches them (`parse_spans`): the
same, and `resource_name:` (or `resource:`) the span's name, and `@duration` (ns, as stored) with a
unit (`@duration:>250ms`, `@duration:[1s TO 5s]`: ns, us, µs, ms, s, m, h).

## 7. API

Under `/api/observe/v1/` (all JSON; the UI uses nothing else), each route under a capability of
kiln's (docs/access.md): every GET, and every query, search and check asked by POST, needs
`observe.read` (a viewer's); dashboards, monitors and data posted need `observe.write` (a
developer's); integrations, clouds, scrape targets, synthetic tests (they reach endpoints from the
machines), log sources (they read machines' files), log pipelines (they change every record stored
after), log archives (they write every record they take to a store outside) and the store's
operations need `observe.manage`
(a maintainer's), but their listings, `observe.read` (their
secrets are never in them).

**`GET openapi.json`** is the API described (`crate::observe::openapi`, OpenAPI 3.1): every route
here and every one that takes what is sent in (OTLP, Firehose, Pub/Sub), each with what it takes
and answers (its schema), its errors, and the capability it needs (`x-kiln-capability`): what a
client, a generated SDK or a provisioning tool is written against. Held to what is so twice: a test
reads the router's routes from its source and finds each in the document with the capability the
gate asks (`crate::authz::rule`), and nothing in the document the router does not serve; a live
scenario asks every route that answers without a service outside and checks each answer against
its schema (§13). Built:

| | |
| --- | --- |
| `POST checks/status` | a service check's status over a time (`check`, `tags`, `group_by` or one `group`, `from`, `to`): each group's (the worst of its series' latest runs), how many groups are in each status, and the worst (§9.4) |
| `POST query` (or `GET query?query=&from=&to=&step=`) | metric query → series on a grid (`from`, `to` in ms, `step` or `points`); NaN is null; the answer states the index it was read at; a series with a band (`anomalies()`, `forecast()`) has `bounds` (`lower`, `upper`, `score`, `k`, `algorithm`, and a forecast's `forecast_from`, its first bucket forecast; `to` past now draws it). With `heatmap: <rows>`, a heatmap instead: `rows` (each `[lo, hi]`) and `counts` (each bucket of time, each row's): a distribution's merged buckets (made coarser until they fit), or another query's series' values in equal rows |
| `GET metrics?match=` , `GET tags?key=&match=` | metric names and tag values, sorted (autocomplete) |
| `GET metrics/summary?match=&limit=`, `GET metrics/summary/{name}` | what each metric is and costs, Datadog's Metrics Summary: its series (its cardinality) by kind, each tag's values and the series carrying it (the tag of most values first; a metric's own with each tag's ten values of most series), the samples applied since the server started and of late a second, its newest sample's time; the metrics of most series first (100 unless said, 1,000 at the most), and how many matched. From the index and the counts kept as samples are applied: nothing read from disk |
| `POST logs/search` | `{query, from, to, limit}` → records newest first, with the blocks pruned; with `view` (a rehydration's id, §10.6) its view's, as each logs query here |
| `POST series` | ingest points as Datadog's v1 series API takes them (`metric`, `type`, `tags`, `points` of `[seconds, value]`) |
| `POST distribution_points` | ingest a distribution's values as Datadog's v1 API takes them (`metric`, `tags`, `host`, `points` of `[seconds, [values]]`): their buckets, count, sum, least and most (§4.4) |
| `POST logs` | ingest records as Datadog's log intake takes them (`message`, `timestamp`, `status`, `service`, `hostname`, `ddsource`, `attributes`) |
| `GET status` | the group: members, leader, term, applied and durable index, series, blocks, bytes on disk (observe's folder, hard-linked snapshot files once), cutoff, this node's writes, each machine's replica, digests (also in hex: a u64 is more than JavaScript's numbers hold) |
| `POST compact`, `POST verify` | cut and compact the log now; take every replica's full digest (an operator's, or a test's, check that replicas agree) |
| `POST logs/aggregate` | log analytics: `{query, from, to, by: [facets], measure, of}` → groups; with `step` (ms), each group's value a bucket on a grid (the explorer's histogram) |
| `POST logs/facets` | `{query, from, to, facets, top}` → each facet's most common values and how many it has, every facet in one pass |
| `POST logs/patterns` | `{query, from, to}` → the search's newest records (10,000 at the most) grouped by their template (Drain, §12.13): each its count, first and last seen, newest message and its records by status, the most common first |
| `GET logs/tail?query=&rate=` | live tail (§6), a WebSocket: each 200 ms, `{type: "records", records, read, matched, missed, ms}`, the tick's records (all the search matched, or a uniform sample past `rate` a second: 50 unless said, 1 to 1,000) and how many rows it read, matched and missed (missed: it fell more than the feed's bound behind); idle, one a second, its counts 0. `{type: "error", message}`, then closed, for a search that does not parse, a rate out of bounds, or a node that 16 tails follow already |
| `GET/POST dashboards`, `GET/PUT/DELETE dashboards/{id}` | dashboards (§11): kiln's own (`builtin:hosts`, `builtin:ci`, `builtin:cd`, `builtin:sync`, read-only) and yours, kept in `<observe>/dashboards.json` |
| `POST dashboards/import`, `GET dashboards/{id}/datadog` | a dashboard made of Datadog's dashboard JSON (as its API gives one), what became of each of its widgets said; and any dashboard given as Datadog's (§11) |
| `POST events`, `POST events/search` | an event (Datadog's v1 shape); events searched |
| `GET/POST monitors`, `GET/PUT/DELETE monitors/{id}`, `POST monitors/{id}/mute`, `…/unmute`, `POST monitors/validate` | monitors (§9), each with its state and its groups', a composite (`type: "composite"`) with what it reads (each monitor's letter, name and state, its condition in letters and in names); validate says what a query watches (the metric query or the log search, and its window), so a page draws what is evaluated, or, given `type`, whether a composite's monitors are there to read; a monitor a composite reads is not deleted (409) |
| `GET/POST downtimes`, `GET/PUT/DELETE downtimes/{id}` | downtimes (§9.3), Datadog's v2 shape: the monitors held (an id, or tags), the groups (a scope), once (times in ms or RFC 3339) or recurring (RRULEs in a timezone); each with its status (`active`, `scheduled`, `ended`, `canceled`) and its window now or next; DELETE cancels (kept); a monitor shows the downtimes that hold it now or will |
| `GET/POST incidents`, `GET incidents/search?query=`, `GET/PATCH/DELETE incidents/{id}`, `GET/POST incidents/{id}/relationships/todos`, `GET/PATCH/DELETE …/todos/{todo}`, `GET/POST incidents/{id}/attachments`, `PATCH/DELETE …/attachments/{id}`, `GET/POST incidents/{id}/timeline` | incidents (§9.5), Datadog's v2 API at kiln's paths (`{id}` its id or its public id): declared, listed, searched (Datadog's syntax; what matched counted by facet), read, changed and removed; their todos and attachments; their timeline (kiln's), a note put on it |
| `GET/POST logs/config/indexes`, `GET/PUT/DELETE logs/config/indexes/{name}`, `GET/PUT logs/config/index-order` | log indexes (§8.10), Datadog's API: listed in order (each its quota's word and kiln's day of it), made, read, changed whole but its name, removed, ordered; a log search's `indexes` |
| `GET containers`, `GET containers/{id}` | every container (§10.10): running, stopped or both, found, ordered and paged; one whole, by its id or the first 12 of its hex |
| `GET/POST notebooks`, `GET/PUT/DELETE notebooks/{id}`, `POST incidents/{id}/postmortem` | notebooks (§11.1), Datadog's v1 API at kiln's paths: made, listed (Datadog's filters, order and pages), read (each graph cell with kiln's drawing of it), changed whole, removed; an incident's postmortem made of a template and attached to it |
| `GET integrations`, `PUT/DELETE integrations/{name}`, `POST integrations/{name}/test` | where monitors say things (§10) |
| `GET clouds`, `PUT/DELETE clouds/{name}`, `POST clouds/{name}/pull`, `POST clouds/{name}/test` | cloud integrations (§10.1), each with its status; a secret left out of a `PUT` (or `"…"`, as shown) is the one kept; `test` pulls a configuration once and writes nothing, saying what it would read |
| `GET synthetics`, `PUT/DELETE synthetics/{name}`, `POST synthetics/{name}/test`, `POST synthetics/{name}/run` | Synthetic tests (§10.4), each with how each location goes (passed, its last run's time, status, certificate's minutes, tries, failures; a runner's as of its last status) and the monitor it owns; a token left out of a `PUT` (or `"…"`) is the one kept, never shown; `test` runs one once from the server, nothing written; `run` runs it now at every location |
| `GET statsd`, `PUT/DELETE statsd/{name}` | DogStatsD listeners (§8.4), each with how each machine that opens it goes (where it listens and the buffer it was granted, datagrams, metrics, events, checks, what did not read, contexts, points written, what waits and what was dropped; a runner's as of its last status) |
| `GET scrapes`, `PUT/DELETE scrapes/{name}`, `POST scrapes/{name}/test` | Prometheus scrape targets (§8), each with how each machine that reads it goes (`up`, its last scrape, samples, error; a runner's as of its last status, every 10 s); a token left out of a `PUT` is the one kept, and never shown; `test` reads a target once from the server and writes nothing, saying what it would get |
| `GET/POST pipelines`, `GET/PUT/DELETE pipelines/{id}`, `PUT pipeline_order`, `POST pipeline_test` | log pipelines (§8.2), in the order they apply, kiln's own first (read-only but for whether they are on: one is turned off, not removed); a pipeline made or changed is checked (it compiles) and applied from the next record on every machine; `pipeline_test` runs a pipeline given (or every one, in order) on records given and keeps nothing |
| `GET/POST log_metrics`, `GET/PUT/DELETE log_metrics/{name}`, `POST log_metric_test` | metrics from logs (§8.3): Datadog's definitions, in kiln's shape or their API's envelope; each with what its latest window with records held, and how counting goes (the settle learned, windows counted again, windows too late); a name whose series another writes is refused (409); what a metric computes stays (400); `log_metric_test` counts a definition over the last fifteen minutes (an hour at the most) and writes nothing |
| `POST spans/search`, `POST spans/aggregate`, `POST spans/facets` | spans as logs (§8.7), their searches the trace explorer's (`resource_name:`, `@duration:>250ms`, `trace_id:`): newest first, analytics by facets (`resource_name` among them), each facet's most common values |
| `GET traces/{id}?from=&to=` | a trace (§8.7): its spans as a waterfall draws them (each its offset from the trace's start, duration, depth and parent's place), its services, errors and roots; without `from` and `to` the spans' whole retention, the blocks without it unread |
| `GET apm/services?from=&to=&env=`, `GET apm/services/{name}` | the services' requests, errors and latency (p50, p95, p99, most) over the range, from the trace metrics; a service's by resource and kind, and the calls it makes by what it calls |
| `GET apm/map?from=&to=&env=` | the service map: nodes (each its requests and errors, whether only named) and edges (calls, errors, mean latency, whether inferred, whether from the trace metrics), and the share of traces read |
| `GET errors/issues?from=&to=&service=&env=&status=&query=&all=&limit=` | Error Tracking's issues with errors in the range (a day unless asked), the most first: each its type, message, place, first and last error, versions, envs, status, regression, activity and errors in the range (§8.8) |
| `GET errors/issues/{id}?from=&to=` | an issue: its errors by bucket, version and env, its latest error whole |
| `PUT errors/issues/{id}` | `{"status": "for_review" \| "reviewed" \| "resolved" \| "ignored"}` |
| `GET/POST span_metrics`, `GET/PUT/DELETE span_metrics/{name}`, `POST span_metric_test` | metrics from spans, as `log_metrics` are; the list says kiln's trace metrics too (`builtin`) |
| `GET log_sources`, `PUT/DELETE log_sources/{name}`, `POST log_sources/{name}/test` | logs machines collect (§8.1), each with how each machine that reads it goes (files open, records written and in flight, bytes behind; a syslog source's connections and datagrams dropped; the last error; counts since `since_ms`, when its process began reading); `test` reads a source on the server and writes nothing: the files it matches and the records of the newest one's last lines, or whether it may listen where it says |
| `GET/POST archives`, `GET/PUT/DELETE archives/{id}`, `GET/PUT archive_order` | log archives (§10.5), in the order records are matched against them, Datadog's shape (plain or in its API's envelope), each with Datadog's `state` and what it wrote, failed with and has yet to write; a change applies to the log files cut after it |
| `GET/POST scanner/groups`, `GET/PUT/DELETE scanner/groups/{id}`, `PUT scanner/group_order`, `GET scanner/standard_patterns`, `POST scanner/test` | the Sensitive Data Scanner (§8.9): its groups in the order they run (Datadog's shape, each rule within, each rule's id given by the server and kept), the standard library, and what a group (or every one that runs) makes of records given, hashing with the kiln's key, nothing kept: reading them an observer's, changing them and the test (an oracle of the kiln's hashes) a maintainer's |
| `GET/POST rehydrations`, `GET/DELETE rehydrations/{id}` | rehydrations from archives (§10.6): `{name, archive_id, query, from, to, retention_days, max_scan_size_in_gb}`, each with its state, what it read and kept, whether its bound stopped it, and when its view goes; removed, its view goes with it (one being read stops) |
| `GET snapshot/{point}/{file}?machine=` | a snapshot's file, for the runner of `machine` with its token |


OTLP/HTTP at its standard paths, `/v1/metrics` and `/v1/logs` (protobuf and JSON), and OTLP/gRPC,
on the server and on every runner at `127.0.0.1:4318` and `:4317`, so an app on a runner sends to its
local runner as it would to a collector (O-5).

## 8. Ingestion

Every node writes its own points (`crate::observe::ingest`: gathered by series, one write out at a
time, sent again until answered; §5), from:

- **OTLP/HTTP** (`POST /v1/metrics`, `/v1/logs`): protobuf or JSON (OTLP's JSON Protobuf Encoding,
  put into protobuf's wire format and read by the one decoder), gzip or not, up to 20 MiB (the
  OpenTelemetry Collector's default). The server asks the ingest token (`<data>/observe/
  ingest.token`) or the admin's; every runner takes OTLP on `127.0.0.1:4318` (OTLP/HTTP's port;
  `KILN_OTLP_ADDR`, or `off`) as a local collector would. Metrics map by type (gauges;
  sums by temporality and monotonicity; histograms' count, sum, min, max and buckets;
  exponential histograms as distributions (§4.4), a cumulative one turned to deltas; summaries); logs keep their time, severity, body,
  attributes and trace context, `service` and `host` from the resource, each decoded straight into
  the row its record would write (`otlp::logs::Decoder`), as spans are, but one a pipeline reads,
  which is made its record (§12.10). An explicit-bucket
  histogram (an SDK's default) answers percentiles as Prometheus's `histogram_quantile` reads
  one: each bucket of time's counts by bound, the rank's bucket found, the value interpolated
  within it (within a bucket's width of the truth; the `+Inf` bucket's, the highest bound).
- **OTLP/gRPC**: the metrics and logs services' `Export` (`opentelemetry.proto.collector.
  metrics.v1.MetricsService/Export`, `….logs.v1.LogsService/Export`), unary, over HTTP/2 with TLS
  (ALPN) or in the clear (prior knowledge, as an SDK's insecure channel speaks), a message
  gzip-compressed or not, read by the same decoder; the token in gRPC's metadata
  (`authorization: Bearer …`). It answers the empty response and `grpc-status` in trailers: what
  does not decode `INVALID_ARGUMENT`, past 20 MiB `RESOURCE_EXHAUSTED`, a node behind on its writes
  `UNAVAILABLE` (which OTLP exporters retry), without the token `UNAUTHENTICATED`. Every runner
  also listens on `127.0.0.1:4317` (OTLP/gRPC's port; `KILN_OTLP_GRPC_ADDR`, or `off`), either
  port taking either protocol. The official Python SDK's gRPC exporters (opentelemetry 1.x,
  grpcio 1.84) were run against it: a counter exported twice read its increase, a delta counter
  its sum, a histogram of 1 to 100 its count, average 50.5, p50 50 and p99 99, and log records
  their service and level, gzip and plain alike.
- **The node's own host** (`crate::observe::host`, every 15 s, the Datadog Agent's interval), in
  Datadog's names and units: CPU (the split over the interval; Linux), load, memory, swap, every
  mount the process sees, disk I/O and network rates, uptime; in a container or a cgroup-limited
  unit, its limits and use (`container.memory.*`, `container.cpu.*`, cgroup v2 or v1), tagged with
  the container's id and the pod's namespace, name and node. Linux reads procfs and sysfs; macOS
  and Windows ask the OS (sysinfo). Network file systems are never looked at (a dead mount's
  `statvfs` never returns), on a thread of its own besides.
- **The process** (`kiln.process.*`: memory, CPU, open files, threads).
- **kiln's registry**, what `/metrics` serves Prometheus, scraped under its Prometheus names
  (`kiln_prom`'s parser takes any text exposition): the server's, and every runner's own (what that
  process counts: its repositories' sync, its moves), each of its host. The repositories' sync has
  a dashboard of kiln's own (`builtin:sync`: steps committed, changes, commit time, each copy's
  lag, state and when it was last heard, what was dropped and why, conflicts, errors, how long a
  change takes to reach other machines, transfers, agents' moves and their barriers, its events),
  and these monitors watch it well:
  `max(last_5m):max:kiln_sync_copy_state{state:error} by {repo,machine} > 0` (a copy in error);
  `max(last_30m):min:kiln_sync_copy_state{state:offline} by {repo,machine} > 0` (away half an hour);
  `sum(last_15m):sum:kiln_sync_dropped_total{reason:too_big} by {repo}.as_count() > 0` (what is
  never synced); `max(last_10m):p95:kiln_sync_apply_lag_seconds{*} by {repo} > 30`;
  `max(last_10m):max:kiln_sync_copy_heard_age_seconds{*} by {repo,machine} > 120`;
  `sum(last_15m):sum:kiln_sync_errors_total{*} by {repo,kind}.as_count() > 0`; and a log monitor
  on `source:kiln.event service:kiln.sync @event.type:sync.conflict`.
- **kiln CI**: each job's and run's duration, queue time, footprint and count by repo, pipeline,
  job, status, runner, branch and trigger, and each an event; and each run a trace (§8.7, Datadog's
  CI visibility's model): the run its root span (service `kiln.ci`, named by its pipeline, a
  server's span, so the trace metrics read each pipeline's runs, failures and durations), each job a
  span under it and each step under its job, their ids derived from the run's and the job's so that
  spans written as each job ends join the run's; the events carry the trace's ids.
- **kiln CD**: each deploy (by app, trigger, result) and each change of an app's health or sync as
  events; apps counted by health and sync (`kiln.cd.apps`), a pair once counted written from then
  on, 0 when no app is in it, as a Prometheus gauge's labels stay: a monitor on the degraded apps
  sees them leave and recovers (it would hold its alert on no data otherwise).
- **GitHub's webhooks**: pushes, pull requests, workflow runs and jobs (durations, conclusions),
  releases, deployments, issues; events and counts.
- **The API**: Datadog's v1 series (`POST series`), log intake (`POST logs`) and events (`POST
  events`).
- **The clouds' pushes** (§10.2): Kinesis Data Firehose's HTTP endpoint (`POST /v1/firehose`,
  CloudWatch Metric Streams and CloudWatch Logs' subscriptions) and Pub/Sub's push subscriptions
  (`POST /v1/pubsub`, a Cloud Logging sink's entries), with the ingest token.
- **Prometheus's remote write** (§8.5, `POST /api/v1/write`), with the ingest token, and on every
  runner's local receiver.
- **Datadog's Agent** (§8.6), at Datadog's own paths, its API key the ingest token.

- **Clouds**: AWS CloudWatch, Google Cloud Monitoring and Azure Monitor, pulled (§10.1).
- **Prometheus's scrapes** (`crate::observe::scrape`): an exporter's `/metrics` read as a
  Prometheus `scrape_config` reads a target. A target is a URL, how often (`every_s`, 15 unless
  said: the Datadog Agent's check interval) and how long a scrape may take (`timeout_s`, 10:
  Prometheus's `scrape_timeout`, never past the interval), and the machine that reads it: the
  server, a runner by name, or every machine its own (`*`, a node exporter on each host). The
  server keeps the targets (a bearer token sealed under the master key) and tells each runner its
  own on observe's lane; each machine reads its targets on a schedule of its own, each offset into
  its interval by a hash of it (Prometheus's), so many targets are not read at once, and kept to it
  (a slow scrape does not push the next later). A scrape asks for OpenMetrics or the text format
  as Prometheus's `Accept` does (gzip allowed; 64 MiB at the most), reads what the answer says it
  is (OpenMetrics' seconds, exemplars and `# EOF`; a counter's `_created` skipped, as Prometheus
  skips it), tags each sample `job` and `instance` over its own labels of those keys (kept as
  `exported_job`, ...: Prometheus's `honor_labels: false`), `host` where every machine reads its
  own, and the target's tags, and writes Prometheus's own series of it: `up`,
  `scrape_duration_seconds`, `scrape_samples_scraped`. A target that starts failing, or recovers,
  is an event once (`source:kiln.event @event.type:scrape`). A histogram is read as an OTLP one
  is: `p95:http_request_duration_seconds{*}` from its `_bucket`s by `le` (Prometheus's
  `histogram_quantile`, interpolated within a bucket), `avg:` its `_sum` over its `_count`,
  `sum:` and `count:` theirs (Prometheus keeps no least or most: `min:` and `max:` are none).
- **kiln's own spans** (`crate::observe::spans`): each closed `tracing` span of a target observe
  keeps (`kiln::upgrade`: runners' blue/green upgrades, `upgrade`, `upgrade.start`,
  `upgrade.healthy`, `upgrade.move`, `upgrade.retire`, `upgrade.rollback`; `kiln.sync`: the
  repositories' sync and agents' moves, `move`, `move.barrier`, `move.files`) is
  `kiln.<span>.duration` (seconds of wall time, a gauge a close) and `kiln.<span>.count`, tagged
  with the fields low in cardinality (`runner`, `from`, `to`, `what`, `result`, `reason`, `repo`,
  `machine`, `direction`, `state`, …; a child's taken from its ancestors), and an event
  (`service:kiln.upgrade @event.type:<span>`) holding every field, ids too (`@upgrade_id:…` finds
  one upgrade's story), an error when its `result` failed or rolled back. An event of those
  targets that names itself (an `event` field: `tracing::warn!(target: "kiln.sync", event =
  "sync.conflict", repo = %r, path = %p, "kept the server's copy")`) is an event too, its level its
  status, every field and its spans' an attribute; one without stays the log's. `kiln.sync` is
  written as a name, not a module's path, so the sync's own tracing (`kiln::sync::…`) is not
  taken. A layer of kiln's subscriber with its own filter: `KILN_LOG` does not hide these from
  observe, nor do they reach the log.

- **Logs collected on every machine** (§8.1): files, containers' logs and the journal, as a source
  says.

**Taking what comes, and refusing** (`crate::observe::owner`, `crate::observe::ingest`): a request
is answered 200 only once its points have room, taken where it is answered (an atomic reservation
against the batcher's bound, 4 M points and 64 MiB of logs; released as writes are cut), so nothing
answered 200 is ever dropped for room. Exporters' requests may take three quarters of it, kiln's
own figures the rest. And a queue is held to a wait, as CoDel holds one (Nichols and Jacobson,
"Controlling Queue Delay", 2012; RFC 8289): series go into writes oldest first, which bounds each
series' wait by the queue's, and while the oldest waiting point has waited more than a second
(`TARGET`, five flushes) for a whole second (`INTERVAL`: its least over it, so a burst that drains
in time is no reason), exporters are refused with 503 and `Retry-After: 1` until it drains; an
exporter keeps a refused batch and sends it again. Measured live (§13): a runner whose server went
away refused within 0.5 s at full rate (the bound) and within 2.4 s at 100 k points a second (the
wait), and stored everything it had taken once the server was back.

A decoded OTLP request is its series' keys once and its samples flat, each with its series' place;
the batcher keeps what waits the same way, one buffer, sorted by series with a counting sort when a
write is cut. A series' key is made (checked, sorted, fingerprinted) once in a node's life: a
sharded cache of keys by their canonical bytes, shared (`Arc`) with every write that holds one.

### 8.1 Logs collected

kiln's machines collect logs as Datadog's Agent does (`crate::observe::collect`): the server keeps
the sources (`<observe>/log_sources.json`) and tells each runner those it reads on observe's lane,
as it tells scrape targets; each machine reads each of its sources on a thread of its own and says
how it goes in its status. A source is:

- its **type**: `file` (globs, `**` across folders, less those it leaves out), `kubernetes` (every
  pod's container log on the node, `/var/log/pods/*/*/*.log` unless given: each record its pod's
  namespace, name and uid, its container's name and restarts, its service the container's), `docker`
  (Docker's json-file logs, `/var/lib/docker/containers/*/*-json.log`: the container's id, and from
  its `config.v2.json` where kiln may read it, its name, image and Compose service), or `journald`
  (the systemd journal, its units or every one), or `syslog` (received where it listens,
  `udp://` and `tcp://` addresses), or `unified_log` (macOS's, an NSPredicate of its events and a
  level), or `windows_event` (the Windows Event Log's channels, an XPath of their events);
- how its lines read: `auto` (a line that is a JSON object is one, else text), `text`, `json`,
  `cri` (`<time> <stream> <P|F> <text>`, a long line's parts joined) or `docker` (the same in JSON),
  in UTF-8 or UTF-16; where a file there when the source starts, with no place kept, is read from
  (`end`, unless `beginning`; a file that appears later is read from its start);
- what its records are: `service` (else a container's, else the source's name), `source` (else its
  type), tags as attributes;
- Datadog's processing rules, in their order: `exclude_at_match`, `include_at_match`,
  `mask_sequences` (a token, a card's number, replaced), and `multi_line` (a record starts at a
  line that matches, a stack trace one record);
- the machine that reads it: the server, a runner, or every machine its own.

A JSON line's message, level (a name, pino's numbers, syslog's), time (RFC 3339, or seconds,
milliseconds, microseconds or nanoseconds since the epoch, told apart by their size), trace and
span are its record's; its other fields its attributes, nested keys joined by dots. A text line's
level is read from its first words as a log pipeline's status remapper reads it; its time is when it
was read. A time more than a minute ahead of the machine's clock is taken as now (one without a zone
read as UTC on a machine west of it).

A line no pipeline reads (§8.2: each one's filter asked of the line itself, as of its record) is
handed over as the row its record would write, written where it is read: a text line from the line
as it is (its message, the source's names, its file's or container's attributes, its tags, an
error's issue), a JSON line from its fields as its reading flattens them into room kept from line
to line. No record is made of it, and a test holds the row and the record's alike over every kind
of line, JSON lines of every shape among them. A line a pipeline (or the scanner) reads is made its
record where it is read, a record of one buffer kept from line to line, run through them, and
written as its row there too (§8.2); a service check's run goes as a record. What a machine reads
goes in the order it was read, rows beside records, and the batcher writes it so: a write's records
and rows one item, compressed whole (§12.17).

**Each line once.** A line's place (a file's offset, the journal's cursor) is kept only once the
write that holds its record is applied by the group (`crate::observe::ingest::Ack`: the batcher
gives it when the write holding the last of an offer's records is answered), and written down a
second at most after. A refused offer (no room) is offered again, and nothing more is read until it
is taken. So a machine that stops (SIGTERM) waits a moment for its last writes and repeats nothing,
and one killed repeats at most what was written in its last second, and loses nothing it read
(Vector's end-to-end acknowledgements; Datadog's Agent and Fluent Bit keep their places the same
way). A runner keeps its loop up for those last writes, three seconds at the most: the supervisor is
told of a shutdown five seconds after the runner, so it does not drop the runner first. Its
aggregators' last counts wait the same: each DogStatsD listener's bucket under way and the tracers'
concentrator's open buckets (§8.7) are offered as it stops, each aggregator saying when its last is
(`crate::observe::owes_last`), and then the runner waits for everything taken before them to be
written (`Observe::settle`: nothing waiting, no write out). A runner that retires after an
upgrade (its new build took over) writes its concentrator's buckets the same way before it goes.
The receivers' ports (OTLP's, the trace agent's) are the new build's once the old one lets them go:
a port held is tried again every five seconds.

**Files** are known by their identity (device and inode on Unix; elsewhere the creation time, which
a rename keeps) and by their leading bytes (a hash of the first KiB, kept with the place), looked
at every 10 s (Datadog's `file_scan_period`), each file read as it grows, a MiB a round at most so
one busy file does not starve the rest, 500 open at once at most (the most recently written first):

- **renamed** (logrotate's `create`): read to its end and closed once it has not grown for 30 s;
  the new file at the path read from its start;
- **truncated or rewritten in place** (logrotate's `copytruncate`; its size under what was read, or
  its leading bytes not what they were): read again from its start; and its **copy**, whose leading
  bytes are the truncated file's, read from where the original had been read to, so nothing is read
  twice and the lines written between the copy and the truncation are not lost;
- **deleted** while read: read to its end;
- a line without its newline is taken once its file has not grown for 5 s; a line longer than 256
  KiB (Datadog's Agent's bound) is cut into records that long, each said to be cut; a multi-line
  record is taken once no line came for a second (the Agent's aggregation timeout).

**The journal** is read through `journalctl --output=json --follow --all`, from just after the
cursor of the last record written (Fluent Bit's and Vector's journald sources read it so); its
entries' severity is their syslog priority's (OpenTelemetry's mapping), their service the syslog
identifier or unit, with the unit, process and boot as attributes. Where there is no journal (macOS,
Windows, a container without systemd's) its status says so.

**Syslog** is received as Datadog's Agent and rsyslog receive it: a message a UDP datagram, and
over TCP each message counted (`<length> <message>`) or ended by a newline or a NUL (RFC 6587's
two framings, told apart by a connection's first byte), 1,024 connections at most, each let go after
five silent minutes. A message is read as RFC 5424's when its version follows its priority (its
time, host, app, process, message id and structured data, each `sd.<id>.<name>` an attribute), else
as RFC 3164's as far as it goes (`Mmm dd hh:mm:ss`, in the source's `timezone`, UTC unless said,
its year the one it is unless that puts it more than a day ahead; an RFC 3339 time where it goes;
its host and `tag[pid]:` each optional, a router's sequence number not taken for a program), else
whole. Its severity and facility are its priority's, its host the one it names (else its sender's
address, `network.client.ip` either way), its service the source's or its program's. Syslog has no
acknowledgement: a datagram there is no room for is dropped, and counted, while a TCP sender is held
back (its connection is not read) until there is.

**macOS's unified log** is read through `log stream --style ndjson --type log`, at the source's
level and its predicate, events as they come (each within some 20–40 ms of being logged, measured
on a laptop through a pipe). An event is known by its boot and its mach time (the machine's
monotonic clock); its place is the last one written. As the source starts again, the stream starts
first, then `log show` reads from that place to just past the stream's start, and the stream's
events the catch-up held are dropped: nothing falls between, nothing is read twice. The catch-up
reads what the log keeps on disk, the default level and above: info and debug events are held in
memory, and `log show --info` reads a laptop's minute in 18 s against 1.5 s without, so what came at
those levels while kiln was not reading is not read. An event's severity is its type's (`Fault`
fatal, `Error`, `Default` a notice, `Info`, `Debug`), its service its process's, with its subsystem,
category, process and thread as attributes.

**The Windows Event Log** is read through `wevtutil`, as Datadog's `windows_event` source reads its
channels: each channel asked each second for the events after its place, `wevtutil qe <channel>
/q:"*[System[(EventRecordID>N)]]" /f:RenderedXml` (the source's query joined to it with `and`), 500
at a time, oldest first, each with its message rendered (one whose provider is not installed here
reads as its provider, event id and data). A channel's place is its last event's record id, kept once
its record is written; a channel cleared (its newest id below the place) is read from its start. An
event's severity is its level's (critical, error, warning, information, verbose), its service its
provider's, with its channel, event id, record id, computer, task, keywords, process, thread, user
and each `EventData` value (`windows.event_data.<name>`) as attributes. kiln reads the XML with a
reader of its own (elements, attributes, text, the five entities and character references, CDATA),
not a library. `wevtutil` writes its events one after another and no newline after the last: a
command's last line is read whole though no newline ends it. Neither source has a machine of its own
platform in the scenarios' Linux fleet: the unified log's scenario runs on a Mac of the fleet, the
Event Log's against a stand-in for `wevtutil` that answers its two queries from a channel's file of
events, written as Windows' writes them (§13); and on a Windows machine, by hand (§13).

A source reads what kiln's process may read on each machine that reads it: a maintainer's to give.

### 8.2 Pipelines

How records are read as they are taken (`crate::observe::pipeline`), Datadog's log pipelines, so one
exported from Datadog reads here as it is: each pipeline a filter (a log search; empty, every
record) and processors run in order on each record it matches, the pipelines in order, each reading
what those before it made. The server keeps them (`<observe>/pipelines.json`) and tells every runner
the list; each node applies it where it takes a record (a request's, a log source's, a cloud's), so
what is stored is read already, on the machine that took it. A log source's line a pipeline (or the
scanner, §8.9) reads is run through where it is read and written as its row then, its record one
buffer (`kiln_obs::logs::buf::RecordBuf`: its fields and attributes side by side, kept from line to
line, a long line's room let go past 128 KiB as a line's own is past 64), what a processor makes written side by side too and taken in one copy (`Attrs`: a grok
rule's values, a URL's parts, a user agent's), so a line costs no string of its own; the row is
the one its record wrote when records were run through as they were offered (a test holds 6,000
lines of every kind to it, byte for byte), and the processors make what they made before, held to
that code by tests (every grok conversion and filter, every processor, on a record of strings and
on one of one buffer). Other records are run
through once, as they are gathered: an offer refused comes back and is not run again.

The processors are Datadog's: the **grok parser** (rules of literal text, a regular expression, and
`%{matcher(args):attribute:filter(args)}` tokens, matching a value whole, the first rule that
matches extracting its attributes; helper rules, and match rules given before, inlined; Datadog's
matchers, `notSpace`, `data`, `word`, `integer`, `number`, `boolean`, the quoted strings, `uuid`,
`mac`, `ip`, `ipOrHost`, `hostname`, `port`, `regex("…")`, `date("pattern"[, "timezone"])` with
Java's date patterns, and filters `number`, `integer`, `boolean`, `nullIf`, `json`, `keyvalue`,
`lowercase`, `uppercase`, `decodeuricomponent`, `querystring`, `url`, `useragent`, `scale`); the
**date**, **status**, **service**, **message** and **trace id** remappers (each reading the first of
its attributes a record has: a time as RFC 3339 or seconds to nanoseconds since the epoch; a status
as Datadog's remapper reads one, syslog's 0–7 or a word by its start); the **attribute remapper**;
the **URL** and **user-agent parsers**; the **category processor** (the first of its searches that
matches names the record); the **arithmetic processor** (`+ - * / %`, `abs`, `ceil`, `floor`,
`round`, `min`, `max`); the **string builder**; the **lookup processor**; and a **pipeline** nested
in one, one deep. Rust's regular expressions do not backtrack: an atomic group `(?>…)` reads as a
group, which matches the same wherever the atomic one would not have to give anything back.

A rule is the value whole (`^(?:…)$`: its alternatives each the whole of it), read by grok's own
matcher (`pipeline::matcher`): the rule's expression as regex's own parser reads it, compiled into a
program a backtracking machine runs from the value's start, reading the rule's groups in that one
pass (`regex` makes two: its lazy DFA finds the match, then its backtracker reads the groups after
clearing a table of every state at every byte). A run of a class (`\S+`, `[^"]*`, `.*?`) is read as a
run: one nothing after it may begin with what it holds gives nothing back, one that may gives back
only where what follows may begin; of two ways on, one that cannot begin where the value is, is
not tried; a value is refused by its last byte, or by a byte every match holds that it does not
(`memchr`); and past a first stretch of work, a table of where the search has been keeps it to
`regex`'s bound (each instruction at each byte once). It finds what `regex` finds, the leftmost-first
match and every group where `regex` puts it (differential tests, §12.17); what it leaves to
`regex`, `regex` reads: a loop that may match nothing each round (its empty rounds are `regex`'s to
tell), and a search past its bound.

kiln carries pipelines for nginx's and Apache's access and error logs (Datadog's own rules, then the
time, the HTTP status's category as its status, the URL and the user agent read), applied to records
of `source:nginx` and `source:apache`: read-only, turned off or copied to be changed.

### 8.3 Metrics from logs

Datadog's "generate metrics from ingested logs" (`crate::observe::logmetrics`,
`kiln_obs::logs::metrics`): a metric that counts the records a search matches, or takes an
attribute's values as a distribution (its count, sum, least and most, and with percentiles its
buckets, §4.4), grouped by facets that become its tags, a point each ten seconds of the records'
time (Datadog's "datapoints for log-based metrics at 10-second intervals"). A definition is Datadog's
(`compute`, `filter`, `group_by`), taken in their API's envelope too; the metric outlives the logs,
at metrics' retention.

**One writer, counting whole windows.** A window's points are a function of the records it holds:
`count` reads them, every metric at once, in one pass over the blocks the window reaches. The
server's replica holds every record, so the server alone counts (each series has one writer, the
rule OpenTelemetry's data model states), and when records come for a window it has counted, it
counts the window again, whole, and writes its points over the ones before (at a time the later
write wins, §4.5; a window a rollup holds is read from its samples while a later write overlaps it,
§4.7). So a record is never counted twice, and a restart loses none: the server counts every window
of the last twenty minutes again as it starts. The replica notes, as it applies a batch, the windows
its records fall in (`Touched`), taken under the same brief hold of the store as the records the
count reads (as they are, `Store::logs_at`, counted with the hold let go: no write waits on a
count), so every record noted is in what that count reads. Records that come more than twenty minutes after
their window are not counted, as Datadog's ("aggregation only considers logs ingested with a
timestamp within the past 20 minutes"); they are stored and searched as any.

**When a window is counted.** Once its records have come: how late each batch's newest record came
(a batch a write's records, however many of its items hold them), the 99th percentile over the last
five minutes, a quarter more, 2 to 30 s (learned as records come, so a fleet whose records take
longer waits longer, and in the steady state each window is counted once). A window
counted and then given more waits half as long as it has been since it ended before it is counted
again, so however records trickle in, one is counted some fifteen times at the most; counting takes
a quarter of each second at the most, never-counted windows first.

**Bounds.** A metric keeps 2,000 groups a window at the most, OpenTelemetry's default cardinality
limit; the records of the groups past them are counted in one group tagged
`otel.metric.overflow:true`, as the SDK's overflow is. A tag's value is cut at Datadog's 200
characters; a record without a group's facet is `N/A`, as log analytics says. Names are Datadog's
custom metrics' (`kiln.*`, `system.*` and `datadog.*` refused: kiln writes them), and a name whose
series something else writes already is refused (409), as is one another metric from logs writes:
a metric has one writer. What a metric computes, and of what, stays; its filter, groups and
percentiles change, counted so from the next window on.

### 8.4 DogStatsD

What an application sends Datadog's DogStatsD (`kiln_obs::statsd`, `crate::observe::statsd`), taken
by the server and every runner: listeners the server keeps (`<observe>/statsd.json`) and tells each
machine its own, as it tells scrape targets, so an application instrumented for Datadog points its
client at kiln on its own host and nothing else changes.

- **Where.** A listener listens over UDP (`udp://127.0.0.1:8125`, the Agent's `dogstatsd_port`;
  `0.0.0.0` takes other machines' too) and, on Unix, over a datagram socket (the Agent's
  `dogstatsd_socket`; anyone on the machine may send to it, only kiln reads), four addresses at the
  most, a thread a socket. It asks for a 4 MiB receive buffer: Linux charges a datagram some 768
  bytes beyond what it holds, so that is some 5,000 datagrams, 50 ms of 100,000 a second, through a
  reading thread's scheduling delay on a loaded machine (the Agent leaves the system's 208 KiB); the
  system grants what its bound allows (`net.core.rmem_max`), and the listener says what it got.
- **What.** Protocol 1.3 (Datadog's "Datagram Format"): a metric, several values a message, a
  sample rate, tags, a container and a timestamp; events and service checks (each run a record, and
  its status a sample of its check's series, §9.4). A message's context is
  its name, how it aggregates, its host (a `host:` tag's, else the machine's) and its tags in any
  order, a key once (its first value: a series has one value a key, so `#team:a,team:b` keeps
  `team:a`, counted); the listener's own tags go where the message has not the key.
- **Aggregated as the Agent aggregates** (its time sampler: buckets of ten seconds of when samples
  came, a bucket's points at its start once it is over): a count sums its values each over its
  sample rate, a count a bucket (`.as_rate()` reads it a second, as Datadog shows a DogStatsD count),
  and writes zero for five minutes after its last sample, so a graph of it falls to zero
  (`dogstatsd_expiry_seconds`); a gauge its last value; a set its distinct values; a histogram or a
  timer the aggregates its listener asks (`histogram_aggregates`: max, median, avg, count unless
  said; min, sum) and its percentiles (`histogram_percentiles`, 0.95 unless said), each its own
  series (`<name>.max`, `.95percentile`, ...), the median and percentiles the Agent's nearest rank
  over the samples' weights; a distribution kiln's distribution (§4.4), merged across hosts and time.
  A gauge or count with a timestamp is written at it as it came.
- **Events** are kiln's events (`source:kiln.event`, as `POST events` makes them); **service
  checks** are records, `source:service_check` with `@check` and `@check_status` (OK an info, WARNING
  a warning, CRITICAL an error), for log monitors to watch.
- **Bounds.** 131,072 contexts a listener (a bucket of that many histograms, five series each, is a
  fifth of what a node takes waiting); past it a new context's samples are refused and counted.
  Histogram samples are held exactly, 1 Mi of them across contexts and 64 Ki a context; past
  either, a context's samples that bucket go into buckets of §4.4's scale, its percentiles then
  within 1.09%. A set counts 64 Ki distinct values a bucket; a distribution keeps 4,096 buckets (the
  Agent's own sketches' limit), a value past them counted in the nearest. A datagram has no sender
  to tell to wait: what observe has no room for is held and offered again (six buckets of points,
  10,000 records), the oldest past that dropped and counted. A listener reconfigured starts again;
  stopping, it writes the bucket under way.

Steady state allocates nothing a message: a context is found by a hash of its name, host and tags,
each tag hashed alone (so their order does not matter) and a key given twice found by its bytes; its
series' keys are made once, when it is first seen, and a bucket is written as an OTLP request's
samples are, each series' key once and its samples flat (§12.20).

### 8.5 Prometheus's remote write

What Prometheus sends a receiver, and what speaks its protocol (Grafana Alloy, VictoriaMetrics'
vmagent, the OpenTelemetry Collector's `prometheusremotewrite`): `kiln_obs::promwrite`,
`crate::observe::remote`. A Prometheus that scrapes a fleet writes it to kiln as it scrapes, its
queries and alerts where they were, kiln's beside them.

- **Where.** `POST /api/v1/write` on the server, its ingest token as Prometheus's `remote_write`
  sends credentials (`authorization: { credentials: … }` or `bearer_token`: a bearer token; or
  `basic_auth`, the token its password); and on every runner's local receiver (`127.0.0.1:4318`,
  no token: loopback), for an agent on the machine.
- **What.** Protocol 1.0 (`prometheus.WriteRequest`) and 2.0 (`io.prometheus.write.v2.Request`,
  its strings interned, each series' type with it), told apart by the `Content-Type` as the
  specification negotiates them (415 for a message of neither); Snappy's block format (kiln's own
  reader, safe and bounded: 32 MiB unzipped at the most, an offset or length past what is written
  an error). Answered as a receiver answers: 204 once taken (2.0's samples written in
  `X-Prometheus-Remote-Write-Samples-Written`), 400 for what does not decode (Prometheus does not
  send it again), 503 with `Retry-After` while the node is behind (it does).
- **As a scrape writes it.** A series is its labels: `__name__` its metric, the rest its tags (a
  label of no value none), so remote write and a scrape of the same target write the same series.
  Its kind as a scrape gives it (§8): its type its series' own (2.0), else its family's from 1.0's
  metadata (sent apart, every minute: what a node learns it keeps), else its name's (`_total`,
  `_bucket`, `_count`, `_sum` counters). Samples keep their times; a staleness marker (a NaN) is
  skipped; exemplars are counted, not taken. Its series' keys are found
  in the receivers' shared cache after their first time, as OTLP's are; what a request decodes to
  is an OTLP request's shape, taken under the same admission.
- **Native histograms** (Prometheus's sparse exponential ones, `send_native_histograms`) are
  kiln's distributions (§4.4), as OpenTelemetry's exponential histograms are: the schema is the
  scale (made kiln's finest past it), a bucket's index one past OpenTelemetry's (Prometheus's
  `(base^(i-1), base^i]`), read from the spans and the counts (each the change from the one before),
  the zero bucket kiln's; a counter histogram's samples turned into what rose since the one before
  (the receivers' state of cumulative histograms, OTLP's too), a gauge histogram's as they come;
  `.count` and `.sum` beside the buckets, so `p99:<name>` reads them. A float histogram (recording
  rules') or one of custom buckets is counted, not yet taken; 2.0's answer says how many were
  written.

### 8.6 Datadog's Agent

An Agent fleet writes to kiln as to Datadog: pointed at it (`dd_url: http://<kiln>:7070`,
`logs_config.logs_dd_url: <kiln>:7070` with `logs_no_ssl` and `force_use_http`, its trace agent's
`apm_config.apm_dd_url: http://<kiln>:7070`), or to kiln beside Datadog (`additional_endpoints`,
and its logs' and its trace agent's), its API key observe's ingest token. What it
submits arrives as Datadog shows it, and with it the hundreds of integrations its checks are
(`crate::observe::ddagent`, `kiln_obs::ddagent`). What an Agent 7.84 sends was recorded first (a
capturing endpoint in kiln's place), and the live scenario (§13) runs that Agent:

| Datadog's path | what it is | taken as |
| --- | --- | --- |
| `POST /api/v2/series` | its metrics, protobuf (`MetricPayload`) | series: its tags (a key once: its first value), `host` and `device` from its resources; a gauge a gauge, a count a count, a rate (DogStatsD's counts, a histogram's `.count`) the count it was, its value times its interval, as kiln's own DogStatsD writes it (§8.4) |
| `POST /api/v1/series` | the same, JSON (older Agents) | the same |
| `POST /api/beta/sketches` | distributions' sketches (`SketchPayload`) | kiln's distributions (§4.4): each bin at its value (the Agent's γ = 1 + 1/64, a key the logarithm rounded to the even and offset by 1338: values 1, 10, 100, 1000, 0.5 and −3 sent through an Agent came back as keys 1338, 1487, 1635, 1784, 1293 and −1409) counted into kiln's buckets, its count, sum, least and most exact |
| `POST /api/v1/check_run` | service checks | records as DogStatsD's are (`source:service_check`), each run's status a sample of its check's series besides (§9.4) |
| `POST /intake/` | events, and host metadata | kiln's events (`source:kiln.event`, `@event.source` the source's name); the metadata passed over |
| `POST /api/v2/logs` | its logs, JSON | records, `ddtags` and any other field attributes (nested keys joined by dots, as Datadog's intake reads them) |
| `POST /api/v0.2/traces` | its tracers' spans (`AgentPayload`, protobuf) | spans (§8.7), `source:datadog-agent` |
| `POST /api/v0.2/stats` | its counts of every span it was sent, before it sampled (`StatsPayload`, MessagePack) | kiln's trace metrics (§8.7) |
| `GET /api/v1/validate` | whether its key is one | `{"valid": true}`, or 403 as Datadog answers |

Bodies come zstd-compressed (the Agent's default; RFC 8878, kiln's own decoder, `kiln_obs::zstd`:
its literals' Huffman tables, its sequences' FSE tables, repeat offsets, the content's checksum;
safe, bounded at 64 MiB, read back to the byte against the reference CLI at every level and
fuzzed), deflate or gzip. Anything else under Datadog's paths (its metadata, its other products'
intakes) is answered 202 and passed over, so an Agent neither retries nor complains; a node behind
answers 503 and the Agent keeps its payload and sends it again.

### 8.7 Traces

Spans come over OTLP: `POST /v1/traces` (protobuf or OTLP's JSON, gzip or not) and OTLP/gRPC's
`opentelemetry.proto.collector.trace.v1.TraceService/Export`, on the server (the ingest token) and
on every runner's receiver (`127.0.0.1:4318` and `:4317`), as metrics and logs come. A span the
specification calls invalid (no trace id of 16 bytes, or span id of 8, not all zero) is not kept
and is said in the answer's partial success (`rejectedSpans`, OTLP's `ExportTracePartialSuccess`),
over HTTP and gRPC alike.

**A span is a record of a store of its own** (`kiln_obs::otlp::traces`, the span store: §4.6's
engine, its own files and retention). The decoder writes each span straight into its row (the log
rows' encoding), no record made of it on the way, its buffers kept from one request to the next:

- its time the span's start; `duration` (ns, Datadog's `@duration`) its end less its start;
- its message the span's name, which OpenTelemetry has be "the most general string that
  identifies a (statistically) interesting class of Spans" (`GET /users/{id}`): Datadog's
  resource, searched as `resource_name:`;
- `service` and `host` the resource's `service.name` and `host.name`, `source` the
  instrumentation scope's name; severity 17 for status ERROR (`status:error`), 9 else
  (`status:ok`);
- its attributes the span's over its resource's, and kiln's own first, so that no attribute of the
  span's stands for them: `span.kind`, `duration`, `parent_id` (hex) and `otel.status_code`; then,
  where the span does not say them itself, Datadog's unified service tags `env` and `version` (from
  `deployment.environment[.name]` and `service.version`), the error (its first exception event's
  `exception.type`, `.message` and `.stacktrace` as `error.type`, `error.message` and
  `error.stack`, as Datadog maps them; else an error's status message), `peer.service` on a
  client's or a producer's span (as given, else the first of `db.system.name`, `db.system`,
  `messaging.system`, `rpc.service`, `server.address`, `net.peer.name` it has: Datadog's inferred
  services' peer tags), Datadog's `http.status_code` (from `http.response.status_code`), and its
  events and links as JSON (an event's time as its offset from the span's start);
- limits Datadog's trace agent's (`pkg/trace/traceutil/truncate.go`): a value at 25,000 bytes, a
  name at 5,000; attributes 128 in all (OpenTelemetry's count limit), the span's past what kiln's
  own leave dropped in key order, its resource's added while room is left (a resource's 128 at the
  most: merging a span's attributes with its resource's takes some thousands of comparisons
  whatever a request holds, not one for every pair of 40,000 and 40,000); a row at most 256 KiB, its
  longest values cut to the one length that brings it within (water-filling), so a write always
  holds it.

**Datadog's tracers** come through their Agent (§8.6: `POST /api/v0.2/traces`, the Agent's
`AgentPayload`; `kiln_obs::ddtrace`), in either of its forms: indexed, as an Agent 7.84 sends them
(each tracer's payload's strings once, in a table written after what refers to it, every string a
reference into it), or inline, as Agents before it did (a span's tags in `meta`, its numbers in
`metrics`). Each span is written straight into its row as OTLP's are: its resource its message, its
operation's name `operation_name`, its type `span.type`, its integration `component`, its kind
(said, else its `span.kind` tag, else internal); its trace's 128 bits as W3C's `traceparent` carries
them (the indexed form's chunk's id; the inline form's 64 and above them the 64 of its chunk's
`_dd.p.tid`), its span's 64, big-endian, so a trace that crosses from a Datadog tracer to an
OpenTelemetry SDK is one trace; its tracer's host, else the Agent's; its tags and numbers (a number
as the shortest text that reads back to it), its chunk's and its payload's where it has none of the
name (`_dd.tags.container` and `_dd.tags.process` each of their `key:value`s); then `env` (its own,
its payload's, the Agent's), `version` (its own only: a tracer sets none on a span of a service it
named otherwise, a database's), `error.message` (older tracers' `error.msg`), `peer.service` (as
OTLP's, Datadog's `out.host` among its sources), its events and links as JSON.

The Agent samples (ten traces a second by default, its priority sampler's target; a tracer's rules
and `manual.drop` drop more), and it counts every span it is sent before it does, sending its
counts on (`POST /api/v0.2/stats`: each ten seconds' grouped stats, a service's spans of one
resource, kind, status and peer, with their hits, errors, total duration and two DDSketches of their
durations). Datadog counts its trace metrics from those, not from the spans it keeps, and so does
kiln: an Agent's spans are not counted by the trace metrics' definitions below (`-source:datadog-agent`),
and its stats are written as the same series (`kiln_obs::ddtrace::stats`): a group of a server's or
a consumer's spans, or of another kind's that are top-level (a trace's root, or a span whose parent
is another service's: the Agent's `TopLevelHits`, its counts in the share they are of its hits), is
its service's entries; a client's or a producer's its exits, by the peer its peer tags name; a group
of spans within a service is passed over. A sketch's bins (its mapping's γ, offset and
interpolation, logarithmic or a power of two's linear, quadratic or cubic, each read back within its
relative accuracy against `sketches-go`'s and `sketches-java`'s index functions) are counted into
kiln's buckets at their values (within the Agent's 1%, then kiln's 1.09%). Each series carries the
`host` its stats came from, so two hosts' are two series a query sums, never one written over the
other; a payload's points are at its buckets' starts and an offset within the bucket its bytes'
CRC-32C gives, the same on any node, so an Agent's retry writes over itself and two payloads that
split a series' bucket between them (`SplitPayload`) add.

**Or straight to kiln, no Agent** (`crate::observe::ddtrace`): every runner's loopback receiver
answers the trace agent's own API, and listens on its port too (`127.0.0.1:8126`, a tracer's
default, unless `KILN_DD_TRACE_ADDR` says another, or `off`), so an app a Datadog tracer traces
sends to the machine's kiln as to its Agent. `/v0.4/traces` and `/v0.5/traces` (MessagePack: each
trace an array of spans, each span a map; or the strings first in a table, each span an array of
twelve) are read as the inline form is (`kiln_obs::ddtrace::Decoder::decode_tracer`), the trace's
high bits from its `_dd.p.tid`, its host the runner's; `/info` says those two alone (no `/v0.6/stats`,
`client_drop_p0s` false), so a tracer computes no stats of its own and drops no trace for them; its
other products' paths are answered and passed over. Each answer gives each service its sampling
rate (`rate_by_service`, `service:<name>,env:<env>`), the Agent's priority sampler's
(`kiln_obs::ddtrace::Sampler`, `pkg/trace/sampler` read and followed): a node's traces counted by
their root's service and env in buckets of five seconds, six kept; at each new bucket, the target
(ten traces a second, the Agent's `target_traces_per_second`; `KILN_DD_TARGET_TPS`, 0 to keep every
trace) spread evenly over them, what a quiet one leaves spread over the rest (water-filling), each
against its busiest bucket (a moving max, against oscillation), a rate rising at most 1.2 times an
update (some thirteen updates, a minute, to rise tenfold), a quiet signature forgotten; a service
not known the rate the whole traffic would have, never above the lowest given. Only a tracer's own
decisions (priority 0 or 1) are counted, each weighed by what its tracer sampled it at, as the
Agent's `weightRoot` weighs it; a user's rules and `manual.keep`/`manual.drop` stand, and a trace
with an error is kept whatever its priority (the Agent keeps some ten a second of those). Every span
of a payload taken is counted by the runner's concentrator (`kiln_obs::ddtrace::Concentrator`; a
payload refused, the runner behind on its writes, is counted as its tracer sends it again: once), as
the Agent's counts them: entries, exits by peer, and spans of no kind that are top-level (marked so
by the tracer, or their parent none of their trace's, or another service's), in buckets of ten
seconds of their ends (the Agent's `start + duration`), each written once twenty seconds past its
own end (the Agent's buffer), a span later than that (its bucket's wait over, written or not) in the
oldest bucket not yet due (as the Agent shifts it), one ending more than a minute ahead in now's:
nine buckets open at the most, whatever times a payload's spans say; 2,000 groups a bucket at the
most, the rest in one of `N/A`. A bucket's points are at its start and an offset within it the
runner draws as it starts, so a runner started again writes a bucket it wrote before beside that
write, the two adding, not over it (a series keeps the later of two writes at one time, §4.5). Then
a trace its tracer dropped (its priority below zero, `manual.drop`, a rule's rejection; or zero and
no error in it) is not kept. The series carry the runner's name as their `host`, the one writer of
each, so each is written once a bucket; the spans are `source:datadog-agent`, so no span is counted
twice.

Rows go in segments of 256 KiB (a log block's text, LZ4's ratio plateau, §12.6), each a write's
item of its own (`SPANS`, as `LOGS` is), packed whole beside points and records and refused at the
same bound as records (§8): a segment's buffer comes back to the receivers for the next request's
rows once it is in a write.

**Found by their trace.** A block of spans' token filter holds each one's trace id (its 16 bytes
hashed, `kiln_obs::logs::bloom::id_hashes`, not its hex as a token: ids of one family, as tracers
make them, Datadog's zero-padded 64-bit ids or X-Ray's time-prefixed ones, must hash apart), every
place in it salted by the block (`salted`, from its summary's first time and rows: filters of one
size otherwise set the same bits for the tokens every block holds, a service's name, `server`, so a
query whose places fell on those passed most filters; measured, §12.23) and
leaves out what a word search never wants and each span has one of (`duration`, `parent_id`,
`events`, `links`: a search of them is never ruled out by it), so `trace_id:<hex>` reads only the
blocks that hold the trace, whatever the range: a trace's page finds its spans in the whole
retention without a time. Logs' filters hold no trace ids (their blocks were sealed without them);
`trace_id:` searches logs by the range alone, and a trace's page asks its logs over its own minute.

**A trace** (`kiln_obs::apm::assemble`) is its spans as a waterfall draws them: each root (a span
whose parent the trace does not hold), then its descendants depth first, siblings by their start; a
span sent twice (an exporter's retry) once; a cycle of parents (malformed) broken where it is met.
The earliest 10,000 are given (a browser parses some 10 MB of them in a tenth of a second), and
the answer says when there were more.

**Trace metrics** are Datadog's, counted on the server as metrics from logs are (§8.3: each
window of ten seconds whole once its spans have come, again when more come for it; one writer, so a
span sent to any node counts once): of the spans its trace agent computes stats of, a service's
entry spans (its servers', its consumers', and a trace's root that calls out to nothing) and its
exits (its clients' and producers'), never a span within a service (a function's span would make
series of every function):

| metric | of | by |
| --- | --- | --- |
| `trace.hits`, `trace.errors` | entry spans (errors: those of status ERROR) | `service`, `resource_name`, `span.kind`, `env`, `version`, `http.status_code` |
| `trace.duration` | their durations, in seconds: a distribution (§4.4), any percentile | the same |
| `trace.outbound.hits`, `.errors`, `.duration` | exits | `service`, `resource_name`, `span.kind`, `env`, `peer.service` |

Each definition keeps 2,000 groups a window at the most (OpenTelemetry's cardinality limit), past
it `otel.metric.overflow:true`, as metrics from logs do. So monitors, SLOs and dashboards read APM
as any metric (`sum:trace.errors{service:checkout}.as_count() / sum:trace.hits{service:checkout}.as_count()`),
and Datadog's "generate metrics from spans" is `span_metrics` (§7), its filters span searches and a
distribution of `@duration` in seconds.

**Services** (`kiln_obs::apm::red`) are read from the trace metrics: over any range, each group's
counts summed and its durations' buckets merged before a percentile is read, so a p95 over a day is
the day's, not an average of windows'. A long range is read from the series' rollups (§4.7): the
coarsest tier at most a twelfth of the range wide, so the two windows at its edges it reads as
samples at the most are a sixth of it; its counts the windows' sums and the samples' added, its
most the greatest of theirs (the same figures as every sample's, a sum within 1e-9 of it, its
counts and percentiles to the bit; a test holds the one to the other). **The service map** (`kiln_obs::apm::MapBuilder`) is read
from the spans: a span whose parent is another service's is a call of that service to its own (the
caller's client span's duration the call's), a client's or a producer's span no other service's span
answered is a call to its `peer.service`, named only. Past 200,000 spans in the range (the blocks'
summaries say how many before any is read), one trace in `k` is read, chosen by its id (W3C's random
low 64 bits, mixed again), so a trace is read whole or not at all.

Its figures are the trace metrics' (`kiln_obs::apm::figures`): every request and call, the traces a
tracer, an Agent or the map itself left unread too. Each span read is in the group of the metrics
that counts it: a call out (a client's or a producer's) its service's calls to the peer it names
(`trace.outbound.*` by `peer.service`); an entry (a server's, a consumer's, a root that is no call;
of a Datadog tracer's, any top-level span that is no call, as its stats count them) its service's
requests (`trace.*`). An edge is counted by the caller's call where it made one, else by the
callee's span (an entry; or a call, of a client library's service, ddtrace's `requests`). Its calls
are then its share of each such group: the group's calls in the metrics times the part of the group's
spans read that are the edge's; its errors the same of the group's errors read (where some were,
else of its calls), so the errors of an edge counted by its caller's calls out are as the caller
saw them (a client's span marks its own, by its tracer's rules: ddtrace's an HTTP client's 4xx, a
server's 5xx), and of one counted by the callee's spans, the callee's; its mean duration the
group's. That is the ratio estimator (Cochran, *Sampling
Techniques*, ch. 6), each trace a cluster of spans: consistent, its bias of the order of one over
the traces read, as far as the traces read of a group are a sample of it independent of where its
calls go (a trace kept by its id and its root's rate, read by its id); and exact where a group's
calls all go one way, as nearly all do (a client's calls to one peer, answered by one service): an
edge's figures are then its group's, as a service's are its own, all as of the windows the metrics
have counted. A group the metrics have nothing of in the range (or a span
none counts: OpenTelemetry's spans of no kind whose parent is another service's) gives the spans
read, `k` times, and the edge says so (`counted` false). A call out the metrics count that no span
read shows (every trace of it dropped) is an edge all the same, to the peer it names (inferred,
unless a service of that name serves requests). Each service's requests and errors are its entries'
(a service only of calls, a client library's, its calls'), the spans' only where the metrics have
none of it; a peer only named, what its callers call it; a service of requests none of whose spans
were kept is on it still.

### 8.8 Error Tracking

Errors are grouped into issues as Datadog's Error Tracking groups them (`kiln_obs::errors`,
`crate::observe::errors`). An error is tracked as Datadog tracks it: the error a request failed with,
once. Of a span: where its service's entry span (an OpenTelemetry root, server or consumer span; a
Datadog tracer's top-level span, its parent another service's or none) is in error, the error said
with its type, message and stack (Datadog's `error.type`, `error.message`, `error.stack`;
OpenTelemetry's exception event, read into those as Datadog reads it), the stack of two lines at
least with a frame: by the entry itself, or (a Datadog tracer's, whose chunk holds the service's
spans) by the top-most span of its service under it that says it. An error reported by many spans
of a service is one, its top-most (Datadog's); ddtrace's Flask, for one, says it on the request's
dispatching span, its entry only a 500 (recorded: `fixtures/datadog/traces-7.84.pb`). Of a log: its
status an error's or worse, its service named, its error's kind (`error.kind`, `exception.type`) or a
stack with a frame said. A user's `error.fingerprint` groups as it says, within its service.

Within a service, one issue is the errors of one type whose messages have the same words, thrown at
one place. The words are Datadog's: numbers, punctuation and whatever is quoted or bracketed left
out, a word with a digit in it too (an id, a hash), so `bad item -42 in cart 'c-294'` and
`bad item -7 in cart 'c-49'` are one. The place is a frame of the stack: the innermost of the
application's own (Sentry's in-app frames: a frame of a language's library, of a runtime, of what
the application installed, packages, modules, gems, vendored code, is passed over), else the
innermost with a file. Datadog takes the topmost frame; kiln takes the application's, so a
library's error (json's `JSONDecodeError`, raised in json's own code) is grouped where the
application called the library, one issue a call site, as a person fixing it reads it. A frame is
its file's name and its function, never its line (an edit above it would move every issue), with
what varies from build to build left out: versions and hashes in a file's name (`main.3f2a9b1c.js`),
a compiler's numbering of lambdas and closures (Java's `lambda$main$3`, .NET's `<Main>b__5_0`,
Go's `func2`, Rust's `{closure#1}`, PHP's `{closure:/app/x.php:30}`).

Stacks are read as each language's tracers and SDKs write them, recorded from the runtimes
themselves (`crates/kiln-obs/src/fixtures/errors/`, the programs that made them beside them):
Python's tracebacks (the innermost last; a chained exception's last traceback), the JVM's
(`Caused by:` its cause's), V8's and Firefox's, .NET's (the inner exceptions' frames first), Go's
(dd-trace-go's, and a panic's past its `panic(`), Ruby's (dd-trace-rb's, its causes after),
PHP's and Rust's. A stack is read to 64 KiB at the most.

An issue's id is a hash of its parts (FNV-1a, then splitmix64's finalizer: frozen, as an issue is
kept and searched for by it), sixteen hex digits, written onto the error as it is taken
(`issue.id`: in the trace decoders for spans; where every log is offered, after the pipelines that
name its parts, for logs). An `issue.id` an error came with is not kept: the id is kiln's alone.
Spans' issues and logs' are never the same, as Datadog keeps its products' apart.

Each issue's errors are counted on the server, as metrics from logs and from spans are (§8.3), in
`error_tracking.errors` by issue, service, env and version: every error once, whichever node took it.
Every ten seconds the server reads that metric over the last twenty minutes (a window is counted again
for late records that long): an issue it has not seen is kept, its type, message and frame read from
its first error in the span or log store; each one's first and last error, its versions and envs
follow its counts. Its status is Datadog's: FOR REVIEW as it comes, REVIEWED, RESOLVED or IGNORED as a
user says (`PUT errors/issues/{id}`). Fourteen days without an error resolve it (Datadog's). A RESOLVED
issue erring again is a regression, back FOR REVIEW, when the error is of a version it had not been
seen in when it was resolved (an old deploy still running does not reopen it), or of any version
where its service says none (Datadog's regression detection). An IGNORED one stays so. Issues are
kept in `<observe>/issues.json` (a user's change at once, what the metric says a minute after at the
most), 10,000 at the most: past them, the one longest without an error among those resolved or
ignored goes, else the one longest without an error.

Issues are told as Datadog tells them, by Error Tracking monitors (§9): one of new issues tells each
as it comes and as it regresses, one of impact each past a number of errors, to the integrations
their messages name.

### 8.9 Sensitive Data Scanner

What records hold that should never be kept (`crate::observe::scanner`), Datadog's Sensitive Data
Scanner for logs: scanning groups in order, each a filter (a log search; empty, every record) and
rules (50 a group, 20 groups); each rule a pattern, one of the standard library's or a regular
expression of its own. It scans log records, kiln's own events among them, wherever they come from
(the API, OTLP, Datadog's Agent, a cloud's pull, a log source); spans are not scanned. The server
keeps them (`<observe>/scanner.json`) and a key made once (`<observe>/scanner.key`, never shown),
and tells every runner both; each node runs them where it takes a record, after the pipelines (a
rule reads what they parsed: a JSON line's fields, a grok rule's attributes), so what a rule
replaces is never stored, replicated, archived or sent anywhere.

- **The library** (`GET scanner/standard_patterns`): credit cards (Visa, Mastercard, American
  Express, Discover, JCB, Diners Club; each its Luhn digit checked), IBANs (their country's length
  and mod-97 checksum, ISO 13616), US Social Security numbers (in the ranges the SSA assigns), email
  addresses, international and North American phone numbers, IPv4, IPv6 and MAC addresses, and
  credentials: AWS access key IDs and secret keys, GitHub, GitLab, Slack, Stripe, npm, SendGrid,
  OpenAI and Anthropic keys and tokens, Google API keys, Slack webhooks, JSON Web Tokens, private
  keys in PEM's armor (whole), a URL's password, a bearer token, a password, secret, API key or token
  given its value (`password=…`, `"api_key": "…"`), Azure storage account keys and Datadog API keys.
  A pattern too loose alone (a 40-character AWS secret, a 32-hex Datadog key) counts only with one
  of its keywords near; a match's tags are `sensitive_data:<id>` and
  `sensitive_data_category:<category>`.
- **A rule**: what it finds; where it looks (`message` and attributes by path, an attribute's own
  beneath it too; none, everywhere) and where it does not; keywords one of which (case aside) must be
  within `character_count` characters before a match (30; 50 at the most), its own or its pattern's;
  what replaces a match: a string (`[sensitive_data]` unless said), its first or last N characters
  hidden (`*` each), a hash, or nothing (the record tagged alone); its tags; its priority (1, critical,
  to 5). A pattern of its own is Rust's regular expression (linear time: no look-around); a group
  named `v` in it names what of a match is replaced (`user=(?P<v>\w+)`), as the library's do for a
  URL's password or a token after `Bearer`. One that matches the empty string is refused.
- **Hashes** are HMAC-SHA256 under the kiln's key (its first 8 bytes in hex): the same value hashes
  alike on every machine of a kiln, so records still group by it; with no key, no value is found
  again by hashing candidates (an unkeyed hash of an SSN or a card number is undone by trying them).
- **Matches**: every rule of a group whose filter picks a record looks through its message and
  attributes; of matches that overlap, the one that starts first (then the rule of higher priority,
  then the longer) is replaced; a rule that only tags never keeps another from replacing. A record's
  tags are its attributes, so a key several matched rules tag holds their values joined by commas
  (`sensitive_data:email_address,visa_card`), after any it held. Scanning again changes nothing.
- **Fast**: a group's rules without keywords are one set (one alone, a single expression, which
  looks for its literal first), its keywords one pattern (one pass each, which says whether any rule
  may match); a line no group may find anything in keeps the collector's and OTLP's fast path (its
  row written from the line, no record made), and that is checked against every scan in tests:
  whatever the check passes over, no rule would have touched.

### 8.10 Log indexes

Datadog's log indexes (`crate::observe::indexes`): which records are kept for search, how many a day
and how long. What is not kept is still seen live, archived and counted by metrics from logs, as
Datadog's are.

- **Routed** where a record is taken (each node: the server's API and intakes, a runner's OTLP
  receiver and collectors), after the pipelines and the scanner: into the first index, in their
  order, whose filter it matches; none, it is not kept for search. There, the first of the index's
  enabled exclusion filters that matches it decides alone: it is excluded at the filter's rate, by a
  hash so that every node decides alike (of the value of the filter's `sample_attribute` where it
  names one, a record without it kept; else of its trace id, a trace's records together, as
  Datadog's; else of the record itself, its message at its time). kiln's events are not an index's:
  a part of their own, whatever the indexes say. Until one is made there is one index, `main`, of
  every record: its part the main one, so nothing changes until an index is made. A run whose
  records go to more than one part is split, and only then: records moved into runs made their
  size, a collector's rows copied as written into buffers of their size (each row's time, its first
  field, chained anew from its part's row before); a run whose records all go into one part goes on
  whole. Where every record goes into one part (`main` as made), nothing is read but each record's
  source, and a collector's rows not at all. The server keeps the indexes (`<observe>/indexes.json`) and tells
  every runner, as it does pipelines.
- **Kept** each index's records as a part of the store of its own (§4.6), for its own time
  (`num_retention_days`; none, the store's); records not kept, an hour (archives take a cut's files
  within a quarter of an hour and half a minute; metrics from logs count within seconds;
  `KILN_OBSERVE_TRANSIT_MS` for tests). An index removed: its records no longer searched at once, its
  files let go within a minute (its part's cutoff moved to now), and its name, Datadog's rule, never
  used again (`main`'s aside, made again of the main part). Only a removal lets a part go: the list
  keeps the parts of indexes removed, and a part no list names is kept as long as the logs are (a
  leader that has not heard the list yet lets nothing go; a list that does not read is kept aside,
  said, and `main` takes every record until indexes are made again, each given a part the store
  holds nothing of).
- **Daily quota** (`daily_limit`): the records an index kept since its day began (its
  `daily_limit_reset`, `HH:00` at `±HH:00`; 14:00 UTC unless said, Datadog's), counted on the server
  from every write it proposes (its items' rows, read from their heads: nothing decompressed); at its
  warning threshold (50 to 99.99%), and at its quota, an event (`event.type:logs.quota`,
  `@datadog_index`), once a day each; past its quota, the index's records go where those not kept go,
  on every node (the server tells them at once), until its day turns: a write or two already on its
  way past it at the most (Datadog's quota is an approximate one too). A server that starts counts
  the day so far from the store, a block at a time (a block reaching back past the day's start
  counted whole: a little more, never less).
- **Estimated usage**, Datadog's metrics: `datadog.estimated_usage.logs.ingested_events` and
  `…_bytes`, counts of what each node took, tagged `datadog_index` and `datadog_is_excluded` (`N/A`
  for a record of no index), every 15 seconds of their own (a node may look at its host once an
  hour, or never); a record over its index's quota counted its index's. A record's bytes are its
  fields' (as its weight in the intake's bound is counted); a collector's rows, their bytes as
  written, whether `main` takes them whole unread or indexes read and split them.
  Not tagged by `service` or `status` as Datadog's events count is: the router reads no more of a
  record than it must.
- **The API**, Datadog's at kiln's paths: `GET/POST logs/config/indexes`, `GET/PUT/DELETE
  logs/config/indexes/{name}` (`disable_daily_limit` to remove a quota), `GET/PUT
  logs/config/index-order`; an index as Datadog gives one (`is_rate_limited`), and beside it, kiln's:
  its part and its day against its quota. Refused, saying why: a name not Datadog's (a lowercase
  letter, then lowercase letters, digits and hyphens), a query that does not read, a rate past 0 to
  1, a reset not `HH:00` or an offset past -12 to +14, a threshold past 50 to 99.99, an order not of
  every index; one past 100 indexes (Datadog's 422). Changing them is a maintainer's
  (`observe.manage`); reading them, a viewer's. A log search, aggregate, facets or patterns read the
  indexes named (`indexes`, Datadog's `filter.indexes`), else every one and kiln's events.

### 8.11 Continuous profiles

Datadog's Continuous Profiler (`crate::observe::profiles`, `kiln_obs::profile`): what an
application's profiler sends (where its CPU went, what it allocated, what it waited on), kept a
while, summed into flame graphs over a time and compared. Its sources' formats from their code and
specs: dd-trace-go's upload (`profiler/upload.go`, `profile.go`), google/pprof's
`proto/profile.proto`, OpenTelemetry's profiles signal (`profiles/v1development`, Development).

- **Taken** where Datadog's tracers send their profiles: a runner's `127.0.0.1:8126`
  `POST /profiling/v1/input` (where a tracer sends the Agent its profiles, beside its traces) and the
  server's `POST /api/v2/profile` (Datadog's own intake, the ingest token): a `multipart/form-data`
  body (RFC 7578) of a part a profile (its name the file's, `cpu.pprof`, `delta-heap.pprof`, …) and
  an `event` part (`event.json`: its `start` and `end`, its `attachments` in order, its profiler's
  `tags_profiler` joined by commas: `service`, `env`, `version`, `host`, `runtime`,
  `profile_seq`, …; its `family`, `version` 4, `endpoint_counts`, `custom_attributes`). A profile is
  pprof, gzip'd, zstd'd or in LZ4's frame format (libdatadog's exporter, under Python's, Ruby's,
  PHP's and .NET's tracers; told by its first bytes, nothing says which; LZ4's frames read by
  `kiln_obs::lz4::decompress_frames`, every checksum they carry checked: XXH32 of the descriptor,
  of each block and of the content); one upload is what Datadog calls a profile, its types collected
  about the same time. Each is read whole before it is taken (its event, its times, every profile
  inflated within 64 MiB and read as pprof, its types' values summed): a tracer hears what is wrong
  with it (400). Java's recording (JFR) is read into profiles where it is taken (below). An upload
  holding nothing kiln reads (an idle JVM's recording, nothing sampled) is taken and not kept,
  counted.
  Checked against what the tracers send, captured on the loopback: dd-trace-go v2.11.2's (gzip'd
  profiles, `metrics.json` beside) and ddtrace 4.16's for Python (libdatadog 1.0.0's exporter: a
  zstd'd `profile.pprof` of 19 sample types, a zstd'd `code-provenance.json` beside it, and
  `endpoint_counts` sent as `null`, which kiln first refused, so no Python profile ever came in:
  every optional field of `event.json` is read as none when it is `null` now). Any attachment not
  plainly another format (`.json`, `.txt`, a trace) is read, as a recording where its name says
  `.jfr` or its bytes begin as one (`FLR\0`), else as pprof whatever its name; one named `.pprof`
  or `.jfr` that does not read refuses the upload. An attachment is the part of its name, else the
  part whose file is of its name (dd-trace-java's: the part `main`, its file `main.jfr`). A tracer's
  container is its `Datadog-Container-ID`, or libdatadog's `Datadog-Entity-ID` of a container id
  (`ci-…`).
- **Java's recordings** (`kiln_obs::profile::jfr`): dd-trace-java uploads a minute's JFR recording
  (`main.jfr`, zstd'd unless set otherwise: gzip, LZ4's frames or none read alike, inflated within
  128 MiB), the JDK's chunks (its execution samples, allocation samples, monitor waits, file and
  socket I/O; and the tracer's own events, its exceptions sampled) and then its own profiler's
  (ddprof's: CPU and wall time sampled, allocations and the live heap by Poisson sampling), chunks
  concatenated. It
  is read where it is taken, into pprof profiles in Go's names, so everything that reads pprof
  reads them (the store, the queries, `go tool pprof`): `cpu.pprof` (`samples`, `cpu` ns),
  `wall.pprof`, `alloc.pprof` (`alloc_objects`, `alloc_space`), `live.pprof` (`inuse_objects`,
  `inuse_space`, a snapshot), `lock.pprof` (`contentions`, `delay`), `exceptions.pprof`,
  `file-io.pprof`, `socket-io.pprof`; each kind from ddprof where both wrote it (the JDK's
  execution samples are of Java threads in Java code alone). The recording itself is not kept (a
  minute's is MiBs; its profiles a few hundred KiB), and a runner sends its profiles on, not it:
  read once, at the edge. The format as the JDK, JMC, async-profiler and ddprof write and read it
  (no spec beside them): each chunk read alone (its 68-byte header; its metadata, the classes its
  events and pools are made of, bound by name, as ids mean other things in another chunk; its
  constant pools, a chain of checkpoints from the newest back; its events, each skipped by its size
  where it is not one kiln reads), JFR's varints (seven bits a byte for eight bytes, a ninth of
  eight; padded forms the norm), strings in each of its six encodings; every count, length and
  offset checked against what is left before anything is kept (chunks, strings, depth of the
  metadata's tree, pool entries, frames all bounded), a chunk that does not read dropped and the
  others kept. What an event weighs is its writer's, which the format does not say: ddprof's CPU
  and wall samples a weight of sampling intervals (the interval from the chunk's settings, else its
  configuration event, else 10 ms and 50 ms, its defaults), its allocation and live-heap samples a
  weight of objects (Poisson sampling's: an estimate of the objects each sample stands for) and
  bytes their size times it; the JDK's execution samples one period each, its allocation samples
  their weight in bytes, its monitor waits and I/O their duration. Frames are named as Datadog's
  profile pages name them (`package.Class.method`; a lambda's and a generated class's numbers left
  out, or each class made at run time would be a function of its own), a native frame by its
  symbol, its file `[native]`.
- **Sent on** by a runner as the Agent does: the tags an Agent adds (`host:` this runner,
  `container_id:` the tracer's `Datadog-Container-ID`) beside it, over the observe lane to the
  server (`wire::Msg::Profile`, in the lane's 512 KiB parts, 16 MiB at the most), only while the
  lane has room (64 frames waiting) and 64 MiB at the most waiting for it: profiles yield to the
  group's own messages, and one that finds no room is refused (503), as the Agent refuses past its
  queue. The server's intake adds an Agent's `X-Datadog-Additional-Tags` alike; a tag the profiler
  said itself is kept.
- **Kept** by the server alone (`crate::observe::profiles::store`), not replicated: a profile is
  read where it is asked of, and a server's disk lost, its profiles are (Datadog keeps them eight
  days, and so does kiln: `profiles_retention_days`). On a writer thread of its own, a bounded queue
  before it (256 MiB, 1,024 uploads: past it, 503), uploads taken in batches (32, or 64 MiB), each
  batch synced once. A segment is a blob file and a header file (`<observe>/profiles/<made ms>.kprof`,
  `.khead`), written for an hour or 256 MiB: each profile kept as it came (gzip'd, zstd'd, LZ4's
  frames; one sent as it was LZ4'd, or kept as it was where that is no smaller) and its endpoints'
  counts in the blob file, written and synced first; then the upload's header (its times, all its
  tags, each profile's name, its sample types and their values summed, its place, length and
  CRC-32C) framed by its length and CRC-32C in the header file, and indexed. The index holds 40 bytes
  an upload (its start and end, its series and its set of profiles interned, its header's place):
  a series is its family and its tags but `profile_seq`, which changes every upload; a million
  uploads (a hundred services profiled for a week) take some 40 MB. Starting, the server reads the
  header files alone (on the writer's thread: uploads are refused until it has); a torn tail (a
  crash mid-write) ends a file's reading, a header whose blobs pass its blob file's end is not
  indexed, and each start writes a segment of its own, so no file is written past a tail. A write
  that fails is cut back and its segment written no more. Whole segments are let go once their
  newest upload is past the retention; past the store's bytes (`profiles_max_gb`, a quarter of its
  disk unless said), the oldest first; and while the disk has under 1 GiB free beside what a batch
  needs, the oldest first again, a batch refused (counted) where that frees too little: profiles
  yield the disk to the group's own store. Never the segment being written for the last two.
- **Asked** (`/api/observe/v1/profiles…`, `crate::observe::profiles::query`): uploads chosen by
  their start and a search of their tags in log search's syntax (`service:checkout env:prod
  -host:web-2`, `version:1.4.*`: `service` and `host` a record's own fields, the family its
  `source`, every other tag an attribute), decided once a series; listed newest first (each with
  its header's types' totals); each tag's values counted (facets); the sample types they hold (a
  type named by its profile and its type: Go's mutex and block profiles both have `delay`); one type
  summed into a flame graph, of one upload or of all chosen (500 read unless asked, 5,000 at the
  most, evenly across the time where more hold it, the answer saying how many of how many), read on
  a quarter of the cores (eight at the most, each its own graph and reader at a lower priority, the
  graphs merged); a type of what happened over the profile's time (CPU, allocations, contention)
  summed, a snapshot's (the heap live, goroutines) averaged over time, each process's summed, as
  Datadog shows the heap (two snapshots summed would count the same memory twice); drawn root first, each node's children after it by their functions' names (as
  Brendan Gregg's flame graphs and pprof's), a node under one in ten thousand of the whole, or past
  50,000 nodes, left out with all under it; and its functions by self, a recursive function's
  total counted once. **Compared** (Datadog's comparison; Brendan Gregg's differential flame
  graphs): the same type over another time or another search (the period before, a day or a week
  before; a version against another), summed alike, the two graphs' paths joined (a path only one
  has is of none in the other), drawn as one side sizes them, and the functions by how much their
  self's share of each whole changed. An upload's profile is read back as pprof, inflated, for `go
  tool pprof`.
- **Shown** (Observe → Profiles): the flame graph root at the top, Datadog's way up, a frame clicked
  filling the width with its callers across above it; frames coloured by where their function
  comes from (your code, a library, the runtime and standard library: kiln's three hues any two of
  which may touch, and grey), each named where it is wide enough, a function found by name lit and
  the rest dimmed; the functions by self beside it; compared, each frame coloured by how its share
  of the whole changed, rose for larger and blue for smaller (kiln's pair, grey between), as far as
  the most changed, its share before in its tooltip, the functions by change beside it; the uploads
  listed, one chosen its own graph and its profiles to download; the unified service tags as
  choices.
- **Read** in place (`kiln_obs::profile::pprof`): its string table first (index 0 the empty
  string), its sample types, functions and locations (a location's lines its inlined functions, the
  last the caller), then each sample's stack (leaf first, its locations' functions in order) and
  values, one a type, and labels (Go's `bytes`, a trace's `span id`, `local root span id`, `trace
  endpoint`); every index and id checked, every size bounded (a million samples, locations and
  functions; a stack's 512 leaf-most frames, a deeper one cut and counted). Checked against Go's own
  profiles (a CPU and a heap profile written by Go 1.27's `runtime/pprof` in a container: their
  totals what Go's `pprof -raw` sums, to the sample and the byte).
- **Summed** into a flame graph (`kiln_obs::profile::flame`): each sample's stack root first, a node
  a function under its caller's node; each node its total (its subtree's values) and its own;
  functions and files interned, a node found under its parent by a table of node ids hashed by
  (parent, function) with kiln-obs's keyed hash (the node itself the key: four bytes a node); a
  graph bounded (a million nodes: a stack past it summed into its deepest node's own, the totals
  whole); graphs summed apart merged path by path.
- Status: done (O-33, O-34). Java's recordings checked against dd-trace-java 1.67.1's own (its debug dumps, two kept as fixtures: the JDK's chunk and ddprof's, and the JDK's alone) and a reader written apart (`kiln-obs/src/fixtures/jfr/check`: jfrdump.py's reader, jfrsum.py's sums; kiln's the same to the nanosecond), live through a runner and in a browser (§13); measured (§12.37).

### 8.12 LLM observability

Datadog's LLM Observability (now its "Agent Observability"; `crate::observe::llm`,
`kiln_obs::llm`): an application's calls to language models and what they cost, its agents' and
tools' steps, read from the spans it already sends. Its sources' formats from their code and specs
(research of 2026-10-23, every name checked against its source at a pinned commit): OpenTelemetry's
GenAI semantic conventions (v1.41.0, their last release in the core repository, and the
`semantic-conventions-genai` repository's main, where they moved, unreleased), Datadog's dd-trace-py
4.16 (`ddtrace.llmobs`: its span events, its intake) and its docs (its OpenTelemetry mapping, its
`ml_obs.*` metrics, its cost), Arize's OpenInference, Traceloop's OpenLLMetry, and Claude Code's own
telemetry.

- **Recognized** where a span is taken (OTLP's traces, Datadog's tracers' spans, Datadog's LLM
  intake), as an error's issue is (`kiln_obs::llm::spans`): a span is an LLM span if it says one of
  the conventions does (any `gen_ai.*` or `llm.*` attribute, `openinference.span.kind`,
  `traceloop.span.kind`, `langfuse.observation.type`, Datadog's own `meta.span.kind`) or is Claude
  Code's (`claude_code.*`); any other span is passed over by its attributes' keys alone, nothing
  read. An LLM span is read whole, in one pass over its attributes, before they are bounded (an
  instrumentation that flattens a conversation into hundreds of attributes keeps its tokens), and
  room is kept for kiln's (24 at the most). It is given Datadog's fields as attributes beside its
  own, each where it says none of the name, so Datadog's LLM queries read it as they read
  Datadog's (`@meta.span.kind:llm`, `@meta.model_name:claude*`, `@metrics.total_tokens:>1000`):
  `meta.span.kind` (Datadog's kinds: `llm`, `embedding`, `retrieval`, `tool`, `agent`, `workflow`,
  `task`, by its operation as Datadog maps OpenTelemetry's: chat, text_completion,
  generate_content, completion an `llm`; embeddings an `embedding`; execute_tool a `tool`;
  invoke_agent, create_agent an `agent`; retrieval a `retrieval` (Datadog's docs leave it a
  workflow); anything else a `workflow`; OpenInference's, OpenLLMetry's and Langfuse's kinds as
  Datadog maps them; a span of no operation that names a model or tokens, OpenTelemetry's before
  1.27, an `llm`; Claude Code's request an `llm`, its tool a `tool`, its interaction an `agent`, the
  rest a `task`), `meta.model_name` (the response's model, else the request's), `meta.model_provider`
  (the older `gen_ai.system`'s names read as the newer: `xai` as `x_ai`, `az.ai.*` as `azure.ai.*`,
  `vertex_ai` and `gemini` as `gcp.*`, `mistral_ai` as main's `mistral`; a model's call that names
  none `custom`, as Datadog's), `ml_app` (its service), `session_id` (`gen_ai.conversation.id`,
  OpenInference's `session.id`), and its tokens as Datadog counts them: `metrics.input_tokens`
  every input token (cache reads and writes in it: OpenTelemetry's, Datadog's and OpenInference's
  count them in; Claude Code's leaves them out and they are added back; an input under its own
  cache reads and writes, an instrumentation passing Anthropic's on, says it left them out),
  `metrics.output_tokens`, `metrics.total_tokens`, `metrics.cache_read_input_tokens`,
  `metrics.cache_write_input_tokens` (`cache_creation` in v1.40–1.41, `cache_write` after),
  `metrics.non_cached_input_tokens`, `metrics.reasoning_output_tokens`. Each convention's names
  read: OpenTelemetry's of every version since 1.26 (`prompt_tokens` before 1.27), OpenInference's
  `llm.token_count.*`, OpenLLMetry's own and legacy `llm.usage.*`, Claude Code's `input_tokens`,
  `cache_read_tokens`, `cache_creation_tokens` (on its own spans alone: the names are generic).
  Datadog's tracer copies an LLM span's fields onto its APM span as `gen_ai.*` tags, marked
  `_dd.llmobs.artificial_gen_ai_tags`: such a span is passed over (its LLM event comes apart, and
  is read), unless it carries the event itself (below).
- **Costed** as Datadog costs it: what a span says it cost (Datadog's `metrics.*_cost` in dollars,
  OpenInference's `llm.cost.*`, OpenLIT's `gen_ai.usage.cost`, Claude Code's `cost_usd`) is taken as
  it is (`cost_source:user`; parts it does not say are the catalog's); else its tokens priced from a
  catalog (`cost_source:auto`, `kiln_obs::llm::prices`): pydantic's genai-prices (MIT), the catalog
  Datadog's docs name, a snapshot of it kept in kiln (commit `b267c2e`, 2026-10-23: 47 providers,
  2,081 models), each matched by its provider and its name's rules (equal, a prefix, a suffix,
  a part, a pattern; a compact date in a name read as a dashed one), its prices per million tokens
  of each of 59 units, tiers past a prompt's size (all of its tokens at the tier's price, the
  catalog's cliff), prices of a date or a time of day (a span priced as it was when it ran), a
  batch's and a service tier's (OpenAI's `openai.response.service_tier`). Each unit billed once, on
  its own count (cache reads and writes out of the input, a 1-hour write out of the writes, each at
  its price). A provider is named as the catalog names it where its rules would miss
  OpenTelemetry's (`x_ai` its `x-ai`, `azure.ai.*` its `azure`, `gcp.gen_ai` its `google`); one not
  named (Datadog's tracer's `unknown`, for a client of another address; `custom`) is found by the
  address called (`server.address`), else by the model's name. Held to the catalog's own Python
  package: a fixture of 4,000 of its answers (drawn from a run of 29,196: every model of the main
  providers and three of each other's, each asked under the ids spans name its provider by, with
  tokens of every kind, past and under tiers, dated across price changes, in each price context;
  the whole run checked the same way), kiln's the same model, priced the same to the nanodollar, or
  refused alike. Kept in nanodollars, as Datadog keeps them: `metrics.estimated_total_cost`,
  `.estimated_input_cost`, `.estimated_output_cost` and Datadog's parts
  (`.estimated_non_cached_input_cost`, `.estimated_cache_read_input_cost`,
  `.estimated_cache_write_input_cost`, `.estimated_reasoning_output_cost`); the catalog's model
  (`matched_model_name`, `matched_model_provider`); a cost not to be had said, not guessed
  (`cost_unavailable`: Datadog's reasons, an unsupported provider or model name, missing token
  counts).
- **Datadog's own intake** (`kiln_obs::llm::intake`), as dd-trace-py 4.16 was recorded sending it
  (fixtures of its three paths): `POST /api/v2/llmobs` on the server (`DD_LLMOBS_OVERRIDE_ORIGIN`
  pointed at kiln) and the Agent's `/evp_proxy/v2/api/v2/llmobs` on every runner's tracer port,
  whose `/info` now lists the proxy and spans' `meta_struct`: a tracer with APM on sends its LLM
  events riding its APM spans (`meta_struct["_llmobs"]`, MessagePack, read into the span itself:
  one row, Datadog's fields over the tags its tracer copied, its LLM trace and parent said as
  `llmobs.trace_id` and `llmobs.parent_id`), and those its APM traces don't carry through the
  proxy. Past Datadog's own Agent, the same: it forwards a span's `meta_struct` (an older Agent in
  its span's field 13; Agent 7.84, in its indexed form, as an attribute of bytes, `_llmobs`, as one
  was recorded forwarding them), read into the span alike, not kept as bytes. An event of the intake is a span of its LLM trace, as Datadog keeps an app's LLM traces
  apart from its APM traces (the APM spans its tracer sends beside them are marked
  `_dd.llmobs.submitted`; `_dd.apm_trace_id` names their trace), source `llmobs`, its fields its
  attributes under Datadog's names (`meta.input.messages`, `meta.output.value`, ... as JSON;
  `meta.metadata.<key>`; `metrics.<key>`; each tag's `key:value`). Datadog's documented spans API
  (`/api/intake/llm-obs/v1/trace/spans`) is read too. Evaluations
  (`/api/intake/llm-obs/v2/eval-metric` and v1's, and the proxy's) are records beside the spans
  they judge (source `llmobs.eval`, its span's ids, its label, type, value, assessment,
  reasoning). A value is kept to 25,000 bytes as any span's is (Datadog keeps an event to 5 MB:
  a long conversation's messages are cut).
- **Metrics** Datadog derives (`ml_obs.*`, `kiln_obs::llm::metrics`), as metrics from spans,
  counted on the server once a span's window is whole as trace metrics are (§8.7), their prefix
  kiln's: `ml_obs.span`, `.duration`, `.error`; the tokens' distributions
  (`ml_obs.span.llm.input.tokens`, `.output`, `.total`, `.input.cache_read`, `.input.cache_write`,
  `.input.non_cached`, `.output.reasoning`, `ml_obs.span.embedding.input.tokens`); cost's
  (`ml_obs.span.llm.total.cost` of calls and embeddings, `.input.cost` and its parts,
  `.output.cost` and its reasoning's, `ml_obs.span.embedding.input.cost`, nanodollars);
  `ml_obs.trace`, `.duration`, `.error` of an LLM trace's root (Datadog's tracer's: no LLM parent;
  any other's: no parent); by `ml_app`, `model_name`, `model_provider`, `span_kind`, `env`,
  `service`, `version`, `status`; tokens' by the catalog's match in place of the kind and status,
  cost's by its source too (a metric from spans is grouped by eight facets at the most: the kind is
  in a token metric's name, and a cost's matched provider is its model's). Datadog's legacy
  `prompt`/`completion` names and its character counts are not made.
  Claude Code's own metrics (`claude_code.cost.usage`, `claude_code.token.usage`, ...) come in as
  any OTLP metric does.
- **Asked and shown** (Observe → LLM, `ui/src/views/observe/llm.tsx`): its apps (one chosen by a
  click); over the range, its calls, tokens, cost, share of errors and models; its cost and tokens
  by model over time, its calls and errors, its calls' latency (p50, p95: the `ml_obs` metrics);
  its models, each one's calls, input and output tokens, cost, errors and mean time (aggregates of
  its spans); its newest LLM traces (its newest steps grouped by their LLM trace, each shown by its
  first, so a trace under a request's span is listed too); a trace's steps as a tree (each under its
  LLM parent: Datadog's tracer's says it, any other's is its span's parent where that is a step),
  each one's kind, name, model, tokens, cost and time; a step opened: its tokens, cost (or why it
  has none), status and error, its input and output as messages (Datadog's, OpenTelemetry's
  `parts` with its system instructions, as attributes or its span's events, OpenInference's and
  OpenLLMetry's flattened ones, else its value; where its span says none, its log events:
  OpenTelemetry's message a record of 1.28 to 1.36, `gen_ai.choice` its output, or 1.37's
  inference details; the tools it calls), its evaluations, its match in the catalog, and its span
  in the APM trace; its sessions (steps that say a session's id: each one's steps, cost and errors, the
  costliest first), one chosen narrowing its traces to it. And a dashboard of kiln's own (LLM
  Observability, `builtin:llm`: cost, calls, tokens by model and what the cache served, calls and
  errors by kind, a call's latency, the costliest apps, traces), its app a template variable.
- Status: under way (O-35): checked live and in browsers (§13, 2026-10-24); what it costs the trace
  decoders is measured next (§12).

## 9. Monitors

A monitor is Datadog's (`kiln_obs::monitor`, `crate::observe::monitors`): a query, a message and
options, so a monitor written for Datadog reads here. Its type, when it says one, is Datadog's
(`metric alert`, `query alert`, `log alert`, `trace-analytics alert`, `slo alert`,
`error-tracking alert`, or `composite`); its query says its kind.

- **Queries**: `avg|sum|min|max|last(last_5m):<metric query> > 90`; change monitors
  `change(avg(last_5m),last_1h):…` and `pct_change(…)`; anomaly monitors
  `avg(last_15m):anomalies(<metric query>, 'robust', 3) >= 0.5`, whose value a group is the share of
  its window's buckets past the band on the side asked (Datadog's: `>= 1`, all of them); log monitors
  `logs("service:web status:error").rollup("count").by("host").last("5m") > 10` (count,
  cardinality, sum, avg, min, max of a facet); trace analytics monitors, Datadog's, the same of a
  span search (`trace-analytics("service:checkout status:error").by("resource_name").last("5m") > 5`,
  or `spans(...)`; §8.7). A string's `\"` is a quote within it, as Datadog's monitor API has it
  (`logs("\"connection refused\"")`). A window is whole buckets of a step that divides it, aligned
  to the step.
- **States** move with hysteresis: a group alerts past the critical threshold and recovers only
  past the recovery threshold (warnings the same), so a value at a threshold does not flap; NO DATA
  after a timeframe when asked; groups silent a day are forgotten.
- **Options**: thresholds (critical, warning, recoveries), `notify_no_data` and its timeframe,
  `renotify_interval`, `evaluation_delay` (late data counts), `every` (60 s unless it says, 10 s
  at the most often); mute until a time or unmuted.
- **Messages** are Datadog's templates (`{{value}}`, `{{threshold}}`, `{{host.name}}`,
  `{{#is_alert}}…{{/is_alert}}`, `{{^is_recovery}}`, …), their `@slack-<name>`,
  `@discord-<name>`, `@teams-<name>`, `@pagerduty-<name>`, `@webhook-<name>` mentions naming
  integrations (§10).
- The server, the leader, evaluates each on the blocking pool; each group's change is an event
  (`source:kiln.event @event.type:monitor.alert`, …) and, unless muted, its notifications; states
  are saved as they change. Each evaluation holds the store on its own (a write waits on no more
  than one); a monitor of metrics (an SLO's too) reads a round of series at a time (§12.19); one of
  logs or spans reads its records as they are, the store let go, as the explorers do (§13, logs
  under load), and so do a notification's searches (a check's last run, an issue's stack).
- **Forecasts**: Datadog's `max(next_1w):forecast(<metric query>, 'linear', 2, ...) >= 90`: a group's
  value is its forecast's highest upper bound over the next window (`max`, alerting above), or its
  lowest lower bound (`min`, with `<=`, below): "the edge of the forecast confidence bounds goes above
  or below the threshold within the next" window, as Datadog says; an hour to three months ahead,
  read in 96 buckets (or the forecast's `interval`). A group whose forecast cannot be made (too
  little history) has no value.
- **Composites** (`type: "composite"`, `kiln_obs::monitor::composite`): Datadog's, a condition over
  up to ten other monitors, none of them a composite, by id in the API (`m1 && (m2 || !m3)`) and by
  letter in the form (`a && (b || !c)`). States are weighed by severity, ALERT over WARN over NO
  DATA over OK, the first three alert-worthy: `a && b` is the less severe of the two (OK unless both
  are alert-worthy), `a || b` the more severe, `!a` OK where `a` is alert-worthy, ALERT where it is
  OK and NO DATA where it is NO DATA; a result of NO DATA keeps the last state unless the composite
  asks to be told. All of it is Datadog's page's, and its table of two monitors is a test row for
  row; Datadog states no precedence, so `!` binds tightest, then `&&`, then `||`. Grouped, a
  composite looks only at what its monitors share, a group known by its tags' values (`host:web04`
  is `service:web04`'s): its groups are those of its most finely grouped monitor that each other
  grouped one holds a group of (`device,host` holds `host`; `host,url` and `device,host` share
  nothing); a monitor not grouped stands for every group. It is evaluated right after any monitor it
  reads, and at its own interval besides; a monitor a composite reads is not deleted while it does
  (409, naming the composites), nor made a composite itself.

- **Outliers**: Datadog's `avg(last_1h):outliers(<metric query> by {host}, 'dbscan', 1) > 0`: a
  group's value is 1 if its series is an outlier of the query's groups over the window, 0 if not;
  each outlier alerts on its own.
- **Error Tracking** (§8.8): Datadog's,
  `error-tracking("service:shop").source("backend").new().rollup("count").by("issue.id").last("1d") > 0`
  (its New Issue alert) or `.impact()` (its High Impact alert). A group is an issue's (and its env's,
  version's or service's, as `.by` says); its value, the issue's errors over the window where the
  search holds of them: of the issue (its service, type, message, file and function: `service:`,
  `@error.type:`, words of its type and message) and of each error's env and version. A monitor of
  new issues watches those FOR REVIEW that came (were made, or regressed) after it was made or last
  changed, and within a day (Datadog's lookback); one of impact, those FOR REVIEW or REVIEWED; RESOLVED
  and IGNORED issues never. A group in trouble whose issue it no longer watches, or without errors in
  the window, counts none and recovers (a count of a group gone quiet is zero, as Datadog evaluates
  its event monitors), told unless its issue is IGNORED (Datadog mutes an ignored issue; its event says
  `muted`). Kiln's issues are backends': `.source()` is `all` or `backend` (unsaid: all), and the
  rollup a count (Datadog's Error Occurrences). Its messages have Datadog's
  `{{issue.attributes.error.type}}`, `.message`, `.file` and `.stack` (the latest error's, searched
  for only when the message asks; what only a browser's or a phone's error says, `is_crash`,
  `category` and `handling`, nothing), and its notices link the issue's page.

### 9.1 Anomalies, forecasts and outliers

`kiln_obs::anomaly`: Datadog's three algorithms, each giving, before it sees a point, its expected
value and its scale (the robust spread of its own one-step errors: 1.4826 × their median absolute
value, the MAD's consistency constant; 1.2533 × their mean absolute value when that is 0, Iglewicz
and Hoaglin):

- `basic`: the median of the recent past; no season; follows a new level in half its window.
- `robust`: the learned season's median of the same phase in the last five seasons (two outlying
  seasons outvoted), moved by the level's median offset over the last season: steady through a long
  anomaly, slow to take a new level.
- `agile`: additive Holt-Winters (Winters 1960), its smoothing fitted to the history by least
  absolute one-step error over the usual grid (Hyndman et al. 2008), its updates Huberised at two
  scales (Gelper, Fried and Croux 2010) so that an outlier does not move it.

**The season is learned**: among a day, a week and the strongest peak of the differenced series'
autocorrelation (above the white-noise band, 3/√n), the period with which the robust algorithm's own
one-step errors on the history are smallest, if smaller than without one by more than twice the MAD's
standard error (1.166 σ/√n): a cycle is kept only if it predicts.

**A deviation is calibrated per series** (`anomaly::Scorer`): a point's error in scales (`z`) means
different things in a calm series and a wild one (NAB's anomalies peak at |z| = 3 in one series and
200,000 in another, and a smooth series without any reaches 140, §12.12), so a deviation is defined
so that the series' own 1-in-500 |z| (its 99.8% quantile, so far) counts as 3.09 deviations, as a
normal law's would. A band of `k` deviations is expected ± k of them; Datadog's `bounds` read the same
in every series. An episode, from a point past 3.09 deviations to the next back under them, has an
onset: an anomaly begins.

The history the scorer reads is all of what is asked (up to 20,000 points): more steadies the
baseline and the scale (§12.12). The calibration is the |z| of a detector that learned the history's
first quarter, over the rest.

Measured on the Numenta Anomaly Benchmark (§12.12): `robust` 52.6 (standard), 41.0 (rewarding few
false positives), 58.3 (rewarding few false negatives); `basic` 50.3, 41.0, 54.8; `agile` 31.3, 13.1,
41.1. Among NAB's published detectors, `robust` is above Random Cut Forest (51.7), Twitter's ADVec
(47.1) and the windowed Gaussian (39.6), below Relative Entropy (54.6).

Built on it: `anomalies()` in queries (§6), drawn as a band with its anomalies marked (§11), and
anomaly monitors (§9).

**Forecasts** (`kiln_obs::forecast`): Datadog's `forecast()` says what its algorithms are for, not
how they work, so kiln's were chosen on the M4 competition's data, scored as M4 scores them, the
scoring checked by remaking M4's own benchmarks to their published figures (§12.15):

- `seasonal`: additive Holt-Winters with a damped trend (Gardner and McKenzie 1985), its smoothing
  the grid's that errs least over the whole horizon from every season's start (one step ahead errs
  0.83 OWA, the horizon 0.63), learned from the last ten seasons (of 6, 8, 10, 12, 15, 20 and all);
  and the seasonal naive (the last season again).
- `linear`: `default`, Holt's linear method, damped or not as it errs least one step ahead, and the
  last value, from the last 30 horizons of points (of 10, 15, 20, 30, 40); `reactive`, Holt's
  undamped from the last 4, alone; `simple`, Theil and Sen's line through the history (the median of
  every pair's slope, robust to 29% of the points being wild; Sen 1968), at most 512 of its points.
- Each forecast is one of three, chosen per series on its own history: the smoothing, the naive,
  or the two averaged (M4's lesson: a combination beats its parts). Each is made from every origin
  of the history as it will be from the last, and the average is kept unless another erred less by
  a fifth: a steady trend (a disk filling) erred far less carried on than averaged with its last
  value, which lags it by half, so it is carried on; a random walk keeps the average.
- The bounds are the chosen way's own errors at each step ahead from those origins, their root mean
  square, never narrower than the step before: `deviations` of it either side (2 unless said).

On M4: hourly series (seasonal), OWA 0.556 against the seasonal naive's 0.628 and M4's Naive2's 1,
95% bounds (1.96 deviations) holding 94.3% of outcomes, MSIS 7.30 (M4's ETS 17.49, ARIMA 7.54);
daily series (linear, default), OWA 0.976 (M4's best entry 0.977, its combination of exponential
smoothings 0.978), 93.3% held, MSIS 28.9 (ETS 29.7, Naive 32.6). A series forecasts in 0.8 ms
(hourly) or 0.2 ms (daily) with ten allocations.

Built on it: `forecast()` in queries (§6), drawn past now with its band (§11), and forecast monitors
(§9).

**Outliers** (`kiln_obs::outlier`) are Datadog's four, as its engineers described them ("Introducing
Outlier Detection in Datadog", 2015): DBSCAN over the members as points, a coordinate a bucket, ε
the median member's distance to the median series times a normalizing constant times the tolerance,
every member outside the largest cluster an outlier; MAD over every point of every member, a point
past the median by the tolerance times 1.4826 MADs an outlier, a member one when more than pct% of
its points are; and each scaled, every bucket first divided by the group's median magnitude, so a
level all share moves no one. A distance between members with gaps is over the buckets both have,
scaled up to all of them. Datadog leaves DBSCAN's constant unsaid; kiln's is the smallest of 1.414,
1.6, 1.8, 2, 2.2, 2.5 and 3 with which a tolerance of 1 flags a member of noise alone at most 1% of
the time, in the worst of groups of 3, 5, 10 and 20 members over 10, 30, 60 and 120 buckets (400
groups each; `normalizer_study`): 2.5, at 1.0%, still catching 92% of members off by three
deviations everywhere and 99.9% of those off by five. √2, which makes ε the distance between two
ordinary members, flagged 66% of them in the worst group; 2, 5%; 3, 0.2%, but caught only 65% of
those off by three.

Built on it: `outliers()` in queries (§6), drawn with every member that is not one grey (§11), and
outlier monitors (§9).

### 9.2 Service level objectives

An SLO (`kiln_obs::slo`, kept by the server in `<observe>/slos.json`) is a promise of how much of
what a service does is good, over a rolling timeframe (7, 30 or 90 days), Datadog's model:

- **ratio**: good events over all of them, each a metric query of counts
  (`sum:http.requests{status:2xx}.as_count()` over `sum:http.requests{*}.as_count()`), every
  series of each summed;
- **time slices**: the share of one- or five-minute slices whose query's value meets a condition
  (`max:http.latency.p99{service:web}` `< 0.3`), of the slices with a value, each series' own.

Its **standing** is read from the store when asked (an SLO keeps no history of its own): the SLI
over the timeframe, the error budget left (the target's 99.9 allows 0.1% to fail; the budget left
is what of that share is not spent, below 0 once it is), its state (OK, WARN under its warning,
BREACHED under its target, NO DATA), each UTC day's SLI and the budget left through it (one read
at a day's step: the rollups' hours, §4.7), and its **burn rates** over the last 1 h, 6 h, 1 d and
3 d: the error rate there over the budget's (Beyer et al., *The Site Reliability Workbook*, ch. 5),
1 spending the budget in the timeframe exactly, 14.4 a 30-day budget's 2% in an hour. Each window
is read over its whole steps (a minute up to a day and a half, five minutes up to a week, an hour
beyond), ending where its last whole step does: a step begun a second ago is not yet one, and a
window holds no part of one either end.

**Alerts are monitors of it**, so everything a monitor does (messages, mentions, renotifying,
muting, composites) an SLO's alert does:

```
slo("<id>").burn_rate("1h", "5m") > 14.4     the lesser of the two windows' rates: both must pass
slo("<id>").error_budget() < 25              the budget left, percent
```

The burn-rate form is the workbook's multiwindow, multi-burn-rate alert: the long window says the
rate is real, the short that it still is, so an alert resolves within the short window of the
burning stopping. Its suggested pairs for a 30-day SLO: 14.4 over 1 h and 5 m, 6 over 6 h and 30 m
(pages), 1 over 3 d and 6 h (a ticket). Those are shares of the budget (2% of it in an hour, 5% in
six, 10% in three days), so another timeframe's rates are scaled to its hours: 3.36 and 1.4 for 7
days, 43.2 and 18 for 90; the SLO's page adds the two that page so. An SLO a monitor reads is not
deleted until the monitor is.

### 9.3 Downtimes

Datadog's downtimes (`crate::observe::downtimes`, `kiln_obs::monitor::downtime`): monitors'
notifications held for a while, for the monitors a downtime names (one by its id, every one that
carries all of some tags, or every monitor: `monitor_tags: ["*"]`, what a host's mute makes, §10.9)
and the groups its scope holds (`env:prod host:(web-1 OR web-2) -region:eu`,
a value a glob; `*` every group; a monitor of one group only by `*`). Its schedule is once (from a
time, now when none, to a time, until canceled when none) or recurring: up to five recurrences, each
an RFC 5545 rule, the wall-clock time its first occurrence begins at, and how long each lasts, in a
timezone. The rules taken are the ones downtimes use: `FREQ` of `DAILY`, `WEEKLY`, `MONTHLY` or
`YEARLY` (none oftener: an hourly downtime is a schedule's job, not a calendar's), `INTERVAL`,
`BYDAY` (with an ordinal in a month or a year: `2TU` Patch Tuesday, `-1FR` the last Friday),
`BYMONTHDAY` (from the end when negative), `BYMONTH`, `COUNT`, `UNTIL` and `WKST`; an occurrence
begins at the start's time of day on each day the rule names, a time a change of offset skips taken
after the change and one it makes twice the earlier, as RFC 5545 has it (a test holds rules to the
days they name across offsets: the second Tuesday of three months at 22:00 New York, EDT then EST).

A group a downtime holds still changes state (an event, `@downtime_id` its downtime's, `muted:true`)
and says nothing. As a downtime's window ends (it expires, or is canceled while it holds, as its
`notify_end_types` say: expiring alone unless said), each group it held that is in one of its
`notify_end_states` (alert, warn, no data: all three unless said) is told once, as a renotification;
and, if it asks (`mute_first_recovery_notification`), that group's first recovery after says
nothing. A window's end is told once whatever the server does meanwhile: the windows that hold are
looked at each second, an end found when the one that held is gone. Canceling keeps a downtime (its
status `canceled`); one canceled or ended is forgotten after 30 days. `<observe>/downtimes.json`.

### 9.4 Service checks

Datadog's service checks (`kiln_obs::monitor::checks`): a check's run, as the Agent submits it
(`/api/v1/check_run`, §8.6) or as DogStatsD's `_sc` datagram (§8.4), is a record
(`source:service_check`, its check, status, message and tags) and a sample of its status besides:
`kiln.service_check.status` (0 OK, 1 WARNING, 2 CRITICAL, 3 UNKNOWN, Datadog's codes) of the series
its check, host and tags name, written beside its record as kiln's own point. A check's last runs
are read from its series, a few compressed samples (a run's status a constant gauge's: a bit or two
a run), never from the blocks of every record; the record keeps what a run says.

Datadog's service check monitor (`type: "service check"`) reads them:
`"datadog.agent.up".over("*").by("host").last(2).count_by_status()`: the runs of the check's series
whose tags hold `.over`'s (a value with `*` a glob; `"*"` all), grouped by `.by`'s tags (none: one
group), each group's latest `.last(n)` runs in time order across its series (two series' runs at one
time, the worse the later). Its thresholds are runs in a row, the latest among them, as Datadog's
are: `critical` CRITICAL to ALERT, `warning` WARNING or worse to WARN, `unknown` UNKNOWN to NO DATA
(Datadog's "notify for Unknown status"; never unless said), `ok` OK back to OK; runs that settle none
of these leave a group as it was (one failure of two asked for does not alert, and does not
recover). Critical and ok are 1 unless said (Datadog's: the first failure alerts, the first success
recovers); `.last(n)` reads at least the most of them (Datadog's rule; 1 to 100). A group's runs are
read over the last quarter hour first (the Agent runs a check every 15 s) and further back only when
that holds fewer than it reads. A group is found while it has a run within the no-data timeframe (a
day unless the monitor says, Datadog's for a service check); one with none for that long goes to NO
DATA at once when the monitor asks. Its messages have Datadog's `{{check_message}}`, the group's
latest run's message (its record's, found by its check and the group's tags), and
`{{check_status}}`.

The check status widget (Datadog's `check_status`, §11) asks `POST checks/status`: of the check's
runs over the time whose tags hold its tags, each group's status, the worst of its series' latest
runs (CRITICAL, then WARNING, UNKNOWN, OK); by `group_by`'s tags, or each series a group of its own.

### 9.5 Incidents

Datadog's incident management (`crate::observe::incidents`): an incident declared from kiln's
pages, through the API (Datadog's v2 body), or by a monitor; worked as people work one; told to the
integrations it names.

- **What one is**: a title; a severity (`UNKNOWN`, `SEV-0` to `SEV-5`, the most severe first) and a
  state (`active`, `stable`, `resolved`), each one of its fields too, as Datadog's API has them
  (`fields.severity`, `fields.state`: dropdowns); its commander (a user, by id or name); whether
  customers are impacted (then how, said: its scope) and from when to when; its other fields by name,
  Datadog's types (`dropdown` and `textbox` a text; `multiselect`, `textarray`, `metrictag` and
  `autocomplete` a list of texts; a null value removes one): `summary`, `root_cause`, `services`,
  `teams` and `detection_method` among them; the integrations it tells (`notification_handles`:
  `@slack-ops`, `@pagerduty-oncall`, as a monitor's message names them); whether it is a test. Its
  public id (`#7`) besides its id (a UUID): either names it in every path, and a public id is never
  given twice.
- **Its timeline**, the oldest first: notes (Markdown, important or not), each change of its
  severity, state, commander or title (from what to what, by whom), its postmortem attached, and its
  monitor's alerts and recoveries (each its state, value and message). 1,000 cells at the most: past
  them its first 100 (its declaring and first notes) stay, and the oldest after them go.
- **Its todos** (Datadog's: what is to do, its assignees, a due date, done or not; 200 at the most)
  and **attachments** (a postmortem, one an incident, and links; HTTP(S) alone; 100). Its
  postmortem written by kiln is a notebook made of a template with its variables filled (§11.1).
- **Datadog's figures of it**, seconds, none below zero: time to detect (from the customers' impact
  starting to its detection), to respond (from its detection to its declaring), to repair (from its
  detection to the impact's end) and to resolve (from its declaring to its resolving), and the
  impact's duration. It is detected as it is declared unless said (`detected`).
- **From a monitor**: a monitor whose message names `@incident` (`@incident-sev2`: its severity;
  `@incident-<anything>`) declares one as a group alerts with none of that monitor's group open: its
  title the monitor's and the group's (`Checkout errors on service:checkout`), its severity the
  mention's (else unknown), its handles the message's other mentions, `detection_method: monitor`,
  the alert its first cell, all written at once under the lock that found none open (one incident a
  group's alert). While it is open, each change of that group the message names `@incident` in goes
  on its timeline (alerting again, recovering: people resolve it, not its monitor); once it is
  resolved, the group's next alert declares another. A muted group, or one a downtime holds,
  declares none, as it tells none; the first `@incident` of a message alone counts.
- **Told**: its declaring, and each change of its state or severity, to each integration its handles
  name, as monitors' notices go (§10): Slack's, Discord's and Teams' messages titled
  `[Declared] Incident #7: …` (`[SEV-1]`, `[Stable]`, `[Resolved]`) with its severity, state and
  commander; PagerDuty one incident an incident (its dedup key its public id's), triggered at its
  severity (SEV-0 and SEV-1 critical, SEV-2 error, SEV-3 warning, the rest info) and resolved as it
  is; Opsgenie one alert by the same alias, its priority its severity's (SEV-0 and SEV-1 P1, to SEV-5
  P5; unknown P3), closed as it is resolved; email; a webhook's `event` `incident.declared`,
  `incident.state` or `incident.severity`, its notice's `headline` and `facts`.
- **Searched** as Datadog's API searches them (`GET incidents/search?query=`): facets joined by `AND`
  (or a space), a facet's values by `OR` within its parentheses (`state:(active OR stable) AND
  severity:SEV-1`), `*` any part of a value, a word alone a part of the title, quotes keeping words
  together; facets `state`, `severity`, `commander`, `created_by`, `last_modified_by`, `postmortem`
  and `impact` (`true`, `false`), `public_id`, `is_test`, and its fields by name (`services:checkout`,
  `fields.teams:payments`); `OR` between facets refused, saying where it goes. What matched is counted
  by facet as Datadog's search counts it (state, severity, commander, …; each field whose values come
  from a list: dropdowns, multiselects, metric tags, autocompletes; not a text of one's own, a
  summary never the same twice; each facet's 100 most common values; the least and the most time to
  repair and to resolve), the newest first unless `sort=created`, a page of `page[size]` (10, 100 at
  the most) from `page[offset]`. Read in place under the store's lock and written as it is read: no
  incident copied, nothing allocated an incident (§12.32).
- **Kept** by the server, each incident a file of its own (`<observe>/incidents/<public id>.json`),
  written whole and atomically as it changes, and only it (a note writes its incident, not every
  one): a change is made to a copy, the copy written, and then kept, so what is not on disk is not
  answered either. The next public id (`next.json`) is written as one is removed (otherwise the
  newest file says it). 10,000 incidents and 64 MiB of their files at the most (about what they take
  in memory, §12.32): past either, the oldest resolved are let go; with none resolved to let go, none
  is declared, and a change to an open one is still made. A file that does not read is left as it is,
  said, and its id never given again; what a write cut short left is removed as the server starts.
- **The API** (kiln's, a session's; reading `observe.read`, the rest `observe.write`): Datadog's v2
  incidents API at kiln's paths (`/api/observe/v1/incidents…`): declared (`POST incidents`,
  Datadog's body: `data.attributes`, `initial_cells` its first notes, `data.relationships.commander_user`),
  listed (a page, the newest first; `include=users,attachments`), searched, read, changed (`PATCH`:
  what the body says; a null clears) and removed; its todos (`relationships/todos`) and attachments,
  Datadog's; and kiln's own, its timeline (read, a note put on it). Times RFC 3339 (or ms), figures
  seconds; kiln's own beside, under `attributes.kiln` (its monitor, its todos counted, its
  postmortem). At Datadog's own paths (`/api/v2/incidents…`) the ingest token, a writer of data,
  reads and changes none (403).

## 10. Integrations

Notifications (`crate::observe::notify`), each in its service's format: Slack (incoming webhook,
Block Kit in a colored attachment), Discord (webhook embeds, within its limits), Microsoft Teams
(an Adaptive Card, what Workflows webhooks take), PagerDuty (Events API v2: trigger while
alerting, resolve on recovery, one dedup key a monitor's group), Opsgenie (the Alert API v2,
`GenieKey`: an alert created while alerting, its priority the monitor's, else P3 and a warning's P4;
closed on recovery by the same alias as PagerDuty's dedup key; its limits kept: a message of 130
characters, 20 tags of 50), email, and generic webhooks (kiln's JSON signed HMAC-SHA256 over the
body, `X-Kiln-Signature`, as GitHub signs its own). Delivered off a bounded queue, tried again with
doubling backoff on errors, 429s (after `Retry-After`) and 5xx; each delivery an event and a count.
Integrations are named (`PUT /api/observe/v1/integrations/<name>`), checked as they are kept, their
secrets never shown, and testable.

**Email** (`crate::observe::smtp`): through the SMTP relay a site already has (RFC 5321), reached
with STARTTLS (RFC 3207, 587 unless said), TLS from the first byte (RFC 8314, 465) or neither (25,
a relay on its own network), its certificate checked against the public roots or a CA given (PEM),
logged in with AUTH PLAIN (RFC 4616) or LOGIN when it asks. A reply is read whole (its continued
lines); a 4xx is tried again, a 5xx is not, and either is said in the relay's words, as is a
certificate not trusted (not tried again either). What the relay sends between STARTTLS's 220 and
the handshake is refused, not read as if it had come over TLS (CVE-2011-0411's plaintext
injection). The message (RFC 5322, MIME): `multipart/alternative` of text and HTML, each base64 (no
line past 76 characters, so none is a lone `.`; the data dot-stuffed besides), its subject the
notice's title, an encoded word (RFC 2047) when it is not ASCII, `Auto-Submitted: auto-generated`
(RFC 3834: no robot answers it), and one `References` a monitor's group, so its alert and its
recovery are one thread. Every header's value is refused if it holds a line break, and an address
with one is refused when the integration is kept: a monitor's name or a recipient cannot add a
`Bcc`. The HTML escapes everything it shows. 50 recipients at the most; addresses not ASCII only
to a relay that says `SMTPUTF8`.

Sources: GitHub's webhooks (§8), and the clouds (§10.1).

### 10.1 Clouds

`crate::observe::cloud`: each cloud integration is named (`PUT /api/observe/v1/clouds/<name>`),
pulled by a task of its own on the server every `interval_s` (300 unless said), each pull reading
back `lookback_s` (three intervals): a point that came late to the cloud, or a write refused while
observe was busy, is read again, and the store keeps a point once and, of two values at one time,
the later (§4.5). A pull's points are offered as an exporter's are (§8), in parts of 100,000. Each
integration writes `max_series` series at the most (20,000), the rest counted and said, not asked
for (each cloud's API costs by the metric asked).

| | AWS CloudWatch | Google Cloud Monitoring | Azure Monitor |
| --- | --- | --- | --- |
| Found | `ListMetrics` by region and namespace, those with points in the last 3 h | `metricDescriptors.list` by project and type prefix | Resource Manager's resources by subscription (and type), each type's `metricDefinitions` |
| Read | `GetMetricData`, 500 queries a call, paged, JSON 1.0, Signature Version 4 | `timeSeries.list` a type, aligned: gauges by their mean, deltas and counters by each period's increase, distributions by each period's | `metrics`, 20 metrics a call a resource, at the period's grain (else the finest above it) |
| Named | `aws.<namespace>.<metric>`: `.sum`, `.samplecount` (counts) and the average `sum / samplecount` as the name itself; other statistics `.<stat>` | `gcp.<service>.<path>`; distributions `.count`, `.sum`, `.avg`, `.p50`, `.p95`, `.p99` | `azure.<type>.<metric>`: the primary aggregation as the name; others asked `.<aggregation>` |
| Tagged | `region`, each dimension by its name in lower case | the metric's and resource's labels, `resource_type`, the metadata's labels | `subscription_id`, `resource_group`, `name`, `region`, the resource's tags |
| Given | keys (and a session token), a role to assume with its external id | a service account's key (JSON) | an app's tenant, client id and secret |
| Ambient | the environment, the shared credentials file, a web identity (EKS), the container's (ECS, EKS Pod Identity), the instance's (IMDSv2); the region from the environment or the instance | `GOOGLE_APPLICATION_CREDENTIALS` (a key or a user's), gcloud's file, the metadata server (GCE, GKE, Cloud Run); the project from them | the environment's app, a workload identity (AKS), App Service's managed identity, the VM's |
| Logs | `log_groups` (names, or prefixes ending in `*` found with `DescribeLogGroups`), each region's, matching `log_filter` (CloudWatch Logs' filter pattern) when said: `FilterLogEvents`, JSON 1.1, signed for `logs`; 100 groups a region (its quota is 5 searches a second) | `logs`: a filter in Logging's query language (empty, every entry), each project's: `entries.list` within the pull's window, oldest first, a thousand a page, 30 pages a pull (half the quota of 60 calls a minute) | `workspaces` (Log Analytics' ids), each `log_query`'s rows (a table, or KQL; every table unless said) within the pull's window, oldest first, 200,000 a pull, with a token of Log Analytics' own audience |
| A record | `source` the AWS service its group is of (`/aws/lambda/…` `lambda`, `/aws/rds/…` `rds`; else `cloudwatch`), `service` the group's last name (a function's), `aws.awslogs.logGroup`, `aws.awslogs.logStream`, `region` | `source` `gcp.<resource type>`, `service` the resource's service, function, container or module (else the log's name), `host` its instance or pod, the trace and span, `textPayload` or the JSON payload's `message` (its other top-level values attributes), an audit entry's method and resource | `source` `azure.<table>`, `service` the resource's name (else the table), `host` `Computer`, the message `Message`, `RenderedDescription`, `Description`, `ResultDescription` or `OperationName` (else the row), the row's other values attributes |

Names are Datadog's, as far as one rule makes them (a word starts at a capital after a lower-case
letter: `CPUUtilization` → `cpuutilization`, `NetworkIn` → `network_in`), so a dashboard or a
monitor written for Datadog's integrations reads these. CloudWatch keeps each period's count, sum,
minimum and maximum, and its average is the sum over the count: asking `Sum` and `SampleCount`
(the default) gives the average without a query of its own, and the sums as counts. A
distribution's quantiles are read from its buckets as Prometheus's `histogram_quantile` reads them
(linear within a bucket), log-linear within an exponential bucket, whose points are spread so. A
Google type with no series is quiet, asked again hourly, not each pull: a project's thousands of
built-in types cost a call each an hour.

**Logs, each record once.** A pull reads its lookback again for logs too: clouds take records
late (CloudWatch Logs within seconds, Cloud Logging and Log Analytics minutes). A record's id is the
cloud's own (CloudWatch Logs' `eventId` in its group, Cloud Logging's `insertId` with its log and
time, Log Analytics' `_ItemId`, else the row's values), and a pull loop keeps the ids it took for
its lookback (`cloud::Seen`; a million at the most, a thousand records a second over 15 minutes,
some 20 MB, the oldest half forgotten past it and the status saying so): a record read again is not
taken again, and one refused while observe was busy (its id not kept) is read again by the next
pull. A record's level is the cloud's (Cloud Logging's severity, Application Insights' levels 0 to
4, a `Level` column), else read from its message as a log pipeline's status remapper reads it: a
JSON object's `level`, `severity`, `levelname`, `log.level` or `status`, else a level word in
capitals among its first four words (`ERROR`, `[WARN]`, Lambda's tab-separated `ERROR`) or a
`level=…`, else info. The integration's tags go on every record. Google's token asks for
`logging.read` beside `monitoring.read` only when logs are read; a record's message is cut at 1 MiB.

Calls throttled or failed on the service's side (429, 5xx, AWS's throttling codes, botocore's
list) are tried again after `Retry-After`, else 1 s doubling, four tries; a failed pull is tried
again after a minute, doubling, up to the interval. A region, project, subscription or resource
that fails is a warning while the others are read, as are a log group, project or workspace whose
logs fail; when all of a pull's metrics fail, the pull fails. The status (`GET clouds`) says each
integration's state, its error and warnings, its last pull (series, points, points left to the
next, log records written and left, calls, time) and the records written since the server started;
kiln's own series count them (`kiln.cloud.points`, `.records`, `.calls`, `.errors`, `.series`,
`.pull.duration`, by `integration` and `cloud`); its first pull, a failure and its recovery are
events (`service:kiln.cloud @event.type:cloud.pull`). A test (`POST clouds/<name>/test`) says what a
pull would read: series, points, and the log records its lookback holds, with the newest few.

Secrets (AWS's secret key and session token, Google's key, Azure's client secret) are sealed at
rest with AES-256-GCM under the server's master key (`<data>/secrets/master.key`, CI's), each bound
to its integration and field; `clouds.json` is written owner-only (0600) in one step; they are
never shown (`"…"`). A secret that does not open (the master key changed) leaves its integration
failing, saying so, until it is given again. Credentials are got again 5 min before they expire;
a page's `nextLink` elsewhere than Resource Manager is not followed (the token goes nowhere else).

### 10.2 The clouds' pushes

`crate::observe::cloud::push`: what a cloud sends kiln, where a pull would ask (a pull every
interval reads a stream's last minutes again; a push is each minute as it is, at a stream's scale).
Both take observe's ingest token (or the admin's), each in its own place:

- **Kinesis Data Firehose**, an HTTP endpoint destination (`https://<kiln>/v1/firehose`, its access
  key the ingest token, `Content-Encoding` gzip or not, its common attributes tags of everything it
  brings): **CloudWatch Metric Streams** in each output format, a minute's statistics of a metric
  named and tagged as the pull names them (`aws.<namespace>.<metric>`: `.sum` and `.samplecount`
  counts, the average `sum / count` the name itself, `.maximum`, `.minimum` and the percentiles
  asked gauges; `region`, `aws_account`, each dimension), the same series from any of them (a test
  of one datum in all three): JSON, each record's lines a datum; OpenTelemetry 1.0.0, each record
  one or more `ExportMetricsServiceRequest`s each after its length (a varint), a datum a summary's
  data point (its `count` the sample count, its `sum` the sum, its quantile 0 the minimum, 1 the
  maximum, the rest percentiles: 0.999 `.p99_9`; its time its start, CloudWatch's period's), its
  `Namespace`, `MetricName` and `Dimensions` (a map) attributes, its resource's `cloud.region` and
  `cloud.account.id`; and 0.7.0, the same but for labels (the namespace, the name, each dimension).
  A request of a record that does not read is not taken, the record counted, the others' points
  kept;
  and **CloudWatch Logs' subscriptions**, each record a gzip'd message of a group's events, as the
  pull makes records (the account `aws_account`, the region the delivery stream's), a
  `CONTROL_MESSAGE` (its check that the destination is there) taken and dropped. Its answer is
  Firehose's (`requestId` echoed, `timestamp`, `errorMessage` when not taken: 401 for a wrong key,
  503 while observe is busy, which Firehose sends again). A record of no shape kiln reads (a log's
  message alone after Firehose's own extraction) is counted and not read.
- **Pub/Sub push subscriptions** (`https://<kiln>/v1/pubsub?token=<the ingest token>`), a Cloud
  Logging sink's topic: each message a log entry, read as the pull reads entries; answered 204 (an
  acknowledgement), and 503 while observe is busy (Pub/Sub sends it again). A message that is no
  log entry is acknowledged and counted, not sent again forever.

Both deliver at least once. A Firehose request (its id), a Pub/Sub message (its subscription and
id) and a log record (its cloud's id, as the pull keeps it) taken are kept for two hours, Firehose's
longest retry, in the pull loop's bounded set (§10.1): sent again, they are answered as taken and
not taken again (points need it not: the store keeps one value a time). kiln counts the pushes
(`kiln.cloud.push.points`, `.records`, `.again`, `.refused`, `.skipped`, by `via`), added up and
written every 15 s: a count a push at its own time would keep one of two pushes at one millisecond.

### 10.3 Event Hubs, over Kafka's protocol

`crate::observe::cloud::eventhubs`: what Azure Monitor's diagnostic settings stream to an event hub
(resources' logs, the activity log, Entra ID's logs), read as it comes by the Azure integration that
names the hubs (`event_hubs`: the namespace, its hubs, and a connection string with Listen rights,
sealed and never shown, else the integration's own credentials), over Event Hubs' Kafka endpoint
(the Standard tier and above), on the server. `metrics: false` makes an Azure integration one of
logs alone (its workspaces', its hubs'): no metrics pulled, no credentials needed for them.

- **Kafka's protocol, a consumer's part** (`crate::observe::kafka`; the Kafka protocol guide): an
  `ApiVersions` (v0) first, so a broker that does not answer what kiln asks is said so; SASL
  (`SaslHandshake` v1, `SaslAuthenticate` v0) with PLAIN (RFC 4616: Event Hubs' `$ConnectionString`
  user and the connection string) or OAUTHBEARER (RFC 7628: an Entra ID token for the namespace);
  `Metadata` v1, the partitions and their leaders, a connection to each; `ListOffsets` v1, an offset
  by time; `Fetch` v4, uncommitted records too (Kafka's consumers' default), 500 ms of waiting, 16 MiB
  a fetch and 1 MiB a partition (Kafka's consumers' own). These versions are what Kafka 4 still
  answers (KIP-896 dropped older ones) and Event Hubs (Kafka 1.0 on) too. Record batches of magic 2
  (Kafka 0.11 on, and Kafka 4's only one), each one's CRC-32C checked, gzip'd or not (Event Hubs'
  only compression); snappy, lz4 and zstd said, not read; transactions' control batches passed over,
  their offsets counted; a batch cut short at an answer's end fetched again from its offset. Every
  length a broker sends is checked against what is there: a malformed answer is an error, never a
  panic nor an allocation it asks for; records are read in place, a batch's unpacked into one buffer
  kept for the next.
- **Offsets kiln keeps**, no consumer group: each partition read from where kiln last read it, else
  from the integration's lookback ago (the first event since, by `ListOffsets`' time), else from its
  end. An offset moves only once what came before it is written (its write applied, as a collected
  log's place moves, §8.1), and is written down each second (`eventhubs-<integration>.json`, beside
  the integrations); a connection that fails first gives what it offered two seconds to be written,
  writes the offsets down, and connects again (1 s, doubling to a minute): a stop repeats nothing,
  nor does a broker gone and back; a kill repeats the last second at most. One stream reads a hub at
  a time: a change of settings waits for the stream before it to write its offsets down. An offset
  that retention passed is read from the earliest still there, and how many events were lost is
  said. Deleted, an integration's offsets go with it.
- **Records**: an event's body is `{"records": [...]}` (else one record, else text, a record of its
  own); each record is a log record as Datadog's Azure forwarder makes one: `source`
  `azure.<provider>` (`Microsoft.Web` `azure.web`, Entra ID's `azure.aadiam`), tags
  `subscription_id`, `resource_group` and `tenant` from its `resourceId`; `service` the resource's
  name, as the Log Analytics pull has it (§10.1); its time `time`; its level `level` (`Informational`,
  `Warning`, `Error`, `Critical`), else a failed `resultType` an error, else its message's; its
  message `properties.message`, `message`, `resultDescription` or `operationName`, else the record;
  its other values attributes (`category`, `operationName`, `properties.*`; 32 at the most), with
  `azure.resource_id`, `azure.event_hub` and the integration's tags. Metric records (a diagnostic
  setting's `AllMetrics`) are counted and not read: Azure Monitor's metrics are pulled, named by
  their primary aggregation, which a stream does not say.
- **What it says**: each hub's stream in its integration's status (`streams`: `reading`, or
  `failing` and why; its partitions; its lag, the events not yet read; events, records and metric
  records passed over), shown on the integrations page; kiln's own `kiln.cloud.stream.records` and
  `kiln.cloud.stream.lag` (by `integration` and `hub`, every 15 s); an event as a stream starts
  failing and as it recovers (`source:kiln.event @event.type:cloud.stream`).

### 10.4 Synthetic tests

`crate::observe::synthetics`: Datadog's Synthetic API tests. An endpoint tried on a schedule from the
machines that are its locations (`locations`: `server`, runners by name, or `*`, every machine), each
run judged by its assertions and written as Datadog writes it. The server keeps the tests
(`PUT /api/observe/v1/synthetics/<name>`; `<observe>/synthetics.json`, a bearer token sealed under the
master key and never shown) and tells each runner its own on observe's lane, when it connects and
when they change, as scrape targets are told (§8); each machine says how its runs go in its status.

- **Checks**: **HTTP** (a request, `method`, `headers`, `body`, a bearer token; its redirects
  followed unless `follow_redirects: false`, ten at the most; its time measured on the way: DNS,
  the connection, TLS, the first byte, the download, the redirects), **TCP** (a connection to a host
  and port), **DNS** (a name resolved by the machine's own resolver: what its applications see) and
  **TLS** (a handshake, the certificate verified for the host against the public roots or a CA
  given, and its days until it expires). A check reaches its endpoint directly, not through the
  machine's proxy: what is tested is whether that machine reaches it.
- **Assertions**, Datadog's: the status code, the response time (ms), a header (by its name, any
  case), the body, the certificate's days left, an address resolved (any of them); each `is`,
  `is_not`, `less_than`, `more_than` (numbers), `contains`, `does_not_contain`, `matches`,
  `does_not_match` (text; a pattern checked when the test is kept). An HTTP test without assertions
  passes under 400. What fails is said: `status_code is 200: was 503`.
- **Runs**: every `every_s` (10 s to a week; 60 unless said), each test at its own offset into its
  interval (a hash of its name, as Prometheus spreads scrapes) and kept to its schedule; a run that
  takes longer than its interval puts the next an interval after it. A run is given `timeout_s` (60
  at the most; 30 unless said) and, failing, is tried again `retries` times (3 at the most)
  `retry_ms` apart (300 ms) before it counts, as Datadog's are.
- **What a run writes**, tagged `test`, `location` and `subtype` and the test's tags: Datadog's series
  (`synthetics.test_runs` by `status`, `passed` or `failed`; `synthetics.http.response.time`,
  `.dns.time`, `.connect.time`, `.ssl.time`, `.firstbyte.time`, `.download.time`, `.redirect.time`
  in ms and `.response.size` in bytes; `synthetics.ssl.response.time`, `.dns.time`,
  `.handshake.time`, `.time_to_expiry` in minutes, also of an HTTPS test's certificate;
  `synthetics.dns.response.time`; `synthetics.tcp.response.time`, `.dns.time`, `.connect.time`), and
  kiln's own `synthetics.failing` (1 while the location's last run failed). A location that starts
  failing, or recovers, is an event once (`source:kiln.event @event.type:synthetics`), its failures
  said.
- **Alerting**, Datadog's: a test with `alert` owns a monitor (`syn-<name>`), `min(last_<w>m):sum:
  synthetics.failing{test:<name>}.rollup(max, <every>) >= <min_locations>`: each bucket the
  locations failing, every bucket of the window at least `min_locations` (the window `for_s`, an
  interval at the least); judged as often as the test runs (10 s to a minute); its message the
  test's (mentions, templates), so it notifies, mutes, renotifies and is held by downtimes as any
  monitor is. It is made, changed and removed with its test; a test whose alerting does not compile
  is refused whole.
- **Trying and running**: `POST synthetics/<name>/test` runs a test (the one given, or the kept one)
  once from the server and answers what it saw, its parts timed, nothing written; `POST
  synthetics/<name>/run` runs it now at every location (the runners told on observe's lane).

### 10.5 Log archives

`crate::observe::archives`: Datadog's Log Archives. The records kiln takes are written by the server
to an object store as Datadog's archives hold them, so what reads Datadog's archives (its
rehydration, Athena, a script) reads kiln's. An archive is Datadog's (its v2 API's JSON, plain or as
`{"data": {"type": "archives", "attributes": …}}`): a name, a search (a log search, `*` unless
said), a destination, and whether its events carry tags; 64 at the most, in an order
(`archive_order`, `<observe>/archives.json`). A record goes to the first archive in order whose
search it matches, once, as Datadog's; kiln's own events (`source:kiln.event`) only to one whose
search names them.

- **Destinations**: an S3 bucket (its region; a storage class; Datadog's encryption: the bucket's,
  `SSE_S3`, or `SSE_KMS` with its key) or any S3-compatible store's at an `endpoint` (MinIO, R2,
  RustFS: by path); a Cloud Storage bucket; an Azure Storage container; a path in it. Each written
  with the credentials of the kiln cloud integration it names (`integration.cloud`, §10.1), of its
  cloud (Datadog's own integration fields kept as given): S3's requests signed with SigV4 (the path
  encoded once, the payload's hash signed, as S3 asks), Cloud Storage's with a token for
  `devstorage.read_write`, Azure's with one for `https://storage.azure.com`; the integration's keys,
  service account or client secret, else the server's own (its environment, its files, a workload's
  identity, the instance's metadata service). An integration archives alone write with is `paused`:
  it pulls nothing.
- **Objects**: `<path>/dt=<YYYYMMDD>/hour=<HH>/archive_<HHmmss.SSSS>.<uuid>.json.gz`, gzip'd JSON
  lines (level 6, as Datadog's writers), each of an hour's events by their own time (what reads an
  hour's folder finds every event of it, as Vector's sink partitioned them) and 100 MB of them at
  the most, named of its newest's time. An event: `_id` (Datadog's 18 bytes: its time in ms, eight
  bytes of the archive and the file, its place in the file), `date` (RFC 3339 to the ms), `host`,
  `source`, `service`, `status`, `message`, `tags` (when asked, the record's unified service tags),
  `trace_id` and `span_id` when it has them, and every other attribute under `attributes`, a dotted
  key a path of objects (`http.status_code`: `{"http": {"status_code": …}}`) unless it is both a
  value and another's path.
- **Each record once**: what is archived is the store's log files, in order. A file is a cut's
  records (§4.5), written once, never changed, expired whole: every record is in exactly one. It is
  read with no hold of the store and encoded on the blocking pool, its parts written as they close
  (one waiting while one is written); its objects are named of its contents (its checksum, size,
  first and last time), the archive, the hour and the part, so a file written again writes the same
  objects over themselves. Each archive keeps its place (the last file all of whose objects are
  written): one failing holds back no other. A file is begun with the archives as they are then and
  kept to them while one of them is yet to write it: a change of searches, tags or order applies to
  the files cut after it, and a file tried again splits into the same parts, those written (to the
  destination as it is) not written again (`<observe>/archives.state.json`). An archive made begins
  after the newest file held (Datadog's begins with what comes after it); one whose place was lost,
  with the oldest. An install numbers the files anew; each place follows its file by its contents.
- **When**: every 30 s, the files cut since: about a minute's records under load, and within 15
  minutes of a record when they come slowly (§4.5), as Datadog's own archives come within minutes.
  Three files at once (each archive still done with them in order, none past one it failed): one
  core writes the events of some 53 MB/s of log lines, half what the store applies at its most
  (§12.26).
- **Failing**: an upload is tried again on a 429 or 5xx (after its `Retry-After`, else 1 s
  doubling; four tries); then the archive is `FAILING`, its error in the store's words, and is tried
  again after a minute, doubling to 16. Its state (Datadog's `UNKNOWN`, `WORKING`, `FAILING`), what
  it wrote and how many files it has yet to write are in the API; starting to fail, and recovering,
  is an event once (`source:kiln.event @event.type:archive.write`). Files expire with the logs'
  retention: an archive failing longer than that loses what expired unwritten.
- **Counted**, by `archive`: `kiln.archives.events`, `.objects`, `.bytes` (gzip'd), `.failures`, and
  `kiln.archives.pending` (files yet to write).

Checked live (§13): three archives (Cloud Storage; Blob Storage, with tags; S3 in RustFS, which
checks SigV4 by its own reading) each given every record it is the first to take, once, as Datadog
writes them, and none of kiln's events; a store failing part way through a file, the server killed
meanwhile and an archive made first in order: nothing lost, nothing written twice.

### 10.6 Rehydration

`crate::observe::rehydrate`: Datadog's rehydration from archives. An archive's records of a time
range that a search takes are read back from its store into a view of their own (Datadog's
historical view), searched, counted, faceted and patterned as the live logs are by naming it
(`view` in a logs query, §7), kept for its retention, then dropped. What is read is the archives'
format (§10.5), so an archive Datadog wrote reads alike.

- **Asked**: a name, the archive (`archive_id`), a search (`*` unless said), a range (`from`, `to`,
  in ms; 400 days at the most), a retention (15 days unless said, 180 at the most) and a scan's
  bound (`max_scan_size_in_gb`; the archive's `rehydration_max_scan_size_in_gb` bounds it too, the
  smaller). 32 at once at the most; one read at a time, in the order asked.
- **Listed**: the range's folders, a day's (`dt=…/`) where the range holds all of it and an hour's
  otherwise (a month in 30 listings, not 720), page by page as each store continues one (S3's
  `continuation-token`, Cloud Storage's `pageToken`, Azure's `marker`), with the archive's cloud
  integration's credentials as it writes (§10.5). Their objects are read in their names' order: an
  hour's, by their newest event's time.
- **Read as it comes**: an object's bytes go from the wire to its reader on the blocking pool, 16
  chunks waiting at the most, gunzip'd and read a line at a time: no object is ever whole in memory
  (an archive's are up to 100 MB of events; another writer's may be more), and reading overlaps
  the download. One that unzips past 1 GiB is refused. A read cut short (an error, or no bytes for
  60 s) asks for its rest (`Range: bytes=<n>-`) and is taken up where it stopped; a store that
  sends the whole again has its start skipped. It is tried again as a cloud's calls are (on a 429,
  a 5xx or no answer; after 1 s doubling, `Retry-After` heeded): four tries in a row that bring
  nothing, or 16 in all, fail it. The scan's bound stops it before an object that would pass it,
  and it says so (`limited`).
- **Kept**: each event read back as its record (§10.5's writing undone; in place, into one record
  kept from event to event: nothing allocated, §12.27): `date` (RFC 3339, or ms), `host`, `source`,
  `service`, `status` (Datadog's names; none, `info`), `message`, `attributes` flattened to dotted
  keys (16 deep, 256 at the most), any other field at the top an attribute too, `tags` as attributes
  where none of their key is (`service`, `host` and `source` being the record's own), its ids (hex,
  or a tracer's decimal 64-bit ids; Datadog's `dd.trace_id` and `dd.span_id` attributes where the
  event says none). The records of the range that the search takes are kept, with the archive's
  `rehydration_tags`; a line that is no event is counted (`malformed`), not kept.
- **The view**: a store of its own on the server (`<observe>/rehydrated/<id>`), not replicated,
  written as the live store is (2,000 records a batch, cut each 32 MiB of them: the live store's
  head, §4.5) and searched as it fills. It goes whole when its retention ends (from when it was
  read) or its rehydration is removed. One the server stopped in is begun again when it is back, its
  view made anew; one that failed keeps none (a view is its whole range, or nothing).
- **Said**: its state (`queued`, `reading`, `done`, or `failed` and why), the objects of its range
  and those read, bytes read, events read, records kept, lines that were no event, whether its bound
  stopped it, when it began and finished, when its view goes.

Checked live (§13): S3's (RustFS), Cloud Storage's and Blob Storage's archives rehydrated, every
read cut short half way by the stand-ins (Google's giving the rest when asked; Microsoft's the
whole again): each view holds exactly its records, once, as they were sent; the server killed as
one reads, it is read again whole; a bound stops one after its first object.

### 10.7 Delivery: DORA metrics

The four keys of software delivery (Forsgren, Humble, Kim 2018; DORA's yearly reports), measured
as Datadog's DORA Metrics measure them (`crate::observe::dora`), from deployments and failures: kiln
CD's own deploys, and those any CD system sends to Datadog's API (`POST /api/v2/dora/deployment`,
`POST /api/v2/dora/failure`; the ingest token, as the Agent's intakes). A deployment is a service's
(in an environment, owned by a team) at a version and a git commit, started and finished; a failure
(an incident) names the services it hurt, its environment, severity, start and, once over, end.

- **Deployment frequency**: the deployments finished in a range, a day. A kiln CD deploy is one when
  it changes the app's revision (a redeploy of what runs, a self-heal, is none).
- **Change lead time**: each commit a deployment brings, from its author's time to the deployment's
  finish; a deployment's is its commits' mean, and a range's the median of its deployments'. The
  commits are git's log from the previous deployment's commit (the same service and environment) to
  its own, merges left out, 5,000 at most, read from kiln's mirror of the repository the deployment
  names (its clone URL or kiln's name for it; a commit the mirror lacks is fetched). None for a
  service's first deployment, one whose previous is more than 60 days older, or a repository kiln
  does not mirror (Datadog's rules, and its: the author's time survives a rebase).
- **Change failure rate**: the deployments marked failed among the range's. A deployment is marked
  - by a rollback: a deployment of a commit (with no git, a version) deployed before, but not the
    one just before it, marks the deployment after that earlier one (V1 → V2 → V3 → V1: V2). kiln
    CD's rollbacks are such;
  - by a revert: a commit `Revert "<title>"` among a deployment's marks the earlier deployment that
    brought the commit it reverts (`This reverts commit <sha>`, else the one titled `<title>`); none
    when both came in one deployment;
  - by a hotfix: a commit among a deployment's that says `hotfix` (a squashed pull request's title,
    a merge's branch), or a version ending `_hotfix`, marks the deployment just before it;
  - or by hand (`PATCH /api/v2/dora/deployments/{id}`: `change_failure`, and the deployment that
    remedied it, by id or version, a `rollback` or a `rollforward`).

  A rollback's target and a revert's commit are looked for among the latest 1,000 deployments of
  the service and environment (a bound on what one deployment costs: at a busy service's six a day,
  five months; a service deployed less often is looked through further back), and a rollback's only
  when what it deploys was deployed before (a count kept of each).
- **Failed deployment recovery time**: from a failed deployment's finish to its remedy's (the
  rollback, revert or hotfix that marked it, or the one given by hand); a range's median.
- **Failures**: kept and listed; their median duration is the time to restore of DORA's reports
  before 2023.

The page shows each beside DORA's levels (the 2024 report's, found by cluster analysis of some
39,000 answers): lead time under a day (elite), a day to a week (high), a week to a month (medium),
a month to six months (low); deployments on demand, several a day; daily to weekly; weekly to
monthly; monthly to every six months; recovery under an hour; under a day (high and medium); a week
to a month. A change failure rate is shown with the clusters' own (5%, 20%, 10%, 40%), no level:
the report's medium cluster failed less often than its high one, so no band orders them.

Each is also a metric, for dashboards and monitors (`dora.deployments`, a count; `dora.change_lead_time`,
a distribution of each commit's, in seconds; `dora.change_failures`, a count at the failed
deployment's finish; `dora.recovery_time`, a distribution; `dora.failures`, a count; and
`dora.time_to_restore`, a distribution; each by `service`, `env` and `team`), and each deployment
and failure an event (`source:dora`). The page's figures are the ledger's, exactly: a metric
written as a deployment comes is not taken back if an earlier one comes after it.

- **The ledger**: the server keeps every deployment and failure of the last 400 days
  (`<observe>/dora.jsonl`, an entry a line, the last for an id standing; rewritten whole once it
  holds twice what stands), and reads it as it starts. A service and environment's deployments go in
  one at a time, so each finds the one before it; one that comes after a later one is put in its
  place, and the one after it read again. It holds a deployment in some 500 bytes (§12.29): the
  names many share (services, environments, teams, repositories) as numbers, its commits' keys in
  one slice, the deployments in a slab and each service and environment's in finish order by their
  place in it.
- **The API**: Datadog's at its paths for what a CD system sends, with the ingest token
  (`POST /api/v2/dora/deployment`, `/failure` and its earlier name `/incident`, `PATCH
  /api/v2/dora/deployments/{id}` or by service, environment and version to mark one, `DELETE`); an
  id given again replaces what it said (a failure's end sent later). The reading is kiln's, with a
  session (`/api/observe/v1/dora/...`): the lists (`POST deployments` and `failures`: a range, a
  search of their fields as logs are searched, 1,000 at the most, either order), one of each, the
  same writes, and `GET summary` for the page: the four keys over a range by service, environment
  and team, week by week. Datadog's list paths answer that they are read through kiln's.

### 10.8 Service Catalog

Every service, who owns it and how it is doing (`crate::observe::catalog`), Datadog's Software
Catalog: its definitions in Datadog's schema (v2.2, and v2.1 as it is read the same; `additionalProperties`
refused as Datadog refuses them), each service's facts from what kiln already holds, and scorecards
of them.

- **Defined**: by Datadog's API (`POST /api/v2/services/definitions`, JSON or YAML, with the ingest
  token; `DELETE` one), kept by the server (`<observe>/catalog.json`); and by `service.datadog.yaml`
  at the root of a repository kiln mirrors (its default branch, read every five minutes and as the
  server starts; several definitions in one file, `---` apart), as Datadog's GitHub integration
  reads it. A service a repository defines is the repository's: the API changing it is refused,
  saying which.
- **Seen**: a service with no definition is listed as it is seen, by its traces (the trace metrics'
  last day) or its deployments (§10.7).
- **Its facts**: its requests, errors and latency (the last hour's trace metrics), its open issues
  (§8.8), the monitors and SLOs that read it (a `service:<name>` in their query or tags) and how they
  stand, its deployments (the last, how many in 30 days, its change failure rate), whether its logs
  carry trace ids (the last day's: `service:<name> trace_id:*`).
- **Scorecards**, Datadog's three, each rule passed or failed and why: production readiness (has
  SLOs, has monitors, names an on-call: PagerDuty or Opsgenie, deployed in the last 3 months);
  observability (its deployments tracked, its logs correlated with its traces); ownership and
  documentation (a team, contacts, a code repository: a `repo` link, or defined in one; docs: a
  `doc` or `runbook` link).
- **Teams in DORA**: a deployment that names no team takes its service's (§10.7), as Datadog's DORA
  Metrics take it from the catalog.
- **The API**: Datadog's at its paths for what is defined (`POST`, `DELETE`; its `GET`s answer that
  they are read through kiln's), and kiln's (`/api/observe/v1/catalog`, a session's): the services
  with their facts and scores, one service, its definition made, changed or removed.

### 10.9 Infrastructure

Every host that reports (`crate::observe::infra`), Datadog's Infrastructure List: a host is a value
of the `host` tag, whatever wrote it (kiln's runners, each with its own figures, §8; Datadog's Agent;
an OpenTelemetry resource's `host.name`; a series posted with `host:<name>`), and what kiln knows of
it besides.

- **Where it comes from** (`sources`): `kiln` (a runner the hub knows: its name, connection, version,
  labels and probed facts), `agent` (Datadog's Agent, by its metadata or its own metrics), `otel` (a
  resource's `host.name`), a cloud (`aws`, `gcp`, `azure`: the Agent's inventory or a resource's
  `cloud.provider`).
- **What the Agent says of its host**, kept by the server (`<observe>/hosts.json`, saved within a
  minute, a host the Agent has said nothing of for seven days let go; 100,000 hosts at the most): its
  host metadata (`POST /intake/`, at its start and every 30 minutes: its version, `systemStats`, its
  aliases, its tags by source, how it was installed, gohai's system information), its checks
  (`/intake/` too: each check, its status and error) and its inventory of the host
  (`/api/v1/metadata`'s `host_metadata`: cores, threads, memory, kernel, OS version, cloud, instance
  type, addresses). An Agent 7.84's own payloads, as it sent them (addresses and identities
  replaced), are the tests' fixtures.
- **When it last reported** (Datadog's `last_reported_time`): its newest sample, from the series
  index alone (each series' newest time; nothing read from disk). Up: a sample within two hours;
  active: within one (Datadog's totals); listed: within three hours unless asked from further back
  (`from`), kept seven days. kiln's page dots it: reporting (within two minutes, eight of the
  15-second intervals the Agent and kiln's own figures report at), late, or gone.
- **Its figures**, its newest of the last ten minutes: CPU in use (`100 - system.cpu.idle`), I/O wait,
  the load over 15 minutes (and a core's), memory in use (`1 - system.mem.pct_usable`) and its
  fullest disk's use (`system.disk.in_use`): the System Check's names, which kiln's own figures use
  too (a runner too old to report its own: its hub sample's CPU and memory). Each series' newest
  sample (`Store::newest_by`: read from the head with nothing decoded where the head holds it in
  order, as a query reads it where not), a host's several series of one taken on average (its disks
  at their most).
- **Its apps**: its metrics' namespaces (a name to its first dot, or a name of none, Prometheus's,
  to its first underscore; the Agent's `datadog.*` its `agent`).
- **Its platform**, from the best of what says it: its runner's probed facts, then the Agent's
  inventory and metadata (its distribution from `nixV`), then its resource's attributes (`os.type`,
  `host.arch`, `cloud.region`, …, read from its first 64 series), then its metrics (cores, memory).
- **Its tags by source**: the Agent's (its own tags `Datadog`, a cloud's under its name), kiln's (its
  runner's labels), OpenTelemetry's (its resource's attributes).
- **Its containers**: each container (`container_id`, or OpenTelemetry's `container.id`) whose
  series say it runs on the host, its name, image and pod, and its CPU and memory of the last ten
  minutes (the container check's `container.cpu.usage`, nanocores; `container.memory.usage`).
- **Muted**: as Datadog's mute makes one, a downtime (§9.3) of every monitor's groups of the host
  (`monitor_tags: ["*"]`, scope `host:<name>`), to an end or until unmuted; muting a muted host again
  changes its mute only when it says so (`override`). A host's name that does not read whole in a
  scope (a space, a parenthesis) is refused.
- **The API** (kiln's, a session's): Datadog's host list (`GET /api/observe/v1/hosts`: `filter`,
  terms each a part of its name, an alias, an app or a source, or a tag, `key:value` with `*` for any
  part; `sort_field` of name, status, cpu, iowait, load, memory, disk, apps or last_reported;
  `sort_dir`; `start`; `count`, 1,000 at the most; `from`; `include_hosts_metadata`), its totals
  (`/hosts/totals`), a host whole (`/hosts/{name}`: its inventory, checks and containers too), and a
  host muted and unmuted (`POST /hosts/{name}/mute`, `…/unmute`, Datadog's bodies and answers). Each
  host as Datadog gives it (times in seconds; its `metrics`; `meta` when asked), kiln's own beside
  under `kiln`. At Datadog's own paths (`/api/v1/hosts`, `/api/v1/host/…`) the ingest token, a
  writer's, reads and mutes nothing.

An inventory is made of the index (each host's series, newest sample, namespaces and platform tags),
the ten figures' newest samples, the Agent's hosts, the hub's runners and the downtimes, on the
blocking pool; reused 15 seconds, then still answered while one is made again (one at a time; one
begun before a mute is not kept).

### 10.10 Containers

Every container the machines run (`crate::observe::containers`), as Datadog's container check
sees them: each node looks at its own every 15 seconds, on a thread of its own.

- **Found** on Linux in the cgroups (`/sys/fs/cgroup`, v2's unified hierarchy, or v1's controllers
  where a node still mounts them), whatever runs it and asking it nothing: the scopes container
  engines make (Docker's `docker-<id>.scope`, containerd's `cri-containerd-<id>.scope`, CRI-O's
  `crio-<id>.scope`, Podman's `libpod-<id>.scope`, its `conmon`'s not one; cgroupfs's `docker/<id>`
  and `kubepods/<class>/pod<uid>/<id>`), a Kubernetes pod's uid and QoS class from its slice, and
  the containers nested in one (a minikube node's pods in its container). 1,000 containers a node at
  the most; the walk itself bounded (100,000 directories, 12 deep), and a directory without
  directories of its own (its links two, as cgroupfs counts them: services, most containers'
  scopes) not read, its 60-odd files not listed.
- **A pod's sandbox is not a container** (Datadog's check leaves it out too): the `pause` process
  Kubernetes' runtimes start in a scope of its own to hold a pod's namespaces, one a pod. Told by its
  first process's name (`cgroup.procs`, then `/proc/<pid>/comm`; asked once a container, its
  answer kept while it runs), and by its image where the engine names it (`pause`, Rancher's
  `mirrored-pause`, OpenShift's `origin-pod`: Docker's Kubernetes). Where neither can be told (kiln in
  a container of its own PID namespace, the scope's process not shown), it is reported.
- **Read**: v2's `cpu.stat` (in use, user, system, throttled time and periods), `cpu.max` (its
  quota), `memory.current`, `memory.max`, `memory.stat` (its RSS, `anon`; its cache, `file`; its
  working set, usage less `inactive_file`, what an out-of-memory kill reads), `memory.events` (its
  OOM kills), `io.stat` (bytes and operations, by direction), `pids.current`; v1's of each
  controller (`cpuacct`, `cpu`, `memory`, `blkio`, `pids`). In Datadog's names and units:
  `container.cpu.usage`, `.user`, `.system`, `.limit` (nanocores), `.throttled` (ns a second),
  `.throttled.periods` (a count); `container.memory.usage`, `.working_set`, `.rss`, `.cache`,
  `.limit` (bytes), `.oom_events` (a count); `container.io.read`, `.write` (bytes a second),
  `.read.operations`, `.write.operations`; `container.pid.count`; `container.uptime` (seconds, where
  its engine says when it was made). Rates against the last look's counters; a container gone, its
  counters let go.
- **A look costs little**: its files read into one buffer (std's `read_to_string` asks a file's size
  and place first, two system calls more a file, and a cgroup's files have none to give), the values
  wanted picked from `memory.stat` in one pass; each container's series' keys made at its first look
  and kept (made again when its tags change: a name learned); its samples written into a decoding
  the store's owner hands back emptied, its room kept, as exporters' are. A node of 1,000
  containers' look: §12.34.
- **Named** by Kubernetes' links (`/var/log/containers/<pod>_<namespace>_<container>-<id>.log`,
  whatever the engine: its pod, namespace and container) and by the engine's API where its socket
  can be read (Docker's, Podman's rootful and rootless, Docker Desktop's; `DOCKER_HOST`'s): its name,
  image (`image_name`, `image_tag`, `short_image`, Datadog's tags), Compose service and project.
  The engine is asked (`GET /containers/json`, HTTP/1.0 over its socket, kiln's own few lines of
  std) at a look where a container appears that it has not named, once a look: a container is named
  at the look it first appears in, or, its cgroup there before the engine lists it as it starts, at
  the next; one the engine still does not name after 30 s (another engine's: a Kubernetes node's
  containerd beside Docker) is no reason to ask again. And every five minutes besides; a node
  without either reports its containers by id. (It was asked at most every 15 s, and about a
  container the engine had not listed yet as it started, not again for five minutes: a runner
  looking every 2 s wrote such a container's samples under a series without its name until then.)
- **Where the cgroups are not seen** (Docker Desktop's VM, on macOS and Windows, its named pipe
  there), the engine's stats are read instead (`GET /containers/{id}/stats?stream=false&one-shot=true`),
  a container at a time, as `docker stats` reads them.
- **Tags**: `host`, `container_id`, `runtime`; as known `container_name`, `image_name`, `image_tag`,
  `short_image`, `kube_namespace`, `pod_name`, `kube_container_name`, `kube_qos`, `compose_service`,
  `compose_project`. Datadog's Agent's container check reports the same names; OpenTelemetry's
  receivers report theirs (`container.id`, `container.name`, `k8s.pod.name`, …), listed beside,
  without figures as yet: their metrics differ in name and unit (`docker_stats`' CPU in
  nanoseconds, `kubeletstats`' `container.cpu.usage` in cores), and are not read as the check's.
- **Listed** by the server (`GET /api/observe/v1/containers`, `…/containers/{id}`: kiln's API): made
  of the store's index, as the host list is (each container's series by its `container_id` or
  OpenTelemetry's `container.id`, their tags and newest sample; `Store::tag_seen`), and of each
  figure's series' newest sample or rate (`Store::read_by`; OpenTelemetry's, §10.11); made again at
  most every 15 seconds. Running
  while it has a sample within two minutes, listed an hour past its last; found by a part of its
  name, image, id, pod, namespace or host, or by a tag (`*` any part of its value); ordered by its
  CPU, memory, either's share of its limit, throttling, I/O or uptime; one found by its id or the
  first 12 of its hex, as `docker ps` shows it. A search is read once and matched with nothing
  allocated (a tag's key compared, its value's glob; a part of a name found case aside), and a page
  is written as JSON from the list itself. A list older than 15 seconds is given while another is
  made, one at a time, up to a minute old; the first asked after a quiet while is made first. A
  host's page lists its containers from it: running ones, then those stopped within the hour.

### 10.11 OpenTelemetry's containers

Containers an OpenTelemetry Collector reports are listed beside those kiln's check finds, read in
their own names and units (`crate::observe::containers::inventory::SOURCES`): what Docker's
(`docker_stats`), the kubelet's (`kubeletstats`) and the cluster's (`k8s_cluster`) receivers write,
and the semantic conventions' container metrics, from the receivers' own documentation (contrib
`main`, 2026-10-23) and the conventions' page. Datadog maps a few of them onto its own names at
intake (`container.cpu.usage.total` to `container.cpu.usage` among them, with no unit changed:
its OTLP mapping's tables), which would put Docker's cumulative nanoseconds and the kubelet's cores
under the name of the check's nanocores a second; kiln keeps each series as it came and reads it for
what it is, so nothing is written twice and no unit is mistaken.

- **Named** by `container.id`, which Docker's receiver sets, and the cluster's (its containers'
  identifying attribute), and the kubelet's where it is asked for (`extra_metadata_labels:
  [container.id]`); a container kiln's check also sees (the same id, without the runtime's
  `containerd://`) is one container, its figures from both. The kubelet not asked for ids names a
  container by its namespace, pod and name alone (`k8s.namespace.name`, `k8s.pod.name`,
  `k8s.container.name`): its series join the container the cluster's receiver or kiln's check
  names so (found by those three's hash, then compared), and its page draws them by those names; a
  container the kubelet alone reports, so named, is not listed (nothing gives its id). Its name, image, engine, pod and namespace from the
  resource's attributes (`container.name`, `container.image.name`, `container.runtime`,
  `k8s.pod.name`, `k8s.namespace.name`, `k8s.container.name`), as the list already read them.
- **Read** each figure from each of its sources: kiln's check's (Datadog's container check's names:
  nanocores, bytes, rates a second, a sample's newest), then Docker's (CPU and throttled time, its
  cumulative nanoseconds a rate; its "usage", Docker's own figure of usage less the inactive file
  cache, which is a working set, not a usage; its memory limit, the host's memory where no limit is
  set, which is the limit in effect; bytes read and written, each device's summed, `operation` read
  or write, its other operations left out; a CPU limit in CPUs, processes and uptime where they are
  asked for, not by default), the kubelet's (CPU in cores, or its cumulative seconds a rate; usage,
  working set), the cluster's (limits in cores and bytes) and the conventions' (`container.cpu.time`
  and `container.cpu.usage` by `cpu.mode`, summed; `container.disk.io` by `disk.io.direction`). A
  rate is a series' last two samples' rise over the time between them, a fall a reset (the newer
  counted from 0), or, of a delta count, its newest over the time since the one before
  (`Store::read_by`, a series' newest samples decoded); a container's series of one reporter at
  its newest time are summed (its devices', its modes': one scrape's), one its reporter no longer
  writes left out however recent (its tags since changed, a name learned: summed with the series
  that took its place, it would double a figure for ten minutes), its reporters' and its sources'
  taken on average. A metric of
  kiln's and OpenTelemetry's both (`container.cpu.usage`: nanocores in one, cores in the other) is
  told apart by the tag that names its container.
- **Drawn** on its page by the first of each figure's sources that reported it: its API answer
  carries each chart's lines, a query each (`sum:container.cpu.usage.total{container.id:…}.as_rate()
  / 1000000000`: a rate a second whether the series are cumulative or each interval's counts), and
  a line no source reported is not there; a chart of none says so.

## 11. UI

Observe, in kiln's console (`ui/src/views/observe/`), built from the console's own parts (panels,
facts, the pill tabs, uPlot charts) and its rules: data through Suspense resources, no effects, the
time range and the search in the address (a page is a link).

- **Overview**: what is in trouble (monitors alerting, warning, without data), the newest events,
  and the store: series, bytes on disk, from when it keeps, each machine's replica, how far behind it
  is and whether it holds the same (its full digest at the server's index; "Compare replicas" asks
  for them).
- **Dashboards**: Datadog's model near enough. Widgets on a grid of twelve columns (one column on a
  phone): timeseries (lines, areas or bars), query value, top list (its limit and order), table,
  heatmap, distribution, pie, log stream, event stream, monitors, check status (a service check's:
  of every group the worst status, how many are in each, and those not OK by name; or one group's
  and when it last ran, §9.4; each status its tone's dot and its word), note (Markdown), and groups (a
  title over widgets of their own on a grid of their own, edited in place as the dashboard's). A
  pie is its series over the range as shares of a ring, the three largest in hues and the rest one
  grey slice ("Other"), each named in its legend with its value and share: in a ring any two slices
  may touch, so every pair of hues is held apart, and three of kiln's own (violet, blue, rose,
  stepped for its dark panels: OKLCH L 0.52 to 0.64, C at least 0.10) are the most that are (worst
  colour-blind ΔE 10.3, worst normal-vision ΔE 16.6, 15.1 beside the grey; the dataviz validator
  over every pair), a slice's hue its series' among them, not its rank's. **Datadog's dashboards**
  are imported from their JSON (as Datadog exports one, or its API gives one, pasted or a file):
  every widget kiln draws drawn, those it draws otherwise said (a change as a top list of the
  changes), those it cannot kept as notes in their places that say what they were, and the import
  said in a line; any dashboard is given back as Datadog's JSON. `$env.value` (Datadog's) stands
  for the variable's value alone. A heatmap is its query's values counted in
  each bucket of time by the row they fall in (a distribution's rows its buckets, §4.4; another
  query's equal ranges of its series' values), each cell in one hue, brighter for more, on a log
  scale (a few busy cells do not wash the rest out), its scale beside it; a distribution widget is
  the whole range's counts as bars, its p50 and p99 marked (the server's percentiles of the same
  query, not read again from bars that may be coarser than the buckets they came of). A cell or a
  bar, hovered or tapped, says its range and count. Template variables above them (`$host` in a query stands for
  `host:<value>`, or any), chosen from the tag's values, kept in the address. Your own are edited in
  place: add, change, move, resize, remove, then save; kiln's own (hosts, CI, CD, repo sync) are
  copied to be changed.
- **Metrics explorer**: a query, its series over the range and each one's last, average, least and
  most; with no query, every metric kiln keeps, found by name. A query goes onto a dashboard, or
  into a monitor, in a step. An `anomalies()` series is drawn with its band beneath it, in its color,
  its points past the band marked (every chart: a dashboard's, a monitor's). A chart of a
  `forecast()` reaches half its range past now (a forecast monitor's, its horizon): each series
  solid to its last point, then dashed in its color with its band; its figures (last, average, least,
  most; a widget's value) are its data's alone. An `outliers()` chart draws its outliers in their
  colors and every other member grey. A calendar rollup's buckets are drawn at their own starts.
- **Metrics summary** (linked from the explorer): every metric kiln keeps, the most series first,
  each with its kind, its series, its tags with their values' counts (what makes the series many)
  and the samples it brings a second and when it last did; found by a part of its name; a click on
  its tags lists each tag's values of most series, one on its name opens it in the explorer. On a
  phone its tags under its name.
- **Patterns** (the logs explorer's and the event stream's other view, `?view=patterns`): the
  search's records grouped by their template (Drain), the most common first, each its count, its
  worst status, its template (variables a quiet `*`) and when last seen; a click narrows the search
  to the pattern's fixed text and shows its records.
- **Live tail** (their third view, `?view=live`): the search's records as they come, the newest on
  top, as the records list shows them; the line above says how many a second match and, when they
  come faster than 50 a second, that a sample of them is shown. Pause holds what comes (counted)
  until Resume; opening a record pauses.
- **APM** (§8.7): the services that send traces, each its requests (and a second), its errors'
  share and its p50, p95 and p99 over the range, from the trace metrics, narrowed to an environment
  when there are several; a service's page its figures, its requests and errors and its latency's
  percentiles over the range, its resources and the calls it makes (each its count, errors, mean and
  p95), a resource a click from its spans.
- **Traces**: the trace explorer, as the logs explorer (a search in the trace explorer's syntax, a
  histogram by status, facets, the newest 200 spans: time, service, name, kind, duration), each span a
  click from its trace. A trace's page: its root, duration, spans, errors and services (each its
  color); its waterfall, each span a row under its parent, its bar where it began and as long as it
  took on the trace's time, its service's color, an error ringed red (the first 400 rows, the rest on
  asking); the span picked (in the address) opened below: its error and stack, its attributes, its
  events at their offsets, its links to their traces; and the logs that carry the trace's id. A log
  record's trace id is a link to its trace.
- **Service map**: who calls whom over the range, each service a box (its requests, its errors'
  share; red when any failed), each call an arrow (as thick as the square root of its share of the
  most, red when any failed, dashed to a peer only named), callers to the left of what they call:
  each service as far right as its longest chain of callers puts it (a cycle's calls back left out),
  each column in the order of its services' callers (the barycenter heuristic's sweep). A box a
  click from its service's page.
- **Logs explorer and event stream**: a search, a histogram of its records by status (stacked, a
  bucket of a round size, about 60 to the range), each facet's most common values (a click narrows
  the search to one, again widens it), the newest 100 records (and 100 more on asking), each opened
  to its fields and attributes, any one a filter a click away.
- **Monitors**: their states, in trouble first; each one's groups, what it watches drawn over the
  range with its thresholds, what it said (its events), mute and unmute; made and edited in a form
  that checks its query. A composite (the form's other kind) is its monitors chosen by letter and a
  condition over the letters; its page lists what it reads, each one's state a link away.
- **Downtimes** (a link from the monitors): each one's status (holding, scheduled, ended, canceled),
  the monitors and groups it holds, when (once, or its rule in words: "Weekly on Sat, Sun at 02:00 for
  4h (Europe/Paris)"), its window now or next; made and edited in a form (a monitor or monitors
  tagged, a scope, once or recurring with a weekly builder or a rule of one's own, what to tell as it
  ends), canceled from its row. A monitor's page says the downtime that holds it, until when.
- **Incidents** (§9.5): by state (active, stable, resolved, all; each counted), by severity and by
  Datadog's search (`services:checkout`, a word of the title), the newest first, 50 a page: each its
  public id and title, when it was declared and by whom (or its monitor), its severity and state, its
  commander, its todos open and how long it has run; on a phone, its title and a line of its severity
  and state. Declared from a dialog (what is happening, its severity, a commander, whether customers
  are impacted and how, the integrations to tell, picked from those set up, a first note), then its
  page: its state and severity changed in place; its commander, customers, how long it has run and
  Datadog's four times; its timeline, the newest first (notes as Markdown, an important one marked,
  each change from what to what, its monitor's alerts and recoveries with their values), a note put
  on it; its todos (an assignee by `@name`), done and removed; its postmortem and links attached and
  removed; its properties (summary, root cause, services, teams, how it was found, whom it tells),
  edited in a dialog with its title, commander and impact (its start and end); deleted.
- **SLOs**: their states, in trouble first; each one's target and timeframe, SLI, budget left (a
  meter) and the hour's burn rate. An SLO's page: its standing and its four burn rates; each day's
  SLI against the target and the budget left through it, as lines (an SLI's days differ in their
  last places, so its axis begins just under the lowest of them and the target, which bars, measured
  from zero, cannot); and the monitors that alert on it, the workbook's two for its timeframe added
  in a click while none reads its burn rate. Made and edited in a form: good events over all, or
  time slices of a query meeting a condition.
- **Catalog** (§10.8): every service, defined or seen, searched by name and narrowed to a team:
  each its team, tier, lifecycle and where its definition came from (a repository, the API, or
  seen), its last hour's requests and errors, open issues, its monitors (how many alert), its
  deployments of 30 days and its score (rules passed of ten, a short bar and the count); on a
  phone, its name and a line of what matters. A service's page: its facts (team, tier, lifecycle,
  requests, errors, p95, open issues, deployments), Datadog's three scorecards with each rule passed
  or failed and why, its owners and links (contacts, runbooks, docs, its repository, its on-call),
  the monitors and SLOs that read it, its latest deployments (to the Delivery page), and its
  definition, made or changed in a dialog (YAML or JSON, as Datadog's API takes it) and removed;
  one a repository defines says so and is changed there.
- **Hosts** (§10.9): every host that reports, found by a part of its name or a tag (`env:prod`) and
  ordered by name, status or a figure: each its status (reporting, late, gone; muted), what reports
  it and its apps, its CPU, I/O wait, load, memory and disk (90% or more told apart); on a phone, its
  name and a line of its status and figures. A host's page: its figures and platform, its CPU, load,
  memory, disks and network over a range, its platform at length (CPU, kernel, cloud, the Agent's
  and kiln's versions, what reports it), its tags by source, its apps, its containers, the Agent's
  checks (a failing one's error by name), its logs a click away; muted from it (an hour, four, a day,
  a week, or until unmuted, saying why) and unmuted.
- **Containers** (§10.10): every container, running ones unless asked (stopped ones an hour after;
  each state counted), found by a part of a name or a tag and ordered by a figure: each its name,
  image and engine, its host (to its page) and pod, its CPU and memory against its limits (90% or
  more of a limit told apart), how long it has run; on a phone, its name and a line of its figures. A
  container's page: its state, id, figures against its limits, throttling, processes, I/O and
  uptime; its CPU (with its limit), memory (in use, its working set, its limit), throttling and I/O
  over a range, each line drawn from the series of the source that reported it (kiln's check's, or
  an OpenTelemetry Collector's in its own names, §10.11), a chart none of its sources reports said
  so; where it runs (host, image, engine, pod, QoS class, Compose service); its tags; its logs a
  click away. A host's containers lead to theirs. Ordered by memory, by the working set the list
  shows (else its usage).
- **Delivery** (§10.7): DORA's four keys over 7, 30 or 90 days, narrowed to a service, an
  environment or a team: each its value, what it was taken from, and the 2024 report's four ranges
  beside it, the one it falls in marked in words (a change failure rate beside the clusters' own
  rates, none marked); deployments and those that failed week by week (bars), the week's median lead
  time (a line: recovery times are few and far between, and an axis for them would flatten it); the
  failures, those going on first; each deployment, its lead time and commits, whether it failed (how
  that was found, what put it right and how soon), marked failed by hand in a dialog (what put it
  right: a later deployment of its service and environment, a rollback or a fix rolled forward) or
  not failed again.
- **Indexes** (a link from the logs explorer): each index in the order records are routed, the
  records it takes, its day against its quota (a meter; past its quota, until when), how long it
  keeps them and its exclusion filters; moved, edited and removed from its row. Made and edited in a
  form: a name, a search, its days, its quota (its day's hour and offset, its warning), each
  exclusion filter in order (a name, a search, the share excluded, the attribute sampled by, on or
  off), its tags. The logs explorer reads every index, or one picked.
- **Pipelines** (a link from the logs explorer): kiln's own and yours in the order they apply, each
  its filter, processors and whether it is on; turned off and on, yours moved and removed, kiln's
  copied to be changed. A pipeline's page lists its processors in order, each edited as its type
  has it (a grok parser's rules tried on its samples as they are written), and tries a record on the
  pipeline as it is, saved or not, before it is kept.
- **Metrics from logs** (a link from the logs explorer): each metric, what it computes of which
  records, its tags, and its latest window with records (how many, in how many groups, those past
  the limit); how long after a
  window ends it is counted, and what came late. Made and edited in a form (what it computes stays
  once made) that counts the definition over the last fifteen minutes, each group's total, before it
  is kept; each charted in the metrics explorer in a click.
- **Archives** (a link from the logs explorer): each archive in the order records are matched
  against them, the records it takes, where it writes (as its store's URL), its state (writing,
  failing in its store's words, waiting for its first file), what it wrote (records, objects, bytes,
  when last) and the files it has yet to write; moved, edited and removed from its row. Made and
  edited in a form: a name, a search, the store (S3, Cloud Storage, Azure Storage) and the cloud
  integration of its cloud that writes there (the only one taken unless chosen), a bucket or account
  and container, a path (its objects' keys shown as it is typed), an S3's region, storage class and
  encryption, another store's endpoint, and whether its events carry tags.
- **Sensitive data** (a link from the logs explorer, §8.9): the scanning groups in the order they
  run, each its filter, the rules it has on, and whether it is on; turned on and off, moved and
  removed from its row. A group's page: its name and filter, its rules in order (each what it finds,
  what becomes of a match, where it looks; moved, edited and removed from its row), a rule edited in
  a dialog (what it finds, a standard pattern by its category or a pattern of one's own; what
  becomes of each match, a string, its first or last characters hidden, a hash, or the record tagged;
  where it looks and where not; keywords near and how near, a pattern's own when it suggests them;
  its tags, its priority, whether it is on), and a message tried on the group's rules as they are,
  saved or not, whatever its filter picks, showing what it becomes and the tags it gets.
- **Rehydrations** (the Archives page's second part, §10.6): each rehydration, its search and its
  archive, its range, how its reading goes (waiting, listing, objects read of its range's), what it
  kept of what it read (records, objects, bytes; whether its bound stopped it) or why it failed, and
  until when its view is kept; its view a click from the logs explorer (Search), removed from its
  row. Asked from an archive's row (that archive chosen) or the part's own button: a name, the
  archive, a search, a range (the last day unless changed), its days and a bound of GB read. The
  logs explorer reads a view picked beside its range (shown while there are views): over the view's
  own range, said above it with how its reading goes (read again as it fills), its histogram, facets,
  records and patterns as the live logs'; no live tail.
- **Logs kiln collects** (Integrations' fourth part): each source's state over the machines that
  read it (reading, failing, no files), what it reads, its records and how far behind it is;
  added and edited in a form that tests the source on the server (the files it matches, how the
  newest one's last lines read, the rules applied) before it is kept.
- **Integrations**: added (Slack, Discord, Teams, PagerDuty, Opsgenie, email through an SMTP relay,
  webhooks, signed), tested, removed;
  each one's mention to copy. The clouds kiln reads: each one's state, what it reads (its logs too:
  log groups, a filter, workspaces, an Azure namespace's event hubs), its credentials and its last
  pull (series, and the log records it took; how many events its hubs are behind); added and edited
  in a form that tests a pull (series, points, log records and the newest of them) before it is
  kept, a filter's or query's field there once there are logs to filter, Azure's metrics read or
  not, its event hubs read with a connection string (kept unshown) or the integration's credentials.
  A hub's stream failing is the integration failing, its error beneath its name. And
  the Prometheus endpoints it scrapes: each
  target's state over the machines that read it (up, down, starting) and its error, its endpoint,
  who reads it and how often, its last scrape; added and edited in a form that tests the endpoint
  once from the server (what it reads, in which format) before it is kept, its token kept unshown.
- **Synthetics**: each test's state over its locations (passing, failing and how many, pending,
  paused), what it tries, why it fails (a location's first failure), each location's dot, its last
  run (when, how long, its status code), the monitor it owns; run now from everywhere, edited and
  removed in a click; made and edited in a form (its kind's fields, an assertions editor of the kinds
  its check takes, its locations, schedule and alerting) that tries it once from the server (passed
  or failed, its time cut into its parts, what failed) before it is kept.
- **Live**: the hosts' samples of the last hour at 5 s, streamed (what the Metrics page was); before
  that, the Hosts dashboard.

Every chart is uPlot, already kiln's, but the heatmap and the distribution, drawn on a canvas. A
series sent less often than its chart's grid (a metric every minute on an hour's grid of 15 s:
OpenTelemetry's SDKs export every 60 s unless told otherwise) is drawn across its empty buckets: a
run of them no wider than twice its usual spacing (the median between its samples: one sample
missed) is joined, a wider one left a gap, a lone value drawn as a point; as first drawn, a line was
broken at every empty bucket, and such a series drew nothing (a notebook's graph found it). A y
axis is as wide as its longest label (52 px at the least, uPlot's own measure: `47.7 KiB/s` was cut
to `.7 KiB/s`), and a page's chart names its queries' lines where it says them (In use, Limit)
rather than numbering them (`1: *`). To
come: series reduced to the chart's width with largest-triangle-three-buckets (Steinarsson 2013) when a range holds more
buckets than pixels (today the query's `points` bounds them: 240 a chart).

### 11.1 Notebooks

Datadog's notebooks (`crate::observe::notebooks`): a document of cells over a time, each cell
Markdown or a graph as Datadog's notebooks have them (a timeseries, a top list, a heatmap, a
distribution, a log stream: their definitions Datadog's dashboards' widgets'), a graph its size
(`xs` to `xl`) and, if it says, a time of its own; the notebook's time the last of a span
(Datadog's: `1h`, `1w`, `1mo`, …) or from one time to another; its type (an investigation, a
runbook, a postmortem, documentation, a report) and whether it is a template.

- **Drawn** as the dashboards draw Datadog's widgets: each graph cell read by the dashboards'
  importer (§11) and answered beside its definition (`kiln.widget`, and what kiln said where it
  draws one otherwise), on the notebook's time or its own: the charts take a fixed time as well as
  the last of a range (a postmortem's is fixed, the incident's).
- **Kept** by the server, each a file of its own (`<observe>/notebooks/<id>.json`, written whole
  and atomically as it changes); in memory only what lists them (its name, author, type, template
  or not, times, size, its incident): a notebook's cells are read from its file when it is read,
  and at the start only each file's head is read (serde skips the cells). 10,000 notebooks and
  1 GiB of their files at the most, a notebook 500 cells and 4 MiB; none let go (a document is not
  dropped): past a bound, none more is made, saying why. Ids go on past a removed one's
  (`next.json`), and past a file that does not read (left as it is, said).
- **The API** (kiln's, a session's; reading `observe.read`, the rest `observe.write`): Datadog's v1
  notebooks API at kiln's paths (`/api/observe/v1/notebooks…`): made (200, as Datadog's), listed
  (`author_handle`, `exclude_author_handle`, `query` a part of the name or the author's handle,
  `is_template`: templates or not, false unless said; `type`; `sort_field` of `modified`, `name` or
  `created`, `sort_dir`; `start`, `count` 100 unless said and 1,000 at the most; `include_cells`, its
  cells and time read from each file, true unless said; `meta.page`: `total_count`,
  `total_filtered_count`), read, changed whole (Datadog's `PUT`: a cell sent with an id it has kept,
  one without a new one) and removed. What Datadog's API refuses refused, saying why (a name past 80
  characters, a cell of a kind it has not, a graph without its requests, a heatmap or a distribution
  of more than one, a span or a size it has not, a time that ends before it starts, a type it has
  not). At Datadog's own paths (`/api/v1/notebooks…`) the ingest token, a writer of data, reads and
  changes none (403).
- **Postmortems**: an incident's postmortem written (`POST /api/observe/v1/incidents/{id}/postmortem`,
  kiln's; `template_id` a notebook that is a template, kiln's own unless said) is a notebook of type
  postmortem made of the template, each Markdown cell's variables filled with what the incident
  says, Datadog's names (the Incident Variables Reference): `{{incident.id}}`, `title`,
  `severity`, `status`, `created`, `detected`, `resolved`, `commander` (and its `_name`, `_handle`,
  `_email`), `created_by_name` (`_handle`, `_email`), `responders_summary` and
  `responders_full_list` (its commander, then who wrote on its timeline), `customer_impacted`,
  `customer_impact`, `scope_of_impact`, `customer_impact_start`, `_end`, `_duration`,
  `timeline` (every cell, its time, who, what), `card` (its facts as a table), and its fields by
  name (`{{incident.root_cause}}`); kiln's own besides: `url`, `todos` (a task list), `links`,
  `time_to_detect`, `time_to_internal_response`, `time_to_repair`, `time_to_resolve`. Channels and
  meetings kiln does not open (Slack's, Teams', Zoom's) and what an AI would write (`ai_*`) are
  empty; a variable neither knows is left as written. A graph keeps its definition and takes the
  notebook's time: the incident's, from 30 minutes before it was detected or its impact began to
  30 minutes after it was resolved or its impact ended (or now). The notebook is attached as the
  incident's postmortem (its link the notebook's page at the server's public URL); an incident with
  a postmortem is refused another (409), and a notebook made as another was attached is removed.
  kiln's own template follows the Google SRE book's example postmortem (Beyer et al., *Site
  Reliability Engineering*, O'Reilly 2016, appendix D): its facts, summary, impact, root causes,
  trigger, resolution, detection (and Datadog's four times), action items (its todos), lessons
  learned (what went well, what went wrong, where we got lucky), timeline, supporting information
  (its links).
- **Pages**: the list (notebooks or templates, by type, found by a part of the name or the author),
  a new one made there (opened to be written); a notebook's page: its cells drawn over its time
  (picked in place, saved with it), its type and author, its incident if a postmortem; written in
  place: its name, type and whether a template; a cell's text, a graph's query and display, or its
  definition as Datadog's JSON where it has more than one query, its size; cells added (text, a
  graph, logs), moved and removed; deleted. An incident's page writes its postmortem (of a template
  picked, if any are kept) and opens it; a postmortem kiln keeps opens in place, whatever host its
  link names.

## 12. Measurements

Every benchmark states the hardware, the date, the exact command, and per operation: time
(quantiles with their intervals, hyper-measure), allocations, reallocations, bytes and page faults.

The corpus: 24 hours of kiln's cluster (its Prometheus: the Kubernetes control plane, nodes,
kiln; 10 s and 30 s scrapes), up to four series a metric name, 5,666 series and 16,274,210 samples,
exported with `crates/kiln-obs/benches/export-prom.py <prometheus> 24 4 <corpus>` on 2026-10-18.

### 12.1 Codec

2026-10-18, AMD Ryzen 9 9950X3D2 (16 cores, 32 threads), Linux 7.2.9:
`KOBS_CHUNK=240 KOBS_CORPUS=<corpus> cargo bench -p kiln-obs --bench codec`

| | |
| --- | --- |
| bytes a sample | 1.167 (raw 16; Gorilla reports 1.37 on Facebook's data) |
| encode | 2.80 ns a sample; 1.08 allocations, 1.26 reallocations (none moved) a chunk; 0.084 page faults a chunk |
| decode into a query's columns | 4.12 ns a sample; no allocation; no page fault |

Decoding was 6.37 ns a sample; perf (2026-10-18, its sampling now open here) put a quarter of a
query's time in the bit reader's refill, which loaded a byte at a time whenever the word was not
empty. It now takes as many whole bytes as the word has room for in one load and one shift, a byte
at a time only in a chunk's last eight: 4.1 ns, the same figures bit for bit (its tests: every width
and alignment). Reading a unary code (a delta-of-delta's bucket, a value's control bits) in one look
was tried and measured slower, 5.4 ns: kept a bit at a time.

### 12.2 Chunk size and timestamp buckets

`... --bench codec -- sweep`. Bytes a sample against chunk length (the timestamp buckets of the
time, Prometheus's):

| samples a chunk | 15 | 30 | 60 | 90 | 120 | 180 | 240 | 360 | 480 | 960 | 1920 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| bytes a sample | 2.271 | 1.687 | 1.397 | 1.299 | 1.255 | 1.211 | 1.191 | 1.175 | 1.170 | 1.169 | 1.175 |

The curve flattens at 1.169. A query decodes on average half a chunk at each end of its range to
reach the samples it wants, so a chunk should be no longer than compression pays for: the rule is
the shortest length within 2% of the plateau, **240 samples** (`CHUNK_SAMPLES`).

Delta-of-deltas (chunks of 120): 91.7% are zero; the rest need 3–8 bits (scrape jitter, 3.7%) or
11–16 (a missed or late scrape, 4.3%); 0.008% need 19–20, none more. Expected timestamp bits a
sample under each bucket set: Gorilla's 7/9/12, 3.710; Prometheus's 14/17/20, 2.290; the cheapest
for this corpus, 7/14/16, 2.089. Its 16-bit outer bucket (±32 s) would send a missed 60 s export to
the 68-bit fallback, a workload this corpus lacks; the cheapest set that keeps Prometheus's ±524 s
outer bucket is **7/14/20, 2.124** (`DOD_BITS`), 7% below Prometheus's.

### 12.3 The store, ingesting

2026-10-18, same machine: `KOBS_CORPUS=<corpus> cargo bench -p kiln-obs --bench store`. The corpus
replayed as an ingest node would send it: each series' key made once and kept, a batch a 60 s
window across every series (1,440 batches), each series defined in its first batch and named by
fingerprint after; one replica applies them in order.

| | |
| --- | --- |
| build a batch (the ingest node) | 74.1 ns a sample; 0.0001 allocations a sample (keys apart, made once) |
| batch size (the Raft log, the wire) | 15.5 bytes a sample (first design, a key in every run: 130.0) |
| apply (a replica) | 17.8 ns a sample; 0.0054 allocations, 0.0081 reallocations a sample; 0.00057 page faults a sample |
| duplicates | 29,040 samples of 11 series exported twice, recognised and dropped; none refused |
| memory | 28.3 MB: 1.57 bytes a sample, 486 bytes a series of index and symbols |
| select a metric (2,022) | p50 0.1 µs, p99 0.6 µs |
| select `job:apiserver` across every metric (756 series) | p50 0.1 µs, p99 0.3 µs |
| select `node_*{instance:*:9100, !mode:idle}` (1,010 series) | p50 28.4 µs, p99 56.3 µs (first glob, allocating: 274 µs) |
| read the last hour of every series | 678,336 samples, 9.1 ms, 13.4 ns a sample, 16 allocations in all |
| read the whole day of every series | 16.2 M samples, 107.5 ms, 6.6 ns a sample, 10 allocations in all |

### 12.4 Blocks, compaction, reopening

2026-10-18, same machine: `... --bench store -- persist`. The same batches into a store on disk,
the head cut into a block every 120 batches (two hours, Gorilla's and Prometheus's block span) and
compacted after each cut by the logarithmic method.

| | |
| --- | --- |
| cut a two-hour head into a block | p50 5.2 ms, max 6.3 ms |
| compaction | p50 60 ms, max 277 ms; write amplification 4.15 (12 cuts: log2 12 + 1 ≈ 4.6 bound) |
| on disk | 24.3 MB in 3 blocks, 1.49 bytes a sample (each block names its series: self-describing) |
| reopen | 9.6 ms for 5,655 series; 9 page faults |
| read the last hour of every series, from blocks | 15.3 ns a sample; no allocation a series |
| read the whole day of every series, from blocks | 7.9 ns a sample; no allocation a series |

### 12.5 Queries

2026-10-18, same machine, loaded (load average 31–39 from other work): `KOBS_CORPUS=<corpus> cargo
bench -p kiln-obs --bench query`. Each query over the corpus's whole day (a grid of at most 1,200
points a series), fifteen runs; allocations the second run's.

| query | series | samples | p50 | p99 | ns a sample | allocations |
| --- | --- | --- | --- | --- | --- | --- |
| `avg:node_load1{*} by {instance}` | 3 | 8,640 | 0.08 ms | 0.12 ms | 8.8 | 49 |
| `p99:node_load1{*}` | 1 | 8,640 | 0.09 ms | 0.12 ms | 10.5 | 39 (a bucket's own list at first: 760) |
| `sum:node_cpu_seconds_total{mode:idle} by {instance}` (a counter's rate) | 1 | 2,880 | 0.03 ms | 0.04 ms | 9.1 | 47 |
| `sum:apiserver_request_total{*} by {verb}` | 2 | 11,520 | 0.06 ms | 0.07 ms | 5.4 | 43 |
| `100 * sum:…{code:5*} / sum:…{*}` | 0 | 11,520 | 0.06 ms | 0.07 ms | 5.3 | 48 |
| `top(avg:container_memory_working_set_bytes{*} by {pod}, 10, 'mean', 'desc')` | 4 | 23,114 | 0.25 ms | 0.29 ms | 10.7 | 76 |
| `avg:node_*{*} by {instance}` | 7 | 2.93 M | 18.1 ms | 19.5 ms | 6.2 | 84 (a group key a series at first: 1,116) |
| `sum:*{job:apiserver}` | 1 | 2.18 M | 13.9 ms | 14.9 ms | 6.4 | 37 |
| `avg:node_*{*} by {instance}.rollup(avg, daily, 'America/New_York')` | 7 | 2.93 M | 16.0 ms | 17.4 ms | 5.5 | 90 |
| `avg:node_*{*} by {instance}.weighted()` | 7 | 2.93 M | 18.7 ms | 20.6 ms | 6.4 | 88 (each series' interval from all its gaps at first: 13% slower) |

The decoder's refill (§12.1) and chunks read whole: the same binary before and after, run in turn
three times under the same load, `avg:node_*` 25.0–27.1 ms before and 18.0–18.3 after (−29%),
`sum:*{job:apiserver}` 19.4–21.2 and 14.0–14.4 (−28%), the small queries 30–40% faster. A chunk that
lies wholly inside the range is now decoded without a look at each sample's time, its columns grown
once to its count: 12 allocations fewer a query (1.5–2.5% of the time alone).

A calendar rollup: its two buckets, faster than the plain query's 1,200; its edges one allocation a
metric, its timezone loaded once a process (47
allocations, the first query to name it). Parsing it costs nothing more than a rollup in seconds
(8 allocations; it compared names lowercased at first: 14): what a query's shape costs on an empty
store, parse and evaluation, is the bench's first table (`rollup(avg, 60)` 8 and 15,
`rollup(avg, daily, 'America/New_York')` 9 and 17, a quoted string's one; quoted strings are now
sized to their quote, one allocation, not one and a growth). `weighted()` reads at most 64 of a
series' gaps, spread over it, for its interval: a median of 64 has a standard error of 0.16 of the
gaps' spread, and a series that reports at an interval has gaps all but equal.

Distributions (§4.4), the bench's second table, a store of its own: 50 hosts' latencies, each
host's log-normal (σ 0.5, its median 18 ms to 135 ms by host), 100 values every 10 s for an hour,
posted as `kiln_obs::dist::series_of` writes them: 8,600 bucket series (172 a host), 1.07 M of their
samples, and beside them a gauge read as often (100 samples every 10 s a host, 1.8 M). Each query
over the hour at 120 points, fifteen runs; the same binary before and after run in turn three times
(load average 33–34):

| query | series | samples | p50 | allocations |
| --- | --- | --- | --- | --- |
| `p99:lat{*}` | 1 | 1.07 M | 14.0–15.9 ms (18.4–21.9 before) | 67 (355 at first) |
| `p99:lat{*} by {host}` | 50 | 1.07 M | 17.9–19.2 ms (22.9–25.8) | 862 (36,500 at first) |
| `avg:lat{*}` (its sums over its counts) | 1 | 36,000 | 0.22 ms | 55 |
| `p99:lat{*}`, a heatmap of 40 rows | 38 rows | 1.07 M | 13.6–15.7 ms (18.3–23.9) | 209 (767 at first) |
| `avg:plain{*}`, the gauge | 1 | 1.8 M | 10.0–10.5 ms (the same) | 41 |

A distribution's buckets are one table (`DistTable`): each bucket a row of counts, their room kept
from group to group, the rows ordered once a group; a group's key is the tags' values borrowed from
the store, copied only for a new group (each bucket series built its own tags at first, three
allocations each). A bucket series is a count, so each of its samples goes straight into its row's
bucket of time, not through `rollup`'s 120 buckets a series and a second pass over them (−20 to
−27%). What remains is decoding (over half the profile): a bucket series holds a count only in the
steps a value fell in its bucket, so its times are irregular and each costs more bits and branches
than a gauge's (14 ns a sample here against the gauge's 5.6). A rollup tier (§0, O-10) is what
would read fewer of them over long ranges.

### 12.6 Log text: LZ4 against deflate

2026-10-18, same machine: `KOBS_LOGS=<journal>:<pods> cargo bench -p kiln-obs --bench lz4`. 43.4 MB
of kiln's logs: the runner's journal and every pod's in the cluster (ArgoCD's logfmt, JSON,
Kubernetes' klog, kiln's tracing), 231,561 lines. LZ4 is `kiln_obs::lz4` (safe code); deflate is
flate2's (miniz_oxide). Throughput is of the uncompressed bytes.

| block | codec | ratio | compress | decompress |
| --- | --- | --- | --- | --- |
| 64 KiB | LZ4 | 6.60 | 1,029 MB/s | 3,701 MB/s |
| 64 KiB | deflate-1 | 8.59 | 640 MB/s | 1,468 MB/s |
| 64 KiB | deflate-6 | 11.85 | 179 MB/s | 1,938 MB/s |
| 256 KiB | LZ4 | 6.92 | 1,052 MB/s | 3,802 MB/s |
| 256 KiB | deflate-1 | 8.81 | 848 MB/s | 1,565 MB/s |
| 256 KiB | deflate-6 | 12.72 | 147 MB/s | 2,026 MB/s |
| 1 MiB | LZ4 | 7.00 | 1,022 MB/s | 3,171 MB/s |
| 1 MiB | deflate-6 | 12.96 | 130 MB/s | 1,883 MB/s |
| 4 MiB | LZ4 | 7.03 | 891 MB/s | 3,608 MB/s |
| 4 MiB | deflate-6 | 13.02 | 125 MB/s | 1,863 MB/s |

Decompressing allocates nothing. The choice follows how blocks are used: recent blocks, searched
most, are LZ4 (twice deflate's decompression rate); blocks compaction rewrites, searched rarely and
holding most of the bytes, are deflate-6 (1.86× smaller), as ClickHouse pairs LZ4 for hot parts
with a stronger codec for cold. A block's text is 256 KiB: the smallest within 2% of LZ4's ratio
plateau, so a search that hits one row decompresses at most 256 KiB (about 70 µs).

### 12.7 Log blocks' token filters

`... --bench lz4 -- tokens`: the corpus of §12.6 in blocks of 256 KiB has 2,603 distinct tokens a
block on average (median 2,326, p99 5,531). A filter for them at each false-positive rate, against the
block's LZ4 text (37.8 KB on average):

| rate | bytes a block | of its text | hashes | wasted decompression a block a missing word |
| --- | --- | --- | --- | --- |
| 0.1% | 4,684 | 12.4% | 10 | 0.07 µs |
| 0.5% | 3,596 | 9.5% | 8 | 0.34 µs |
| **1%** | **3,124** | **8.3%** | **7** | **0.69 µs** |
| 2% | 2,660 | 7.0% | 6 | 1.38 µs |
| 5% | 2,036 | 5.4% | 4 | 3.45 µs |
| 10% | 1,564 | 4.1% | 3 | 6.90 µs |

1% is where tenfold fewer false positives stop costing little space (from 5% to 1%: 2.9 points of
space for a fifth of the waste; from 1% to 0.1%: 4.1 points for a tenth), and RocksDB's default for the
same trade (10 bits a key).

**Across blocks.** Filters of one size put a token at the same places in each, and the tokens every
block holds set those places in each, so a word whose places fall on them could pass every block's
filter at once. `KOBS_LOGS=… cargo bench -p kiln-obs --bench filters` (fedora, 2026-10-19): a
machine's journal, 370,258 lines in 398 blocks of 256 KiB, its filters of 140 sizes (the commonest
16 blocks'); 2,000 words no block holds each passed 1.00% of the blocks on average, median 4, p99 10,
the most 14 (Binomial(398, 1%): p99 9 to 10), and salted by their block (`bloom::salted`) the same
(the most 12). Logs' blocks differ in their tokens, so in their filters' sizes, and that decorrelates
them: they stay as they are. Spans' blocks are alike (one service's spans, block after block), their
filters of one size: those are salted (§8.7, §12.23).

### 12.8 Logs: ingest and search

2026-10-18, same machine: `KOBS_LOGS=<journal>:<pods> cargo bench -p kiln-obs --bench logs`. The
lines of §12.6 as 194,690 records (time, severity from the line, service from its source and
format, logfmt pairs as attributes), ingested in batches of 1,000 with a cut every 50 batches into a
store on disk, then searched for the newest 100 over the whole range, nine runs each.

| | |
| --- | --- |
| ingest: build, apply, seal, cut | 579,154 records/s, 105.9 MB/s of lines, one core; 0.15 allocations a record (copying each row out to re-encode it at first: 8.96) |
| batches | 0.21 bytes a byte of line |
| on disk | 7.66 MB: 0.215 of the lines |
| reopen | 0.4 ms; 0.50 MB of summaries in memory |

| search | found | blocks pruned | rows read | p50 | p99 |
| --- | --- | --- | --- | --- | --- |
| `*` | 100 | 0 of 111 | 690 | 0.23 ms | 0.26 ms |
| `status:error` | 100 | 92 | 30,000 | 5.39 ms | 5.46 ms |
| `service:kube status:warn` | 100 | 0 | 690 | 0.09 ms | 0.14 ms |
| `"connection refused"` | 12 | 109 | 4,000 | 1.91 ms | 1.93 ms |
| `@application:kiln-namespaces` | 100 | 0 | 10,690 | 1.70 ms | 1.82 ms |
| `nosuchtokenanywhere` | 0 | 110 | 2,000 | 0.63 ms | 0.70 ms |
| `service:argocd -status:info` | 0 | 111 | 0 | 0.01 ms | 0.07 ms |

The absent word's one unpruned block is the token filter's false positive, as its 1% rate predicts
for 111 blocks.

Log analytics over the whole range (every record read), nine runs each; allocations counted by
hyper-measure's counting allocator:

| aggregation | groups | rows | allocations a row | p50 |
| --- | --- | --- | --- | --- |
| `count by {service}` | 4 | 194,690 | 0.0007 (first: 3.04) | 36.7 ms (first: 46.1) |
| `count by {service,status}` | 10 | 194,690 | 0.0009 (first: 5.04) | 37.4 ms (first: 52.5) |
| `count by {@application}` | 5 | 194,690 | 0.0008 (first: 3.04) | 41.7 ms (first: 55.3) |
| `cardinality(host) by {service}` over `status:error` | 2 | 30,000 | 0.0047 (first: 0.064) | 5.5 ms |
| the explorer's facets: service, host, source, status | 4 | 194,690 | 0.0009 | 50.6 ms (as four scans: 152) |
| the explorer's histogram: count by status, 60 buckets | 4 | 194,690 | 0.0008 | 37.0 ms |

At first each row allocated its group's key and cloned it to look it up, and each block of a file
read cloned the block's whole summary (its dictionaries and token filter) to get at its text. Groups
are now found by their values borrowed from the row (a keyed table of the groups' places; a group's
key allocated once, when it is new), and a file's block text is read into a buffer kept across
blocks. What is left is reading: about 1 GB/s of rows decoded, LZ4 included.

Then a scan split among threads (2026-10-20): the blocks a search reads (once the summaries have
ruled out the rest) split into runs of about equal rows, in order, each run read on a thread of the
budget metric queries read on (`query::eval`'s helpers, a quarter of the cores less one unless the
server's `query_helpers` says otherwise; one thread a 65,536 rows at the most, some ten milliseconds
of reading, `logs::store::SPLIT_ROWS`), each thread its own groups, merged in the runs' order. For
counts, distinct values and the least and most: what does not hang on the order records come in, so
the answer is one thread's to the bit (a test over four searches, two ranges, three groupings and
those four measures, on two to five threads, bucketed and whole, and the explorer's facets); a sum or
an average stays one thread's, its bits the order's. On this machine's journal (fedora, 367,666
records, 89.5 MB of lines, others' builds at a load of 20 to 27), `KOBS_HELPERS=0` against the
default (five threads for these), two runs each in turn:

| aggregation | one thread | five threads |
| --- | --- | --- |
| `count by {service}` | 88.4–89.7 ms | 27.5–28.1 ms |
| `count by {status}` | 86.3–88.3 ms | 25.7–26.8 ms |
| `count by {service,status}` | 88.9–93.7 ms | 25.4–28.5 ms |
| `count by {@application}` | 117.9–122.7 ms | 36.2–37.6 ms |
| the explorer's facets (four, one pass) | 119.9–124.9 ms | 37.3–38.3 ms |
| the explorer's histogram (60 buckets) | 84.1–85.8 ms | 27.5–27.6 ms |

A third of the time on five threads, not a fifth: the reading is memory's (LZ4 and the rows), on a
machine others share. A search of the newest records stays one thread's: it stops at its limit, its
blocks newest first.

Then the seal (2026-10-20). Under the logs flood (§13) the thread that applies spent 54.5 thousand
instructions a record (perf, user space, counted over the whole run), nearly all of it sealing the
head into blocks, under the store's exclusive hold: the token set's SipHash (23%), LZ4 (15%; half of
it extending a match a byte at a time), UTF-8 checks (15%: each row's attributes checked as the row
was read, again as it was written into the block, again as it was tokenized), the tokenizer itself
(14%: a character decoded and tested against Unicode's tables at a time) and each record's service,
host, source and keys searched for in the block's lists (6%). Now, each to the byte of what was
written before (tests against copies of the code as it was: the tokens, a block sealed, a row
written again, LZ4's output):

- the token set hashes its keys (two hashes already) folded with a key the process draws and mixed
  once, so no tokens a sender writes collide in it (sorting them instead was measured slower: a
  log's words repeat);
- ASCII is tokenized a run at a time, a character past it decoded alone;
- a row's attributes are checked as they are read (`RecordRef::attrs`), and written into a block as
  their bytes are, unread; every row is still checked whole as its batch is applied (`within_logs`:
  a malformed one, a remote writer's, is refused there, and never reaches the head, whose seals and
  cuts read every row);
- a record's service, host and source are looked for only when they differ from the last record's,
  its keys only where they differ from the last record's in place;
- a seal works in what the thread kept from its last (its text alone past the allocator's
  threshold for memory of its own);
- LZ4 extends a match eight bytes at a time.

The bench above: ingest 1.00–1.01 M records a second before, 1.19–1.20 M after the seal's changes,
1.16–1.25 M with LZ4's too (nine runs), its searches and analytics as they were (±5%). The flood: 34.2 thousand
instructions a record against 54.4 (37% fewer, the server's and the runner's alike, both of two runs in
turn).

### 12.9 The group: writes through the replicated log

2026-10-18, same machine, NVMe under btrfs (a raw `write` + `fdatasync` there: 2.8 ms for 4 KiB,
4.2 ms for 4 MiB): `KOBS_CORPUS=<corpus> KOBS_DIR=<a folder on that disk> cargo bench -p kiln-obs
--bench group`. The corpus as 10 s windows across every series (1,883 samples, 52 KB a write), `S`
senders each with one write out, proposed at the leader, done when the leader applied it.

hyper-log answers a write once a later durable record confirms its flush (the next frame's persist
record, or a confirmation of its own), so a write alone costs two flushes: 6.0 ms at the median
for one sender. Writes that arrive while a flush runs share the next one (16 senders: 16 writes a
log write; 64: 62.5), and the frame bounds what one flush carries:

| frame | 64 senders, M samples/s (3 runs) | median | commit p50 (median) |
| --- | --- | --- | --- |
| 1 MiB | 1.95, 3.77, 3.99 | 3.77 | 30 ms |
| 2 MiB | 5.36, 3.51, 5.78 | 5.36 | 19 ms |
| 4 MiB | 6.11, 6.48, 8.75 | 6.48 | 14 ms |
| 8 MiB | 8.04, 7.38, 7.35 | 7.38 | 14 ms |
| 16 MiB | 7.31, 5.84, 5.81 | 5.84 | 15 ms |

At 16 senders every frame gives some 3 M samples/s. The group's frames are 8 MiB
(`kiln_obs::group::FRAME_BYTES`). With them: one sender 145 writes/s (0.27 M samples/s), commit p50
6.0 ms [5.9–6.0], p99 21 ms; 0.032 allocations and 0.0015 page faults a sample, every thread of the
process counted. Two learners in the same process on the same disk (three logs flushing, one thread
driving them all) take everything 11–13 ms after the leader, and cost the leader's commit about as
much again.

Two waits found and removed on the way: the bench, and kiln's owner the same way, waited out a
timeout after a write was answered before sending the next (up to the owner's 100 ms tick); the
owner now waits exactly until the earliest of its tick, the batcher's next write and the replica's
own wake.

### 12.10 OTLP decoded

2026-10-18, same machine: `cargo bench -p kiln-obs --bench otlp`. A request decoded into series,
the median of 51; allocations and page faults counted over one; "warm" is a receiver's steady
state, its series' keys made by the requests before (the node's key cache).

| request | points | before: ns, allocations a point | cold | warm |
| --- | --- | --- | --- | --- |
| the load test's: 1 resource attribute, 1,000 series of a gauge | 1,000 | 328, 18.0 | 309, 4.1 | 158, 1.04 |
| an SDK's: 9 resource attributes, a histogram of 14 bounds and a sum by method, route and status, 4 runtime gauges (50 routes) | 904 | 1,134, 40.6 | 895, 6.2 | 361, 1.09 |

Before, every point's tags were cloned three times (the attributes, the merge over the resource's,
the scratch) and its series' key made again, SHA-256 and all. Tags are read borrowed from the
request now (a value is copied only when it is not text), merged into a reused list, written into a
reused buffer as the canonical key, and found by a keyed hash: first in the request's own series,
then in the node's keys. gzip's inflating is 6 to 12 ns a point. Since the samples went flat
(§8) a warm request allocates twice, not once a series.

perf then put an eighth of a request's time in its own series table growing: it starts empty and
grows ten times for 1,000 series, and each growth hashed every key held again. Both tables (the
request's and the node's keys) now keep each entry's hash beside it, so growing moves entries and
hashes nothing. The same binary before and after, in turn, three runs each under the same load:
warm 148–164 to 130 ns a point (the load test's request), 363–367 to 276–279 (the SDK's), 385–390 to
285–306 (500 routes); cold 900–910 to 750–768 (the SDK's). A point's transient bytes grow by a
fifth (166 to 199 the load test's: an entry is 16 bytes, not 8), its allocations not at all.

A receiver now decodes into a decoding handed back once the node's owner has taken its samples
(`decode_into`; a few kept, `crate::observe::owner::Spares`), the request's series table and key
buffer kept with it, and unzips into bytes handed back the same way: a request's transient bytes a
point fall from 199 to 17 (the load test's), 184 to 5.4 (the SDK's) and 164 to 2.1 (500 routes), its
allocations from 0.03 to 0.013, 0.09 to 0.062 and 0.010 to 0.007, and it is 4–7% faster (the bench's
"kept" columns).

**OTLP logs** (2026-10-20, `KOBS_SIGNAL=logs cargo bench -p kiln-obs --bench otlp`, fedora quiet:
load 3 to 5): a request of 500 records decoded, the median of 51, allocations of one. Before, each
record was made a `Record` (its strings, and every attribute of its resource cloned into it: a bare
record allocated 26 times), and the batcher wrote its row after; now a decoder that keeps its room
(`otlp::logs::Decoder`, as spans' does) writes each record's row straight from the request, the
record made only where a pipeline reads it (asked of a borrowed view of it), and a map's or an
array's text is written with its frames held on the stack (`any_value_push`: a `Vec` each before).
A test holds every row to the one its record writes, its error's issue on it, over 600 random
requests (every value's kind, attributes past the bound, ids of every length, errors that are
issues), and the two take or refuse 3,000 corrupted ones alike.

| request | records and their rows: ns a record | allocations | bytes | rows: ns a record | allocations | bytes |
| --- | --- | --- | --- | --- | --- | --- |
| an SDK's: 9 resource attributes, 4 a record | 1,350–1,380 | 34.08 | 1,370 | 432–442 | 0.002 | 0.32 |
| an SDK's: 12 a record, a map as its body | 2,330–2,340 | 55.08 | 2,358 | 1,121–1,128 | 0.002 | 0.32 |
| bare: 9 resource attributes, none a record | 650–660 | 26.08 | 1,291 | 177–183 | 0.002 | 0.32 |

What is left is the rows' own room (a segment's buffer, handed back by the node's writes) and,
where a pipeline reads a record, the record.

### 12.11 Under load, end to end

2026-10-18, same machine (32 threads, shared: the load average stood at 30 to 36 from other work,
so absolute rates vary between runs; what a point costs varies less): `cargo test --release -p
kiln --test observe_load -- --ignored --nocapture`. A server and a runner as processes; 8 senders,
half to each, of OTLP/HTTP protobuf, gzip, kept-alive connections, 1,000 points a request, as fast
as they are answered; no reader (`KOBS_LOAD_READ=0`) unless said. Every point answered 200 was
stored exactly once and every replica agreed at the end of every run below.

| | 20 s, at first | 20 s, samples flat (§8) | 40 s, blocks of format 2 (§4.5) |
| --- | --- | --- | --- |
| points taken | 174 M (8.7 M/s) | 400 M (20.0 M/s) | 536 M (13.4 M/s, maintenance merging; the machine loaded) |
| a request | p50 0.28 ms, p99 0.72 ms | p50 0.24, p99 0.55 | — |
| the newest points readable | — | 225 ms after the flood | 237 ms |
| server: peak RSS | 691 MiB | 418 | 297 |
| runner: peak RSS | 629 MiB | 331 | 187 |
| the server's store | — | 22 blocks, index 105 MiB | 3 blocks, index 2.6 MiB |
| maintenance, CPU a node | — | none ran (the stall) | 1.9 s |

What the runs found: a flood taken and then dropped (fixed by admission, §8); glibc's per-thread
arenas keeping freed request buffers (`arena_max=2` alone halved the server's memory; samples flat
removed the cause: a thousand blocks a request allocated on one thread and freed on another);
merges that stopped, an index that grew with samples, and merges that held whole blocks (§4.5).
With a reader alongside (a dashboard's two queries every 250 ms: one series, and all 8,000 summed by
host over 15 minutes), one series answered in 0.19 ms at the median, all of them by host in 335 ms.

### 12.12 Anomalies on the Numenta Anomaly Benchmark

`NAB=<a checkout of github.com/numenta/NAB> cargo bench -p kiln-obs --bench nab` (NAB v1.1,
commit ea702d7: 58 labelled series, 365,558 points, 116 anomaly windows; Lavin and Ahmad 2015).

**The scorer** (`kiln_obs::anomaly::nab`) is NAB's, ported from `sweeper.py`, `optimizer.py`,
`scorer.py` and the normalisation of `runner.py`: scored at the thresholds NAB keeps
(`config/thresholds.json`), the results NAB publishes for numenta, Twitter's ADVec, the windowed
Gaussian, random and null come out as `final_results.json` says, all 15 within 4.5e-11. Swept here
over every threshold, a detector's threshold is the corpus' best: never below NAB's (numenta's
standard 70.10 either way; its low-FP 63.21 against the kept threshold's 63.12).

**Each algorithm runs as it would live**: started on the series' probation (its first 15%, 750
points at most: NAB's learning period, unscored), each later point's score from what it expected
before it saw it; NAB's threshold sweep is the only thing fitted to the labels.

**How a point's error became its score** (standard, low FP, low FN; and, not NAB's, each file at
its own best threshold: an upper bound set by the ranking within files), the window the whole
probation, the calibration a quarter's detector over the rest:

| | basic | agile | robust | robust, each file's threshold |
| --- | --- | --- | --- | --- |
| \|z\| itself, one threshold for the corpus | 1.3 | 0.0 | 1.3 | 66.7 |
| a generalised Pareto tail of the series' \|z\| (SPOT; probability-weighted moments) | 35.4 | 0.0 | 31.6 | 65.2 |
| \|z\| over the series' largest so far | 43.9 | 23.2 | 42.1 | 62.1 |
| \|z\| over its 99.9% quantile | 46.0 | 17.5 | 44.4 | 65.7 |
| … over its 99.7% quantile, an episode scored by its onset | 48.1 | 32.5 | 49.4 | 68.8 |
| **… over its 99.8% quantile, onsets** | **50.3** | 31.3 | **52.6** | 68.8 |
| … over its 99.9% quantile, onsets | 46.3 | 31.1 | 46.3 | 67.5 |
| … the 99.8% quantile from an exponential tail over the 98% (peaks over threshold) | 45.3 | 30.6 | 49.1 | 67.7 |

Within a file the errors rank anomalies well from the start (66.7 with each file at its own
threshold), but |z| is not comparable across files: the anomalies' largest |z| is 3 in one series
and 200,000 in another, and a smooth series without any anomaly reaches 140. Calibrating each
series against its own past does it; a fitted tail (SPOT, Siffer et al. 2017) is less steady than
the empirical quantile on so few peaks (the PWM fit's shape kept at 0 or above, or a bounded tail
calls anything past it impossible). Scoring an episode by its onset spares NAB's per-point false
positives the long run of points past the band that a level shift leaves (as a monitor counts an
episode once). The level is the best of a smooth, single-peaked curve (49.4, 52.6, 49.0, 46.3 from
99.7% to 99.9%; on another calibration split 47.6, 51.5, 48.7, 46.3).

**How much history**: the window (the median's, the offsets', the errors' scale's) read from a day
to the whole probation: `robust` 45.2 at a day, 47.1 at two, 52.6 at the whole (with its best level
at each); the calibration's detector learning an eighth or a quarter of the probation: 51.5 and 52.6.

**Against NAB's published detectors** (standard, low FP, low FN; `results/final_results.json`):

| detector | standard | low FP | low FN |
| --- | --- | --- | --- |
| ARTime | 74.85 | 65.15 | 80.36 |
| Numenta HTM | 70.10 | 63.12 | 74.32 |
| CAD OSE | 69.90 | 66.98 | 73.18 |
| earthgecko Skyline | 58.20 | 46.20 | 63.80 |
| KNN CAD | 57.99 | 43.41 | 64.81 |
| Relative Entropy | 54.64 | 47.60 | 58.84 |
| **kiln `robust`** | **52.59** | **41.02** | **58.34** |
| Random Cut Forest | 51.72 | 38.36 | 59.75 |
| **kiln `basic`** | **50.31** | **40.99** | **54.81** |
| Twitter ADVec | 47.06 | 33.61 | 53.50 |
| Windowed Gaussian | 39.65 | 20.87 | 47.41 |
| Etsy Skyline | 35.69 | 27.08 | 44.48 |
| **kiln `agile`** | **31.34** | **13.05** | **41.12** |
| random | 16.83 | 5.76 | 25.88 |

A point costs 0.36–0.46 µs (robust; the calibration's sorted window the larger part). `agile`'s
ranking within files is as good (62.3 each file at its own threshold) but its errors' spread moves as
its smoothing follows level shifts: next to be studied.

### 12.13 Log patterns on Loghub

`LOGHUB=<a checkout of github.com/logpai/loghub> cargo bench -p kiln-obs --bench loghub`: 16
systems, 2,000 labelled lines each (Zhu et al., "Tools and Benchmarks for Automated Log Parsing",
ICSE-SEIP 2019), scored by grouping accuracy (a line is right when its group holds exactly the
lines of its true event).

**The port** (`kiln_obs::logs::patterns`, from `logparser/Drain/Drain.py`): given the benchmark's
own settings (each system's regexes, similarity and depth, `benchmark.py`), every system's accuracy
is what Drain's README publishes, to the line (HDFS 0.9975 … Proxifier 0.5265; average 0.8654).

**kiln's**: no system's rules, one masking for all (a number with its sign, between characters not
letters or digits; `0x` hex; runs of 8+ hex digits with a digit), and two changes to how Drain reads
masked messages: a token with a part masked goes the tree's wildcard way (masking
`www.example.com:443` leaves no digit, and each host would be a branch of its own), and a masked
variable where the template has a wildcard is alike (the reference counts no wildcard as alike, so a
message of eleven tokens with four masked could be at most 7/11 like any template). The average
accuracy over the 16, by similarity and depth:

| similarity | depth 3 | depth 4 | depth 5 | depth 6 | depth 7 |
| --- | --- | --- | --- | --- | --- |
| 0.3 | 0.693 | 0.761 | 0.821 | 0.828 | 0.817 |
| 0.5 | 0.724 | 0.769 | 0.825 | 0.830 | 0.817 |
| 0.6 | 0.787 | 0.815 | 0.860 | 0.862 | 0.849 |
| **0.7** | 0.830 | 0.849 | **0.887** | 0.879 | 0.863 |
| 0.8 | 0.838 | 0.838 | 0.875 | 0.867 | 0.854 |

Leaving each system out and choosing on the other fifteen, every fold chooses depth 5 and 0.7: 0.887
on the system held out, the estimate for a system kiln has never seen, against the benchmark's
0.865 with each system's own rules and settings. Each step was measured: masking alone at Drain's
usual setting, 0.778; routing masked tokens as variables, 0.827 at the best of the grid; masked
variables alike, 0.874; a number's sign with it (`blk_-…` and `blk_…` one token), 0.887.

| system | kiln | Drain, tuned | | system | kiln | Drain, tuned |
| --- | --- | --- | --- | --- | --- | --- |
| HDFS | 0.998 | 0.998 | | Linux | 0.690 | 0.690 |
| Hadoop | 0.974 | 0.948 | | Android | 0.918 | 0.911 |
| Spark | 0.925 | 0.920 | | HealthApp | 1.000 | 0.780 |
| Zookeeper | 0.989 | 0.967 | | Apache | 1.000 | 1.000 |
| BGL | 0.985 | 0.963 | | Proxifier | 0.527 | 0.527 |
| HPC | 0.906 | 0.887 | | OpenSSH | 0.995 | 0.788 |
| Thunderbird | 0.961 | 0.955 | | OpenStack | 0.881 | 0.733 |
| Windows | 0.695 | 0.997 | | Mac | 0.756 | 0.787 |

Joining templates Drain's tree kept apart (a variable among the first tokens it branches on, as in
`user ada signed in` and `user sam signed in`, sends each value down a branch of its own) was
measured three ways and is off: joining those as alike as a message must be, 0.865 at the grid's
best; only those differing among the branched tokens, 0.838; only where three values or more stand
at one such place (IPLoM's reading of a position, Makanju et al. 2009), 0.838. On these systems the
leading tokens that vary are mostly an event's own (a method's or a component's name), so, as Drain
has it, a variable among a message's first two tokens makes a pattern of each of its values.

Where it loses: Loghub labels some values as constants (`client SPP` and
`client WindowsUpdateAgent`, `user=root` and `user=guest` are each two events), and a group with one
line of another event is all wrong, so Windows' largest group (608 lines) falls with one line;
Proxifier's `lifetime <1 sec` and `lifetime 00:01` are one event of different lengths, which Drain
never joins (nor does the reference, given no rule for it). A line costs 0.2–0.8 µs.

### 12.14 Live tail

2026-10-18, same machine, loaded (load average 40–48: other sessions compiling), so its figures are
compared with each other, not with §12.8's: `KOBS_LOGS=<journal>:<pods> cargo bench -p kiln-obs
--bench tail`. The records of §12.8 as 195 entries of 1,000, applied to an in-memory store; the three
ways of applying taken in turn in each of nine rounds, the median of each:

| applying | records/s | allocations a record |
| --- | --- | --- |
| no feed | 624,090 | 0.055 |
| a feed, no one following | 632,220 | 0.055 |
| followed (the apply alone) | 644,293 | 0.056 |

A feed costs nothing measurable; followed, its copy into the ring is one allocation an entry. A
follower reads every row applied (here a tick of 5,000) and keeps a uniform sample:

| search | matched | ns a row | allocations a tick, 10 kept / 200 kept |
| --- | --- | --- | --- |
| `*` | 194,690 | 82 | 14.6 / 194 |
| `status:error` | 122 | 86 | 0 / 1.5 |
| `service:kube status:warn` | 25,961 | 90 | 0 / 0 |
| `"connection refused"` | 12 | 103 | 0.06 / 0.06 |
| `@application:kiln-namespaces` | 4,254 | 109 | 13 / 181 |

At the most kiln applies on a core (§12.8: 580,000 records a second) a follower costs 5–6% of a core,
so the 16 a node takes cost about one. With 0, 1, 4 and 16 followers on threads of their own, each
reading every 50 ms, while a thread applies everything as fast as it can: 597,408, 652,498, 660,409
and 645,529 records a second, and no row missed (a follower holds the ring's lock to clone an entry's
chunk out, no longer).

The passes it took:
- A phrase was matched in the message lowered a char at a time (`char::to_lowercase`): 421 ns a row.
  An ASCII line is now lowered a byte at a time, what `to_lowercase` gives there: 103 ns, and the
  phrase search of §12.8 1.25 ms where it was 1.91.
- A kept record set over another let go of the attribute pairs it had no more use for, and the next
  that needed them allocated them again: they go to a pool of spares now, and a tick's allocations
  at 10 kept fell from 235 to 14.6 (at 200, from 2,892 to 194). What remains is buffers growing to
  the longest records yet, which ends as they have.
- A record was written to the API as a `serde_json::Value` (some fifteen allocations a record); it
  is written from the record now (a search's 1,000, a tail's tick), byte for byte what the `Value`
  gave (`observe::api` tests), in the key order serde_json's `preserve_order` gives (the QUIC stack's
  qlog turns it on, so kiln's `Value`s keep keys in the order they were put in).

Live, a debug build (§13): a record posted comes on the tail within 0.41 s at the most; in a steady
flood at 50 asked the tail shows 51 a second (10 a tick, 20 after a tick that read nothing), every
one of 18,700 posted counted and none missed.

### 12.15 Forecasts on M4

`M4=<M4's Dataset folder> cargo bench -p kiln-obs --bench m4`: the M4 competition's hourly series
(414, 48 points ahead, a season of 24) and daily ones (4,227, 14 ahead), their test periods held out,
scored as M4 scores them (its `Benchmarks and Evaluation.R`): sMAPE, MASE (scaled by the in-sample
seasonal naive's errors), OWA against Naive2's, and 95% intervals by MSIS (Gneiting and Raftery's
interval score at α 0.05, scaled as MASE) and coverage.

**The scoring checked**: M4's own benchmarks made again (Naive, seasonal naive, Naive2 with M4's
90% seasonality test and classical multiplicative decomposition, Naive's random-walk intervals)
score what M4 published for them (`Evaluation and Ranks.xlsx`) to the third decimal: hourly Naive
43.003 / 11.608 / 3.593, seasonal naive 13.912 / 1.193, Naive2 18.383 / 2.395, Naive's intervals'
MSIS 71.245 and its coverage's distance from 95%, 0.011; daily 3.045 / 3.278 and 32.552, 0.021.

| hourly | sMAPE | MASE | OWA | MSIS | held |
| --- | --- | --- | --- | --- | --- |
| seasonal naive | 13.912 | 1.193 | 0.628 | 9.111 (its own errors' bounds) | 0.963 |
| Holt-Winters, fitted one step ahead | 18.633 | 1.536 | 0.827 | 11.894 | 0.965 |
| Holt-Winters, fitted to the horizon | 15.187 | 1.018 | 0.626 | 7.806 | 0.960 |
| … averaged with the seasonal naive | 13.778 | 1.006 | 0.585 | 7.880 | 0.962 |
| … from the last 10 seasons (the last 6: 0.624) | 13.569 | 1.020 | 0.582 | 7.642 | 0.952 |
| **kiln `seasonal`** (each series choosing, by a fifth) | 13.216 | 0.942 | **0.556** | **7.301** | 0.943 |
| M4: ETS / ARIMA intervals; its best entry | | | 0.440 | 17.487 / 7.538 | |

| daily | sMAPE | MASE | OWA | MSIS | held |
| --- | --- | --- | --- | --- | --- |
| Naive (M4's) | 3.045 | 3.278 | 1.000 | 32.552 | 0.929 |
| Holt, one step, last 20 horizons | 3.045 | 3.237 | 0.994 | 28.724 | 0.928 |
| … averaged with the last value | 2.974 | 3.232 | 0.981 | 28.491 | 0.930 |
| Theil-Sen over the last 2 horizons | 4.315 | 4.657 | 1.419 | 38.910 | 0.909 |
| **kiln `linear`, default** (each series choosing) | 2.955 | 3.220 | **0.976** | 28.903 | 0.933 |
| kiln `linear`, reactive | 3.285 | 3.490 | 1.072 | 29.126 | 0.894 |
| M4: Comb (SES, Holt, damped); best entry; ETS intervals | 2.980 | 3.203 | 0.978; 0.977 | 29.7 | |

Every choice was made on all the series and held on their halves (odd and even places): the
seasons (10 best on both hourly halves), the horizons (30 on both daily halves), the margin a way
must win by to be chosen over the average (0.2: best or tied on three of the four halves, 0.006
behind 0.1 on the fourth). Daily outcomes fall inside 1.96 deviations 93.3% of the time, not 95%:
errors at in-sample origins understate those to come a little; the figure is reported, not tuned.
`simple` over a daily series' whole history (up to 27 years) is 6.99: a line through a random walk
is the wrong model, as Datadog says (`linear` "for metrics that have steady trends"); on a steady
trend it is near exact (a filling disk, §13). A series costs 0.8 ms (hourly) and 0.2 ms (daily) and
ten allocations: the model's buffers kept across the 800 smoothings it tries (160 without a
season), each stopped once it errs more than the best (11 times faster).

### 12.16 Rollups and long ranges

2026-10-18, same machine, loaded (load average 12–20 from other work unless said).

**Tiers on the corpus** (`KOBS_CHUNK=240 KOBS_CORPUS=<corpus> cargo bench -p kiln-obs --bench codec
-- rollups`): 5,666 series, 16.27 M samples, 19.0 MB in chunks of 240.

| tier | windows | samples a window | bytes a window | were every series to keep it |
| --- | --- | --- | --- | --- |
| 5 min | 1,587,768 | 10.2 | 6.54 | 0.547 of the samples' bytes |
| 1 h | 137,498 | 118.4 | 8.91 | 0.065 |

What each keep ratio keeps (a tier kept for a series when its bytes times the ratio are at most
the samples'):

| ratio | bytes added, of the samples' | samples the 5 min tier serves | the hour's |
| --- | --- | --- | --- |
| 1 | 0.533 | 0.930 | 1.000 |
| 2 | 0.264 | 0.170 | 0.999 |
| **4** | **0.074** | **0.059** | **0.981** |
| 8 | 0.056 | 0.022 | 0.883 |
| 16 | 0.027 | 0.014 | 0.219 |

A tier read whole: the five minutes' 11.3 ns a window for its sums (1.11 ns a sample it stands
for), 27.2 every column; the hour's 19.1 (0.16), 34.8 (0.29); decoding a sample is 4.1 ns (§12.1).
Without the byte lengths before each tier and window list, finding the hour's meant reading the five
minutes' windows and finding a column every window: 21.0 and 95.0 ns a window.

**Long ranges** (`KOBS_DIR=<a folder> KOBS_CORPUS=<corpus> cargo bench -p kiln-obs --bench query --
long`): the corpus's node and apiserver series (1,033) replayed for 30 days, 88.1 M samples, into a
store on disk cut every six hours of data (120 cuts) and merged as maintenance merges: 4 blocks,
139.1 MB (1.58 bytes a sample), rollups 4.2% of them. The same store asked with rollups read and not
(`Store::set_read_rollups`), every answer checked the same to the bit; p50 of five runs.

| query | range | points | samples | samples read | rollups read |
| --- | --- | --- | --- | --- | --- |
| `avg:node_*{*} by {instance}` | 1 d | 240 | 2.94 M | 24.9 ms | 24.9 ms |
| | 7 d | 240 | 20.6 M | 152.8 ms | 15.6 ms |
| | 30 d | 240 | 87.8 M | 634.5 ms | 36.4 ms |
| | 7 d | 1,200 | 20.5 M | 152.4 ms | 152.9 ms |
| | 30 d | 1,200 | 87.8 M | 641.5 ms | 38.0 ms |
| `avg:node_load1{*} by {instance}` | 7 d | 240 | 60,755 | 0.69 ms | 0.06 ms |
| | 30 d | 240 | 259,200 | 2.94 ms | 0.13 ms |
| `max:node_memory_MemAvailable_bytes{*}` | 30 d | 240 | 259,200 | 2.28 ms | 0.10 ms |
| `sum:node_cpu_seconds_total{mode:idle} by {instance}` (a counter's rate) | 30 d | 240 | 86,400 | 0.91 ms | 0.05 ms |
| `sum:apiserver_request_total{*} by {verb}` | 7 d | 240 | 81,488 | 0.47 ms | 0.06 ms |
| | 30 d | 240 | 345,600 | 1.96 ms | 0.12 ms |

A chart's 240 points make a week's buckets hours and a month's three hours: the hour's tier. A day's
are ten minutes (the five minutes' tier, where kept), and a week at the API's default 1,200 points
ten minutes too: on this corpus, mostly samples. Allocations are the same either way (the month of
node series 108 read as samples, 127 rolled up; the windows' columns are sized once from the range,
176 when they grew a window at a time).

The month first took 78 ms rolled up: half of it loading whole regions to read the windows cuts
split (perf: `Block::load` 46%, the kernel's copies 11%). Tables and chunks checked on their own,
read in part (§4.5), brought it to 36 ms, and a day read as samples from month-long blocks from 0.15
to 0.11 ms (`node_load1`). What remains (perf): reading the tiers' windows 34%, folding them 12%,
small regions read whole 10%. How long a region is read whole at most (`block::READ_WHOLE`), each
build in turn, two rounds (one 1 KiB and one 4 KiB run of the second hit a load spike and are left
out):

| read whole at most | `node_load1` 1 d | `node_*` 1 d | `node_*` 30 d rolled up |
| --- | --- | --- | --- |
| 1 KiB | 0.18–0.19 ms | 28.1–28.2 ms | 35.5 ms |
| **4 KiB** | **0.11** | **24.7–24.8** | **35.8–36.5** |
| 16 KiB | 0.11 | 24.9 | 37.3–38.5 |
| 64 KiB | 0.14 | 25.7–26.7 | 47.1–47.3 |

**Merging.** Making rollups at merges, the same binary with and without (an environment switch in
a copy of the tree, in turn, load average 58–76): merges 7.6 s and 8.9 s of the 30 days' writing
(+17%), all of it 12.0 and 12.2 s. Joining inputs' rollups instead of making them anew from the
samples took that back and more (2026-10-23, the bench's two builds in turn, three rounds each, load
average 19–27): merges 12.0–12.9 s making them, 7.4–7.9 s joining them where the inputs keep them
(42,116 series' joined, 77,335 made: a cut's block keeps none), all of the writing 18.6–19.5 s and
13.2–14.5 s; the same store either way (4 blocks, 139.1 MB, rollups 4.2% of them). An appending
merge decoded every sample for its rollups; it now decodes those of the windows a cut fell inside
alone. The rollups first came from a series' samples decoded whole
(16 bytes a sample); built as the chunks are read instead, the most heap a merge holds over the 30
days' writing fell from 6.1 MB to 4.2 MB (counted by hyper-measure, the same either run).

### 12.17 Logs collected

`cargo bench -p kiln --bench collect` (`KOBS_LINES`, 300,000 lines a file): a file written, then
read whole by the collector's own code on one thread (its writes answered at once), the median of
three reads, allocations and page faults of one. 2026-10-18, AMD Ryzen 9 9950X3D2, Linux 7.2.9, a
machine other sessions share (rates over runs given as a range):

| | lines a second | MB a second | allocations a line | bytes a line |
| --- | --- | --- | --- | --- |
| text (an nginx access log) | 2.9–3.1 M | 405–444 | 9.0 | 571 |
| JSON lines | 680–767 k | 129–145 | 36 | 2,417 |
| CRI, JSON in it | 657–709 k | 150–162 | 39 | 2,718 |
| Java stack traces, `multi_line` (10 lines a record) | 11.9–12.2 M | 612–630 | 2.2 | 156 |
| the access log, an exclude and a mask | 2.5–2.9 M | 353–409 | 9.1 | 681 |
| the access log through kiln's nginx pipeline (§8.2) | 145–160 k | 20–23 | 75 | 3,731 |

A busy node writes some thousands of lines a second; one core reads hundreds of thousands. A text
line's nine allocations are its record's strings (its text, service, host, source, the file's path
and the tags, each a `String` of `kiln_obs`'s `Record`); a JSON line's thirty-six were mostly
serde_json's tree of it: its fields moved into the record rather than copied took 46 to 37, the
record's attributes sized once 37 to 36.

Then the tree went (`kiln_obs::json`, 2026-10-20): a JSON line's object read in place, its fields
borrowed from the line until each is an attribute, checked as serde_json checks it (a differential
test over 20,000 random objects, a fifth of them broken, takes and refuses alike), the lists of
fields kept from line to line, its message written into the line's own room, and a line that names
no message read for its level from the fields already read (serde_json parsed it again). The same
records come out (40,000 random log lines read both ways in a test; the attributes in the line's
order now, a number as written). The same machine (fedora, a Ryzen of 32 threads others share),
before and after, three reads each taken in turn, medians:

| | lines a second | allocations a line | bytes a line | page faults a read |
| --- | --- | --- | --- | --- |
| JSON lines, before | 559 k | 36 | 2,417 | 72 k |
| JSON lines, after | 780 k (+40%) | 18 | 1,063 | 83 k |
| CRI, JSON in it, before | 535 k | 39 | 2,718 | 67 k |
| CRI, JSON in it, after | 800 k (+50%) | 21 | 1,364 | 39 k |

Next: records handed to the batcher encoded, as rows (no strings a record at all): the batcher's
log queue then holds rows beside records, its acks counted across both. Done (below).

Through kiln's nginx pipeline a line costs some ten plain ones (the same run: 1.8 M lines a second
plain, 160 k parsed, on a loaded machine): perf puts 18% in the regular expressions' backtracker
reading a matching rule's groups (and a date's again), 14% in their lazy DFAs, 8% setting
attributes, the rest allocation. A rule is told by the DFA alone before its groups are read, each
group found by its place (a name looked up hashed it every match: 2.5%), and dates in UTC or with an
offset reckoned without the zone table's lock; a set of every rule, one pass for all, was tried and
was no faster.

Then a pass at its allocations and the date (2026-10-20, `KOBS_CASE=pipeline`, 100,000 lines, the
binaries before and after built alike and run in turn on one core of this machine, which others'
work held at a load of 20 to 30: instructions and allocations hold whatever the load, cycles and
rates less so):

| | allocations a line | bytes a line | instructions a line | cycles a line | lines a second |
| --- | --- | --- | --- | --- | --- |
| before | 75.0 | 3,735 | 65.5 k | 22.0 k | 237 k |
| after | 49.0 | 2,329 | 64.0 k | 19.9 k | 257 k |

An attribute's name is moved into the record rather than copied there again (a rule's names, the
URL's and the user agent's: a copy each); the value a rule reads is read in place, not copied out;
the user agent is read in place too, without a lowered copy to find a bot's words in; what
`nullIf("-")` drops is never made; the record's attributes get their room once; a rule's groups are
read into capture locations each thread keeps (`captures_read`), not into a `Captures` made and
dropped each line; a rule is asked for its groups at once (its lazy DFA finds the match first, and
`is_match` first scanned a matching line twice); and a date is read by its pattern's pieces, each
part taken as its expression takes it first (an alternative's first choice first, each part as
long as it may be), the expression asked only where that does not read the date whole (a
differential test: 39,000 texts made of thirteen patterns' pieces, real parts and near misses, read
alike both ways). The 25 allocations left past a plain line's are the attributes' own strings, a
record's as `kiln_obs` keeps one; the cycles are the backtracker's still, reading the access rule's
groups (Datadog's `(?>%{_method} |)` makes the rule no one-pass DFA's), which a matcher of grok's
own would read in one pass: not done.

The test found a date past what nanoseconds since the epoch hold (the year 2262 on, as a malformed
`yyyyMMdd` line can say: `71250101`) overflowed its reckoning: a panic in a debug build, a wrong
time in a release one, by the expression too. It is no date now.

Page faults rose, 10,054 to 13,862 a read: glibc hands the heap's top back between batches of
records (its trim threshold) and faults it in again, and the smaller allocations leave that top
free more often. With trimming off (`GLIBC_TUNABLES=glibc.malloc.trim_threshold=1073741824`), the
pages first touched fell, 6,220 to 5,439: kiln uses the system's allocator, and a long-lived server's
trimming is left as glibc does it.

Then records handed over encoded (2026-10-20). A text line no pipeline reads is written as its row
where it is read (§8.1): the row its record would write, from the line as it is, into a segment the
source's batch keeps (a block's text, 256 KiB, then another); the line's `String` comes back from
the write for the next line's text (a row only reads it; a line left out or blank gives it back too,
as does each line a `multi_line` record or a CRI or Docker line joins), and a mask writes the masked
line into room kept from line to line (as `replace_all` with `NoExpand` writes it: a differential
test over seven patterns, empty matches among them). A JSON line, a line a pipeline reads (each
pipeline's filter asked of the line itself) and a service check's run stay records. The batch keeps
what it holds in the order it was read, runs of records and segments of rows (`ingest::Logs`), and
the batcher queues it so: records written as rows as they come, segments whole, a write taking whole
segments by their raw bytes and joining them into one item, compressed whole, each segment's first
time chained again from the last before it (`kiln_obs::logs::record::put_rows_after`, a test over
runs of any times). Its compressed form is written into room the batch's builder keeps.

Then JSON lines the same way (2026-10-20): a JSON line's fields flattened into one text kept from
line to line (each key and its value side by side, their ends noted), which the pipelines' filters,
the row writer and the error's issue all read; a level's name read without lowercasing it into a
new string first (`severities`: one allocation a line, the search's readers and the pipelines'
status remapper too); a container's line joined in room kept too (two rooms a source: the
container's line and the line it joins into). The test that holds a row to its record's reads
4,000 JSON lines of every shape beside 4,000 text lines.

Measured with the bench's `KOBS_WRITTEN=1` (each offer through a batcher into its writes, each
answered at once, as a runner's thread takes them, and the bytes of the writes counted): HEAD's
collector with that mode patched in against this one, the same bench built alike, run in turn on one
core of fedora (others' builds at a load of 14 to 23 through it: rates as ranges, allocations and
bytes exact), three rounds each, 300,000 lines a file:

| | allocations a line | bytes a line | lines a second | bytes written a line |
| --- | --- | --- | --- | --- |
| JSON lines, before | 18.01 | 1,545 | 577–583 k | 15.6 |
| JSON lines, after | 0.01 | 343 | 0.87–1.20 M | 15.3 |
| CRI, JSON in it, before | 21.01 | 1,907 | 510–529 k | 16.1 |
| CRI, JSON in it, after | 0.01 | 410 | 0.80–1.11 M | 16.2 |
| text (an nginx access log), before | 9.01 | 1,299 | 1.22–1.24 M | 22.2 |
| text (an nginx access log), after | 0.01 | 477 | 1.94–2.63 M | 22.1 |
| the access log, an exclude and a mask, before | 9.14 | 1,293 | 1.04–1.07 M | 13.0 |
| the access log, an exclude and a mask, after | 0.03 | 410 | 1.58–2.12 M | 13.2 |
| Java stack traces, `multi_line`, before | 2.22 | 258 | 5.15–5.22 M | 2.0 |
| Java stack traces, `multi_line`, after | 0.02 | 128 | 6.99–9.58 M | 2.0 |
| through kiln's nginx pipeline, before | 49.03 | 3,853 | 148–150 k | 51.1 |
| through kiln's nginx pipeline, after | 48.04 | 3,185 | 146–197 k | 51.2 |

The collector alone (its offers dropped, as the tables above measure it), the same run: a JSON line
18.00 allocations to none, 760 k lines a second to 1.19 M; a CRI line 21.00 to 0.01, 798 k to 1.07
M; a text line 9.00 to 0.01, 2.09 M to 3.47 M. What is left is a record's where a pipeline reads its
line: its strings, the attributes the pipeline sets among them (48 a line through kiln's nginx
pipeline, §8.2). What is written is what was: a write's rows one item as before, compressed as well
(the first measure of this step wrote an item a segment, each offer's, and lost that).

Page faults with glibc's trimming as it is moved both ways (JSON 1.6–3.1 k to 1.1 k a read, CRI 2.1
k to 1.0 k; text 3.8 k to 5.5 k, through the pipeline 5–6 k to 26–43 k): the heap's top handed back
and faulted in again, as before (with trimming off, text 16.2 k both, the pipeline 9.8 k to 4.1 k).

Two things the runs found beside it. Metrics from logs learned how late records come from each
write's first item alone (a write held one item before): a write's records are one batch again,
whatever items hold them (§13, metrics from logs). And the full disk's scenario assumed the machine it
ran on (§13, a full disk).

Then a matcher of grok's own (2026-10-22). The profile of a line through kiln's nginx pipeline
(the collector's whole read, one core of adacachy): regex's backtracker 16.5%, its lazy DFA 9.9%,
the user agent read 9% (a substring search for each of some twenty words, each making its
searcher), attributes set 4% (`put`), their keys made by `format!` 3.5%, the rest allocation, 48 a
line. Each step below measured on the collect bench's pipeline case, the binaries before and after
built alike and run in turn on one core (the machine's load 10 to 37 through it; instructions and
allocations hold, rates less so):

| | lines a second | allocations a line | page faults a read |
| --- | --- | --- | --- |
| before | 282–292 k | 48.01 | 13.9 k |
| grok's own matcher (§8.2) | 403–421 k | 48.00 | 49 k |
| and a line refused by a byte every match holds | 445–450 k | 48.00 | 49 k |
| the user agent's words found in one pass, keys joined without `format!` | 542–544 k | 43.00 | 47 k |
| a line a pipeline reads written as its row where it is read, its strings' room kept | 585–615 k | 0.02 | 15.2–16.0 k |

grok alone (`cargo bench -p kiln --bench grok`: kiln's nginx rules, Datadog's, on 100,000 lines of
each kind, each line's attributes handed back once read, the median of five rounds):

| | ns a line, grok's own | ns a line, `regex` | allocations a line |
| --- | --- | --- | --- |
| a combined access line (the second rule) | 605–610 | 1,807–1,834 | 0 |
| a common one (the first) | 535–540 | 1,008–1,029 | 0 |
| an error line (the third) | 894–896 | 2,821–2,863 | 0 |
| a line no rule reads | 12 | 85 | 0 |

How it is held to `regex`: 3,000 random patterns of what rules are made of (literal text, classes
Unicode's and ASCII's, greedy and lazy runs, counted repetitions, alternatives, groups optional and
nested, word boundaries and anchors), each on texts it would match, those changed a character, and
random texts of the characters they name (Unicode's spaces, digits and letters among them): 89,344
values, 28,371 matching, every group where `regex` puts it (208 patterns of the 3,000 are
`regex`'s: a loop that may match nothing). Datadog's access rule on lines cut short, changed and
doubled; kiln's access and error rules on 18,000 lines of every part's shapes, by grok's own and by
`regex` alone: the same rule, the same attributes. Searches that would take a value's square (`(?:a+)+b.`
on 20,000 `a`s) fail in milliseconds. The user agent's one pass against the twenty searches on
20,000 random agents; integers' text against `parse` and `to_string`; the severity of a line's first
words (below) against its reading before on 50,000 lines.

Page faults rose with the matcher, 13.9 k to 49 k a read: with glibc's trimming off
(`GLIBC_TUNABLES=glibc.malloc.trim_threshold=1073741824`) they are 5.5 k and 5.3 k, and the matcher's
gain +60% (291 k to 467 k): `regex`'s lazy DFAs' caches, long-lived, had held the heap's top, which
glibc now handed back between batches and faulted in again. With no allocation a line, 15–16 k.

The row where the line is read: a line a pipeline (or the scanner) reads was a record, carried in its
batch to the offer, run through there, written as its row by the batcher, and dropped. It is made
where it is read now (its strings taken from those the thread's records handed back), run through
every pipeline and the scanner, given its issue, written as its row into the batch's segment, and
its strings handed back (a thread keeps 1,024 at the most, 1 MiB of room, none longer than 4 KiB).
The processors take theirs there too (grok's names and values, the URL's and the user agent's parts,
a category's name, a builder's text); what one replaces is handed back. A JSON line the scanner reads
for an email each: 20 allocations a line to 5.

A line's level read from its first words passes a word with no `=` whose first letter or digit is
not a capital at once (only capitals are a level there): plain text lines (an nginx access log, no
pipeline) 5.54 M to 6.15 M lines a second, the access log with an exclude and a mask 3.27 M to
3.57 M. JSON and CRI lines run 2% more cycles at 0.15% fewer instructions (fewer branch and
instruction-cache misses): the code's layout, nothing they do.

What is left through the pipeline: the matcher 18% (an interpreter's dispatch, spread across its
instructions), a record's ~47 strings each copied in and handed back 25% (the time falls on the string's
reload as its copy returns: some 20 cycles a string), the row's encoding 5%. Then a record of one
buffer, its attributes' text side by side (the processors, the search's filters and the scanner
reading it; §8.2), measured against the code before it next.

### 12.18 Metrics from logs

2026-10-19 on this machine, loaded (load average 50–130: other sessions compiling), so compared
with each other: `KOBS_LOGS=<journal>:<pods> cargo bench -p kiln-obs --bench logmetrics`. The records
of §12.8 (194,690 over 48.7 hours, moved to end now), applied as 195 entries of 1,000; then every
window counted once, one metric (`status:error` by service and host) and five at once (every record
by service; errors by service and host; `service:kube status:warn`; `"connection refused"` by
service; Argo CD's `@grpc.time_ms` as a distribution with percentiles by its gRPC service), the
median of five rounds:

| | records/s | allocations a record |
| --- | --- | --- |
| applying, windows not noted | 680,000–708,000 | 0.055 |
| applying, each batch's windows noted | 677,000–710,000 | 0.055 |

| counting every window | windows | a window | records/s | a record | allocations a window |
| --- | --- | --- | --- | --- | --- |
| 48.7 hours (1.1 records a second), one metric | 17,549 | 0.19–0.22 ms | | | 20.5 |
| 48.7 hours, five metrics | 17,549 | 0.23–0.31 ms | | | 61 |
| the same records in 100 s (19,500 a window), one metric | 11 | 3.4 ms | 5.2 M | 193 ns | 38 |
| the same records in 100 s, five metrics | 11 | 7.1–9.1 ms | 2.0–2.5 M | 400–513 ns | 1,040 |

Noting the windows costs applying nothing measurable. A sparse window costs what reading the
block it falls in does (a block of 256 KiB holds some twenty minutes of these records, read again
for each of its windows): 0.2 ms each ten seconds. A dense one costs its records: at 100,000 records
a second, five metrics take some 4–5% of a core. Of that (perf), a third is checking each row's
text is UTF-8 as it is read, the rest reading rows and evaluating the searches; a distribution's
allocations are its series' keys, a window's, not a record's.

### 12.19 A dashboard's widest query

The soak's slowest dashboard query (§13) was its own reading, not a wait: with each query's time
told apart (`queued_ms`, `waited_ms` and `took_ms` in its answer), a 20-minute soak's
`sum:load.v{*} by {host}` took p50 648 ms and p99 1.17 s, its slowest hundredth waiting 0.00 ms for
the store and 0.12 for a thread; the one-series query's slowest waited 4.3 ms. It reads 4,000 series
written 50 times a second over 15 minutes: 180 M samples. `cargo bench -p kiln-obs --bench wide`
builds that store (cut each minute, merged as maintenance merges) and times the query, median of
nine, 2026-10-19 on this machine (32 threads, load average 39–150 from other sessions):

| | median | a sample |
| --- | --- | --- |
| before | 1,527 ms | 8.5 ns |
| a run of repeated samples decoded a word at a time | 508 ms | 2.8 ns |
| and its series read on helper threads | 74 ms | 0.41 ns |

A sample that repeats the one before (its interval and its value: a regular scrape of a gauge that
holds) is two zero bits; a run of them is counted with one leading-zeros instruction and its samples
written in a loop (`ChunkRef::decode_into`; a test holds it to the sample-by-sample decoder on runs
of every length at every place, to the bit). Then perf put the time in the decoder (31% stepping,
21% reading values, 12% in its loop), and the series are independent: a query reads them on its own
thread while that is quick (2 ms), then takes helper threads, shared by every query, and reads the
rest a round at a time (64 series a thread), each round's values added into their groups in the
series' fingerprints' order, as one thread adds them: the same bits on any number of threads (a test
of seven queries on 1, 2, 3 and 7 threads, sums twelve orders of magnitude apart).

How many helpers was measured where it costs: the soak's writes (§13; 200,000 points a second, 10
minutes a run), each request told apart by whether the by-host query was reading while it was taken.
fedora (an i9-14900HX: 8 cores of two threads and 16 of one, 32 threads; other sessions' work beside,
5 to 14 cores busy), runs alternated so the machine's other load falls on both, 2026-10-19:

| helpers | runs | by host p50 | by host p99 | writes beside it, p99 / the rest's |
| --- | --- | --- | --- | --- |
| none | 11 | 175–257 ms | 418–699 ms | 0.9–1.3× (1.08–2.20 ms) |
| 15 (half the threads, less one) | 8 | 31–84 ms | 84–1,482 ms | 1.7–3.9× (1.94–5.61 ms) |
| 7 (a quarter, less one) | 4 | 42–72 ms | 115–227 ms | 1.1–1.75× (1.72–2.69 ms) |

Without helpers the widest query costs the writes beside it nothing; with them it reads in a quarter
of the time, and the 2–3% of writes taken while it reads wait longer in their tail. Fifteen buy
little over seven (`cargo bench -p kiln-obs --bench beside`, the 4,000 series of 10 minutes: 28 ms a
query on 15, 38 on 7, 219 on none) at twice the cost to the writes beside them: the reading is bound
by memory past some eight threads, and every core then carries a helper beside the threads taking
writes. So `query_helpers` defaults to a quarter of the cores the server may run on, less one (none
on four or fewer); helpers run ten nice levels down (a CFS weight of 110 against 1,024), which only
matters where cores are contended: with idle cores the 15 helpers' tail was alike niced or not (1.7–
2.4× against 2.0–3.9×, three runs each). Memory: helper threads raise the server's anonymous memory
once (the bench: 23 MiB to 42, the C library's arenas of the threads that read), then hold it (no
page faults a query after the first); every soak run held its level.

Then a query of metrics holds the store a part at a time (2026-10-20). The writes above are
requests taken; what applies them takes the store's exclusive hold, and a query held its shared
hold for all its reading, so an apply that came meanwhile waited for the whole query, and every
reader after it waited behind the apply (the lock lets a waiting writer in before new readers): a
dashboard watched by two, the one-series query waited for the wide one's reading. Now a query finds
its series under one hold, then reads them a round at a time (`ROUND`, 64 series a thread), each
round under a hold of its own (`query::eval::Lease`), so a write waits for one round at the most.
Retention numbers the series anew (an id is another series' after), so each round checks the
numbering it began with (`Store::generation`, a number no other numbering of the process had); a
query that finds it changed is asked again, twice at the most, then once more holding the store
throughout, so it always ends. Each series is read whole within one round, as its samples are then;
the operands of a formula (and a distribution's sums and counts) each as of their own rounds, as
Datadog's are (no query of it reads a snapshot). A thread never takes a part's hold within another
(it would wait behind a writer that waits for the first): asked to, it refuses, an error and not a
hang. The API's queries and heatmaps read so (`eval_shared`), as do the monitors of metrics
(`monitor::evaluate_shared`) and the SLOs, their monitors and their pages (`slo::status`, `burn_rate`
of an `eval::Source`). Tests: every kind of query both ways (gauges, counts, counters, a rate, a
rollup, weighted, a percentile across series, a distribution's percentile, mean, count and threshold
count, logs in a formula, heatmaps), on one thread and on three, to the bit; the series numbered
anew between two parts (asked again once, the new numbering's answer); queries beside a cutoff
passing a series at a time 299 times (every answer of one numbering, none of a series' values under
another's name); and a write beside a query of 2,000 series read over and over waiting a third as
long at the median as beside it held throughout, half as long at p90 (both measured alike, in turn).

Measured in the soak (§13), its dashboard now watched by two at once (each query on a thread and a
connection of its own, every 2 s), the build before against this one, two runs each in turn on
fedora (others' work at a load of 3 to 12), 2026-10-20:

| | one series p99 (most) | its wait for the store, most | by host p50 / p99 | a request p99 | a dashboard query p99 |
| --- | --- | --- | --- | --- | --- |
| held throughout | 68.3 ms (299), 1.93 (2.42) | 298 ms, 0.00 | 41.7 / 349 ms, 44.8 / 89.0 | 3.87 ms, 1.78 | 327 ms, 85.0 |
| a round at a time | 1.20 ms (1.45), 1.25 (4.24) | 0.00 ms, 0.00 | 32.2 / 54.5 ms, 18.0 / 38.4 | 1.18 ms, 1.36 | 53.9 ms, 30.6 |

The one-series query met the wide one's reading in the first run held throughout (298 ms waited
behind a write behind it, its p99 68 ms) and not in the second (the two readers wait 2 s after each
answer, so whether one comes while the other reads hangs on how they began): read a round at a time,
it waited for nothing in either. The writes taken meanwhile waited less in their tail (p99 1.2–1.4 ms
against 1.8–3.9), and the wide query itself read faster in both (its p50 18–32 ms against 42–45),
which was not foreseen and is not yet explained. Every run stored every point once, its replicas
agreeing, its memory level.

### 12.20 DogStatsD

`cargo bench -p kiln-obs --bench statsd`: datagrams as Datadog's clients pack them (1,432 bytes over
UDP, 8 KiB over a Unix socket); a service's traffic (by route, method and status: four in ten a
count, four a timer, a gauge, a set or a distribution; six tags each, Datadog's unified service
tagging's and the route's), and many contexts. Once its contexts exist, a message (fedora,
2026-10-19):

| traffic | contexts | ns a message | allocations a message | page faults | a bucket's flush |
| --- | --- | --- | --- | --- | --- |
| a service, 200 routes, UDP | 1,817 | 238 (550 before) | 0 | 0 | 854 ns a context, 0 allocations (1,663 and 1,200) |
| a service, 200 routes, Unix | 1,817 | 246 | 0 | 0 | 904 ns |
| a service, 5,000 routes, UDP | 43,594 | 394 (840) | 0 | 0 | 109 ns (311 and 28,295) |
| 100,000 contexts (counts) | 100,000 | 171 (398) | 0 | 0 | 19 ns |

What moved it (perf, then the bench again): a datagram's UTF-8 checked once rather than a line at a
time, its lines split by `memchr` rather than a byte at a time; a tag hashed once (its key and value
had been hashed apart and mixed); a histogram's samples sorted once a flush (each quantile had
sorted them) and its values worked out in a buffer kept from flush to flush. What is left is spread
thin: hashing a sixth, splitting a sixth, parsing a tenth, and at 100,000 contexts the context's
lookup, cache misses mostly.

Live (`observe_statsd.rs`, a release build, fedora): four senders flood a listener's UDP socket for
20 s, 13 counts a datagram over 10,000 contexts. Every datagram the listener read was stored exactly
(its counts summed in kiln equal what it read):

| messages a second | datagrams | read | the server, cores while it came |
| --- | --- | --- | --- |
| 500,000 | 768,909 | all | 0.20 |
| 2,000,000 | 3,075,770 | all | 0.48 |
| 5,000,000 | 7,687,608 | all (in another run, 98%: the system dropped 2% before they were read) | 0.53 |

Five million a second is one socket's thread's edge on this machine, past what a host's DogStatsD
sees; a listener with more addresses reads each on a thread of its own.

### 12.21 Prometheus's remote write

`cargo bench -p kiln-obs --bench promwrite`: requests as Prometheus sends them (2,000 samples, its
`max_samples_per_send`, and 10,000), node exporters' series (CPU by core and mode, network by
device, file systems), a sample each, compressed by a reference Snappy encoder (the bench checks the
reader gives back what it compressed), the receiver's keys made by the requests before (fedora,
2026-10-19):

| request | bytes | Snappy's | unzipped, ns a sample | decoded, ns a sample | allocations | page faults |
| --- | --- | --- | --- | --- | --- | --- |
| 2,000 samples, 50 hosts | 247,796 | 27,548 | 30.8 | 350 | 7 a request | 0 |
| 10,000 samples, 500 hosts | 1,244,539 | 138,485 | 16.5 | 416 | 7 a request | 0 |

Each sample here is a series of its own (the costly case: its labels read, sorted and its key
found); one core takes some 2.5 million samples a second this way.

### 12.22 Zstandard

`cargo bench -p kiln-obs --bench zstd`: frames the reference CLI (zstd 1.5.7) wrote, unzipped by a
decoder and into a buffer both kept from one to the next (as the Agent's intake keeps one a
thread), beside the reference's own decompression (`zstd -b`) of the same content on the same
machine (adacachy, AMD Ryzen 9 9950X3D2, 2026-10-19):

| frame | content | kiln, MB/s | the reference, MB/s | allocations | page faults |
| --- | --- | --- | --- | --- | --- |
| text, two blocks, level 3 with its checksum | 179,250 | 1,720 | 5,089 | 0 (33 before) | 0 |
| the same, level 19 | 179,250 | 2,782 | 7,644 | 0 (38) | 0 |
| the same, --fast=5 | 179,250 | 1,827 | — | 0 (19) | 0 |
| binary, level 3 | 34,608 | 9,212 | 19,158 | 0 (10) | 0 |

A third to a half of the reference's speed, in safe code. What moved it (from a quarter, and 19
to 38 allocations a frame): a bitstream's bits read a word at a time where eight bytes are there
(they had been gathered a byte at a time), and a decoder that keeps its tables (the predefined ones
made once), its literals and its scratch from frame to frame. A few KB of an Agent's payload is a
few microseconds.

### 12.23 Traces

`cargo bench -p kiln-obs --bench traces` (fedora, 2026-10-19): OTLP trace requests as an SDK's batch
span processor exports them (512 spans, its `max_export_batch_size`; and 4,096), of a web service:
its servers' spans, its HTTP clients' and its database calls' (a resource of ten attributes, each
span five to eight of its own, one in twenty an exception event), decoded into span rows with the
decoder's room kept from request to request:

| request | spans | bytes | rows, B a span | LZ4, B a span | decode, ns a span | allocations a span | page faults |
| --- | --- | --- | --- | --- | --- | --- | --- |
| a web service's | 512 | 168,534 | 685 | 41 | 628 | 0.0020 (one a request) | 0 |
| the same | 4,096 | 1,346,671 | 685 | 41 | 614 | 0.013 | 0 |

A span's row repeats its resource's attributes (685 bytes against OTLP's 329), which LZ4 takes back
(41 bytes a span in a write: 17 to one), so a span costs the group less than its request did. Some
1.6 million spans a second a core.

A Datadog Agent's payloads (`kiln_obs::ddtrace`), indexed as Agent 7.84 sends them: a ddtrace Flask
app's traces of ten spans each (its request's, Flask's own, the view's, a database call's; the
request's span some twenty tags, one trace in twenty failing), each payload's strings once in its
table:

| payload | spans | bytes | rows, B a span | LZ4, B a span | decode, ns a span | allocations a span | page faults |
| --- | --- | --- | --- | --- | --- | --- | --- |
| an Agent's | 520 | 42,446 | 270 | 21 | 532 | 0.0019 (one a payload) | 0 |
| the same | 4,100 | 329,614 | 270 | 20 | 534 | 0.0005 | 0 |
| a tracer's own (`/v0.5/traces`, kiln its Agent) | 520 | 104,881 | | | 488 | 0.0019 | 0 |
| the same | 4,100 | 820,881 | | | 491 | 0.0005 | 0 |

A tracer's own payload is read twice a trace (its spans' ids, services and priority first, so a
trace's top-level spans and its keeping are known before its spans are written), and still
reads faster than OTLP's: its strings are in one table, each a reference. Its spans' counting by the
runner's concentrator (`kiln_obs::ddtrace::Concentrator::add`, two spans of a trace's ten: its
request and its database's) is 133 ns a span counted, nothing allocated once its group is made.
Counting a span by its end and a late one in the oldest bucket not yet due (§8.7) cost nothing
measurable: the build before and this one in turn, ten times each on one core of a machine others
were building on, 142 and 143 ns at their fastest, the median of their ten differences 0.5 ns.

Its stats (`kiln_obs::ddtrace::stats_into`): a grouped stat a service's resource of 50 hits, its
sketch twenty bins, each written as some 23 series (its counts, its durations' sum, least, most and
buckets) through the receivers' keys, a series seen before found and not made: 6.0 µs and 0.2
allocations a grouped stat (of 2,000 in a payload; 0.97 of 100), no page faults; the first payload,
its series made, 13 µs a group. Written first as a series and its points each (`Vec` a series),
the same took 14.5 µs and 225 allocations a group. An Agent's 2,000 groups every ten seconds are
12 ms of a core.

A service's figures over a long range (`kiln_obs::apm::red`, by resource): a week of 20 groups'
trace metrics (280 series, 16.9 M points, a block a day):

| range | read from rollups | from every sample |
| --- | --- | --- |
| an hour | 1.85 ms | 1.93 ms |
| a day | 29.5 ms | 29.5 ms |
| a week | 34.4 ms | 216.7 ms (6.3×) |

The last day is read as samples either way: it is in the newest block, which a cut writes without
rollups (it decodes nothing it need not) and no merge has taken yet; the week's other six days are
read as their hours' sums.

The service map (`kiln_obs::apm::MapBuilder`, each span's group of the trace metrics found as it
is read; `benches/traces.rs`, `map`): a fleet of 100 services each serving three resources and
calling three peers (its database, a cache they share, the next service), 200,000 spans (the most
the map reads, `MAP_SPANS`) in 77 ms, 387 ns and 0.016 allocations a span, its 300 edges each seen
by its group. Reading a span's attributes in one pass, not one each for its kind, duration,
parent, peer and top-level marks, took it from 580 to 387 ns a span (five runs each in turn on one
core of a quiet machine, every one within 1% of its median; on a loaded one, 1,165 to 800). Its
figures from the trace metrics (`kiln_obs::apm::figures`, 1,800 series: each service's entries and
its calls by peer, a read of the metrics a calling service): 3.2 ms over 15 minutes, 10.0 ms over an
hour.

A trace by its id, in a span store of 1,024,000 spans (341,334 traces) in 2,000 blocks: 1.6 ms, its
own block and a median of 20 others read (p99 31, the most 37): the 1% each filter is sized for,
whichever trace. Before the filters were salted by their block (§8.7), the median was 7 and the most
654: filters of one size set the same bits for the tokens every block holds (the service, `server`,
the names), so a query whose places fell on them passed most of them.

### 12.24 Error Tracking

An error's issue as it is taken (`kiln_obs::errors::span_issue`; `benches/errors.rs`): its
message's words, its stack's frame, its id, of each language's stacks as its runtime wrote them
(`fixtures/errors/`, their messages a variable one's), on one core of fedora, the first build and
the allocation pass's in turn, three runs each (the medians; runs within 2% of each other but one
of the first build's, 5% slower):

| stacks | cases | bytes a stack | ns an error, first | ns an error | allocations an error, first | allocations an error |
| --- | --- | --- | --- | --- | --- | --- |
| Python | 5 | 613 | 1,311 | 862 | 10.2 | 0 |
| Node | 5 | 257 | 1,159 | 780 | 11.6 | 0 |
| Go | 4 | 214 | 1,184 | 1,070 | 11.2 | 0.2 |
| Java | 5 | 859 | 1,622 | 909 | 13.8 | 0.2 |
| Ruby | 5 | 306 | 1,051 | 641 | 12.2 | 0 |
| PHP | 8 | 277 | 1,153 | 818 | 11.0 | 0 |
| .NET | 6 | 407 | 1,375 | 1,005 | 12.0 | 0.2 |
| Rust | 4 | 5,597 | 2,828 | 1,011 | 15.2 | 0.5 |

The pass: a stack is walked once and its frames kept nowhere, innermost first (Python's from its
end, a frame of Python's a line of its own), to the first of the application's (Go's to its end,
whose frames a panic's begin again); the id is hashed as its words, file and function are read,
none of them made whole; a function is copied only where a compiler numbers it (a lambda's, a
closure's) or a PHP closure names its place, the 0.2 to 0.5 allocations left. Ids are unchanged:
every fixture's, of a span and of a log, is pinned (`issue_ids_are_frozen`), and the walked frame
and streamed id are checked against the listed frames and the id of the parts made whole. Only an
error is read so (a span in error, a log of an error's status): of a stream of spans one in a
hundred failing, some 9 ns a span.

What the server does with issues, at Error Tracking's bound: 10,000 issues, each erring in three
series (two versions of an env, one of another) in one window of ten seconds in twenty over a day,
the last two hours in the head as the server holds them (kiln-obs's `benches/errors.rs`, kiln's
`benches/issues.rs`; one core of fedora). The metric read (`kiln_obs::errors::seen`, and
`seen_of` for some issues alone):

| read | twenty minutes | an hour | a day |
| --- | --- | --- | --- |
| every issue's series | 19.0 ms | 19.7 ms | 75.3 ms |
| one issue's | 0.002 ms | 0.002 ms | 0.008 ms |
| ten issues' | 0.016 ms | 0.017 ms | 0.062 ms |
| a hundred issues' | 0.20 ms | 0.21 ms | 0.69 ms |
| a thousand issues' | 2.08 ms | 2.22 ms | 7.04 ms |
| all 10,000, each's | 20.8 ms | 22.0 ms | 77.0 ms |

A series costs 4 allocations read with the whole (its tags' text), 7.3 read by its issue (the
issue's selection besides). Read by issue, an issue costs 1.03 to 1.09 times its share of the whole,
so a monitor reads the series of the issues it watches alone until they are nine tenths of the
issues kept: a monitor of new issues, watching a few, reads them in microseconds where every series
would cost it 19 to 75 ms an evaluation. All of them on the metric written as blocks alone, the
last twenty minutes cost 51 ms: the head is the cheaper read. The registry, handed the rows read:

| the registry's work | time | allocations |
| --- | --- | --- |
| its read each ten seconds, every issue known | 5.4 ms | 0 |
| a monitor of impact, by issue (10,000 groups) | 4.2 ms | 30,018 |
| a monitor of impact, by issue and env (20,000 groups) | 6.5 ms | 100,019 |
| a monitor of impact of a service in an env (100 groups) | 2.4 ms | 311 |
| a monitor of new issues, none new | 0.03 to 0.06 ms | 0 |

Two passes cut these: the read sorts the rows it is handed in place and reads an issue's as a run,
an issue known found by its id as it is (21,298 allocations a read to none); a monitor's groups are
kept by their tags' values as they are, their text made once, for the answer (4 allocations a group
to 3). At the bound, the registry's read every ten seconds holds the store's read lock some 25 ms (the
metric's read and its own).

### 12.25 Datadog's dashboards

Datadog's own dashboards, every one its integrations carry (`DataDog/integrations-core`'s
`assets/dashboards`, 688 files at tree `fdcc0bc`), imported as kiln's and given back as Datadog's
(`observe::ddash`; its test `datadog_s_integration_dashboards_are_taken_and_come_back_the_same`,
`KILN_DD_DASHBOARDS=<their folder>`): 678 taken, 9 of the files not JSON as they stand. Each is
kept as kiln keeps a dashboard (its checks passed), given back as Datadog's, and taken again the
same: every widget's kind, title, search, text and queries (their syntax trees compared), its
columns exactly and its rows within one (Datadog's rows are coarser than kiln's). Of their 21,424
widgets:

| widgets | count | share |
| --- | --- | --- |
| drawn as Datadog draws them | 19,459 | 90.8% |
| drawn otherwise, as said | 715 | 3.3% |
| kept as a note saying what they were, not drawn | 1,250 | 5.8% |

Drawn otherwise: an image of the web as a link to it (325), a geomap as a top list of its places
(235), a bar chart, a change and a host map as top lists (118), a treemap as a pie (37). Not
drawn, nearly all data kiln does not hold: Cloud SIEM's security signals (762), Datadog's own
images, its integrations' logos (165), cloud costs
(97), network paths and device flows (142), database queries, LLM observability and processes (16);
widgets kiln has no form of (flame graphs, Sankey diagrams, topology maps, Vega's wildcard, an
iframe, a scatter plot: 25); lists kiln does not keep (infrastructure, log patterns, trace streams
and two more: 31); requests of a form kiln does not read (7). Queries kiln cannot read: 4
(`reduce(…, 'last')` over `sum(…, { * })`, 3, a form no page of Datadog's describes; a filter of
Datadog's own that is not one, `{(status:400 OR s) …}`, 1).

The first import drew 89.0% and could not read 60 queries. What moved it, each checked against a
reading of its own (§6): Datadog's boolean filters (a differential test, 3,000 random filters in
every spelling against each series' tags read directly); `max` and `min`; arithmetic without
spaces (a number outside a filter ends where an operator starts: before, `2*avg:cpu{*}` lexed as a
metric named `2*avg:cpu`, a glob matching nothing, and answered nothing without an error); a log
search's value lists (before, `@evt.name:(a OR b)` was read as the attribute empty and a word of
the message, a wrong answer and no error); `status:*`; `.fill(0)`: 89.9%, 12 unread. Then service
checks (§9.4): the 180 check status widgets drawn; and distributions' threshold counts
(`count(v: v>=0):…`) and a rollup's seconds alone (`.rollup(10)`): 8 more, 90.8%, 4 unread. Then a
distribution's newer request, one `query` rather than a list (`request_type: histogram`): 4 more.

An import costs 105 to 112 µs a dashboard (three runs each, a release build, fedora), from 133 to
157 µs: the lexer tells a filter's `*` from a product by the braces it has open, counted as it goes,
where it scanned its tokens back each time.

### 12.26 Log archives' encoding

`KOBS_LOGS=<journal> cargo bench -p kiln-obs --bench archive` (adacachy, 2026-10-21, a release
build, one thread pinned to a core; the machine was busy, so throughput is per second of CPU, which
its load does not inflate): the journal of §12.8 as 325,068 records (50.2 MB of lines) cut into seven
logs files as a busy store cuts them, each file read back and written as Datadog's archived events,
then gzip'd as an object holds them.

| | events / CPU-s | JSON MB / CPU-s | allocations an event | gzip's ratio | bytes kept a byte of line |
| --- | --- | --- | --- | --- | --- |
| read, written as events | 1,354,450 | 426.0 | 0.0001 | | 2.04 (the JSON) |
| … with tags | 1,250,262 | 458.5 | 0.0001 | | 2.38 (the JSON) |
| … gzip'd, level 1 | 738,791 | 232.4 | 0.0003 | 9.62 | 0.212 |
| … level 3 | 433,424 | 136.3 | 0.0003 | 14.18 | 0.144 |
| … level 6 | 328,352 | 103.3 | 0.0003 | 15.17 | 0.134 |
| … level 9 | 255,959 | 80.5 | 0.0003 | 15.55 | 0.131 |
| the archiver's path: searched (`*`), written, level 6 | 342,177 | 107.6 | 0.0003 | 15.17 | 0.134 |

An event is read and written in some 740 ns and allocates nothing (its attributes listed on the
stack, its date and tags written in place, where at first each event allocated a vector and a string
or more); gzip is three quarters of the cost at level 6. Level 6, zlib's default and Datadog's
writers', keeps 15.2 times less than the events' JSON (a seventh of the lines' own bytes); level 3
would take a quarter less CPU and keep 7.5% more, level 9 28% more CPU to keep 2% less. An archive is
written once and kept for years: level 6. At it one core writes the events of 53 MB/s of log lines
(the corpus' lines are 154 bytes), half what the store applies at its most (106 MB/s, §12.8): so the
archiver works on three files at once (`IN_FLIGHT`), each encoded on a thread of the blocking pool.

### 12.27 Rehydration's reading

The same bench, its last rows (adacachy, 2026-10-21, idle; one thread pinned to a core): the
corpus' events as §12.26 writes them (no tags, 103 MB of JSON), read back as a rehydration reads
them (§10.6).

| | events / CPU-s | JSON MB / CPU-s | allocations an event |
| --- | --- | --- | --- |
| each line into `serde_json`'s tree (no record made) | 1,354,450 | 426.0 | 20.24 |
| each line into its record (`archive::read_event`) | 2,500,523 | 786.4 | 0.0003 |
| a rehydration's reading but its store's: gunzip'd as it comes, read, searched (`*`), written as rows, built into batches | 1,805,933 | 568.0 | 0.0010 |
| … searched (`status:error`: a few kept) | 2,321,914 | 730.3 | 0.0005 |

A rehydration first read each line into `serde_json`'s tree and then made its record: the tree
alone takes 740 ns and 20 allocations an event. `read_event` reads the line in place
(`kiln_obs::json::object`: each field borrowed, a string decoded only when it holds an escape),
writes its fields over the strings of one record kept from event to event, and lets an event's
attributes past the last one's go into a pool the next take from: 400 ns and nothing allocated. A
first cut kept 2,000 records (a batch's) and allocated 0.24 times an event, each record's strings
growing to what the events that came to it held; so a kept event is written as a row at once
(`Builder::log_rows`) and one record serves them all. Its usual date (`2018-05-15T14:31:16.003Z`) is
read in place, any other form by jiff, the two the same to the nanosecond over i64's range of
years (a test of 200,000). One core reads, searches and builds 1.8 million events a second as
gzip'd objects come (568 MB/s of their JSON, 278 MB/s of the lines they were); the store applies
106 MB/s of lines at its most (§12.8), so what bounds a rehydration is its view's store and the bytes
its store sends, not its reading.

### 12.28 The Sensitive Data Scanner

`KOBS_LOGS=<journal> cargo bench -p kiln --bench scanner` and `cargo bench -p kiln --bench
collect` (fedora, 2026-10-21, a release build, one thread pinned to a core). The scanner's own bench
reads the journal of §12.8 as 367,666 records (89.5 MB of lines) through one group of every standard
pattern but the network addresses (28 rules, emails hashed), of those without keywords alone, of
those with keywords alone, and of one pattern of one's own (`order-(?P<v>[0-9]{6})`): the check a
reader makes of a line before it makes a record of it (`Scanner::reads`), and a scan of a copy of
each record (the copy's time taken out); 0.41% of the records may hold something, 0.07% are changed.

| | check, ns a record | allocations a record | scan, ns a record | allocations a record |
| --- | --- | --- | --- | --- |
| the library: first cut (bounded runs from any word) | 2,689 | 0.51 | 2,670 | |
| the library | 513 | 0.003 | 514 | 0.013 |
| its patterns without keywords | 369 | 0.003 | 358 | 0.011 |
| its patterns with keywords | 134 | 0.0002 | 103 | 0 |
| one pattern of one's own: first cut (a set of one) | 394 | 0 | 338 | 0 |
| one pattern of one's own | 77 | 0 | 43 | 0 |

The first cut's set thrashed `regex`'s lazy DFA: an email's local part (`{0,63}`), its labels
(`{0,61}`), an IBAN's body (`{11,30}`) and a URL's scheme (`{1,20}`) are runs that may begin at any
word, every count a state of every word's, and a cache cleared over and over is a cache allocated
over and over (the 0.51). Unbounded, with `\b` and `\d` ASCII's (a Unicode word boundary sends the
lazy DFA to a slower engine on the first byte past ASCII), the set runs at some 2 ns a byte of a
record's texts, with no allocation; its cache made larger (8 MiB) took the rest. A text shorter than
any match of a group's rules is not looked through (`regex-syntax`'s minimum length). A set has no
prefilter: a set of one pattern ran its automaton over every byte, so a group's one rule without
keywords runs as a single `Regex`, whose literal (`order-`) is looked for first, by `memchr`: a
record without it is passed in 77 ns. A rule's captures are asked for only where its
pattern (or the set) has found a match: a `captures_iter` over every text made its captures' room
each time.

Through the collector (300,000 lines, the median of three reads; §12.17's bench):

| | lines a second | MB a second | allocations a line |
| --- | --- | --- | --- |
| an nginx access log | 4,202,730 | 594 | 0.01 |
| … scanned by the library, nothing found | 1,946,209 | 275 | 0.01 |
| JSON lines, an email each | 1,703,532 | 249 | 0.00 |
| … scanned, each email hashed | 644,251 | 94 | 20.00 |

A line the scanner finds nothing in keeps the collector's fast path (its row written from the line,
no record made: 0.01 allocations a line): the check costs some 276 ns of a 141-byte line. A line it
changes is made a record, scanned, its email hashed (HMAC-SHA256 under the kiln's key, written in
place) and its record tagged: 20 allocations, of which the scan's are its two new tags' strings and
the set's list of which patterns matched; the rest are the record's own, as a pipeline's (48 a line,
kiln's nginx pipeline). A busy machine's 10,000 lines a second cost the library's check 0.3% of a
core (nginx's lines) to 0.5% (the journal's).

### 12.29 DORA's ledger

`KOBS_DORA_DIR=<a folder on disk> cargo bench -p kiln --bench dora` (adacachy, 2026-10-22, a
release build pinned to one core, the machine's load some 50 on 32 cores): a large kiln's history,
200 services in two environments, a deployment of each every two to six hours for 400 days
(957,452 deployments, 3.1 million commits), one in fifteen a rollback to an earlier commit, one in
thirty reverting an earlier deployment's commit (63,643 rollbacks and 25,516 reverts found).

| | first cut | now |
| --- | --- | --- |
| a deployment taken: kept, what it marks found and marked | 591,570 ns, 1,150 allocations | 3,048 ns, 20 allocations |
| held, a deployment | 1,509 bytes (resident) | 500 bytes (live) |
| the four keys, 30 days, every service (72,081 deployments) | 22.3 ms | 2.5 ms |
| the four keys, 30 days, one service (355) | 20.2 µs | 9.9 µs |
| the four keys, a year, every service (875,845) | 314 ms | 164 ms |
| its services, environments and teams | 734 ms | 0.02 ms |
| a list's search, a deployment | 246 ns | 29 ns |
| the ledger read as the server starts (539 MB, a line each) | | 2.3 s |
| a deployment taken whole, its line synced (NVMe, btrfs) | | 5.5 ms |

The first cut looked for a rollback's target through the whole of a service and environment's
history, comparing what each deployment deployed as a new lowercase string: 1,150 allocations and
0.6 ms a deployment, growing with the history. A count is now kept of each commit (or version) a
service and environment deployed, so the target is looked for only when what a deployment deploys
was deployed before, among the latest 1,000, comparing hashes. The page's services, environments
and teams were a sorted set of every deployment's names each time it asked: they are now what the
ledger keeps of them. A deployment was a struct of a dozen strings in a hash map, its id held three
times: its shared names are numbers now, its id one allocation, its commits' keys one slice, the
rows in a slab (a map of rows reserved twice their room, rounded to a power of two), and each
service and environment's in finish order by their place in it, so that a range is read without a
lookup by id. The bench's commits first had shas that began alike, and every commit's key (its
first 16 hex digits) was the same: a key is now its whole sha's hash, and a revert found by its
title compares titles without a squashed pull request's number (GitHub's `Revert "Add search
(#122)" (#123)`). A sync costs what the disk takes (a temporary folder is often a tmpfs, where it
costs nothing: the first measure said 0.02 ms).

### 12.30 The catalog's list

`cargo bench -p kiln --bench catalog` (adacachy, 2026-10-22, release, pinned to one core, the
machine's load some 60 on 32 cores): 2,000 services in two environments, 240 deployments each in
DORA's ledger (960,000), 500 monitors reading them.

| | ns |
| --- | --- |
| a service's deployments: its last, its 30 days' and how many failed | 8.3 µs (19 allocations) |
| … every service's | 16.5 ms |
| the first cut's last deployment, a look at every deployment | 10.0 ms a service (20 s the list) |
| 500 monitors indexed by the services they read | 0.26 ms |

The first cut found a service's last deployment by looking at every deployment, and each monitor
and SLO that read a service by asking each one of each service (a string made each time): the list
of 2,000 services would have taken 20 seconds. A service's deployments are now read from its
environments' orders in the ledger (their last, and the range of 30 days), and the monitors and SLOs
read once each, indexed by the services their queries and tags name (`service:<name>`, a whole value).
What is costly to know of every service (who sent traces in the last day, whose logs carry trace ids)
is read once in five minutes; a service's page reads its own, narrowed to it.

### 12.31 The hosts' list

`cargo bench -p kiln-obs --bench hosts` (adacachy, 2026-10-22, release, pinned to one core, the
machine's load some 70 on 32 cores): fleets of hosts each reporting as kiln's own figures or the
Agent's System Check report one (50 series: CPU, load, memory, swap, two disks, two interfaces) and an
app's ten; two samples a series in the last minute; every third a resource's `os.type`, every fifth
its cloud.

| hosts | series | each host's series, newest, apps, platform | the ten figures' queries in all | the ten figures by newest samples |
| --- | --- | --- | --- | --- |
| 1,000 | 55,000 | 0.57–0.67 ms (6.2 allocations a host) | 4.3–5.1 ms | 0.34 ms (1,019 allocations) |
| 5,000 | 275,000 | 3.6–6.2 ms | 28–43 ms | 2.6 ms |
| 20,000 | 1,100,000 | 18–24 ms | 264–375 ms (six allocations a series, its answer's) | 33 ms (20,028 allocations: a host's name each) |

The figures were first ten queries of every host (`avg:system.cpu.idle{*} by {host}`, …): the query
engine's grid and answer for every series, some 400 ms and 1.2 million allocations at 20,000 hosts.
They are each series' newest sample now, read where the head holds it with nothing decoded (the
Gorilla encoder keeps its last sample), a host's taken together as they come: 8 to 13 times faster,
an allocation a host. A test holds them to reading each series whole, a late write beside the newest
and a block holding it among them. The list is reused 15 seconds and made again on the blocking pool
while the last is answered: no request waits on it but the first.

Beside it, the browser suite found a debug build's server dead of a stack overflow on a cloud's
test (`hyper-rt-shard-2 has overflowed its stack`): at `-O0` a router's frame holds every arm's
temporaries apart (kiln's `authorized` 676 KB, observe's `route` 412 KB, kiln's `route` 275 KB), and
a cloud's pull nests under them past a shard's 2 MiB. In a release build the three are 6, 18 and
55 KB. Observe's routes run as a task of their own now (kiln's `api.rs`): their handlers nest from
the shard's loop, not under the routers.

### 12.32 The incidents' store

`cargo bench -p kiln --bench incidents` (adacachy, 2026-10-22, release; the files on its NVMe
disk, `KOBS_DIR`; the machine's load some 70 to 140 on 32 cores, so times are rough, allocations
exact): 10,000 incidents as an organization's years of them come to be (one in twenty active, two
stable, the rest resolved; one to three of 40 services, a team of ten, a summary; five to fifteen
notes, a monitor's alerts on half, its changes; up to four todos; a postmortem for three in five
resolved), 52.4 MiB of files, 5.5 KB each.

| search (a page of 50, what matched counted by facet) | matched | as first built | read in place, written as read |
| --- | --- | --- | --- |
| everything | 10,000 | 163 ms, 1,501,165 allocations | 22 ms, 83 allocations |
| `state:active` (the page's first view) | 516 | 79 ms, 1,026,049 | 1.0 ms, 51 |
| `state:resolved AND severity:(SEV-1 OR SEV-2)` | 2,453 | 74 ms, 1,199,591 | 1.6 ms, 87 |
| `services:svc-07` | 481 | 84 ms, 1,076,862 | 1.5–4.6 ms, 76 |
| `teams:team-3 postmortem:true` | 519 | 93 ms, 1,056,000 | 1.5 ms, 101 |
| `checkout` (a word of the title) | 1,296 | 88 ms, 1,049,889 | 1.2 ms, 78 |

As first built, a search copied every incident (some 94 allocations each, its timeline among them),
then cloned and lowercased each value a term read, cloned each value a facet counted (every summary
and root cause among them, each its own facet value), and made a tree of JSON of the page (193
allocations an incident). Now a search reads the incidents in place under the store's lock: a term
compares without allocating (ASCII case folded in place; a glob or a capital outside ASCII lowercased
alone), facets count borrowed values in one pass over what matched (fields whose values come from a
list, each facet's 100 most common), and the answer is written as it is serialized from borrowed
structs (a time formatted on the stack): its allocations are the answer's buffer and a few map nodes,
whatever the number of incidents. One incident read and written: 7 allocations (193 as a value).
Opening the store (each file read) takes 0.2–0.26 s and holds 100.8 MiB, 1.92 times its files' bytes:
the 64 MiB bound on the files holds an incident store to some 125 MiB of memory.

A change writes its incident's file alone (and syncs it and its folder): a note took 6.8 ms here,
where writing the whole store (as it was first kept, one file) took 147 ms and wrote 52 MiB. Its
bytes formatted, `time` 0.3.55's `format_into` counts a fraction of a second short (`.123Z` as
`.1`): the length written is taken from how far the writer went, and a test holds every fraction's
form to `format`'s.

### 12.33 The notebooks' store

`cargo bench -p kiln --bench notebooks` (adacachy, 2026-10-22, release; the files on its NVMe disk;
the machine's load some 20 on 32 cores): 10,000 notebooks as a team's years of them (one to three
Markdown cells of a few hundred words, three to eight timeseries of Datadog's definitions, a log
stream; investigations, runbooks, postmortems, reports; one in fifty a template), 54.4 MiB of files,
5.7 KB each.

| what | time | allocations |
| --- | --- | --- |
| opened: each file's head (the store's start) | 37–41 ms, 2.97 MiB held | |
| each file read whole, as a notebook | 106–114 ms | 1,898,317 |
| listed: every notebook, a page of 100 | 0.17–0.19 ms | 315 |
| listed: runbooks | 0.07–0.08 ms | 312 |
| listed: a part of the name | 0.38 ms | 313 (27,988 as first written) |
| listed: by name, case aside, a page of 100 | 4.0 ms | 303 |
| one read and answered, its graphs drawn by the importer | 0.03 ms | 747 |
| a postmortem of kiln's template, a timeline of 1,000 cells (71 KiB of Markdown) | 0.2 ms | 7,248 |

The store holds a notebook's summary alone (its name, author, type, times, size, incident: some 300
bytes) and reads its cells from its file when it is read: at the start, each file's head is read and
its cells skipped, a third of reading each whole and none of its 190 allocations a notebook, and a
store of 10,000 holds 3 MiB rather than its 54 MiB of cells made into JSON. A list's page is copied
out of the lock, nothing else: a part of the name is found case aside without a lowercase copy of
each name (as first written, two a notebook), and names are ordered case aside a byte at a time.

### 12.34 The container check and list

`cargo bench -p kiln --bench containers` (adacachy, 2026-10-22, release; the machine's load 55–90 on
32 cores from others' builds, so each time now is the least of nine runs and their median; as first
written, their median alone). A node's
look, on a copy of a busy node's cgroup v2 hierarchy (tmpfs): 1,000 containers (500 Docker's
scopes; 500 of containerd's in 251 Kubernetes pods, each pod's sandbox beside them: 1,251 scopes),
300 services, a user's session; each container's files as Linux 6.x writes them (`memory.stat`'s
72 lines); 500 links in `/var/log/containers`. And this machine's own hierarchy as it is (its
Docker containers, five minikube nodes' pods nested in theirs: some 200 containers, their
sandboxes besides). The server's list: fleets of 10,000 and 50,000 containers on nodes of 1,000,
each its 18 series as the check writes them (11 tags), a sixth stopped half an hour before:
180,000 and 900,000 series.

| what | as first written | now |
| --- | --- | --- |
| a node of 1,000: found (the walk) | 8.2 ms, 38,407 allocations | 0.84 / 0.90 ms, 8,522 |
| a node of 1,000: looked at whole (16,416 series) | 41.6 ms, 151,776 (its 251 sandboxes reported) | 13.5 / 14.1 ms, 13,208 |
| this machine's own: found | 41.3 ms, 92,693 | 1.7 / 1.8 ms, 16,327 |
| this machine's own: looked at whole | 59.2 ms, 116,311 | 4.2 / 4.3 ms, 16,335 |
| 10,000: the list made of the store | 34.7 ms, 373,508 | 16.5 / 17.5 ms, 40,069 |
| 10,000: a page of 100 by name, answered | 0.27 ms, 10,613 | 0.10 ms, 25 |
| 10,000: found by a part of a name | 1.3 ms, 58,331 | 0.50 / 0.58 ms, 12 |
| 10,000: found by a tag's glob | 6.5 ms, 260,783 | 0.22 / 0.23 ms, 20 |
| 10,000: ordered by CPU against its limit | 1.5 ms, 10,614 | 0.19 / 0.20 ms, 26 |
| 50,000: the list made of the store | 233 ms, 1,866,862 | 129 / 142 ms, 200,115 |
| 50,000: a page of 100 by name, answered | 0.53 ms, 10,615 | 0.19 / 0.22 ms, 27 |
| 50,000: found by a part of a name | 15.9 ms, 292,480 | 4.5 / 8.2 ms, 17 |
| 50,000: found by a tag's glob | 40.1 ms, 1,274,096 | 0.99 / 1.3 ms, 20 |
| 50,000: ordered by CPU against its limit | 7.4 ms, 10,616 | 0.65 / 0.85 ms, 28 |
| 50,000: the stopped, by when last seen | 1.4 ms, 10,614 | 0.22 / 0.24 ms, 26 |
| 50,000: one by the first 12 of its id | 0.09 ms, 0 | 0.10 ms, 0 |

As first written, a look read each file with std's `read_to_string` (a string a file, and two
system calls more to ask its size and place), listed every directory's files (each container's
scope's 60-odd), made each container's tags and series keys again (three allocations a key), and a
`Vec` a sample; and it reported pods' sandboxes as containers. Now its files are read into one
buffer, the values wanted picked from `memory.stat` in one pass, a directory without directories of
its own (its links two) not read, a container's keys made at its first look and kept, its samples
written into a decoding the owner hands back: a twelfth of the allocations, the walk nine times as
fast, a look three times, and what is left of its time the kernel's (some 9,000 files opened and
read). On this machine's hierarchy, five minikube nodes' nested in it, a look takes a fourteenth of
what it took.

The list was made of `Store::tag_seen`'s copies (each container's id, its namespaces, each tag's
key and value: some 26 allocations a container) and each figure's samples by an id copied a
container a figure. It is made of `Store::tag_seen_each` (the index's names borrowed under the
store's hold), each tag's key `inventory::TAGS`' own and each value made once for every container that has
it (a host's, an image's, a namespace's): 4 allocations a container, a ninth of what they were,
half the time at 50,000, and less held between askings. A search is read once: a tag's key compared
and its value's glob matched, nothing allocated; a part of a name found by memchr's search in each
name lowercased into one buffer. `str::to_lowercase` of each name made 292,480 allocations at
50,000; comparing a byte at a time without them was slower still (45 ms: each 64-character id
compared at each of its places), hence memchr's SIMD search. A page is written as JSON from the
list itself, 27 allocations where `serde_json::Value`s made 10,615. Ordered by a figure, each
container's is read once (it was read again at each comparison, a match on the field's name among
them) and the page selected before it alone is sorted (a selection is linear): 0.65 ms at 50,000
where sorting all of them took 7.4.

**OpenTelemetry's sources (O-32, §10.11).** The same fleets, and as OpenTelemetry Collectors'
`docker_stats` receivers write them (each container 21 series: cumulative CPU and throttled time,
each of two devices' bytes read and written, its memory, limits, processes, uptime, network;
1,050,000 series at 50,000), the list made by each build in turn (2026-10-23, release; each round
the least of nine runs and their median; the builds run one after another, three rounds, the
machine's load 22 to 45 from others' work). O-30 is the list before OpenTelemetry's sources (its
commit, built apart).

| the list made, least / median of a round (ms) | 10,000, kiln's check's | 50,000 | 10,000, Collectors' | 50,000 |
| --- | --- | --- | --- | --- |
| O-30 | 13.4–15.3 / 13.8–18.6 | 80.8–96.1 / 85.8–107.4 | | |
| O-32 as first written | 19.2–20.5 / 19.7–24.2 | 125.9–134.0 / 131.1–141.1 | 19.4–20.1 / 19.8–20.9 | 146.8–150.1 / 153.6–157.3 |
| the tags a source needs, foldhash | 14.3–15.6 / 14.8–17.3 | 93.8–115.2 / 97.1–130.9 | 14.4–15.1 / 14.9–15.8 | 115.9–119.6 / 117.7–127.6 |
| kiln's series by their id and host | 13.8–15.9 / 14.2–17.5 | 88.1–105.6 / 91.6–121.9 | 13.5–19.8 / 14.3–22.4 | 107.7–142.5 / 114.6–163.9 |
| each container's first reporter a slot | 12.0–13.6 / 12.4–16.3 | 75.5–109.9 / 77.5–119.0 | 11.9–21.2 / 14.7–22.7 | 101.9–109.2 / 106.7–124.3 |

As first written, the list read each figure from its 27 sources a source at a time: five of
OpenTelemetry's metrics have kiln's names (`container.cpu.usage` among them), so their sources read
kiln's every series again only to find no `container.id` on them; each series' five tags were
looked up (its id, host, namespace, pod and name) where kiln's need two; and each series went into a
map by its container and reporter, its container's id (64 characters) found by std's SipHash: half
again O-30's time at 50,000. Now a source passes over a series that has none of the tags it needs
before anything of it is read (`Reads::any_of`); kiln's sources look up two tags; the maps hash
with foldhash (seeded, already built for hashbrown and serde_json); and a container's first reporter
in a pass sums into a slot of its own (a container most often has one; a second goes into a map),
the slots the pass reached listed and made empty after it: the map's two hashings a series gone.
Its allocations are O-30's (40,107 and 200,157 against 40,069 and 200,115), and its time no more
than O-30's, though it reads three times the sources; a Collectors' fleet, a third less than as
first written. The rounds' spread is others' work on the machine: measured in turn, each round's
order holds (the slots' version the least in each of the two quieter rounds).

### 12.35 Log indexes

`cargo bench -p kiln --bench indexes` (adacachy, 2026-10-23, release; the least of nine runs and
their median, and the allocations of one; built holding the machine's heavy-job lock, run at normal
priority once it was free, the load 78 to 115 on 32 cores from others' work). Records as a busy
node takes them (an access log's lines of five services, a tenth debug, a third with a trace id),
100,000 routed: as a collector's rows (segments of a block's text) and as records in runs of a
thousand; by `main` as kiln makes it, and by four indexes (`payments`, `audit`, `errors`, and `main`
last with two exclusion filters: every debug record, half of `web`'s). What routing gives back is
let go after it is timed.

| routed, a record | as first written | now |
| --- | --- | --- |
| `main` alone, a collector's rows | 7.0 / 8.5 ns, 0 | under 0.05 ns, 0 |
| `main` alone, records | 69.7 / 121.4 ns, 0 | 11.5 / 22.9 ns, 0 |
| four indexes, a collector's rows | 198.6 / 257.9 ns, 0.038 | 96.3 / 104.4 ns, 0.004 |
| four indexes, records | 546.8 / 784.8 ns, 0.040 | 75.5 / 128.0 ns, 0.006 |

As first written, the bench timed the records' freeing too (nine frees a record, and a collector's
segments' buffers): most of `main`'s time and of the records' under four indexes. Under `main` as
made, a collector's rows are handed on whole, nothing read: the time of a few hundred segments, none
a record's. Of the rest: a run of records was read twice for kiln's events (a count, then each
record's bytes counted alone), one pass now; a split run's records were pushed into growing runs,
its rows written again field by field into growing buffers (`put_row_ref`: each attribute's key
checked, each value cut). Now each record's part and bytes are read once into buffers kept from run
to run, a run whose records go into one part goes on whole, and a split one's runs are made their
size; a collector's rows are copied as written, but for each row's time, its first field, chained
anew from its part's row before (`put_row_rechained`), into buffers made their size. What is left
is reading each record against the indexes' filters (four, and `main`'s two exclusion filters, one
of them sampled by a hash): a collector's rows, each read in place (its lengths' varints) and copied,
96 ns; records, read where they lie and moved, 76 ns.

A store of 2,000,000 of them, in one part, and in four (as four indexes keep them), written a
thousand at a time (a head is sealed whole once it holds a block's text: writes of one size seal
blocks alike, one part or four), searched:

| searched | one part | four parts |
| --- | --- | --- |
| the newest 100 | 0.395 / 0.459 ms, 955 | 1.103 / 1.218 ms, 957 |
| a service's newest 100 | 0.240 / 0.270 ms, 946 | 0.973 / 1.050 ms, 948 |
| a count by status of all | 46.3 / 62.7 ms, 1,076 | 46.4 / 59.9 ms, 1,076 |

As first written, a search made a record of each row that came among the best so far (some nine
allocations) as it read: a block is read oldest first, so each newer row was made a record and most
were let go again, 45,029 allocations for the newest 100 (1.78 ms, its writes then of 5,000, its
blocks 5,000 rows) and 9,029 for a service's. The best are kept as their rows' bytes now (each
copied as written, its time chained from 0, into one buffer written again, the best alone, once
rows let go are most of it), a record made of each of the last alone: 955 allocations, nearly all
of them the 100 records'. A count of all costs the same in one part as in four. The newest 100 of
four parts cost three times one part's: the newest block of each part reaches back to the same
minute, so four blocks are read where one was (a block's rows are read from its oldest; reading it
from its newest would want an index of its rows' places, a change of the blocks' format left for
when it is worth it). As first written, the four parts' count took 125 ms against one part's 80:
their writes of 5,000 split four ways sealed blocks half the size, each read with its own cost; of
writes alike, the same.

### 12.36 Continuous profiles

`cargo bench -p kiln --bench profiles` and `cargo bench -p kiln-obs --bench flame` (2026-10-23, adacachy, release; the
least of nine runs and their median, allocations of one run; the machine shared, its load near 60, so the times are
noisy and only compared run against run). A busy service's minute of CPU: synthetic, its shape a real one's (8,000
functions under four entry points, 40,000 distinct stacks 10 to 40 deep, a million frames; 3.1 MiB inflated, 1.7 MiB
gzip'd).

| | Time | Allocations |
|---|---:|---:|
| An upload read (Go's fixtures: CPU and heap, 6.6 KB) | 0.068 ms | 162 |
| An upload read (the busy CPU profile: inflated, read, its types summed) | 20.0 ms (163 MB/s) | 173 |
| The busy profile read alone (`pprof::read`) | 11.2 ms (292 MB/s) | 130 |
| Summed into a new graph (750,000 nodes) | 22.2 ms | 9,990 |
| Summed into a graph that has its nodes | 22.2 ms | 0 |
| A graph drawn (one node in 10,000 of the whole the least drawn: 4,664 of 750,000) | 3.3 ms | 35 |
| Its functions by self | 10.3 ms | 28 |
| 20,000 uploads kept, batches of 32, a sync a batch | 132 ms (6.6 µs an upload) | 6 an upload |
| The store opened again (its header files read) | 14.7 ms | 67 bytes held an upload |
| 500 uploads of a service summed (Go's CPU fixture), one thread / eight | 13.6 / 5.4 ms | 57 an upload |

The flame graph's sums, before (std's `HashMap` of (parent, function) and of interned strings, SipHash) and after
(hashbrown's `HashTable` of node ids on kiln-obs's keyed hash, the node itself the key), the same bench run in turn,
three rounds each:

| A million frames | Before | After |
|---|---:|---:|
| Into a new graph | 50–53 ns a frame, 19,889 allocations | 22–25 ns a frame, 9,990 |
| Into a graph that has its nodes | 31–40 ns a frame | 26 ns a frame, 0 |
| Ten profiles in turn (a million nodes, the bound) | 14–16 ns a frame | 11–13 ns a frame |

The next pass (2026-10-23, after O-34), the same bench: a query read an upload with 57 allocations (its header's
lists, its blob's buffer, gzip's state, `pprof::read`'s lists grown as it read) and merged its threads' graphs one
after another. Now an upload's header is walked to the profile asked for, nothing made (`find_profile`); its blob
read into the reader's buffer; gzip inflated with one state a reader keeps (`flate2::Decompress`: its member's header
fields passed over, its header's CRC-16 and its trailer's CRC-32 and length checked, as `GzDecoder` checks them);
pprof read with a scratch a thread keeps (`pprof::read_with`: its tables emptied and kept, each of the profile's
lists made its size once, from one counting pass and the last profile's); and the threads' graphs merged in pairs at
once, a round at a time, each pair the one before into the one after, so the graph is the same whichever thread
ends first (a test: one, three, four and eight threads, the same graph drawn and the same functions):

| | Before | After |
|---|---:|---:|
| 500 uploads of one service (Go's CPU fixture), one thread | 13.6 ms, 28,588 allocations (57 an upload) | 10.2 ms, 3,110 (6 an upload: the profile's own lists) |
| The same, on eight threads | 5.4 ms | 2.1 ms |
| An hour's of one service (60 uploads), one thread / eight | 1.62 / 0.77 ms, 3,505 allocations | 1.19 / 0.46 ms, 467 |

Left: merging two large graphs is still one thread's (51 ms for two of 750,000 nodes: a node found under its parent
by a lookup each); eight threads' graphs take three rounds of it, not seven.

### 12.37 Java's recordings

`cargo bench -p kiln-obs --bench jfr` (`KOBS_JFR` naming dd-trace-java's own recordings, its debug dumps of
`main.jfr` uncompressed) and the Java lines of `cargo bench -p kiln --bench profiles` (2026-10-23, adacachy, release;
the least of nine runs and their median, allocations of one; the machine shared, its load 30 to 70). A minute as
ddprof records one, synthetic, its shape a large service's: methods in classes of eight packages, stacks 20 to 80
deep sharing their callers as a framework's do, CPU, wall and allocation samples on them (half, a third, a sixth),
most on a few. The reader as first written (std's maps on SipHash; each metadata string, field and class name, each
symbol and frame name owned; each frame's name hashed and compared again for each kind) and as rewritten (§8.11: in
place, classes by their place, a frame's function once a chunk, kiln-obs's keyed hash), the same bench, the same
output to the byte:

| A recording | Before | Rewritten | Its second pass |
|---|---:|---:|---:|
| A quiet service's minute (3,000 methods, 2,000 stacks, 10,000 events; 940 KiB) | 63.6 ms, 34,491 allocations | 10.0 ms, 543 | 7.2 ms (133 MB/s), 511, 2 page faults |
| A busy service's (12,000 methods, 20,000 stacks, 100,000 events; 9.0 MiB) | 695 ms (13 MB/s), 157,707 | 104 ms, 651 | 57.5 ms (161 MB/s), 605, 886 |
| A very busy one's (24,000 methods, 60,000 stacks, 400,000 events; 30 MiB) | 1,541 ms, 361,897 | 392 ms, 698 | 215 ms (148 MB/s), 643, 2,854 |
| dd-trace-java's, `Burn.java`'s last upload (the JDK's chunk and ddprof's; 390 KiB) | 3.79 ms, 30,612 | 1.52 ms, 661 | 1.15 ms (347 MB/s), 682, 0 |
| Its first upload (812 KiB) | 8.30 ms, 41,590 | 2.81 ms, 721 | 2.09 ms (399 MB/s), 728, 0 |
| The JDK's chunk alone (389 KiB) | 3.82 ms, 29,605 | 1.33 ms, 429 | 0.93 ms (428 MB/s), 446, 0 |

Profiled first (`perf record -g`, frame pointers): a quarter of the time was SipHash, three tenths libc's `memcmp`
and `memcpy` (each frame's name compared and copied as a map's key, each stack's locations cloned as one), a tenth
the frames' names interned again for each kind. Profiled again, rewritten: each kind still looked each frame up
twice (its location by (method, line), its stack by its locations), the table of stacks grew by rehashing (7%), a
field read went through its class's way when most are one varint. The second pass: each stack's locations of the
chunk made once for every kind (each kind's own found by place, no lookup), the stacks' table made the chunk's
size at once, a field of one varint read as one. Left: page faults where a recording is large (each call's buffers
new, 886 for a busy minute: a reader keeping its buffers between uploads would leave them; a dd-trace-java upload of
some hundreds of KiB has none). Where a recording goes (the Java upload `kiln-obs`'s fixture is, as
dd-trace-java sends it):

| A Java upload (110 KiB sent: its recording zstd'd) | Before | After |
|---|---:|---:|
| Read at the runner (inflated, read into six profiles, 63 KiB of pprof) | 4.75 ms, 30,716 allocations | 2.37 ms, 786 (its recording read in 1.15 ms of it, above; the rest its inflating) |
| Repacked to send on (64 KiB: its profiles, not its recording) | 0.002 ms, 54 | the same |
| Read again at the server (pprof, as any upload) | 0.13 ms, 396 | the same |

A runner reads a recording once and the server what it became: the lane carries 64 KiB of an upload that came as
110, and the server reads it in 0.16 ms, not 2.4.

## 13. Scenarios

What a user would see, checked end to end; not the code's internals.

**On macOS** too (2026-10-24, aarch64, at 28e01fba): every scenario below that needs no Docker passes
on a Mac, 38 in all: `observe_live` (17), the clouds' (6), the tail (4), statsd (2), the catalog,
DORA, incidents, indexes, infrastructure, metrics from logs, notebooks, the scanner, synthetic tests
and macOS's own unified log; with kiln-obs's tests (351, and its replication's 4) and kiln's observe
tests (254). Running them there found three things Linux hid: std's read-write lock on macOS let
a reader that takes it again at once hold a writer off for good (now `kiln_obs::lock`, §4); the
test fleet's server advertised kiln's default QUIC address, `<hostname>:7071`, so on Linux its
runners reached it only through the HTTP tunnel, and on macOS, where a bare hostname does not
resolve, not at all (it advertises its listener now; a runner that cannot resolve the server's
address never trying the tunnel is kiln's own, said, not changed here); and two tests' own
assumptions (an `openssl` that binds one address; a metric read before its write landed).

**On Windows** (2026-10-24, jupiter: Windows 11, x86_64, MSVC, Git for Windows' own
`core.autocrlf`): 42 of the scenarios below pass there, at 7f607f14, ef10dc30 and 4747aca3 (the same
kiln): `observe_live` (17: CI through a Windows runner, every integration a monitor names, the group
through kills and an image, OTLP over gRPC, GitHub's webhooks, the API against its OpenAPI document,
...), the clouds' (6), the tail (4), the logs collected (4: pipelines on every machine, containers'
logs, files through rotation, truncation, a stop and a kill, syslog over UDP and TCP), DogStatsD (2,
over UDP), the catalog, DORA, incidents, indexes, infrastructure, metrics from logs, notebooks, the
scanner and synthetic tests; and at c1d7efd9, kiln-obs's tests (351, its group's 4) and kiln's
observe and CD tests (274). Running them there found four things. kiln's server overflowed its main
thread's stack as it started (Windows gives it 1 MiB, Linux and macOS 8; the binary is linked with 8
there now). The log collector, which knew a file off Unix by its creation time, lost the first lines
of a rotated log's new file: NTFS gives a file made at a name another was renamed from moments
before that one's creation time (its "tunneling", 15 s by default), so the new file passed for the
old; a file there is now its volume's serial number and its file index
(`GetFileInformationByHandle`, through `winapi-util`), as a device and an inode are on Unix. Git
turned a recorded Python traceback's lines into CRLF as it checked it out, and the test reading it
failed (fixtures and test data are now `-text`). And the test fleet itself was Unix's: it stops a
node there by closing its stdin, as kiln's supervisor does (`KILN_SUPERVISED`), passes Windows its
own variables, and writes paths into TOML and `file:` URLs as Windows needs them. Not run there:
DogStatsD's Unix socket (Windows has no Unix datagram sockets), the Event Log's through Windows's
own `wevtutil` (a channel of the test's own needs an administrator; its stand-in is a shell script,
run on Unix), macOS's unified log, kiln CD's (it needs a cluster), and those that need Docker.

**Live** (`crates/kiln/tests/observe_live.rs`, `cargo test -p kiln --test observe_live -- --ignored`):
a kiln server and two runners as processes of their own, each with a home of its own and a clean
environment, on real sockets and disks. Points and log records are written through the API and read
back exactly; the runners join as learners and catch up; their own figures arrive through their own
writes; a runner killed (SIGKILL) while the leader compacts its log past it comes back by an image
whose files it fetches over HTTP; the server killed and started again answers what it answered. At
each stage every replica's full digest (every sample, every record) agrees at one index. 2026-10-18:
passes, agreeing at indexes 10, 14 and 22.

**A group on real logs** (`crates/kiln-obs/tests/group.rs`): replicas on hyper-log files in one
process, messages carried as the lane does, snapshots' files fetched before they are stepped,
senders that hear only the member they wrote to:
- three voters through a leader crash, restarts and a partition;
- the leader killed six times the moment a follower takes its entries: 18 writes sent again, every
  one found applied by its session, so applied once;
- a voter away past two compactions, back by an image, then leading;
- kiln's shape: one voter and a learner that joins after compaction.
Each checked against a store that applied the committed log.

**Replicas alike** (`kiln_obs::store` tests): one random stream (writes sent twice, late writes,
corrections, logs several to a time, retention, a bound on series, a series that expires and comes
back as another kind) applied to an in-memory store and to two on disk that cut, compact, retain and
restart on schedules of their own, and to rewrites built apart while the store goes on (planned,
held across entries, taken in later; a real maintenance thread beside the applying one): all agree
at every checkpoint, over 1,000 seeds (`KILN_OBS_SEEDS=1000`), digests included.

**Rollups change no answer** (`kiln_obs::query` tests,
`rollups_answer_to_the_bit_as_samples_do_whatever_the_blocks`): 50 hours of gauges of noise, a
level, counts of fractions, counters that reset, samples that are not finite, a distribution of two
hosts, writes late into merged time and late writes not merged; three replicas, two on disk cutting
and merging every five and every two hours of writes, one holding all in its head. Every query whose
buckets are whole windows (steps of five minutes to three hours; calendar days in UTC, New York and
Kathmandu; every rollup function, rates and counts, a percentile across series, a distribution's
percentiles by host, shifts by an hour and a day) answers alike on all three to the bit, rollups
read or not, with every kind read from rollups somewhere (the test says so: a tier unread would
prove nothing). 2026-10-18: passes over 100 seeds (`KILN_OBS_SEEDS=100`, each its own writes and
cut schedules), and the replicas-alike scenarios above over 1,000 with blocks of format 3. Blocks' own tests: rollups read back exactly; a series' rollups given another's
found by `verify` though their checksum holds; a long region read in part, 300 random sets of
ranges, exactly as whole; a damaged chunk found by the read that wants it and not by one that does
not; format 2 still read.

Also live: OTLP/JSON gzip to the server (a wrong token refused, a malformed body refused) and
protobuf to a runner's receiver, which arrive and replicate; a CI pipeline run on a runner from a
`file://` repository (a job that passes and one after it that fails), its jobs' counts, durations
and events found, and the run a trace (2026-10-21): found by its root among CI's spans and read as a
waterfall draws it, the run its root, each job under it and each step under its job, the failing
job, its step and the run marked failed (the step's exit code, the job's error, the run's branch);
a trace analytics monitor of CI's failed spans by resource alerting on those three alone. A monitor
that alerts six
integrations (services in the test) each in its format, a webhook refused once and delivered on
its second try with its signature checked, a recovery that resolves PagerDuty's incident by the
same key and closes Opsgenie's alert by it (its key sent, never shown), and muted, a change of
state with nothing said; GitHub's webhooks and posted events, searched and counted.

**Email** (`crates/kiln/tests/observe_email.rs`; Docker): Mailpit as the relay, requiring STARTTLS
and a login, its certificate signed by a CA the test makes (rcgen) and the integration is given; a
monitor whose name and message hold `<`, `>` and `&` alerts and recovers: two messages to both
recipients, the text without mentions and with the value and threshold, the HTML escaped, both
`Auto-Submitted`, one `References` (the alert's thread); each delivery an event, its relay's 250. A
CA that is not the relay's and a wrong password each fail once, said (TLS; 535) and not tried
again; a plain relay without a login takes the test notice; an address holding a line break is
refused when kept (400), and the password and CA are never shown. 2026-10-19: passes (33 s).

**Anomalies** (`observe_live.rs`): ten days of a daily cycle every five minutes, two hosts, one's
last point a spike, posted to a server: `anomalies(…, 'robust', 3)` over the last six hours bands
both from the history before them, the spike past its band and at most a few of 143 ordinary points
(its 1-in-500 calibration); at 5 deviations the spike alone; an anomaly monitor of the last 15
minutes alerts the spiking host's group alone, and a webhook hears of it. 2026-10-18: passes, four
runs of four.

**Live tail** (`crates/kiln/tests/observe_tail.rs`, a server as a process, its tail followed over a
real WebSocket while records are posted to its API): what is posted after a tail opens comes on it,
each record once, in the order posted, and only those its search matches, never one posted before
it opened; each within 0.41 s of its post at the most; nothing coming, it says so each second. A
flood of 40,000, a tenth of them of a rare service, followed at 1,000 a second: no message past the
rate, no record twice, every one counted and none missed, the rare share of the 4,200 shown 0.096
(within the 0.999 interval of 0.1, ±0.015). The server killed and started again: a tail opened as soon as it
answers shows none of the 20,000 records it applied again, only what is posted after, and the store
holds the 20,000 once. A search that does not parse, a rate out of bounds and a seventeenth tail are
told why; one that closes makes room. 2026-10-18: all four pass.

**In a browser** (`ui/e2e/observe.mjs`, Playwright against a live server; how to run it is at
its top): a query explored and put on a new dashboard, which is then given a note, saved and
deleted; kiln's own dashboard narrowed by its template variable, then copied; logs searched,
narrowed by a facet and by an attribute of a record, the API answering the page's search alike; a
search's patterns, one of them clicked, the search it narrows to finding its records alone (checked
through the API); a search followed live: what is posted after comes, the newest on top, another
service's never; paused, what comes waits (said how many) and resuming shows it; opening a record
pauses; a
monitor made from the form, muted, unmuted and deleted; a composite made from the form of two
monitors grouped by host (web01–03 and web02–04), alerting web02 and web03 alone, listing what it
reads, and one of them refused deletion from its page while the composite reads it; a distribution
posted as Datadog's distribution points drawn on a dashboard as a heatmap and as bars, both
p50 and p99 marked, a cell tapped saying its range and count; a scrape target tested, added, seen
up and removed; a signed webhook
added, tested (the receiver checks the signature) and removed; an Azure integration's event hubs
(nothing answering at their endpoint): its row failing with the hub's error and what it reads, the
form showing metrics not read, its namespace and hubs and the connection string kept, a hub added
(the connection string, endpoint and TLS kept), the connection string given up for the
integration's credentials, removed (2026-10-19: passes in Chromium and WebKit as a phone); a
metric posted as six series (three hosts, two environments) found in the metrics summary by a part
of its name, its row saying six series, host 3 and env 2, its tags' values listed in a click (each
host on two series), and opened in the explorer from it (2026-10-19: passes in Chromium and WebKit as
a phone); a synthetic test made from the form (an endpoint in the scenario), tried (`Passed in … · 200`), kept
as made, run now and seen passing, an assertion added the endpoint does not meet, tried
(`body contains "nope": was …`), saved, run and seen failing with why, removed (2026-10-19: passes in
Chromium and WebKit as a phone); an email
integration added from the form (its recipients, sender, relay, security and port) and tested, an
SMTP sink in the scenario sent the test's message to both recipients, and an Opsgenie one added
(its key never shown) and tested, its Alert API hearing the key, both removed (2026-10-19: passes
in Chromium and WebKit as a phone); ten orders' traces sent over OTLP's JSON (a frontend calling a
cart, whose database call fails once), the failing span found in the trace explorer by service,
status and resource, its trace opened from it, its four spans in the waterfall's order, the picked
span's error its exception's (`ValueError: cart 3 is locked`), another picked and the address
saying it; the cart in APM once its trace metrics were counted (10 requests, 1 error), its page
listing its resource and its call to postgresql; the map drawing the three services and the
frontend's 10 calls to the cart as answered (their span ids numbered alike in every run: each
span's parent its own trace's) (2026-10-19: passes in Chromium and WebKit as a phone, on fedora);
six error logs of a service, their messages differing in their numbers and what they quote, found
in Error Tracking as one issue by the service's name, opened, its stack shown, resolved from its
page as the API keeps it, neither page wider than the screen (anything wider, a stack's lines,
scrolls in its own box; the check itself tried on a page too wide, which it names, and on a wide
box that scrolls, which it lets be); then a monitor of the service's new issues made from the form,
checked as Datadog's Error Tracking monitor (`Alerts when an issue's error count is > 0`), kept as
made, its issues' errors drawn by issue on its page, which fits the screen too (2026-10-20: passes
in Chromium at 1,440 px and as a phone, and in WebKit as a phone, on fedora); a Datadog dashboard
pasted into the import dialog as its JSON (a note; a timeseries whose formula divides a count of
error logs, searched by a value list, by a metric's requests under a boolean filter; an SLO widget
kiln does not draw; a group of a sunburst and a log stream): its group drawn under its title, the
sunburst a pie of the service's logs by status (the errors a third), the formula's chart, the SLO
kept as a note saying what it was, the page within the screen's width; the boolean filter selecting
the requests through the query API and its opposite none; the dashboard given back as Datadog's JSON
(2026-10-20: passes in Chromium at 1,440 px and as a phone, and in WebKit as a phone, on fedora); a
service check's runs sent as Datadog's Agent sends them (with observe's ingest token,
`KILN_INGEST_TOKEN`): its status on a dashboard (`1 Critical · 1 OK`, the failing host by name), and a
monitor of it made from the form (two CRITICAL in a row, evaluated every ten seconds) alerting that
host and not the other, its page saying `2 CRITICAL in a row` and the host's value `Critical`, both
pages within the screen's width (2026-10-20: passes in Chromium at 1,440 px and as a phone, and in
WebKit as a phone, on fedora).
Each makes its own records and checks through the API, not the page. Every page a scenario checks
for width is also checked for its tables' names: a row's name cut short must show 150 px of itself
at least (a phone's rows put their buttons under the name). 2026-10-21, after the Sensitive Data
Scanner: all 33 pass in Chromium and WebKit, each at 1,440 px and as a phone, on adacachy (a server
seeded with three services' latency and 300 logs, a receiver and a logs folder). Four lists' buttons
left a name too little room on a phone: pipelines' and archives' (seen in screenshots, laid out as
the scanner's before the run), rehydrations' and synthetic tests' (found by the check in Chromium,
149 and 135 px, then laid out the same). The first run also found an SLO scenario that, begun in
the last minute of a five-minute span, found its alerts' five-minute window empty: it now waits for
the next edge when it begins within 90 seconds of one. 2026-10-20, after Datadog's boolean filters
and the lexer's numbers: all 31 pass in Chromium at 1,440 px and in WebKit as a
phone, on fedora (a server seeded with three services' latency and 300 logs, a receiver and a logs
folder of the scenarios'); after service checks, the same 31 pass again in both, and the service
check scenario, made to wait for its runs to be stored before it opens the page, passes in both on
its own. 2026-10-18: all pass in Chromium at 1,440 px and in WebKit as a phone (390 px, touch);
and every Observe page loads in both with no error, failed request or sideways overflow, against a
server holding two hours of metrics, an hour of logs, events, and three monitors alerting to a
webhook and to Slack. Run again and again, two flakes showed and are gone: WebKit, a moment after a
page makes a field, can drop text typed into it with no input event at all (every field is now
checked to hold what was typed, and typed again if not); and a button found by part of its name
also found kiln's own metrics named `…_running` (every button is found by its whole name).

**Forecasts** (`observe_live.rs`): two disks, one filling a percent an hour from 62 and one steady
at 40, two days of a point every ten minutes: the query API forecasts both past now, the filling one
74 twelve hours on within its band and the steady one 40; `max(next_2d):forecast(..., 'linear', 2)
>= 95` alerts the filling disk alone, and a webhook hears it once. 2026-10-18: passes (23 s). In a
browser, the explorer draws a filling disk's forecast and its table says the data's last (62), not
the forecast's; in Chromium and WebKit as a phone.

**Roles** (`observe_live.rs`): tokens of a viewer, a developer and a maintainer against a
server: the viewer reads (status, a query, a search, dashboards, monitors, integrations,
a check) and is refused (403) a monitor, a dashboard, data posted, an integration and a compaction;
the developer makes a monitor and a dashboard and posts data, and is refused an integration, a
cloud and a check of the replicas; the maintainer adds an integration and removes it; the OpenAPI
document says what each route needs. 2026-10-18: passes (18 s).

**Outliers** (`observe_live.rs`): five hosts' CPU, four near 40 and one at 80, an hour of noise:
`outliers(…, 'dbscan', 1)` and `outliers(…, 'mad', 3, 20)` mark the fifth alone; an outlier monitor
alerts its group alone, the four others OK, and a webhook hears it once. In a browser, the explorer
draws the outlier in its color and the others grey; in Chromium and WebKit as a phone. 2026-10-18:
passes.

**Calendar rollups and weights** (`observe_live.rs`): three days of a count of one an hour, through
the API: by UTC days begun at 9am, every bucket a 9am a day apart and every hour counted; by New
York's days, each bucket a local midnight (4am or 5am UTC), 23 to 25 hours long, each whole day as
many as its hours; Datadog's six pods, 36 and 46 summed by version and 12 and 16 weighted; a
timezone that is none refused, naming it. 2026-10-18: passes (17 s).

**Composites** (`observe_live.rs`): two monitors grouped by host, one alerting web01–05 and the
other web04–09, and their composite `a && b`: it alerts web04 and web05 and no other host, a webhook
hears of those two alone; web04 recovering in one recovers it in the composite, and is said; a
monitor it reads is refused deletion (409, naming it) until it goes; a composite of a composite or of
a monitor that isn't there is refused. 2026-10-18: passes (53 s).

**Service checks** (`observe_live.rs`): a server, a webhook and a DogStatsD listener of the server's;
a check's runs sent as Datadog's Agent sends them (`/api/v1/check_run`, the ingest token) and as
`_sc` datagrams, each at its own second; a service check monitor by host, two CRITICAL in a row to
alert. web-1 passing then failing once is evaluated (its group's value CRITICAL's code) and stays OK,
nothing said, while the check status widget's answer says CRITICAL, one host OK and one CRITICAL; its
second failure alerts it, the webhook's notice the run's message (`web-1: connection refused by
10.0.0.7:5432 (CRITICAL)`), web-2 OK all along; its first OK, by DogStatsD, recovers it (`web-1 is
reachable`) and the widget's answer is OK for both; each of the six runs a record too. 2026-10-20:
passes (with the roles and OpenAPI scenarios, 39 s).

**Under load** (`crates/kiln/tests/observe_load.rs`): the flood of §12.11, every point taken
stored exactly once, the replicas alike; and a runner flooded while its server is killed (SIGKILL)
and later started again: it refuses in time (0.5 s at full rate, 2.4 s at 100 k points a second),
its memory bounded, and everything it took is stored once the server is back.

**Traces under load** (`observe_load.rs`, `a_trace_flood_…`): eight senders flood OTLP/HTTP trace
requests (510 spans each, gzip, kept alive) as fast as they are answered, half to the server and
half to a runner's receiver, each trace a server's span, its call out and a database's query, while
a reader looks traces up by their ids and asks for the services. Every span answered 200 is stored
once (counted by an aggregate over the range: what was taken, no more, no less), each sender's newest
trace whole by its id; the trace metrics count each request once and each call out once
(`trace.hits` the traces taken, `trace.outbound.hits` twice them), as their windows settle; the
replicas agree. 2026-10-20, fedora (32 threads others share), 20 s: 6.6 M spans taken, 330 k a
second, 1.2% of requests refused (503, sent again when told); an answer 0.45 ms p50, 1.2 ms p99; a
trace by its id 44 ms p50, 184 ms p99 meanwhile; the newest trace readable 131 ms after the flood,
the trace metrics counted 18 s after (their windows' settling, §8.3); the server 1.7 cores and 390
MiB at its peak, the runner 0.75 cores and 340 MiB; 0.013 page faults a span.

**Logs under load** (`observe_load.rs`, `an_otlp_log_flood_…`): eight senders flood OTLP/HTTP logs
requests (500 records each, an SDK's resource and four attributes a record, an error in fifty, gzip,
kept alive) as fast as they are answered, half to the server and half to a runner's receiver, while
a reader asks what the logs explorer asks: the last minute's newest 50 errors, and its records
counted by status, each answer saying what it waited for the store and what it took reading
(`queued_ms`, `waited_ms`, `took_ms`). Every record answered 200 is stored once (each sender's,
counted by its host: what it was answered for, no more, no less), none dropped for room; the
replicas agree. 2026-10-20, adacachy (32 threads others share, a load of 25 to 30), 20 s: 3.9 M
records taken, 196 k a second, 1.9% of requests refused (503, sent again when told); an answer 0.52
ms p50, 3.4 ms p99; the errors' search 5.3 ms p50 (2.3 of it waiting for the store), 19 ms p99 (12);
the minute counted by status 118 ms p50, 226 ms p99, nearly all of it reading; every record stored
0.66 s after the flood; the server 2.85 cores and 402 MiB at its peak, the runner 0.74 cores and 306
MiB.

A search waits for the store while a write holds it, and a write's apply seals the log head's blocks
there (§4.6: sorted, re-encoded, its words hashed into a filter, compressed), some milliseconds a
block. Sealing them apart was tried (the head's segments copied under a shared hold, sealed under
none, taken in under a brief exclusive one, each segment numbered so a seal a cut overtook is
refused): a search's wait grew, 2.3 to 12 ms at the median, 12 to 111 at p99, and the count by
status's to 1.2 s at p99. A reader holds the store for its whole scan, a writer waits behind a long
one, and every reader after waits behind the writer (the lock writes first): a second writer is a
second convoy. Left out; what removes the wait is a reader that does not hold the store while it
reads (its blocks and files taken as they are, read with no hold of it).

Done so (2026-10-20): a log store's head segments, sealed blocks and files are shared, and a reading
of logs or spans (the explorers' search, analytics, facets and patterns) takes them as they are under
a brief hold (`Store::logs_at`, `spans_at`: a few dozen shared pointers copied, with the cutoff then)
and reads with the hold let go (`api::read_apart`); a file let go meanwhile stays readable (its open
handle keeps it, on Windows too: Rust opens files sharing their deletion). A test reads a snapshot
taken before rows came, a cut was written and taken in, and the oldest file was deleted: what it held,
to the record. Measured against the build before, in turn: on fedora (others' work light, a load of
9 to 16), the errors' search waited for the store 922 and 848 ms at p99 before, 23 and 20 after (the
count by status's 29 and 19, 25 and 19; the search had waited behind the count's whole scan, a
writer between them); on adacachy (a load of 18 to 30), 1.58 s before, 7.3 ms after, and the flood
taken 241 k records a second against 173 k (a write no longer waits on a scan, so the batcher
refuses less). The thread that applies does the work it did a record: 54.4 thousand instructions
against 53.9 (perf, user space, the server's and the runner's alike, on adacachy). On fedora its CPU
time a record rose (4.6 µs from 3.3, both nodes' alike, with the same records a second): not more
work, so, by the counts, more cycles for it, as the scans now run beside it on the machine's cores and
caches rather than in its place.

Metrics from logs and from spans count as the explorers read: each window's records taken with the
windows touched under a brief hold, counted with it let go (`LogsAt::count_metrics`; the log-metric
test endpoint one taking for its whole range). The monitors evaluate each under a hold of its own (a
write waits for one at the most), a monitor of logs or spans from its records as they are, the store
let go; Error Tracking's scan reads its metric under a hold and searches for a new issue's first
error in the records as they are. A query of metrics (the API's, a metric monitor's, an SLO's)
holds the store a round of series at a time (§12.19), as do the metrics read without a query
(APM's RED and map figures, Error Tracking's: its issues' list and timeline, its scan, its
monitors), and APM's trace and map read their spans as they are. What is left whole under a hold:
service checks' status series (a few series each), the metrics' list and summaries (the index,
once). A cut writes its files under a shared hold too, but on the thread that applies: no write
waits for it, so no reader does.

The flood now runs as a server does, with its work beside it: eight log monitors over the last five
minutes, each evaluated every ten seconds (counts by host, route and thread, a cardinality of users,
words searched for), and three metrics from logs (each host's records, the errors by thread, a word
every record says, by route), made a window before the flood (a metric counts from the first window
that begins after it is made). What it proves besides: the monitors' first evaluation after the
flood finds every record (the errors, one in fifty of each sender's; each host's records, to the
record), and the metrics count every record once (a count above what was sent fails at once).
Measured in turn on adacachy (others' work at a load of 17 to 34), the build before the monitors'
holds and the seal's pass against both, two runs each: the errors' search waited for the store 2.14
s and 493 ms at p99 before (the monitors' one hold, a writer behind it), 5.5 and 34 ms after; the
count by status's 11.6 and 12.3 ms, 5.9 and 34; the flood taken at 213 k and 236 k records a second,
254 k and 188 k after (the second run's machine was busier: its answers' p99 9.5 ms against 1.3 to
1.5 in the others, its count by status's reading 2.2 s at p99, starved); the thread that applies
54.4 thousand instructions a record before, 34.2 after (§12.8).

**Clock skew** (`observe_live.rs`): a runner whose clock is an hour ahead
(`KILN_OBSERVE_CLOCK_SKEW_MS` moves the clock observe uses; a real one takes root to move) is
measured an hour ahead from its writes and said so once; its own figures, an hour ahead, are dropped
and counted (`ahead`), not kept; points it takes with true times are stored; started again with a
true clock, it is said to agree (each event looked for until it comes: written as any record is, it
is searchable a moment after the status shows the skew). Samples more than ten minutes past their write's time and log
records more than two hours past it are dropped by every replica alike: the leader stamps every
write it proposes with its clock.

**A full disk** (`observe_load.rs`, with Docker: the server's data on a 96 MiB tmpfs and a 30 MiB
balloon file beside it): it refuses exporters while the disk has less room than its next cut needs
(the log's compactable bytes and 16 MiB; taken again once 16 MiB more is free), so the cut that
frees its log can always be made; it does not spin (0.01 CPU s in 5 s while full); freed, it takes
again; everything it took is stored (21.4 M, then 27.6 M more). Before, the first cut that met a
full disk failed, the replica stopped and was opened again at once, the log replayed, the cut failed
again: 83% CPU and a warning every 0.4 s. A replica that stops is now opened again after a second,
twice as long each time it stops again (to a minute); a merge waits for room for its block.

The scenario no longer assumes the machine it runs on (2026-10-20): the host's dynamic loader is
found from the path its ABI names, wherever the distribution keeps it (Arch's `/usr/lib`, Fedora's
`/usr/lib64`, Debian's `/usr/lib/x86_64-linux-gnu`); it floods until the server refuses (two minutes
at the most) rather than for 25 s, since how soon a disk fills is the machine's speed (on fedora,
22 M points in 38 s); and what was taken is counted once it is written (a point answered 200 is
taken, and written a flush and a write later), as the floods count, not the instant the flood ends.
That last was a race the test had: on a loaded machine it counted 4 to 54 thousand points short,
every one of them stored a moment later. Were points ever lost, it names them: each series' count
against its sender's requests taken, and the first short series read a millisecond at a time.

**Clouds** (`crates/kiln/tests/observe_clouds.rs`): a kiln server pulling from services that
stand in for AWS (STS, CloudWatch, IMDSv2), Google (OAuth, Cloud Monitoring, the metadata
server) and Azure (Entra, Resource Manager, the VM's metadata service). Each checks what the real
one checks: every AWS call's Signature Version 4 recomputed with the key's secret over the method,
path, signed headers and body, within 5 min of now; Google's assertion verified with the service
account's public key (its issuer, audience, scope and times); Azure's client secret; every call's
token. Their figures are functions of the minute, so what is stored is checked minute by minute:
- with the credentials given (keys and a role assumed with its external id; a service account's
  key; an app's secret): each cloud's series named and tagged as Datadog names them; CloudWatch's
  averages from its sums and counts, its lists and results over two pages, a throttled call tried
  again; Google's gauge, delta and distribution (its count, sum, average and quantiles), its quiet
  type asked once over several pulls and its string type never; Azure's resources over two pages,
  a type without metrics asked once, a disabled subscription not read; the role assumed once a
  process, its keys kept from pull to pull; secrets never shown, never at rest in the clear, the
  file the owner's alone, and kept across a restart;
- a cloud that fails (CloudWatch answering 503): four tries, then failing, said once as an event,
  the next pull a minute away; back, recovered and said; Entra's words for a wrong secret; a change
  of settings without the secret keeps the kept one; settings that cannot be refused, saying
  which; a deleted integration pulled no more;
- the server's own credentials where it runs: an EC2 instance's (its region too), a Compute Engine
  VM's (its project too), an Azure VM's managed identity;
- each cloud's logs, every pull reading its ten minutes back every 10 s: CloudWatch Logs' groups
  found by their prefix over two pages (another's group not read), each search's first page empty
  as CloudWatch Logs' can be, ten events a page; Cloud Logging's entries of the filter asked (another
  resource's not read), eight a page, with Logging's scope in the assertion and its token taken by
  `entries.list` alone; Log Analytics' rows with a token of its own audience. Every record stored
  once over a dozen pulls (35, 21 and 16), a late one of each, inside the lookback, taken by the next
  pull once; Datadog's names (`source:lambda service:checkout`, the group and stream, the region and
  the integration's tags), levels from each cloud's own or the message's (five errors of thirty),
  a JSON payload's values and its trace and span, Application Insights' levels; `kiln.cloud.records`
  the same counts;
- the clouds' pushes, sent as the clouds send them: a Firehose delivery (gzip'd, its stream's ARN
  and common attributes in its headers) of a metric stream's five minutes of two instances' CPU, a
  stream's five minutes of a table's reads in OpenTelemetry 1.0.0's format (its average, count,
  p99, minimum and maximum read back), a
  CloudWatch Logs subscription's six events and its health check; a wrong key refused in Firehose's
  words, the request's id echoed; the same delivery sent again, and another with an event already
  taken: every series as the pull names it (the average, the counts, the extremes) and every event
  once (seven); a Pub/Sub push of a Cloud Logging entry sent twice, taken once; a wrong token
  refused; kiln's counts of what was taken and sent again.
2026-10-18: all pass (243 s; the pushes 99 s).

**Synthetic tests** (`crates/kiln/tests/observe_synthetics.rs`): a kiln server and a runner, the
locations; an HTTP endpoint the test serves, a TLS one (`openssl s_server`, its certificate signed
for 90 days by a CA the test makes) and a port nothing listens on. An HTTP test of both locations,
through a redirect, its status, body, a header (named in another case) and its time asserted, passes
at each; DNS at every machine, TLS's certificate 89 to 90 days left (in minutes), HTTPS from the
runner trusting the CA given; the closed port failing with why (`connect`), said once; Datadog's
series by location (its response time at each, its runs by status, its connection, first byte,
redirect, DNS and expiry); a test tried once from the server, nothing kept (`status_code is 201: was
200`); an hourly test run now, at once at both. The endpoint switched to 500: both locations fail,
each said once, and the monitor the test owns alerts a webhook (its message the test's); switched
back, it recovers, told, and each location's recovery is said. Removed, its monitor goes with it
(404). In the OpenAPI document and its conformance test; the routes a maintainer's. 2026-10-19:
passes (181 s).

**DogStatsD** (`crates/kiln/tests/observe_statsd.rs`): a kiln server and a runner, each opening the
listener the server keeps for it: the server's over UDP and a Unix socket with a tag of its own, the
runner's over UDP with histograms of max, min, count and the 99th percentile. Datagrams as the
protocol spells them: 110 counts over sample rates come out of `sum:…as_count()` as 110, a `host:`
tag's as that host, the listener's tag on every one; a gauge's last value, a set's two distinct
values; a timer's 22 samples as the Agent writes them (its median the nearest rank's 11, its 95th
100, its average and count); a distribution's count, sum and percentiles (within 1.09%); a gauge
with a timestamp two minutes back written there; the Unix socket's and the runner's (of host `r1`,
only the aggregates its listener asked); an event in the event stream, a service check a record
(`status:error`, `@check_status:critical`); a count that stopped falling to zero; two lines that do
not read counted, nothing else lost. Then Datadog's own client (datadog-py 0.55: increments,
decrements, gauges, sets, 100 timings, distributions, `gauge_with_timestamp`, an event and a service
check, over UDP and the Unix socket): every one as sent. The runner stopped (SIGTERM) a moment after
a count came, early in its bucket: the bucket under way written as it stops and waited for, the
count all there (the runner gone in 0.4 s; without the wait, none of it). What the API refuses (a
TCP address, the 99.9th percentile, an unknown aggregate) is refused; removed, a listener closes
where it was opened, its socket file with it. 2026-10-20: passes (129 s). The flood beside it is
§12.20's.

**Prometheus's remote write** (`crates/kiln/tests/observe_remote_write.rs`; Docker): a kiln server
and a runner, and Prometheus 3.7 itself scraping its own `/metrics` every 2 s, writing what it
scrapes three ways, each its own label (`rw`, by Prometheus's write relabelling): to the server
over 1.0 (the ingest token its bearer token), over 2.0 (the token as basic auth's password), and to
the runner's local receiver. Every sample Prometheus holds of a gauge over 20 s (its range query's
raw samples) is in kiln at its time with its value, to the bit, all three ways; its `/metrics`
handler's requests counted as `.as_count()` counts them (one a scrape); the handler's p99 read from
its histogram's buckets; with native histograms on (`--enable-feature=native-histograms`,
`send_native_histograms`), the handler's request durations arrive as a distribution, counted and
read as percentiles; its counter a counter and its gauge a gauge in the metrics summary. No
token is 401, a body that is not Snappy 400, JSON 415. 2026-10-19: passes (95 s).

**Datadog's Agent** (`crates/kiln/tests/observe_ddagent.rs`; Docker): a kiln server, and Datadog's
Agent 7.84.4 pointed at it (its metrics' and its logs' URLs, its API key the ingest token). Its own
checks' metrics arrive of its hostname (`system.load.1`, `system.cpu.user`) and its own service
check (`datadog.agent.up`); through its DogStatsD, 30 counts come out of `.as_count()` as 30, a
gauge's last value, 30 distribution values their count exactly and their median within the
sketches' bounds, an event of its host with its tags, a service check a record (`status:error`); a
file it tails, its five lines records of its service and source, `filename` among their
attributes. A key that is not one: 403, as Datadog answers. 2026-10-19: passes (71 s).

**Traces** (`crates/kiln/tests/observe_traces.rs`; Docker): a kiln server and a runner, and two
services instrumented with OpenTelemetry's own Python SDK (1.45.1; Flask's and requests'
instrumentation 0.66b1, grpcio 1.84.0): a frontend exporting OTLP/HTTP to the server, its spans and
its logs, and a cart service it calls exporting OTLP/gRPC to the runner's receiver, the cart's
database call failing one order in ten. Forty orders: each a trace of four spans across the two
services (the frontend's server span and its call, the cart's server span and its query), the
cart's by way of the runner and the group; a failing order's trace found by its id, its spans at
depths 0 to 3, each its parent's child, its offsets in order, its database span's error the
exception's (`ValueError`, its message) and its peer `postgresql`, its unified service tags; the
frontend's log of the order found by the trace's id; the trace explorer's syntax finding the four
database failures of 20 ms or more; from the trace metrics, each service's 40 requests and 4
errors, the cart's median past its 20 ms, its one resource's 40, its 40 calls to its database
with their 4 failures; the map's frontend → cart (answered by the cart's spans) and
cart → postgresql (named) at 40 calls each, both from the trace metrics (`counted`: the callers'
calls out, by the peers their spans name, joined to the spans). Spans with no ids refused, said as
the partial success (protobuf and JSON). 2026-10-20: passes (82 s, on fedora).

**Datadog's traces** (`crates/kiln/tests/observe_ddtraces.rs`; Docker): a kiln server, Datadog's
Agent 7.84.4 with its trace agent pointed at it (`DD_APM_DD_URL`), and two services traced by
Datadog's own Python tracer (ddtrace 4.16.0, Flask 3.1.3's and requests' integrations, their
defaults): a shop calling a cart, the cart's database span (a service of its own, `cart-db`)
failing orders ending in 4 or 9; the shop keeps the trace of every even order and drops every odd
one (`manual.keep`, `manual.drop`, as a sampling rule decides). Forty orders: the 20 kept traces in
kiln whole, the shop's, the cart's and its database's spans each of them, and none of the 20
dropped; the trace explorer's syntax finding the four kept database failures of 20 ms or more; a
kept failing order's trace by its id (the trace id the shop's tracer gave it, 128 bits): the shop's
request at its root, its requests integration's call, the cart's request and the database span,
each its caller's descendant, placed in that order, the failure's message and type
(`builtins.ValueError`) and `span.type:sql`, the unified service tags, the Agent's host. From the
Agent's stats, every request, kept or dropped, once: the shop's and the cart's resources 40
requests and 8 errors each, the database's 40 and 8 (its spans top-level, so a service's own
requests), its median past its 20 ms; the three in the services' list. The map: shop →
requests → cart (ddtrace's requests integration a service of its own, as Datadog's map shows it)
and cart → cart-db, each at every one of the 40 calls though 20 traces were kept (each its share of
the stats that count its calls: the requests integration's calls out, the database's spans as its
entries), cart → cart-db's 8 errors every one of the database's failures, each service's requests
and errors its stats' 40 and 8; `trace.hits` of one host, the Agent's. Then the same services, their tracers sending straight
to a kiln runner's receiver on the Agent's port (no Agent anywhere): `/info` says `/v0.4/traces`
and `/v0.5/traces` and no client drops; the same 20 kept traces whole and none of the 20 dropped,
every request counted by the runner's concentrator (40 and 8, the database's 40 and 8), `host` the
runner's. (The services flush their tracers and say they are up on control ports of their own,
untraced, so no driver's call is a trace.) 2026-10-20: both pass (136 s, on fedora).

Kiln's own sampler, against a tracer straight to a runner whose target is five traces a second (no
Docker): the tracer keeps by ddtrace's own rule (a trace kept when its id times Knuth's factor,
modulo 2^64, is under its rate's share of 2^64), against the rates the runner answers, sending some
fifty traces a second for 26 s. The service's rate falls to 0.104 and the last ten seconds keep 11%;
the 255 kept stored whole and none of those dropped; `trace.hits` and `trace.outbound.hits` 1,243
each, every trace sent, kept or dropped, and the map's call from the service to its database
(named, inferred) all 1,243 of them, from the trace metrics, its traces read a fifth of them. Its first runs found a fault since fixed: a decoding handed
back to the receivers' spares kept its samples (the owner takes their keys alone), so the
concentrator's task took the next one it was handed for one refused and offered it again, its keys
gone, every second after: only the first bucket was ever written (15 and 179 counted of some 1,200).
Spares are handed out empty now (`Decoded::clear`), and the task keeps what is refused itself, and a
payload refused (429) is counted as it comes again, not twice. Then a runner stopped (SIGTERM)
while its tracer sends, and started again, its port held a moment as it starts (as its last build
holds it until it retires after an upgrade): 200 traces before and 200 after, their spans ending
forty seconds ahead, so one bucket is open across the stop. The runner gone in 0.6 s; its receiver
on its port once let go; every span stored (800), every trace counted once (400 hits, 400 calls
out), the bucket's two writes points of their own (200 and 200, each life's offset within it).
Without the stop's wait (`crate::observe::last_written` answering at once, an A/B), the scenario
fails: spans taken just before the stop are lost, and so is DogStatsD's count below. 2026-10-20:
all four pass (399 s, on fedora).

**Error Tracking** (`crates/kiln/tests/observe_errors.rs`; Docker): a kiln server and a runner, and a
shop (Flask) that fails three ways: a cart's item it will not total (`ValueError` in `Cart.total`),
an order it cannot load (`KeyError` in `load`), a body it cannot parse (json's `JSONDecodeError`,
raised in json's own code, called from `parse`), each failure's message its own (the order's
number, a cart's id in quotes). Traced by OpenTelemetry's own Python SDK (1.45: its spans and the
app's log of each failure, over OTLP/HTTP to the server) and by ddtrace 4.16 (straight to the
runner's receiver). Five failures of each place: nine issues, each place's one of each service and
source (the SDK's spans, its logs, ddtrace's spans), each counting its five, its type as the source
names it (`ValueError`; ddtrace's `builtins.ValueError`; the SDK's span's `json.decoder.JSONDecodeError`
and its log's `JSONDecodeError`), its place `/e2e/shop.py` and its function (`total`, `load`,
`parse`: json's frames passed over), versions `1.0`, for review. ddtrace's Flask says each error on
the request's dispatching span, its entry only a 500: its top-most error carried, one an issue. One
resolved; its version failing five times again: its errors counted (ten), still resolved; version
`2.0` failing five times: a regression, back for review, its versions `1.0, 2.0`, its errors by
version ten and five, its timeline's buckets fifteen, its latest error's stack whole (a
traceback, `in total`), its activity three (its first error, a user's resolve, kiln's reopening
of version 2.0), its fifteen spans found by `@issue.id`. Its first runs found three faults, each
fixed: an OTLP log's attributes were read to a tag's 800 bytes (a Python traceback, its innermost
frame last, lost its frames: now a span's 25,000, a stack's longer); a stored log's attributes were
cut so too (a stack is now kept to 25,000 bytes, both its ends: Python's innermost frame is its
last, every other language's its first); OTLP logs took no `env` nor `version` from their resource
(now as spans do). 2026-10-20: passes (119 s, on fedora). With it, three Error Tracking monitors
(§9) telling a webhook in the scenario: one of the SDK's shop's new issues, made before its first
failures, tells each of its six as it comes (its type, message, place and latest stack in its
message; the issue's page its link) and none of ddtrace's; one issue ignored, its alert ends told
nothing (its event `muted`); the resolved one's end is told, and its regression told again; a monitor
of new issues made after the first failures tells the regression alone; a monitor of impact (more
than nine errors in an hour) tells the two issues past it, the log's issue of the same place (past
it with its old version's errors, never resolved) and the regressed one (once for review again),
none while resolved; eleven notices in all, nothing more two evaluations on. 2026-10-20: passes
(163 s, on fedora).

**Event Hubs** (`crates/kiln/tests/observe_eventhubs.rs`; Docker): Apache Kafka's own broker (4.1,
KRaft, one node) in a namespace's place, taking Event Hubs' SASL PLAIN user `$ConnectionString` and
its connection string, a hub of three partitions; events sent with Kafka's console producer, each
a diagnostic setting's two App Service logs and a metric record. Thirty sent before the
integration (within its lookback): its 60 records stored once each, named as Datadog's forwarder
names them (the six errors by their level; `subscription_id`, `resource_group`, the hub, the
integration's tags), the 30 metric records counted, the connection string never shown; 30 more
gzip'd: 120; the server stopped and started again, 10 more: 140 exactly; killed while idle and
started again, 10 more gzip'd: 160 exactly, its lag 0; the broker restarted while read, 20 more:
200 exactly, the stream failing and recovering, each an event; a wrong connection string said in
the broker's words (`SASL_AUTHENTICATION_FAILED`); the integration deleted, its offsets file gone.
Every count is waited for and then held three seconds more: one record twice would show. Kafka's
4.1 broker answers every version kiln asks (it dropped those before Kafka 2.1). The first runs
found two faults since fixed: a fetch with no records was refused (a partition's least size read as
34 bytes, not 30), and a stream that failed read again what it had offered (its offsets now outlive
its connection). 2026-10-19: passes (128 s; the records come within 2.5 s of being sent).

**Log archives** (`crates/kiln/tests/observe_archives.rs`; the first needs Docker): a kiln server
writing to RustFS (1.0.3, an S3-compatible store in a container, which checks SigV4 by its own
reading: the path as encoded, the payload's hash) and to Cloud Storage and Blob Storage stood in
for, which take an object only with a token their token services gave for their scope or audience
(a service account's assertion verified against its key; a client's secret). Archives in order:
`status:error` to Cloud Storage (made as Datadog's API makes one, its integration's fields kept),
`service:web` to Blob Storage with tags, the rest to S3; 600 records over three hours, and five of
kiln's own events. Each record is in the first archive that takes it, exactly once, and none of
kiln's events; every object is named as Datadog names them, in the folder of its events' hour, after
its newest event's time; every event is its record's (`date`, the reserved attributes, `attributes`
nested, tags only where asked, an `_id` of its time); each archive is `WORKING`, its events as the
API and `kiln.archives.events` count them; nothing was compacted by hand (the store cut two seconds
after the first record waited, `KILN_OBSERVE_CUT_AFTER_MS`). Then an S3 stood in for that lets one
object through and fails the rest (S3's SlowDown): the second of three batches written in part, the
archive `FAILING` with the store's 503 and an event saying so; an archive made and put first in
order (as Datadog's API orders them); the server killed (SIGKILL), the store back, the server
started again, a third batch: the first two batches' every record in the first archive, the
third's in the second, each once, and no object written twice (the part written before the failure
not written again). 2026-10-21: both pass (19 s and 29 s; macOS, arm64). In a browser
(`ui/e2e/observe.mjs`; Chromium and WebKit, a desktop's width and a phone's): an archive made on its
page (its cloud integration chosen, a path, another store's endpoint, tags) writes the records that
come after to an S3 stood in for, as Datadog's archives hold them, its row saying what it wrote; the
store failing (S3's SlowDown), the row says so, its reason first; another put ahead of it from its
row, it edited, both removed; the page within a phone's width. 2026-10-21: passes in all four
(24 s each).

**Rehydration** (`observe_archives.rs`, the first test's second part): what the three archives
wrote, read back. S3's (RustFS) `service:api` records over the whole range: the view holds exactly
the archive's api records, once each, every one as it was sent (its time to the millisecond, its
fields, its nested attributes flat), searched (`status:warn`) and counted by status as the live
logs are; it read every object of the archive and every event in them. Cloud Storage's errors of
the last half hour: exactly those, each with the archive's rehydration tag, read from the objects
of those hours' folders alone; kept three days. Blob Storage's bounded to its first object: that
object's records, and it says its bound stopped it. The stand-ins cut every read half way
(Google's giving the rest when asked with `Range`, Microsoft's sending the whole again), so every
object of the last two is read in two tries; nothing is doubled. The live logs hold none of the
views' records. Then every error rehydrated and the server killed (SIGKILL) once it has read an
object: back, it reads the whole again, its view exactly every error once, and the views read
before are as they were. One removed: its view is gone (404). What is refused, saying why: an
archive not there, a range backwards or past 400 days, a search that does not parse, a bound of
0 GB, a name taken. 2026-10-21: passes (both tests 115 s; fedora, x86-64). In a browser
(`ui/e2e/observe.mjs`; Chromium and WebKit, a desktop's width and a phone's): an archive's errors of
the last day rehydrated from its row (that archive chosen, the last day the range), its row saying
it kept 2 of the 4 records it read; its view searched in the logs explorer (picked there, over its
own range, no live tail) holding those two, a search within it finding one; the live logs picked
again; removed from its row, and from the server. 2026-10-21: passes in all four (9 s each).

**Service Catalog** (`crates/kiln/tests/observe_catalog.rs`): a kiln server as a process of its own,
mirroring a git repository (`[[repos]]`, a `file://` URL) whose `service.datadog.yaml` defines two
services (`---` apart); a third defined through Datadog's API (YAML, the ingest token); a fourth only
seen, its spans sent over OTLP. Each listed with where its definition came from, the seen one with
its requests and its error rate (a quarter of them failed). The repository's first service read by a
monitor and an SLO, deployed (the deployment naming no team takes the catalog's: `checkout`), its
logs carrying a trace id (over OTLP): every rule of Datadog's three scorecards passed; the second,
defined and nothing more, passes its team and its repository alone. What is refused, saying why: a
key that is not one, a definition of a service the repository defines (409, naming it), a field the
schema does not name, a contact that is no email address, Datadog's `GET`s (read through kiln's
API). The server killed and started again: the API's definitions kept, the repository's read again;
the API's removed (204, then 404), the repository's refused (409). 2026-10-22: passes (84 s;
adacachy). In a browser (`ui/e2e/observe.mjs`; Chromium and WebKit, a desktop's width and a
phone's): a service seen by its deployment listed as seen, opened, defined from its page (its team,
an email contact, a runbook link, in the dialog), its facts and owners shown, its rules (a team,
contacts, docs, deployments tracked passed; SLOs failed), the API agreeing; the next deployment
takes its team; its definition removed, it is seen again. Nothing on either page wider than a
phone. 2026-10-22: passes in all four (2–4 s each; adacachy).

**Infrastructure** (`crates/kiln/tests/observe_infra.rs`): a kiln server and a runner as processes
of their own, the runner looking at its host every 2 s; Datadog's Agent 7.84's own payloads of its
host, as it sent them (its host metadata and checks to `/intake/`, its inventory to
`/api/v1/metadata`, the fixtures), and its series (`/api/v1/series`). Both listed as Datadog's list
gives them: the runner from kiln (its own figures once it has looked twice, its platform from its
facts), the Agent's from its series and its word (`sources: ["agent"]`, apps `agent`, `ntp`,
`system`, its CPU 30%, I/O wait 2.5%, load 0.75, memory 40%, fullest disk 55%; its version, its 13
checks, its tags `Datadog`, its cores and OS from its inventory; its newest sample in seconds); its
metadata only when asked. Totals; found by a tag and by a part of its name; ordered by CPU, a page of
one; an order kiln does not have refused. The host whole (its kernel from its inventory, no
containers); one that is not, 404. A monitor of every host's `e2e.infra.hot`; the Agent's host muted
for an hour (a downtime of every monitor scoped to it); muted again refused but with `override`
(its end changed, one downtime still); both hosts hot: the muted one's alert held (its event naming
the downtime, muted), the runner's told; unmuted, its downtime canceled; unmuted again refused.
Datadog's host API refused the ingest token (403, reading and muting). The server killed and started
again: what the Agent said kept. 2026-10-22: passes (117 s; adacachy). In a browser
(`ui/e2e/observe.mjs`; Chromium and WebKit, a desktop's width and a phone's): a host the Agent reports
(its metadata, its checks, its series) listed with its figures, found by its name, opened: its
figures and platform (its busy memory told apart), the Agent's failing check's error, its tags;
muted for an hour from its page, saying why, the API agreeing; unmuted. Nothing on either page wider
than a phone. 2026-10-22: passes in all four (14–18 s each, most of it the list's 15 seconds of
reuse before its series show; adacachy); and the whole suite, 36 scenarios, in all four.

**Incidents** (`crates/kiln/tests/observe_incidents.rs`): a kiln server as a process of its own,
and a service for it to tell on a port of its own (Slack, PagerDuty, Opsgenie and a webhook). An
incident declared with Datadog's body (its severity a field, its services and teams, the
integrations it tells, a first note, its commander; the customers' impact begun five minutes
before): public id 1, its fields and commander as given, declared by the caller; detected five
minutes after its impact began and declared at once, as Datadog's figures say. Told: Slack's
`[Declared] Incident #1: …` with its severity and commander, PagerDuty's trigger at `error` (SEV-2;
`kiln.incident`, its facts, its link), Opsgenie's alert P2 by the same key, the webhook's
`incident.declared`. Found by state and severity (`state:active AND severity:(SEV-1 OR SEV-2)`), by
its fields (`teams:pay*`) and by a word of its title; what matched counted by facet (each of its
services once); `OR` between facets refused. Two todos (one with a due date), one done (counted: two,
one open), one removed (and again, 404). Its postmortem; a second refused (one an incident), a
`javascript:` link refused, a link kept; found by `postmortem:true`. A note; raised to SEV-1 as
Datadog's field (Slack's `[SEV-1]`, PagerDuty's incident raised to critical on its key); stable
(`[Stable]`); resolved with the impact's end 30 s after its detection (time to repair 30 s; the
impact's duration and time to resolve as they follow): PagerDuty's incident resolved and Opsgenie's
alert closed on the one key, Slack's `[Resolved]`; its timeline each of these in order. A monitor
naming `@incident-sev1` and `@slack-ops`: its group alerting declares #2 (`e2e checkout errors on
host:web-1`, SEV-1, `detection_method: monitor`, telling Slack, declared by its monitor), its alert
on its timeline; recovering, its recovery there too, #2 still active; #2 resolved, the group
alerting again declares #3. A viewer's token reads and is refused declaring and noting (403); a
developer's declares (#4, a test, declared by its token). Datadog's incident API refused to the
ingest token (403, reading and declaring). Refused, saying why: a title of 513 bytes, `SEV-9`, an
impact without its scope, a multiselect of one text. The server killed and started again: #1's
timeline the same, its state, todos and postmortem kept, all four there; the next declared #5. The
drill removed, then read and removed again: 404. 2026-10-22: passes (63 s; adacachy). In a
browser (`ui/e2e/observe.mjs`; Chromium and WebKit, a desktop's width and a phone's): an incident
declared from the list (SEV-2, a commander, the webhook at the receiver picked from the
integrations, a first note), then on its page: its first note as Markdown; a note; a todo, `@ada`
its assignee, done; its postmortem attached; its services and root cause edited; raised to SEV-1,
stable, resolved, each on its timeline; found among the resolved by `services:checkout`; the webhook
told four times (its declaring, its severity, its state twice); the API agreeing at each step.
Nothing on either page wider than a phone. A todo done, a state or a severity picked shows at once,
put back if the server refuses it (the first run found the checkbox waiting on the server's answer).
2026-10-22: passes in all four (3–7 s each; adacachy); and the whole suite's other 36 scenarios in
all four, Incidents among Observe's tabs.

**Notebooks and postmortems** (`crates/kiln/tests/observe_notebooks.rs`): a kiln server as a process
of its own. A notebook made with Datadog's body (a heading, a timeseries, a log stream on a time of
its own): id 1, its cells `c1` to `c3`, the timeseries and the log stream each with kiln's drawing
of it beside (the query its own), Markdown without; read back the same. Changed whole: its first
cell kept by its id (its text changed), the graph dropped, a cell added (`c4`), its time a day, its
type cleared (Datadog's `PUT` is whole). A runbook and a template made; listed as Datadog's list
asks: the newest changed first, templates apart; templates alone, a type, a part of the name (case
aside), by name; a page of one without cells or time, `total_count` 3 and `total_filtered_count` 2.
Refused, saying why: a name past 80 characters, a cell of a kind it has not, a type it has not, a
span it has not; a notebook that is not, 404. An incident declared (SEV-2, a commander, its
customers' impact, its services and root cause, a note), a todo, resolved; its postmortem written:
201, of type postmortem, named after it, its incident's; its Markdown with its facts (severity,
commander, services), root cause, impact, action item (the todo, its assignee), timeline (the note,
its resolving), no variable left; on the incident's time; attached as its postmortem (the
notebook's link, its name); a second refused (409), one postmortem made. A template of one's own
(its heading of `{{incident.title}}`, `{{incident.commander}}` and `{{incident.status}}`, a graph on
a time of its own): a runbook named as a template refused (404); another incident's postmortem of
it: `## Search slow, led by  (Active)`, its graph kept on the notebook's time. A viewer reads and is
refused making, removing and writing a postmortem (403); Datadog's notebooks API refused to the
ingest token (403). The server killed and started again: the postmortem the same, all five kept,
the next made 6; one removed, then 404. 2026-10-22: passes (43 s; adacachy). In a browser
(`ui/e2e/observe.mjs`; Chromium and WebKit, a desktop's width and a phone's): a notebook made from
the list, named, typed, a heading and a sentence written and a graph of the seeded latencies by
service added, saved: its heading and its bold drawn, the graph's services in its legend, the API
agreeing; its time changed in place and kept. An incident declared with a note; its postmortem
written from its page: its notebook opened (its name the incident's, its note in it, a link to
its incident), the incident's postmortem it; opened again from the incident's page, in place.
Nothing on either page wider than a phone. 2026-10-22: passes in all four (4.6–5.6 s each;
adacachy); and the whole suite, 38 scenarios, in all four. Looking at its pages found two things the
scenarios could not: the seeded latencies (a point a minute) drew no line on an hour's grid of 15 s
(§11: now drawn across their empty buckets), and a notebook's Markdown went unstyled where no
dashboard had been opened first (its styles were the dashboards' alone).

**Containers** (`crates/kiln/tests/observe_containers.rs`, on a machine with Docker): a kiln server
and a runner as processes of their own, the runner looking every 2 s. Two containers of its own
(`alpine:latest`): one busy under a quota of half a core and 64 MiB; one whose `awk` doubles a
string until the kernel kills it, again every second, under 32 MiB and no swap. The busy one listed
by its name, with its id, engine and image (`docker`, `alpine`, `latest`); its CPU between 0.35 and
0.55 cores, its limits (0.5 cores, 64 MiB), throttled, its processes. The other's OOM kills counted
by Datadog's query (`sum:container.memory.oom_events{container_name:…}.as_count()`), each once: over
20 s the count grows as the kernel's own does (`memory.events`, read in the container), within a
look's kills at either end, and is never more (the kills before kiln's first look are not counted,
as Datadog's are not: a count begins at its first sample); it running on under its 32 MiB. Every container Docker ran before and after 20 s found among those listed
(others' jobs' come and go here; five minikube nodes' pods besides); one found by the first 12 of
its id; an id that is none, 404. The busy one removed: among the stopped (its samples stopped), no
longer among the running, counted. 2026-10-22: passes (125–151 s; adacachy, its load 55–90); with
the kernel's count, passes (159 s). Its first run found no OOM kill in two minutes: the `awk`
appended 32 bytes at a time, copying its string each time, and never reached its limit. The
kernel's count found the check writing its per-look deltas (OOM kills, throttled periods) as
cumulative counters, which `.as_count()` undercounts (counts now). A run whose server did not
answer in a minute found the server's HTTP listener started 25 to 60 s late: as it began, the fleet
loop hashed kiln's own binary (a debug build's 1.25 GB) on a runtime shard, and every task on that
shard waited; it is hashed on the blocking pool first now (§13's servers listen within a second). In a browser (`ui/e2e/observe.mjs`; Chromium and
WebKit, a desktop's width and a phone's): two containers as Datadog's Agent's container check sends
them (`/api/v1/series`, its tags: id, name, image, `containerd`, a pod of `shop`, `burstable`): one
reporting now, near its quota (0.46 of 0.5 cores: 92%, told apart), throttled 150 ms a second, its
working set 180 MiB of 256 MiB, up two hours; one whose samples stopped five minutes before. Found
by `kube_namespace:shop` and a part of its name: its CPU and memory against its limits, its image
and engine (on a phone, a line of its figures); the other not among the running; under Stopped,
counted and said to be stopped. Its page: its figures (0.46 · 92%, 0.5 cores, 180 MiB · 70.3%,
256 MiB, 150 ms/s, its I/O, 2h 00m), its throttling told apart, its pod and QoS class, its tags, its
logs a click away (`@container.id:… OR container_id:…`); its host's page lists it and leads back;
found by the first 12 of its id. Nothing on either page wider than a phone. 2026-10-22: passes in
all four (18.6–19.4 s each, most of it the list made again; adacachy); and the whole suite, 39
scenarios, in all four. Looking at its pages with a node's worth of containers and this machine's
own found what the scenario could not: a CPU of 0.000434 cores written `4.34e-4` (now `<0.001`), a
host's name wrapping in its column, charts' lines named `1: *` and `2: *` (now In use, Limit, …;
a host's page's too), and a y axis cutting `47.7 KiB/s` to `.7 KiB/s` (every chart's axis is now as
wide as its longest label).

**OpenTelemetry's containers** (`crates/kiln/tests/observe_containers.rs`, on a machine with
Docker): a kiln server and a runner as processes of their own, each looking at its host's
containers once as it starts and not again for an hour; then a container of the test's own
(`alpine:latest`, busy under half a core and 64 MiB, writing and flushing a megabyte a second), and
an OpenTelemetry Collector (`otel/opentelemetry-collector-contrib:0.162.0`) reading Docker every
2 s with its `docker_stats` receiver (its CPU limit, throttled time, processes and uptime asked for;
the socket by its group alone, `--group-add`) and sending OTLP to the runner: the Collector its only
source. The container listed in kiln's units: its CPU between 0.35 and 0.55 cores (Docker's
cumulative nanoseconds read as a rate), its limits as Docker has them (0.5 cores, 64 MiB),
throttled, written to, its working set below its limit (Docker's usage less the cache) and no usage
said, its processes and uptime; its name, image (`alpine…`) and engine (`docker`) from the
Collector's attributes. Its page's CPU chart the Collector's series' query
(`sum:container.cpu.usage.total{container.id:…}.as_rate() / 1000000000`), its I/O and working set
drawn; that query's answer, the median of its finished buckets, its cores. 2026-10-23: passes on
its first run (beside the check's scenario: 132 s for both; adacachy).

**Log indexes** (`crates/kiln/tests/observe_indexes.rs`): a kiln server and a runner as processes of
their own. `main` as kiln makes it: alone, of every record, of the main part. Refused, saying why
(400): a name with a capital, a sample rate of 1.5, a reset at 14:30, a filter that does not read,
`main` again; an index that is not, 404. `audit` made (of `service:audit`, a quota of 100 a day
warned at 80%, kept 30 days), then `payments` (of `service:payments`), each of a part of its own; an
order leaving one out refused, then `audit`, `payments`, `main`. `main` given two exclusion filters:
every debug record, and half of `web`'s. A metric from logs of every `web` record. A viewer reads the
indexes and is refused changing them (403). The runner joins, told the indexes as it connects.
Through the server's API, 400 of `web`'s, 20 debug records, 30 of `payments`', and `audit`'s 150
ten at a time; through the runner's OTLP receiver, 20 lines, half of them debug (routed there).
Found: the runner's 10 of info (its debug ones excluded there), no debug record, about half of
`web`'s (150 to 250), `audit`'s 100 to 120 (its quota, a write or two past it), `payments`' 30; a
search of `audit` alone, of `payments` alone (its 30), none of `audit`'s in `main`; of an index
that is not, 400. `audit` over its quota (`is_rate_limited`, its day's count against its limit),
its warning and its reaching each an event, once. Those not kept still counted: every `web` record,
400, by the metric from logs, each once; Datadog's estimated usage, `audit`'s 150 (those past its
quota too), `payments`' 30, `main`'s 440 kept and excluded, 180 of them excluded at the least.
`audit` removed: its records no longer found, its name refused again, the order `payments`, `main`.
The server stopped and started: the indexes as they were (`main`'s two exclusion filters), and a
moment on (the leader states each part's cutoff as it begins to lead) the records of `web`, the
runner's and `payments`' all as they were, `audit`'s still not found. 2026-10-22: passes (141 s;
adacachy). Its first run found the estimated usage never written on the server: it was written as
often as a node looks at its host (the fleet's server once an hour; a node may be told never); it
has its own 15 s now. Reviewing the code before it ran found worse: the leader let go every part of
the logs no index named, `main`'s too, so a leader that had not yet heard the list (the server's,
as it starts) or a list that did not read would have expired every record. Only a part said removed
is let go now (the list keeps them); `payments`' part kept across the restart checks it. In a
browser (`ui/e2e/observe.mjs`; Chromium and WebKit, a desktop's width and a phone's): an index made
from its form (its records' filter, a daily quota of 1,000 warned at 80%, an exclusion filter of
debug records), the API agreeing; moved up from its row until it is first; nothing wider than a
phone; ten of its records sent, five of them debug: the explorer, its index picked, lists its five
of info alone; its quota removed in its form (none on the server); removed from its row, gone.
2026-10-23: passes in all four (3.3–7.8 s each; adacachy); and the whole suite, 40 scenarios, in
all four but one: WebKit's phone failed the metrics from logs' scenario, which left the metrics
explorer as it loaded (its query's fetch cut short, "Load failed", or the next page's load cut
short by its own); the scenario waits for its chart now, as a person would (5 of 5 there since).
Looking at the page with a team's indexes in it found its phone rows cutting their retention short
("30 da…") beside their buttons, and an index past its warning told by a colour alone: the buttons
go beside the name now, its day and time on a line of their own, and past its warning or over its
quota is said in words on both widths.

**Continuous profiles** (`crates/kiln/tests/observe_profiles.rs`): a kiln server and a runner as
processes of their own. Go's own profiles (a CPU and a heap profile, gzip'd as Go writes them) sent
as dd-trace-go sends them: to the runner's tracer port, the tracer's container in its
`Datadog-Container-ID` (taken, 202); a malformed one there refused (400); to the server's intake
without a key refused, with the ingest token and an Agent's `X-Datadog-Additional-Tags` taken. Both
listed: the runner's host and the container on the one it sent on, the Agent's tags on the other,
its start its event's, its pprof profiles and not its `metrics.json`; found by searches of their
tags (`service:checkout env:prod`, `host:agent-host OR host:web-1`, `-env:prod`); their CPU summed
(3 s, 1.5 s each, as Go's own `pprof -raw` sums them), its hottest function by self SHA-256's
block function, nine tenths of it; one service's alone; the heap's allocations of the one that sent
them; the two services compared, path by path the same share; a profile read back byte for byte as
`go tool pprof` reads it; its endpoints. The store's status: two uploads in one segment, both kept.
The server killed and started again: all of it the same, read again from its files. Then real
profilers, in Docker: a Go service with dd-trace-go v2.11.2's profiler (a period of 10 s) hashing
in one loop and allocating in another, uploading to the runner; its uploads listed with the
runner's host and its own tags; its CPU graph its hashing loop's for half of it at the least, its
allocations its allocating loop's. And a Python service with ddtrace 4.16's (libdatadog's
exporter), through Datadog's Agent 7.84 whose profiling proxy points at the server's intake: its
uploads with the Agent's host on each; its CPU time (`cpu-time`, whatever its profile is named) its
hashing loop's for half of it at the least. In a browser (`ui/e2e/observe.mjs`; Chromium and
WebKit, a desktop's width and a phone's): two uploads of a service sent to the intake; its flame
graph's summary (3 s in 2 uploads), the hottest function by self SHA-256's; a frame clicked fills
the width, and the whole comes back; a function found from its row; compared with another
service's upload (another search), its summary and its functions by change; the heap's allocations
(25 MiB in 2); the uploads listed, one read alone (its allocations, 12.5 MiB: the type chosen stays
chosen), its profiles to download; nothing wider than a phone. 2026-10-23: passes (the three live tests one at a time, 189 s; adacachy), and in all four browsers
(3.2–8.0 s each). Their first run found what no fixture of kiln's own had: libdatadog's `"endpoint_counts": null`
refused every Python upload (each read as none now, a capture of a real one kept as a fixture), and dd-trace-go's
own `host` tag (its machine's name), which the runner's does not replace.

Java's (O-34), the same test file: a Java app (`Burn.java`: a recursive Fibonacci its main thread's
CPU; an allocation and an exception every 16th turn of its loop; two threads taking turns on a
monitor, each holding it 20 ms) under dd-trace-java 1.67.1's profiler (Maven Central's jar checked
by its SHA-256; Temurin 21 in Docker; uploads every 10 s, wall-clock samples of every thread, not
of those in a trace alone, its default), uploading to a kiln runner. Its recordings (`main.jfr`, the
JDK's chunk and ddprof's, zstd'd) read into profiles at the runner and sent on as them, two uploads
kept with its tags (`env`, `version`, family `java`); its types listed (`cpu.pprof`'s `cpu`,
`wall.pprof`, `alloc.pprof`'s `alloc_space`, `lock.pprof`'s `delay`, `exceptions.pprof`'s
`exception-samples`); its CPU its recursive function's for half of it at the least, its
allocations its loop's, its lock waits its two threads' (`Burn.contend`), its exceptions where its
loop throws them. 2026-10-23: passes (77 s; adacachy): `Burn.fib` 94% of its CPU, `Burn.main` 93% of
its allocations, 19.4 s of lock waits. Its first recordings, taken to make the fixtures, showed what
the app was not: its main thread waiting on the monitor between turns spent 1% of a core, its CPU
profile the JIT compiler's and the tracer's own start (16 samples in 10 s); the monitor went to two
threads of their own. In a browser: a recording sent to the server's intake as dd-trace-java sends
it (the part `main`, its file `main.jfr`); its flame graph's summary, the hottest function by self
`Burn.fib`; its lock waits chosen, `Burn.contend` the hottest; its exceptions among its types.

**LLM Observability** (`crates/kiln/tests/observe_llm.rs`): a kiln server and a runner as
processes of their own; an LLM app in Docker (a workflow of a session, a planner agent that asks
OpenAI's chat model, calls a weather tool, asks Anthropic's model and embeds its question, then an
evaluation of its answer) against a fake model server on its container's loopback (fixed answers:
the chat 1,200 input tokens, 1,000 cached, 40 out; Anthropic 20 uncached, 1,000 read from its cache,
100 written, 40 of them for an hour), traced four ways, each its own app: dd-trace-py 4.16 with LLM
Observability on and APM on, sending to the runner as to its Agent (its `/info` lists the event
platform's proxy and `meta_struct`), its events riding its APM spans; the same with APM off, its
events through the runner's proxy, each a span of its LLM trace (source `llmobs`), the trace whole
(six steps); OpenTelemetry's openai-v2 instrumentation (2.4b0, the latest conventions opted in)
and OpenInference's (0.1.66), exporting OTLP to the runner. Each step its kind and name; each call
priced to the nanodollar as its instrumentation says its tokens (Datadog's and OpenInference's
chat 129,000: its cache reads at theirs; openai-v2's 204,000, saying none; Anthropic's 1,700,000:
its 1-hour writes at theirs; an embedding's 160, but dd-trace's, its provider `unknown` (a client of
another address) and its model held by two of the catalog's providers: unpriced, and said so); the
evaluation a record of its span, waited for a minute at the most (where it does not come: the
tracer's own logged sends said, and its payload posted straight to the runner, so a run says which
of them lost it); `ml_obs.span` and `ml_obs.span.llm.total.cost` counting every call once.
2026-10-24: passes (80 s; fedora). In a browser (`ui/e2e/observe.mjs`; Chromium and WebKit, a
desktop's width and a phone's): the fixture of dd-trace-py's events sent to the server's intake
(its ids and times its own), and a judge's evaluation of its chat (the tracer's own payload): its
LLM page's models priced ($0.000129 the chat, $0.0017 Anthropic's, $0.00183 in all); its session
chosen, its traces narrowed to it; its newest trace opened, its six steps; the chat's messages,
its evaluation (label, score, verdict and reason) and its match in the catalog; its cost in
`ml_obs` and kiln's LLM dashboard; an OpenTelemetry call whose messages came as its log events
(1.28 to 1.36), read from them; nothing wider than a phone. 2026-10-24: passes in all four
(30–40 s each; fedora). The phone runs found a step's start sent to the search as fractional
milliseconds (a count of nanoseconds is no exact number in a browser: divided, it keeps a fraction)
and refused, so a step's messages and evaluations failed to load as the fraction fell: now whole
milliseconds, as a trace's own records are asked for.

**DORA metrics** (`crates/kiln/tests/observe_dora.rs`): a kiln server as a process of its own,
mirroring a git repository (`[[repos]]`, a `file://` URL) made with known author times: a commit,
two more, a feature merged, the second reverted (`git revert`), a hotfix branch merged. Six
deployments of it sent to Datadog's API with the ingest token (times in seconds, milliseconds and
nanoseconds; one naming the repository by kiln's name for it): each commit's lead time is its
author's time to the deployment's finish to the second, merges left out, none for the first or for a
rollback (16 h, 2 h, 1 h, 2 h); the revert marks the deployment that brought the reverted commit
(recovered in 15 h, a rollforward), the hotfix the one before it (5 h), the rollback to an earlier
commit the deployment after that one (16 h, a rollback). The four keys over ten days (6 deployments,
3 failed, the lead time's median 2 h and recovery's 15 h, their levels), a list by a search of their
fields and in either order, `dora.deployments` and `dora.change_failures` as the query API sums them,
an event each. A deployment marked failed by hand through Datadog's `PATCH`, one refused as its own
remediation; a failure, then its end at Datadog's earlier path (its duration, none going on). What
is refused, saying why: a key that is not one, no service, an id too short, a start after its end,
microseconds. The server killed and started again: the same, from its ledger; a deployment removed,
the one it put right failed still, without its remedy. 2026-10-21: passes (44 s; adacachy). In a
browser (`ui/e2e/observe.mjs`; Chromium and WebKit, a desktop's width and a phone's): a service's
four deployments, the last back to the first's version: the Delivery page says 1 of 4 failed (25%)
and recovery in 2 h, the second's row failed by a rollback; the third marked failed from its row
(put right by the rollback, in 1 h), 2 of 4 then, the API agreeing; then not failed again. Nothing on
the page wider than a phone. 2026-10-21: passes in all four (2–3 s each; adacachy).

**Sensitive Data Scanner** (`crates/kiln/tests/observe_scanner.rs`): a kiln server and a runner as
processes of their own; a scanning group made through the API before the runner joins (it is told
the group as it connects): cards replaced, emails hashed, AWS key IDs replaced, a password given its
value replaced, an order number of a pattern of its own partly hidden (its group `v`, its own tags).
What is refused, saying why: a pattern that does not compile, a standard pattern not there, a name
taken. Records through the server's API, the runner's OTLP receiver (decoded on the runner) and a
file the runner collects (a text line, a JSON line, a line with nothing in it) are stored as the
rules make them: the email's hash the same on both machines and the one the scanner's test answers;
each record tagged with what matched (`sensitive_data` holding every matching rule's, joined), a
rule's own tags; the line with nothing in it as it came. No raw value is found by searching for it,
in a message or an attribute. A rule turned off: the runner keeps cards after it; the group removed:
an email after it is stored as it came. 2026-10-21: passes (67 s; fedora). In a browser
(`ui/e2e/observe.mjs`; Chromium and WebKit, a desktop's width and a phone's): a group made from its
page, its filter a service, a rule of the library's Visa cards put in a dialog (a string in its
place, `[card]`) and one hashing emails; a message tried on its rules before it is kept, coming back
with its card replaced, its email's hash and both rules' tags; the group as the API keeps it; a
record of that service taken after it stored with its card replaced, one of another service as it
came; its row in the list (its rules, on); removed from its row, and from the server. Nothing on
either page wider than a phone. 2026-10-21: passes in all four (4–6 s each; adacachy).

**SLOs** (`observe_live.rs`): requests counted a minute for three hours ending at a five-minute
edge, a thousandth failing but in the last 70 minutes, when half do, and a latency high then; a
ratio SLO of them and a time-slice SLO of the latency. Its SLI, budget left and burn rates come out
as the workbook reckons them, exactly (the hour's 50, all of it bad; the six hours' 19.5); 22 of 36
five-minute slices met; a monitor `slo("…").burn_rate("1h", "15m") > 14.4` alerting at the lesser
window's 50, the SLO's page listing it; the SLO not deleted while the monitor reads it. 2026-10-18:
passes (21 s). In a browser: an SLO made from the form over counts of which 0.5% fail, a target of
99 warned under 99.6: WARN, its SLI 99.500%, half its budget left and every burn rate 0.5, the page
and the API alike; its two burn-rate alerts added from its page, their queries the workbook's for
seven days (3.36 over 1 h and 5 m, 1.4 over 6 h and 30 m), each evaluated to 0.5 and OK; its
deletion refused while they read it; its target lowered to 99.4 and its warning taken off: OK, a
sixth of its budget left; then deleted, and gone from the list. A time-slice SLO made from the
form, minutes of a latency the last twelve of 120 over 300 ms: 108 met, 90%, breached against 95,
its budget spent twice over (−100%). 2026-10-18: both pass in Chromium and in WebKit as a phone.

**Logs collected** (`observe_collect.rs`): a server and a runner; a source of the runner's files,
its lines JSON and, one in three, text (the rows the runner writes where it reads them, mixed with
records in its batches: §8.1), each numbered, every tenth a health check a rule leaves out, a token
in each a rule masks; 500 lines there before it, read from the beginning as it says; then the file renamed
(logrotate's `create`) with lines written to both the old and the new, copied and truncated
(logrotate's `copytruncate`), and 4,000 lines written at 2,000 a second while the runner is stopped
(SIGTERM) and started, then 4,000 more while it is killed (SIGKILL) and started. Every line's records
counted by its number: none lost, none of those left out stored, none repeated through the renaming,
the copy and the stop; the kill repeats what was written in its last second (0 to 477 of the 4,000
over six runs). A record as stored: masked, its level, its host, its file's path, the source's tag;
a text line's alike, from its row (2026-10-20: passes with the text lines, a text line counted by the
number its message says, its group's search at a time).
2026-10-18: passes (each run 2 min). Then a pod's CRI log (a long line in parts, then JSON on
stderr), a Docker container's json-file log (its name, image and Compose service from its config)
and the journal through a stand-in for `journalctl` that keeps cursors as the real one does: the
parts joined, the records named as their pods and containers are, the journal's levels its
priorities'; the runner stopped, an entry more meanwhile, and started: read from after its cursor,
none twice. Passes. Syslog: a runner listening on UDP and TCP; RFC 5424 with structured data and RFC 3164
by datagram, then over TCP two counted messages in one write and one split across two writes,
and messages ended by a newline and by a NUL: each stored once with its host, program, process,
facility, level (a priority's: notice is info, critical fatal) and structured data, the sender's
address, the source's tag; one the rules leave out counted as left out, none dropped. Passes
(2 min). In a browser: a source added from the form, tried on the server first (its file
matched, its last lines read, a token masked), its records found by a search once each, its row
saying it reads, then removed; and a syslog source made from the form, its address tried, a
datagram sent there found as a record with its program, level and process; in Chromium and in
WebKit as a phone.

**The unified log and the Event Log** (`observe_unified.rs` on a Mac of the fleet;
`observe_collect.rs` on Linux): a server and a runner on the Mac, a `unified_log` source of the
runner's reading what `logger` writes (its predicate the run's own lines), every line numbered: 50
lines read as they are written, each stored once with its process and the source's service and
tags; the runner stopped, 50 lines written meanwhile, started again (they are read by `log show`,
the stream started first): none lost, none repeated; then 300 lines at 100 a second while the runner
is killed and started: none lost, 15 stored twice (in flight). Passes (70 s). The run found that a
read waiting for a process's output to end can wait forever: on macOS a pipe is made and then marked
close-on-exec in two steps, so a process the server starts meanwhile may hold its writing end open;
a read now ends when the process has exited and its output has been quiet a moment. The Event Log's,
against a stand-in for `wevtutil` (a channel's events one a line in a file, its two queries
answered): 20 events there before the source, read from the channel's start; 40 more; 40 written
while the runner is stopped; 300 written while it is killed: every event stored once (a kill
repeated none of the 300), its message rendered, its level, provider, event id and data as
attributes; then the channel cleared and five new events written: read from its start. Passes
(2 min). On Windows itself (2026-10-21, Windows 11, x86-64; kiln cross-built as the release is, a
server of its own on the machine, its user not an administrator): an `Application` source made,
two events written by PowerShell's `Write-EventLog` (an error, an information), both stored within
2 s, each its record id, rendered message, provider, event id, level and data; the source counted
those two and nothing before them. The first run found that a command's last line without its
newline was dropped as too long to read: the newest record id came out 0, the source read the
channel from its start (25,074 records of a channel of 3,455 in 100 s) and, catching up, took the
channel for cleared and read it again, over and over. The stand-in had ended each event with a
newline; it now writes them as `wevtutil` does, and the scenario passes with it (115 s).

**Pipelines** (`observe_collect.rs`): a server and a runner; a pipeline made through the API (a
grok rule, an arithmetic processor, a status remapper) tried on records (a line its rule reads
parsed, one it does not left as it is), then a record posted to the server stored parsed; a log
source of the runner reading nginx's access log, `source:nginx`, its 20 lines stored parsed on the
runner by kiln's own pipeline (each its method, its URL's query, its 5xx an error, found by
`@http.status_code:>=500`); that pipeline turned off (kiln's own are not removed: 400) and a line
after stored as it came, on the runner too. Passes (2 min). In a browser: a pipeline made on its
page, its grok rules tried on a sample in the processor's dialog, the whole tried on a line, kept, a
record posted after stored parsed; kiln's nginx pipeline turned off and on from its row; the
pipeline removed; in Chromium and WebKit as a phone. The two bugs these found: a processor's dialog
kept inside the pipeline's form saved the pipeline before the processor was in it (a portal's
submit still reaches the form React holds it in), and the source tried guessed once from the filter
as it was when the page opened.

**Metrics from logs** (`observe_logmetrics.rs`): a server and a runner; four metrics (one in
Datadog's own envelope): errors by status code, a latency's distribution with percentiles by
environment, a size's without, every record by service; a name of kiln's refused (400), a change of
what one computes refused (400), and a name whose series the API wrote refused (409). Then 80
records a second to the server's intake for 40 s, and lines to a file the runner reads beside them:
each window's points as the records sent make them (counts, sums, the least and most exactly; p50
and p99 within the 1.09% bound of the value of rank q(n - 1); a distribution without percentiles
answering its average and no percentile), and each as log analytics counts the records stored, the
runner's too; a value above what was sent fails at once (a record counted twice never comes
right). Then 400 records late for two windows counted a minute before, and 50 records 25 minutes
back; then the server stopped and started, 300 more records late for windows before it sent with
the first new ones (as an exporter back from a restart sends what it held), and 15 s more. Every window holds what was sent through all of it, and the records 25 minutes back are
stored and never counted. Passes (4.5 min): each of the 125 windows counted once on time (the
settle learned at 2 s), exactly the two late windows counted again, and after the restart every
window of the twenty minutes counted again from the store, alike. In a browser: a metric tried on
the last fifteen minutes' records (their count and groups), made, the records after it counted (as
the API's query says, and its row), charted in the explorer from its row, and removed; in Chromium
and WebKit as a phone. What the runs found: a batch holding records old and new reached every
window between them (102 of them counted again, each read whole), now each batch's own windows; one
late batch held every window back five minutes (the settle was the most any batch took), now the
99th percentile; the entries a server applies again as it opens looked minutes late (the settle 30
s after a restart), now told apart as the live tail tells them; and the log intake kept a nested
attribute as one JSON text where Datadog's reads `@http.status_code`, now flattened as theirs. Then
(2026-10-20) a write came to hold several items of records (a segment each, §12.17), and how late a
batch came was taken of an entry's first item alone (the rest of it "applied before"): the restart's
late records, their own segment, made a write look 23 s late and the settle 29 s. A write's records
are one batch again whichever items hold them, as when one item held them all (spans' segments had
the same flaw already); a store test holds it, and the late records go with new ones, so the
scenario does not hang on whether the server's batcher writes them alone.

**OTLP over gRPC** (`observe_live.rs`): a server and a runner (its gRPC receiver on a port of its
own); OTLP messages made from OTLP/JSON, each sent as a gRPC call over HTTP/2 in the clear: a gauge
to the server with its token, another gzip-compressed, a third to the runner's loopback with none,
log records to the server: each answered with the empty response and `grpc-status: 0`, and each
stored as sent; then without a token (16), a message cut short (3), bytes that are no OTLP (3, said
why) and a compression it does not know (12). Passes (67 s).

**Downtimes** (`observe_live.rs`): a server and a webhook; a monitor of two hosts, and a downtime of
the monitor's tag scoped to host `a` for 40 s, asking to be told of alerts as it expires and to keep
the first recovery after quiet. Both hosts alert: `b` is told, `a` is not (its alert an event marked
with the downtime, `muted:true`); the monitor shows the downtime. As it expires, `a`, still alerting,
is told once (no more twelve seconds on); both recover: `b`'s recovery is told, `a`'s is not. A daily
downtime begun a minute before, an hour long, holds at once; canceled, it tells nothing (it asked of
expiring only); both are listed after, the first ended, the second canceled. Passes (100 s). In a
browser: a downtime made in the form for a monitor's group (a time to end), holding at once, shown on
the monitor's page, canceled from its row; a weekly one made with the builder, said in words, its
rule `FREQ=WEEKLY;BYDAY=SA,SU` in Europe/Paris; in Chromium and WebKit as a phone.

**The API as its document says** (`observe_live.rs`): a server; points, a distribution, records
and an event posted; then every route that answers without a service outside, each asked as a
client asks it (queries, a heatmap, a calendar rollup, a query in the query string, a query that
does not parse; names and tags; log search, analytics with and without buckets, facets, patterns;
events; the status; a monitor made, read, changed, validated, muted, unmuted, listed, missed and
deleted; a dashboard the same; an integration, a scrape target and a cloud kept, listed (no secret
shown) and removed, one removed twice), its answer checked against the schema
`GET openapi.json` gives that route and status, a JSON Schema checker that is shown first to find a
field of another type, one missing and a value of no enum. 2026-10-18: passes (23 s).

**Distributions** (`observe_live.rs`): three hosts' latencies posted as Datadog's distribution
points (2,000 each, log-normal), a fourth's as an OTLP exponential histogram (delta, 3,000) and a
fifth's as a cumulative one exported twice (1,000, then 2,500 since it began): `count` all 11,500
once; p50, p99 and p99.9 across them within 1.09% of the exact percentiles, each host's p99 within it
of its own; the average exact; a heatmap of it holding every value once in 40 rows at the most.
2026-10-18: passes (18 s). And Datadog's threshold queries through the API: each host's values at
or past the 90th percentile of all (`count(v: v>=x)`) within the one bucket the bound falls in, and
with its complement (`v<x`) every value. 2026-10-20: passes (16 s).

**Prometheus's scrapes** (`observe_clouds.rs`): an exporter served by the test (a counter climbing
a hundred a second, a gauge), and four targets: one the server reads, one the runner reads, one
every machine reads, one that asks for a token. Each is read where it says (the store's `up` by job
and host; each machine's own report), its samples tagged `job` and `instance` (the sample's own
`job` kept as `exported_job`) and the target's tags; the counter read as a rate is 90–110 a second;
the exporter failing makes `up` 0 and is said once, its recovery too; a target deleted is read no
more, by the runner either; the token is sent and never shown. 2026-10-18: passes (109 s).

**A soak** (`observe_load.rs`, `KOBS_SOAK_SECS`, 300 unless said; hours for a soak worth the
name): a server and a runner, 200,000 points a second held steady (4 senders paced, OTLP protobuf,
gzip, half to the runner's receiver), 2,000 log records a second through the API, a dashboard's two
queries every 2 s and a monitor every 10 s. Every point and record taken is stored once, the
replicas agree, the store cuts (about once a minute) and merges and its log compacts throughout, and
each process's memory holds a level: its highest in the run's last third within a tenth of its
second's. 2026-10-18, 300 s: 60,008,000 points and 600,200 records, all stored; the server 82–95
MiB resident (72–80 anonymous), the runner 61–69 (50–56), the head 1–9 MiB between cuts, 1–3 blocks;
the server's highest 94 MiB in the second third and 95 in the last; a request p50 0.24 ms, p99 2.8
ms; a dashboard query p50 7.9 ms, p99 349 ms (every host's 15 minutes, read raw: some 60 M points;
a rollup tier, as Datadog keeps, is what would make it fast).

Two hours (`KOBS_SOAK_SECS=7200`, a release build, 2026-10-18 on a 32-thread Linux machine of the
fleet): 1,439,493,000 points and 14.4 M records, every one stored once, none dropped. The server
held 257–258 MiB resident (204–205 anonymous) from its first hour to its last, the runner 141
(102); where the server's memory lay, said every ten minutes, did not move: its heap 1 MiB, the C
library's 33 arenas 158–169 MiB (the largest 50), the rest 39–41. The same two hours in a debug
build climbed to 785 MiB: a slower server's backlogs, not a leak. The run found two faults in its
last check, the replicas compared: a full digest gathered every record with a search, sorted as it
went (14.4 M of them: hours, and gigabytes), and each request for one started another beside those
still running. A digest now visits each record once and keeps none (the same sum, so replicas of
two versions agree: a test against the digest of before), one at a time a replica, a request
meanwhile answered by the next. It still holds the replica's writes while it reads (some seconds a
billion samples): an operator's check, not a routine.

The two hours again with that fix (2026-10-19, the same machine, a release build): 1,438,389,000
points (199,776 a second) and 14,398,400 records, every one stored once; 45,000 points answered 503
and sent again (the senders behind 17 times a second, of 200,000 points); the replicas agreed at
index 49,913 (`dd3c9c946a742a6d`), their digests taken in the run's last check in seconds. The
server's highest 281 MiB in the second third and 287 in the last (2% more, within the tenth), the
runner's 140 and 132; where the server's lay, every ten minutes: its heap 1–7 MiB, the arenas
185–214 MiB (moving both ways, the largest 52–59), the rest 39–41. A request p50 0.70 ms, p99 2.58
ms; a dashboard query p50 16.5 ms, p99 2.16 s, against 8.5 ms and 693 ms over the run's first ten
minutes (the ten-minute run's): what a query waits for in a long run (a cut's hold, a merge's
blocks) is the next thing measured.

Measured (§12.19), it was no wait. Twenty minutes each, 2026-10-19, the same machine, its other load
varying (12 to 29 of 32 cores idle):

| | by host p50 | by host p99 | one series' slowest: waited for the store | a dashboard query p99 | a request p99 |
| --- | --- | --- | --- | --- | --- |
| before | 648 ms | 1,172 ms | 4.3 ms | 1,147 ms | 0.99 ms |
| files written beside queries (§4.5) | 800 ms | 1,606 ms | 0.00 ms | 1,573 ms | 1.16 ms |
| and repeats decoded a word at a time, series read on helpers | 71 ms | 421 ms | 0.00 ms | 307 ms | 4.46 ms |

Every run stored every point and record once, its replicas agreeing, its memory level. The last
run's request p99 rose (4.5 ms against 1.0): the machine was the busiest of the three (12 cores idle,
2 GB of memory free), and a wide query now takes helper threads in bursts. Measured apart (§12.19,
each request told apart by whether a wide query read while it was taken): both. The machine's other
load moves every request's tail; the helpers move only the writes taken beside a wide query, which
seven helpers (now the default on 32 threads) hold to 1.1–1.75 times the others' p99.

The soak also watches the server answer (every 500 ms, `/healthz` and observe's status, 5 s a
request at the most): one that takes over 2 s has the server's threads' stacks taken (`eu-stack`,
else `gdb`; `KOBS_STACKS` says where), and one that is not answered fails the run with them. One
run of twelve (2026-10-19, before the watchdog) had a status request unanswered for 30 s at 502 s;
the six runs since, watched, answered every request within 3.84 ms, and three more of 20 minutes
each (2026-10-23, the machine at a load of 35 to 58 from other sessions) within 401 ms, 44 ms and
18 ms: no stall in nine. What held it is not yet known: the next time, the stacks say. Those three
found another wait: the wide dashboard query (`sum:load.v{*} by {host}`, §12.19), its slowest
hundredth waiting 469 ms, 127 ms and then 1.71 s for the store (p99 944 ms, 577 ms, 1.66 s; the
one-series query's waited none), late in each run, as the store holds more: its rounds (64 series a
hold) each wait behind the writes queued for the store, a wait §12.19 measured as none on
2026-10-19. A snapshot of the series it reads, taken under one hold, would end it (O-10, next). Where the server's anonymous memory lies is said at each third's
end, so a climb the memory check finds names what grew.

What floods leave (§12.11) is a high-water mark, not a leak: after one, the server held some 300 MiB
more than before, most of it in the C library's per-thread arenas (32 of them, from
`/proc/<pid>/smaps`), while its store held under 15 MiB; `GLIBC_TUNABLES=glibc.malloc.arena_max=2`
held it to 90 MiB more, a fixed `mmap_threshold` to 164. kiln cannot set either from inside without
`unsafe` (and a variable it set would reach the jobs it runs): an operator who runs floods on a small
machine can.

## 14. References

- Pelkonen et al. Gorilla: a fast, scalable, in-memory time series database. VLDB 2015.
- Masson, Rim, Lee. DDSketch: a fast and fully-mergeable quantile sketch with relative-error
  guarantees. VLDB 2019.
- OpenTelemetry. Metrics data model; OTLP specification (exponential histograms, temporality).
- Bentley, Yao. An almost optimal algorithm for unbounded searching. IPL 1976.
- Demaine, López-Ortiz, Munro. Adaptive set intersections, unions, and differences. SODA 2000.
- Bloom. Space/time trade-offs in hash coding with allowable errors. CACM 1970.
- Kirsch, Mitzenmacher. Less hashing, same performance: building a better Bloom filter. ESA 2006.
- Abadi, Madden, Ferreira. Integrating compression and execution in column-oriented database
  systems. SIGMOD 2006.
- Ongaro, Ousterhout. In search of an understandable consensus algorithm. USENIX ATC 2014; Ongaro,
  Consensus: bridging theory and practice (thesis), 2014 (§6.3 client sessions, §6.4 ReadIndex).
- TiKV, `components/raftstore/src/store/snap.rs`: a region's snapshot files sent apart from the
  Raft message that names them, which is stepped once they are received.
- OpenTelemetry Collector, `processor/batchprocessor`: batches sent at 200 ms or when full.
- Steele, Lea, Flood. Fast splittable pseudorandom number generators. OOPSLA 2014 (SplitMix64).
- Iglewicz, Hoaglin. How to detect and handle outliers. ASQC 1993.
- Hochenbaum, Vallis, Kejariwal. Automatic anomaly detection in the cloud via statistical learning.
  arXiv 1704.07706, 2017.
- Winters. Forecasting sales by exponentially weighted moving averages. Management Science 1960.
- Lavin, Ahmad. Evaluating real-time anomaly detection algorithms: the Numenta Anomaly Benchmark.
  ICMLA 2015.
- Steinarsson. Downsampling time series for visual representation. MSc thesis, Univ. of Iceland 2013.
- He, Zhu, Zheng, Lyu. Drain: an online log parsing approach with fixed depth tree. ICWS 2017 (log
  patterns, O-6).
- Forsgren, Humble, Kim. Accelerate: the science of lean software and DevOps. IT Revolution 2018
  (the four keys; software delivery performance by cluster analysis).
- DORA (Google Cloud). Accelerate State of DevOps Report 2024, its software delivery performance
  levels (p. 13: four clusters, their lead time, deployment frequency, change failure rate and
  failed deployment recovery time).
- Datadog. DORA Metrics: calculation and change failure detection (docs.datadoghq.com/dora_metrics),
  and its API (`/api/v2/dora`).
