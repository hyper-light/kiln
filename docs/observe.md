# Observe: kiln's own metrics, logs, events, monitors and dashboards

kiln watches what it runs without Grafana, Prometheus or a SaaS: one engine, inside the kiln binary,
that stores metrics, logs and events in a columnar store replicated across runners with hyper-raft,
answers a query language in milliseconds on any replica, evaluates monitors and notifies Slack,
Discord, PagerDuty, Teams or a webhook, and draws dashboards in kiln's UI. It takes OpenTelemetry
(OTLP) from anything, and kiln CI, kiln CD, GitHub and the clouds (AWS, GCP, Azure) feed it too.

This document is the design and the record: what is built, how it is measured, and why each number
is what it is. A change in behaviour updates it in the same commit.

## 0. Status

| Milestone | What | State |
| --- | --- | --- |
| O-1 | Engine core: Gorilla chunks, symbols, series index and selection, head with late writes, write batches; measured (§12.1–12.3) | done |
| O-2 | Columnar blocks on disk, compaction, retention; restart from disk; measured (§12.4) | done |
| O-3 | Replication: the store as a hyper-durable state machine; kiln carries Raft between nodes | planned |
| O-4 | Query language: metrics done (parser without recursion, evaluator, §12.5); logs and events with O-6; the HTTP API with O-3 | in progress |
| O-5 | Ingestion: kiln itself, runners' hosts, OTLP/HTTP (protobuf and JSON), kiln CI and CD | planned |
| O-6 | Logs: columnar log blocks, search, facets, live tail, patterns | planned |
| O-7 | Monitors: threshold, change, anomaly, forecast, logs, events, CI/CD, composite; notifications | planned |
| O-8 | Dashboards and explorers in the UI | planned |
| O-9 | Integrations: Slack, Discord, Teams, PagerDuty, webhooks, GitHub, AWS, GCP, Azure | planned |
| O-10 | Load, soak and fault scenarios end to end; profiles; allocation and fault passes | continuous |

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
                                        |  write batch (postcard)
                                        v
                       obs group: hyper-durable replica on each member
                       (Raft log = the write-ahead log; leader orders writes)
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
| distribution | exponential histogram per point (§4.4) | percentiles | ExponentialHistogram; Histogram (explicit buckets, re-binned) |

**Logs.** `ts`, `severity` (OTel's 1–24), `service`, `host`, `source`, `message`, `attributes` (a
flat map of `@`-addressable fields), `trace_id`, `span_id`.

**Events.** `ts`, `title`, `text`, `source` (kiln, kiln-ci, kiln-cd, github, aws, ...), `tags`,
`priority`, `alert_type` (info, success, warning, error), `aggregation_key`.

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
symbol table's sorted value index per key.

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

A distribution point is a base-2 exponential histogram, OTel's (`scale`, `zero_count`, positive and
negative bucket arrays). It is DDSketch's logarithmic mapping (Masson, Rim and Lee, VLDB 2019) with
`γ = 2^(2^-scale)`: relative error `(γ−1)/(γ+1)`, 0.54% at scale 6. Histograms merge exactly by
downscaling to the coarser scale, so percentiles across hosts and time are mergeable, which averages
of percentiles are not. Explicit-bucket histograms are re-binned at query time with the stated error.

### 4.5 Head and blocks

The head holds recent samples in memory: each series' open chunk and its closed chunks. When the
head spans `BLOCK_SPAN` of time or holds `HEAD_BYTES`, it is cut into an immutable **block** file:

```
block = header | symbols | series table | postings | chunk index | chunk data | footer(crc32c)
```

Columnar within the block: the series table, postings and chunk index are each one contiguous,
sorted column; a query reads the index columns once and then only the chunks it needs, with
positioned reads (`pread`), letting the page cache keep hot blocks. Blocks are compacted into larger
ones by time (levels of spans, Prometheus's TSDB layout), and dropped past retention. A manifest
(rewritten by rename) lists the live blocks and the Raft point the head was cut at.

### 4.6 Logs and events

Log blocks are column chunks of `LOG_BLOCK_ROWS` rows: `ts` delta-encoded varints; `severity`
run-length encoded; `service`, `host`, `source` dictionary-encoded and run-length encoded; `message`
and `attributes` compressed together per block; trace ids raw. Each block carries the minimum and
maximum of `ts`, the dictionaries, and a Bloom filter over the message's tokens sized for
`LOG_BLOOM_FPR` (Bloom 1970; `k = (m/n) ln 2` hashes with Kirsch and Mitzenmacher's double hashing,
2006), so a search reads only blocks that may match and the time range they cover. Events are a
small log of their own, kept whole.

## 5. Replication

The store on each member is a `hyper_durable::StateMachine`. The Raft log is its write-ahead log:

- **apply** decodes a write batch and appends it to the head;
- **persist** cuts the head into a block, fsyncs it and the manifest, and moves **durable** to the
  batch it was cut at; the log is compacted behind it;
- **image** and **install** carry the manifest and the blocks to a member that lacks the log;
- **acts_at_start** is false for every entry: a replica acts on nothing at start before its group.

Members are kiln nodes (the server and runners that keep state). The server names the voters
(itself and up to two runners, so one may fail) and the learners (every other runner that wants a
local copy for fast queries). Raft's messages travel on kiln's QUIC links: a runner's to another
runner through the server, as reach's frames do. Writes go to the leader (a node that is not
forwards its batch). Reads are local: a query answers from the replica it runs on, as fresh as its
applied index, which the response states; `consistency: "leader"` waits for a ReadIndex
(Ongaro, thesis §6.4) first.

