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
| O-4 | Query language: metrics (parser without recursion, evaluator, §12.5; answers agree to the bit on every replica), log search and analytics, events; the API (§7) | done |
| O-5 | Ingestion (§8): OTLP/HTTP metrics and logs (protobuf, JSON, gzip) on the server and every runner; every node's host, container and process; kiln's registry; kiln CI's jobs and runs, kiln CD's deploys and status; GitHub's webhooks; Datadog-style series, logs and events | done |
| O-6 | Logs: LZ4 (§12.6), blocks with summaries and token filters (§12.7), Datadog's search syntax, the store's log head and files, OTLP logs, analytics (count, cardinality, sum, avg, min, max by facets, in time buckets; every facet's counts in one pass), allocating a group, not a row (§12.8); next: live tail, patterns | in progress |
| O-7 | Monitors (§9): metric, change, log; thresholds with recovery, warnings, NO DATA, renotify, mute, evaluation delay; Datadog's templates and mentions; next: anomaly, forecast, composite | in progress |
| O-8 | The UI (§11): Observe's overview, dashboards (kiln's own and yours, template variables, edited in place), the metrics explorer, the logs explorer and event stream, monitors and integrations; checked by scenarios in Chromium and WebKit (§13); next: distribution and heatmap widgets, live tail | done |
| O-9 | Integrations (§10): Slack, Discord, Teams, PagerDuty, signed webhooks, GitHub; AWS CloudWatch, Google Cloud Monitoring and Azure Monitor pulled with given or ambient credentials (§10.1), checked live against services that verify as the clouds do (§13); next: CloudWatch Metric Streams, Prometheus scrapes, the clouds' logs | in progress |
| O-10 | Load, soak and fault scenarios end to end; allocation and fault passes. Done: the flood and the server-away scenarios (§13), OTLP decoding and the sample path (§12.10), blocks and merges (§4.5); measured in §12.11. Full disks and clock skew done (§13). Next: soak; sampling profiles (samply needs `perf_event_paranoid` ≤ 1 here) | continuous |

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

The head holds recent samples in memory: each series' open chunk and its closed chunks. The head is
cut into an immutable **block** file when the group compacts its log (§5: the log is the
write-ahead log, so a cut is what lets it be trimmed):

```
block  = MAGIC | FORMAT 2 | data | index | footer
data   = each series' region, by fingerprint
region = table length | table (chunks | per chunk: first, last-first, count, length) | chunks
index  = per series, by fingerprint: fp | kind | key | region offset, length, crc32c | first, last
footer = index offset, length | min ts | max ts | crc32c(index, footer) | TAIL
```

An open block holds, of each series, its fingerprint and where its region is (some 48 bytes):
nothing a chunk or a key, so what a store holds in memory grows with its series and its blocks, not
its samples (Prometheus's blocks and ClickHouse's parts read their marks from disk the same way). A
read is one positioned read of a series' region, checked against its own CRC-32C, its table first
and its length before it, so the chunks a range wants are found without reading the table twice.
Keys are read from the index when the store opens (to rebuild its index of series) and when blocks
merge.

Blocks merge by the logarithmic method (Bentley and Saxe 1980): the oldest neighbouring pair whose
newer block holds at least half the older's is merged, in its place, until none is, so a store keeps
O(log n) blocks and a sample is rewritten O(log n) times. (Merging only the newest pair stalled: a
busy group cuts a big block and then a small one, a compaction's and a snapshot's, and a small cut
always sat at the newest end.) A merge is written as it is read, a series at a time: a series one
input holds is copied whole; inputs whose spans of a series follow one another have their chunks
appended (a Gorilla chunk stands alone); only overlap (late writes), retention and chunks under half
full are decoded and packed anew. Maintenance runs on a thread of its own, told of every cut, and
takes a merge in only if its inputs are still there. A manifest (rewritten by rename) lists the live
blocks and the log point the head was cut at; files wholly past retention are deleted, a block more
than half past it rewritten.

### 4.6 Logs and events

Log blocks are column chunks of `LOG_BLOCK_ROWS` rows: `ts` delta-encoded varints; `severity`
run-length encoded; `service`, `host`, `source` dictionary-encoded and run-length encoded; `message`
and `attributes` compressed together per block; trace ids raw. Each block carries the minimum and
maximum of `ts`, the dictionaries, and a Bloom filter over the message's tokens sized for
`LOG_BLOOM_FPR` (Bloom 1970; `k = (m/n) ln 2` hashes with Kirsch and Mitzenmacher's double hashing,
2006), so a search reads only blocks that may match and the time range they cover.

