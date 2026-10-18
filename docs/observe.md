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
| O-3 | Replication: the store as a hyper-durable state machine (§5); kiln carries Raft on its own lane, snapshots' files over HTTP; deterministic replicas (1,000-seed scenario), a group on real logs (crashes, partitions, images), live processes killed and restarted (§13); measured (§12.9) | done |
| O-4 | Query language: metrics done (parser without recursion, evaluator, §12.5; answers agree to the bit on every replica); the HTTP API's first routes (§7); logs and events with O-6 | in progress |
| O-5 | Ingestion: kiln itself, runners' hosts, OTLP/HTTP (protobuf and JSON), kiln CI and CD | planned |
| O-6 | Logs: LZ4 (§12.6), blocks with summaries and token filters (§12.7), Datadog's search syntax; next: the store's log head, persistence, OTLP logs, facets, live tail, patterns | in progress |
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
applies it; skipping it would leave another state), and the owner says why.

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
under a read lock; std's lock prefers a waiting writer, so a query holds applying off for no longer
than one query takes.

**Writes.** A node gathers its points by series (`crate::observe::ingest`) and sends one write at a
time, as the sessions assume, again and again until it is answered: a runner's go on the lane to the
server, which proposes them and routes their answers back. A series is defined in a write until a
write of the sender defining it is applied (and again an hour after); one the group does not know
is sent again with its definition. Gathering follows the OpenTelemetry Collector's batch processor
(200 ms); a write is never larger than the group's entry (1 MiB).

**The lane.** `LANE_OBSERVE` (5) carries the group's messages and writes as JSON beside their bytes,
payloads past 512 KiB in parts put back together under bounds (16 MiB, four a side, a minute).
Snapshot files go over HTTP so that a big image never waits ahead of a heartbeat.

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

Under `/api/observe/v1/` (all JSON; the UI uses nothing else). Built:

| | |
| --- | --- |
| `POST query` (or `GET query?query=&from=&to=&step=`) | metric query → series on a grid (`from`, `to` in ms, `step` or `points`); NaN is null; the answer states the index it was read at |
| `GET metrics?match=` , `GET tags?key=&match=` | metric names and tag values, sorted (autocomplete) |
| `POST logs/search` | `{query, from, to, limit}` → records newest first, with the blocks pruned |
| `POST series` | ingest points as Datadog's v1 series API takes them (`metric`, `type`, `tags`, `points` of `[seconds, value]`) |
| `POST logs` | ingest records as Datadog's log intake takes them (`message`, `timestamp`, `status`, `service`, `hostname`, `ddsource`, `attributes`) |
| `GET status` | the group: members, leader, term, applied and durable index, series, blocks, bytes, cutoff, this node's writes, each machine's replica, digests |
| `POST compact`, `POST verify` | cut and compact the log now; take every replica's full digest (an operator's, or a test's, check that replicas agree) |
| `GET snapshot/{point}/{file}?machine=` | a snapshot's file, for the runner of `machine` with its token |

To come: `GET logs/tail` (websocket), events, monitors, dashboards, integrations.

OTLP/HTTP at its standard paths, `/v1/metrics` and `/v1/logs` (protobuf and JSON), on the server and
on every runner at `127.0.0.1:4318`, so an app on a runner sends to its local runner as it would to
a collector (O-5).

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

## 13. Scenarios

What a user would see, checked end to end; not the code's internals.

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

To come: OTLP senders, mock Slack and Discord endpoints, full disks, clock skew, load and soak.

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