## 6. Query language

One language for metrics, logs and events. Metric queries follow Datadog's syntax, which kiln's
users know:

```
query      := expr ( "," expr )*
expr       := term (("+" | "-") term)*
term       := factor (("*" | "/") factor)*
factor     := number | "(" expr ")" | function "(" args ")" | metric_q
metric_q   := [agg ":"] metric ["{" filters "}"] ["by" "{" tags "}"] modifier*
agg        := avg | sum | min | max | count | p50 | p75 | p90 | p95 | p99 | pNN.N
filters    := filter ("," filter)*            // AND; "*" = all
filter     := ["!"] key ":" value             // value: exact, wildcard "web-*", or (a OR b)
modifier   := ".rollup(" fn ["," seconds] ")" | ".as_count()" | ".as_rate()" | ".fill(" how ")"
function   := rate | per_second | per_minute | derivative | diff | abs | log2 | log10 | cumsum
            | top | bottom | timeshift | moving_avg | ewma | anomalies | forecast | outliers
```

Examples:

```
avg:system.cpu.user{host:web-*} by {host}
sum:kiln.ci.job.finished{status:failure}.as_count().rollup(sum, 3600)
p99:kiln.http.request.duration{route:/api/*} by {route}
100 * sum:kiln.ci.job.finished{status:failure}.as_count() / sum:kiln.ci.job.finished{*}.as_count()
anomalies(avg:system.load.1{host:adacachy}, 'seasonal', 3)
```