Events are records too, of source `kiln.event`: one store, one search. But they are not logs: a
monitor's own alerts are events at error's severity, and a monitor of `status:error` that counted
them would count its own alerting. A log monitor, and the logs explorer, read logs only
(`kiln_obs::monitor::logs_only`: the search and `-source:kiln.event`) unless the search names
`kiln.event`; the event stream reads events only.

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
filter     := ["!"] key ":" value             // value: exact, wildcard "web-*", (a OR b), or
                                              // "quoted" (any characters, exactly; \" and \\ within)
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
log analytics `count() by {service}` over a search. A `-` that starts a term is NOT wherever it
stands (`(a OR b) -env:dev`); one inside a term is the term's (`web-1`). A quoted value holds any
characters (`@note:"say \"hi\""`), so a facet's every value can be searched for. Events take the
same search syntax.

## 7. API

Under `/api/observe/v1/` (all JSON; the UI uses nothing else). Built:

| | |
| --- | --- |
| `POST query` (or `GET query?query=&from=&to=&step=`) | metric query → series on a grid (`from`, `to` in ms, `step` or `points`); NaN is null; the answer states the index it was read at |
| `GET metrics?match=` , `GET tags?key=&match=` | metric names and tag values, sorted (autocomplete) |
| `POST logs/search` | `{query, from, to, limit}` → records newest first, with the blocks pruned |
| `POST series` | ingest points as Datadog's v1 series API takes them (`metric`, `type`, `tags`, `points` of `[seconds, value]`) |
| `POST logs` | ingest records as Datadog's log intake takes them (`message`, `timestamp`, `status`, `service`, `hostname`, `ddsource`, `attributes`) |
| `GET status` | the group: members, leader, term, applied and durable index, series, blocks, bytes on disk (observe's folder, hard-linked snapshot files once), cutoff, this node's writes, each machine's replica, digests (also in hex: a u64 is more than JavaScript's numbers hold) |
| `POST compact`, `POST verify` | cut and compact the log now; take every replica's full digest (an operator's, or a test's, check that replicas agree) |
| `POST logs/aggregate` | log analytics: `{query, from, to, by: [facets], measure, of}` → groups; with `step` (ms), each group's value a bucket on a grid (the explorer's histogram) |
| `POST logs/facets` | `{query, from, to, facets, top}` → each facet's most common values and how many it has, every facet in one pass |
| `GET/POST dashboards`, `GET/PUT/DELETE dashboards/{id}` | dashboards (§11): kiln's own (`builtin:hosts`, `builtin:ci`, `builtin:cd`, read-only) and yours, kept in `<observe>/dashboards.json` |
| `POST events`, `POST events/search` | an event (Datadog's v1 shape); events searched |
| `GET/POST monitors`, `GET/PUT/DELETE monitors/{id}`, `POST monitors/{id}/mute`, `…/unmute`, `POST monitors/validate` | monitors (§9), each with its state and its groups'; validate says what a query watches (the metric query or the log search, and its window), so a page draws what is evaluated |
| `GET integrations`, `PUT/DELETE integrations/{name}`, `POST integrations/{name}/test` | where monitors say things (§10) |
| `GET clouds`, `PUT/DELETE clouds/{name}`, `POST clouds/{name}/pull`, `POST clouds/{name}/test` | cloud integrations (§10.1), each with its status; a secret left out of a `PUT` (or `"…"`, as shown) is the one kept; `test` pulls a configuration once and writes nothing, saying what it would read |
| `GET snapshot/{point}/{file}?machine=` | a snapshot's file, for the runner of `machine` with its token |

To come: `GET logs/tail` (websocket).

OTLP/HTTP at its standard paths, `/v1/metrics` and `/v1/logs` (protobuf and JSON), on the server and
on every runner at `127.0.0.1:4318`, so an app on a runner sends to its local runner as it would to
a collector (O-5).

## 8. Ingestion

Every node writes its own points (`crate::observe::ingest`: gathered by series, one write out at a
time, sent again until answered; §5), from:

- **OTLP/HTTP** (`POST /v1/metrics`, `/v1/logs`): protobuf or JSON (OTLP's JSON Protobuf Encoding,
  put into protobuf's wire format and read by the one decoder), gzip or not, up to 20 MiB (the
  OpenTelemetry Collector's default). The server asks the ingest token (`<data>/observe/
  ingest.token`) or the admin's; every runner takes OTLP on `127.0.0.1:4318` (OTLP/HTTP's port;
  `KILN_OTLP_ADDR`, or `off`) as a local collector would. Metrics map by type (gauges;
  sums by temporality and monotonicity; histograms' count, sum, min, max and buckets;
  exponential histograms' quantiles too; summaries); logs keep their time, severity, body,
  attributes and trace context, `service` and `host` from the resource.
- **The node's own host** (`crate::observe::host`, every 15 s, the Datadog Agent's interval), in
  Datadog's names and units: CPU (the split over the interval; Linux), load, memory, swap, every
  mount the process sees, disk I/O and network rates, uptime; in a container or a cgroup-limited
  unit, its limits and use (`container.memory.*`, `container.cpu.*`, cgroup v2 or v1), tagged with
  the container's id and the pod's namespace, name and node. Linux reads procfs and sysfs; macOS
  and Windows ask the OS (sysinfo). Network file systems are never looked at (a dead mount's
  `statvfs` never returns), on a thread of its own besides.
- **The process** (`kiln.process.*`: memory, CPU, open files, threads).
- **kiln's registry**, what `/metrics` serves Prometheus, scraped under its Prometheus names
  (`kiln_prom`'s parser takes any text exposition).
- **kiln CI**: each job's and run's duration, queue time, footprint and count by repo, pipeline,
  job, status, runner, branch and trigger, and each an event.
- **kiln CD**: each deploy (by app, trigger, result) and each change of an app's health or sync as
  events; apps counted by health and sync.
- **GitHub's webhooks**: pushes, pull requests, workflow runs and jobs (durations, conclusions),
  releases, deployments, issues; events and counts.
- **The API**: Datadog's v1 series (`POST series`), log intake (`POST logs`) and events (`POST
  events`).

- **Clouds**: AWS CloudWatch, Google Cloud Monitoring and Azure Monitor, pulled (§10.1).

To come: Prometheus endpoints scraped; CloudWatch Metric Streams taken (Firehose's HTTP endpoint
delivery of OTLP).

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

## 9. Monitors

A monitor is Datadog's (`kiln_obs::monitor`, `crate::observe::monitors`): a query, a message and
options, so a monitor written for Datadog reads here.

- **Queries**: `avg|sum|min|max|last(last_5m):<metric query> > 90`; change monitors
  `change(avg(last_5m),last_1h):…` and `pct_change(…)`; log monitors
  `logs("service:web status:error").rollup("count").by("host").last("5m") > 10` (count,
  cardinality, sum, avg, min, max of a facet). A window is whole buckets of a step that divides it,
  aligned to the step.
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
  are saved as they change.

To come: anomaly, forecast, outlier and composite monitors (§9.1).

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

Notifications (`crate::observe::notify`), each in its service's format: Slack (incoming webhook,
Block Kit in a colored attachment), Discord (webhook embeds, within its limits), Microsoft Teams
(an Adaptive Card, what Workflows webhooks take), PagerDuty (Events API v2: trigger while
alerting, resolve on recovery, one dedup key a monitor's group), and generic webhooks (kiln's JSON
signed HMAC-SHA256 over the body, `X-Kiln-Signature`, as GitHub signs its own). Delivered off a
bounded queue, tried again with doubling backoff on errors, 429s (after `Retry-After`) and 5xx;
each delivery an event and a count. Integrations are named (`PUT /api/observe/v1/integrations/
<name>`), their secrets never shown, and testable.

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

Names are Datadog's, as far as one rule makes them (a word starts at a capital after a lower-case
letter: `CPUUtilization` → `cpuutilization`, `NetworkIn` → `network_in`), so a dashboard or a
monitor written for Datadog's integrations reads these. CloudWatch keeps each period's count, sum,
minimum and maximum, and its average is the sum over the count: asking `Sum` and `SampleCount`
(the default) gives the average without a query of its own, and the sums as counts. A
distribution's quantiles are read from its buckets as Prometheus's `histogram_quantile` reads them
(linear within a bucket), log-linear within an exponential bucket, whose points are spread so. A
Google type with no series is quiet, asked again hourly, not each pull: a project's thousands of
built-in types cost a call each an hour.

Calls throttled or failed on the service's side (429, 5xx, AWS's throttling codes, botocore's
list) are tried again after `Retry-After`, else 1 s doubling, four tries; a failed pull is tried
again after a minute, doubling, up to the interval. A region, project, subscription or resource
that fails is a warning while the others are read; when all fail, the pull fails. The status
(`GET clouds`) says each integration's state, its error and warnings, its last pull (series,
points, points left to the next, calls, time); kiln's own series count them (`kiln.cloud.points`,
`.calls`, `.errors`, `.series`, `.pull.duration`, by `integration` and `cloud`); its first pull, a
failure and its recovery are events (`service:kiln.cloud @event.type:cloud.pull`).

Secrets (AWS's secret key and session token, Google's key, Azure's client secret) are sealed at
rest with AES-256-GCM under the server's master key (`<data>/secrets/master.key`, CI's), each bound
to its integration and field; `clouds.json` is written owner-only (0600) in one step; they are
never shown (`"…"`). A secret that does not open (the master key changed) leaves its integration
failing, saying so, until it is given again. Credentials are got again 5 min before they expire;
a page's `nextLink` elsewhere than Resource Manager is not followed (the token goes nowhere else).

## 11. UI

Observe, in kiln's console (`ui/src/views/observe/`), built from the console's own parts (panels,
facts, the pill tabs, uPlot charts) and its rules: data through Suspense resources, no effects, the
time range and the search in the address (a page is a link).

- **Overview**: what is in trouble (monitors alerting, warning, without data), the newest events,
  and the store: series, bytes on disk, from when it keeps, each machine's replica, how far behind it
  is and whether it holds the same (its full digest at the server's index; "Compare replicas" asks
  for them).
- **Dashboards**: Datadog's model near enough. Widgets on a grid of twelve columns (one column on a
  phone): timeseries (lines, areas or bars), query value, top list, table, log stream, event
  stream, monitors, note (Markdown). Template variables above them (`$host` in a query stands for
  `host:<value>`, or any), chosen from the tag's values, kept in the address. Your own are edited in
  place: add, change, move, resize, remove, then save; kiln's own (hosts, CI, CD) are copied to be
  changed.
- **Metrics explorer**: a query, its series over the range and each one's last, average, least and
  most; with no query, every metric kiln keeps, found by name. A query goes onto a dashboard, or
  into a monitor, in a step.
- **Logs explorer and event stream**: a search, a histogram of its records by status (stacked, a
  bucket of a round size, about 60 to the range), each facet's most common values (a click narrows
  the search to one, again widens it), the newest 100 records (and 100 more on asking), each opened
  to its fields and attributes, any one a filter a click away.
- **Monitors**: their states, in trouble first; each one's groups, what it watches drawn over the
  range with its thresholds, what it said (its events), mute and unmute; made and edited in a form
  that checks its query.
- **Integrations**: added (Slack, Discord, Teams, PagerDuty, webhooks, signed), tested, removed;
  each one's mention to copy.
- **Live**: the hosts' samples of the last hour at 5 s, streamed (what the Metrics page was); before
  that, the Hosts dashboard.

Every chart is uPlot, already kiln's. To come: distribution and heatmap widgets; series reduced to
the chart's width with largest-triangle-three-buckets (Steinarsson 2013) when a range holds more
buckets than pixels (today the query's `points` bounds them: 240 a chart).

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

Also live: OTLP/JSON gzip to the server (a wrong token refused, a malformed body refused) and
protobuf to a runner's receiver, which arrive and replicate; a CI pipeline run on a runner from a
`file://` repository, its job's count, duration and event found; a monitor that alerts five
integrations (services in the test) each in its format, a webhook refused once and delivered on
its second try with its signature checked, a recovery that resolves PagerDuty's incident by the
same key, and muted, a change of state with nothing said; GitHub's webhooks and posted events,
searched and counted.

**In a browser** (`ui/e2e/observe.mjs`, Playwright against a live server; how to run it is at
its top): a query explored and put on a new dashboard, which is then given a note, saved and
deleted; kiln's own dashboard narrowed by its template variable, then copied; logs searched,
narrowed by a facet and by an attribute of a record, the API answering the page's search alike; a
monitor made from the form, muted, unmuted and deleted; a signed webhook added, tested (the
receiver checks the signature) and removed. Each checked through the API, not the page. 2026-10-18:
all pass in Chromium at 1,440 px and in WebKit as a phone (390 px, touch); and every Observe page
loads in both with no error, failed request or sideways overflow, against a server holding two
hours of metrics, an hour of logs, events, and three monitors alerting to a webhook and to Slack.

**Under load** (`crates/kiln/tests/observe_load.rs`): the flood of §12.11, every point taken
stored exactly once, the replicas alike; and a runner flooded while its server is killed (SIGKILL)
and later started again: it refuses in time (0.5 s at full rate, 2.4 s at 100 k points a second),
its memory bounded, and everything it took is stored once the server is back.

**Clock skew** (`observe_live.rs`): a runner whose clock is an hour ahead
(`KILN_OBSERVE_CLOCK_SKEW_MS` moves the clock observe uses; a real one takes root to move) is
measured an hour ahead from its writes and said so once; its own figures, an hour ahead, are dropped
and counted (`ahead`), not kept; points it takes with true times are stored; started again with a
true clock, it is said to agree. Samples more than ten minutes past their write's time and log
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
  VM's (its project too), an Azure VM's managed identity.
2026-10-18: all pass (48 s).

To come: soak.

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