Log search follows Datadog's log syntax: `service:web status:error @http.status_code:>=500
"connection reset" -host:old-*`, with `AND`, `OR`, `NOT`, parentheses, wildcards and ranges, and
log analytics `count() by {service}` over a search. Events take the same search syntax.

## 7. API

Under `/api/observe/v1/` (all JSON; the UI uses nothing else):

| | |
| --- | --- |
| `POST query` | metric query → series (`from`, `to`, `step` or a point budget) |
| `POST series` | ingest points (JSON) |
| `POST logs/search`, `GET logs/tail` (websocket) | log search, facets, live tail |
| `POST events`, `POST events/search` | post and search events |
| `GET/POST/PUT/DELETE monitors[/id]`, `POST monitors/{id}/mute` | monitors |
| `GET/POST/PUT/DELETE dashboards[/id]` | dashboards |
| `GET/POST/PUT/DELETE integrations[/id]`, `POST integrations/{id}/test` | notification and source integrations |
| `GET metrics`, `GET tags` | metric names, tag keys and values (autocomplete) |
| `GET status` | the group: members, leader, lag, block counts, sizes |

OTLP/HTTP at its standard paths, `/v1/metrics` and `/v1/logs` (protobuf and JSON), on the server and
on every runner at `127.0.0.1:4318`, so an app on a runner sends to its local runner as it would to
a collector.

## 8. Ingestion

- kiln itself: CI runs, jobs, steps (durations, queue waits, outcomes), CD syncs and deploys,
  runners' slots, the server's own HTTP, QUIC and process figures.
- Runners' hosts: CPU (per state), memory, disk and network, load, processes, every
  `HOST_SAMPLE_INTERVAL`.
- OTLP from applications.
- GitHub webhooks (pushes, pull requests, workflow runs, deployments) as events and metrics.
- AWS CloudWatch, GCP Cloud Monitoring and Azure Monitor pulled on a schedule (§10).
- Prometheus text endpoints scraped, for anything that only speaks that.

## 9. Monitors

A monitor is a query, a condition and where to say so. Kinds: metric threshold; change (against a
time shift); anomaly (a seasonal baseline learned from the series, §9.1); forecast; log count;
event; kiln CI (failures, queue time); kiln CD (sync, health); runner offline; composite (a boolean
of monitors). States: OK, WARN, ALERT, NO DATA, with recovery thresholds so a monitor does not flap,
a renotify interval, mute windows and downtimes. Notifications are rendered from the monitor's
message template with the triggering values and links back to kiln.

### 9.1 Anomalies and forecasts

Learned, not configured: the season is found from the series' autocorrelation (the lag of its
strongest peak among the candidate periods hour, day, week); the baseline is the seasonal median
and the scale the median absolute deviation (Iglewicz and Hoaglin 1993) of the deseasonalised
residuals; a point is anomalous past `k` scales, `k` chosen per series from the target false-positive
rate measured on its own history; extreme points are found with Seasonal Hybrid ESD (Hochenbaum,
Vallis and Kejariwal 2017) for the batch views. Forecasts use additive Holt-Winters (Winters 1960)
with its three smoothing parameters fitted by minimising one-step squared error on the history. The
detectors are scored on the Numenta Anomaly Benchmark's labelled data (Lavin and Ahmad 2015); the
scores are in §12.

## 10. Integrations

Notifications: Slack (incoming webhook, Block Kit), Discord (webhook embeds), Microsoft Teams
(incoming webhook), PagerDuty (Events API v2, trigger and resolve by dedup key), generic webhooks
(signed with HMAC-SHA256 over the body). Sources: GitHub (webhooks verified by
`X-Hub-Signature-256`), AWS CloudWatch (`GetMetricData`, Signature Version 4), GCP Cloud Monitoring
(`timeSeries.list`, a service account's JWT, RS256), Azure Monitor (metrics REST, OAuth 2.0 client
credentials). Each is a few hundred lines against its published API, on the HTTP client kiln has.

## 11. UI

An Observe area: dashboards (a grid of widgets: timeseries, query value, top list, distribution and
heatmap, table, log stream, event stream, monitor summary, note), with template variables, a time
range, live refresh; metric, log and event explorers; monitors with their history; integrations.
Charts are uPlot, already kiln's; a series sent to a chart is reduced to the chart's width with
largest-triangle-three-buckets (Steinarsson 2013), so a week of 10-second data draws in milliseconds.

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
| decode into a query's columns | 6.37 ns a sample; no allocation; no page fault |

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

2026-10-18, same machine: `KOBS_CORPUS=<corpus> cargo bench -p kiln-obs --bench query`. Each query
over the corpus's whole day (a grid of at most 1,200 points a series), fifteen runs.

| query | series | samples | p50 | p99 | ns a sample | allocations |
| --- | --- | --- | --- | --- | --- | --- |
| `avg:node_load1{*} by {instance}` | 3 | 8,640 | 0.13 ms | 0.16 ms | 14.5 | 60 |
| `p99:node_load1{*}` | 1 | 8,640 | 0.14 ms | 0.17 ms | 16.4 | 50 (a bucket's own list at first: 760) |
| `sum:node_cpu_seconds_total{mode:idle} by {instance}` (a counter's rate) | 1 | 2,880 | 0.05 ms | 0.05 ms | 15.9 | 58 |
| `sum:apiserver_request_total{*} by {verb}` | 2 | 11,520 | 0.08 ms | 0.09 ms | 7.2 | 54 |
| `100 * sum:…{code:5*} / sum:…{*}` | 0 | 11,520 | 0.08 ms | 0.10 ms | 7.1 | 59 |
| `top(avg:container_memory_working_set_bytes{*} by {pod}, 10, 'mean', 'desc')` | 4 | 23,114 | 0.39 ms | 0.43 ms | 16.9 | 87 |
| `avg:node_*{*} by {instance}` | 7 | 2.93 M | 27.0 ms | 27.3 ms | 9.2 | 95 (a group key a series at first: 1,116) |
| `sum:*{job:apiserver}` | 1 | 2.18 M | 20.2 ms | 20.9 ms | 9.3 | 48 |

## 13. Scenarios

Live, end to end: a kiln server and runners as separate processes, OTLP senders, mock Slack and
Discord endpoints, and faults (killed members, partitions, full disks, clock skew). Each scenario
states what a user would see and checks that, not the code's internals. (Listed as built.)

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
  Consensus: bridging theory and practice (thesis), 2014.
- Iglewicz, Hoaglin. How to detect and handle outliers. ASQC 1993.
- Hochenbaum, Vallis, Kejariwal. Automatic anomaly detection in the cloud via statistical learning.
  arXiv 1704.07706, 2017.
- Winters. Forecasting sales by exponentially weighted moving averages. Management Science 1960.
- Lavin, Ahmad. Evaluating real-time anomaly detection algorithms: the Numenta Anomaly Benchmark.
  ICMLA 2015.
- Steinarsson. Downsampling time series for visual representation. MSc thesis, Univ. of Iceland 2013.
- He, Zhu, Zheng, Lyu. Drain: an online log parsing approach with fixed depth tree. ICWS 2017 (log
  patterns, O-6).
