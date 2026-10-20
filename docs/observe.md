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
| O-5 | Ingestion (§8): OTLP/HTTP and OTLP/gRPC metrics and logs (protobuf, JSON, gzip; gRPC over HTTP/2, an official SDK's exporters checked against it) on the server and every runner; every node's host, container and process; kiln's registry; kiln CI's jobs and runs, kiln CD's deploys and status; GitHub's webhooks; Datadog-style series, logs and events; DogStatsD on every machine (§8.4: Datadog's protocol over UDP and Unix sockets, aggregated as its Agent aggregates, checked live with Datadog's own client; 5 M messages a second a listener, §12.20); Prometheus's remote write 1.0 and 2.0 on the server and every runner, native histograms as distributions (§8.5, checked live against Prometheus 3.7, sample for sample; §12.21); Datadog's Agent at Datadog's own paths (§8.6: its metrics, sketches, service checks, events and logs, zstd read by kiln's own decoder; checked live against Agent 7.84) | done |
| O-6 | Logs: LZ4 (§12.6), blocks with summaries and token filters (§12.7), Datadog's search syntax, the store's log head and files, OTLP logs, analytics (count, cardinality, sum, avg, min, max by facets, in time buckets; every facet's counts in one pass), allocating a group, not a row (§12.8); patterns (Drain, 0.887 on Loghub, §12.13); live tail (each record once as the replica applies it, a uniform sample past the rate asked, §6, §12.14) | done |
| O-7 | Monitors (§9): metric, change, log, anomaly; thresholds with recovery, warnings, NO DATA, renotify, mute, evaluation delay; Datadog's templates and mentions; anomaly detection (§9.1): Datadog's three algorithms, seasons learned, deviations calibrated per series, scored on NAB 52.6 (§12.12), `anomalies()` in queries with its band drawn; composites (Datadog's algebra and group matching, its table a test); forecasts (`forecast()` and forecast monitors; chosen and scored on M4, §12.15); outliers (`outliers()` and outlier monitors; DBSCAN's constant from a simulation, §9.1) | done |
| O-8 | The UI (§11): Observe's overview, dashboards (kiln's own and yours, template variables, edited in place; heatmap and distribution widgets), the metrics explorer, the logs explorer and event stream, monitors (composites among them) and integrations; checked by scenarios in Chromium and WebKit (§13); distributions (§4.4: percentiles merged across hosts and time) | done |
| O-11 | Service level objectives (§9.2): ratio and time-slice SLIs over rolling timeframes, the standing (SLI, budget left, day by day, burn rates), burn-rate and budget alerts as monitors; checked live (§13); their pages (§11): the list, each one's standing and days, the workbook's alerts for its timeframe in a click, made and edited in a form; checked in Chromium and WebKit (§13) | done |
| O-12 | Logs collected on every machine (§8.1): files by their globs (renamed, truncated, copied, deleted while read), Kubernetes' pods' and Docker's containers' logs with what they name, the systemd journal by its cursor, syslog received over UDP and TCP (RFC 5424 and 3164, RFC 6587's framings); Datadog's processing rules; macOS's unified log (`log stream`, and `log show` from its place after a restart) and the Windows Event Log's channels (`wevtutil`, each channel's record id its place); each line's place kept only once its record is written, so a stop repeats nothing and a kill at most the last second; checked live (a Mac's unified log; the Event Log against a stand-in for `wevtutil`) and in browsers (§13), measured (§12.17); a JSON line read in place, no tree of it made (36 allocations a line to 18, 40% more lines a second). Next: records handed over encoded; the Event Log on a Windows machine | in progress |
| O-13 | Log pipelines (§8.2): Datadog's, its JSON read as it is: a filter and processors in order (the grok parser with Datadog's matchers, filters and date patterns; the date, status, service, message, trace id and attribute remappers; the URL and user-agent parsers; category, arithmetic, string builder and lookup processors; a pipeline nested in one), applied where records are taken on every machine; kiln's own for nginx and Apache (Datadog's rules); kept by the server and told to runners; their page with a record tried before a pipeline is kept; checked live and in browsers (§13), measured (§12.17). Next: a date read without a second expression, fewer allocations a record | in progress |
| O-14 | Metrics from logs (§8.3): Datadog's definitions (a count, or a distribution with or without percentiles, by facets), counted on the server, the one writer, each window whole once its records have come (the 99th percentile of how late they come) and again when more come within twenty minutes; OpenTelemetry's cardinality limit; their page, a definition tried on the last fifteen minutes before it is kept; checked live and in browsers (§13). Next: counting measured at a fleet's rates | in progress |
| O-15 | Downtimes (§9.3): Datadog's, once or recurring (RFC 5545's rules in a timezone, their offsets' changes as RFC 5545 has them), for monitors by id or tags and the groups a scope holds; held groups' changes still events; as one ends, what is still wrong told once, a first recovery kept quiet when asked; their page and a monitor's line; checked live and in browsers (§13) | done |
| O-9 | Integrations (§10): Slack, Discord, Teams, PagerDuty, Opsgenie, email through an SMTP relay (checked live against Mailpit: STARTTLS, a login, a CA of its own), signed webhooks, GitHub; AWS CloudWatch, Google Cloud Monitoring and Azure Monitor pulled with given or ambient credentials (§10.1), checked live against services that verify as the clouds do (§13); Prometheus's scrapes by the server and every runner (§8); the clouds' logs (CloudWatch Logs, Cloud Logging, Log Analytics), each record once though every pull reads its lookback (§10.1); their pushes: Firehose (CloudWatch Metric Streams in JSON and in OpenTelemetry 1.0.0 and 0.7.0, Logs' subscriptions) and Pub/Sub (Cloud Logging's sinks), each delivery taken once (§10.2); Azure's event hubs read as they come over Kafka's protocol (a consumer of its own, offsets kept by kiln, each record once through restarts and a broker's), checked live against Apache Kafka's broker (§10.3) | in progress |
| O-16 | Synthetic tests (§10.4): Datadog's API tests, HTTP (its time cut into DNS, connection, TLS, first byte, download and redirects), TCP, DNS and TLS (a certificate's days left), from the server and runners as locations; Datadog's assertions and series; a location failing or recovering an event once; alerting by a monitor each test owns (at least N locations failing for a while); tried once from the server, run now everywhere; their page; checked live (a server and a runner, an endpoint switched to 500 and back, a TLS endpoint with a CA of its own) and in browsers (§13) | done |
| O-17 | Metrics summary (§7, §11): Datadog's, each metric's series by kind, each tag's values and its values of most series, the samples it brings (counted as they are applied, by metric; carried over when retention numbers the symbols anew) and of late a second, its newest sample; from the index, nothing read from disk; its page; checked by the API's conformance test, a unit test of the counts through retention, and in browsers (§13) | done |
| O-18 | Traces (APM, §8.7): OTLP traces over HTTP (protobuf, JSON) and gRPC on the server and every runner, each span decoded straight into its row (614 ns, no allocation a span past the request's, §12.23) in a span store of its own beside the logs (its own files and retention, replicated as logs are; formats a build that knows no spans still reads until the first span comes); found by their trace in the whole retention, their blocks' filters holding their ids (salted by the block: a trace reads its own block and 1% of the rest, whichever trace); the trace explorer's search syntax; a trace assembled as a waterfall draws it; Datadog's trace metrics (hits, errors, latency distributions, of entry spans and calls out by peer) counted on the server as metrics from logs are, and metrics from spans; services, resources and calls from them over any range; the service map from the spans, sampled by trace past 200,000; the API, described in OpenAPI; APM's pages (services, a service, the trace explorer, a trace's waterfall and its logs, the map); checked live against OpenTelemetry's Python SDK across two services by way of a runner (§13). Datadog's Agent's traces (§8.7: its indexed and inline forms, 532 ns a span, §12.23) and its stats as the trace metrics (every span counted before it sampled, its DDSketches' four mappings read within their accuracy; 6 µs and 0.2 allocations a grouped stat), checked live against Agent 7.84 and ddtrace 4.16 with traces kept and dropped (§13). Datadog's tracers straight to kiln on every runner (kiln in the Agent's place on its port: `/v0.4/traces`, `/v0.5/traces`, `/info`; 490 ns a span), every span counted by the runner's concentrator before a dropped trace is let go, as the Agent counts (133 ns a span); checked live with ddtrace 4.16, no Agent (§13). CI runs as traces, span monitors. Next: the map's calls from stats, an ingest-side sampler for tracers that keep everything (the Agent's target rate) | in progress |
| O-10 | Load, soak and fault scenarios end to end; allocation and fault passes. Done: the flood and the server-away scenarios (§13), OTLP decoding and the sample path (§12.10), blocks and merges (§4.5); measured in §12.11. Full disks and clock skew done (§13). The soak (§13: memory holds its level). Sampling profiles (perf): decoding a third faster (§12.1, §12.5), OTLP a fifth (§12.10), requests' buffers handed back. Rollups (§4.7): five-minute and hour tiers in blocks, every replica's answers the same to the bit, read rolled up or not; a month 17 times faster; part of a long region read without the rest (§4.5, §12.16). Two hours' soak passing (§13): every point and record stored once, the replicas agreeing, memory level. A dashboard's widest query (§12.19): its time its own reading, not a wait; a run of repeated samples decoded a word at a time, series read on helper threads in rounds, added in their fingerprints' order (the same bits): 1,527 ms to 74 ms. The store's files written beside queries (§4.5). Next: merges that join their inputs' rollups rather than make them anew | continuous |

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
write-ahead log, so a cut is what lets it be trimmed):

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
have their chunks appended (a Gorilla chunk stands alone; their rollups are made from the samples,
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

**Made at merges, checked when fetched.** A cut writes none (it decodes nothing, under the store's
hold); the first merge to take its block makes them. A merge that copies a series' region whole
copies its rollups; one that appends builds them as it reads the chunks, every tier in one pass,
holding a series' windows and not its samples (a week's are 2,016 and 168); one that packs anew,
from the samples it holds to pack (17% more merging time, measured). A block fetched from another replica (§5) is verified with every series'
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
under a read lock; std's lock prefers a waiting writer, so a query holds applying off for no longer
than one query takes.

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
factor     := number | "(" expr ")" | function "(" args ")" | metric_q
metric_q   := [agg ":"] metric ["{" filters "}"] ["by" "{" tags "}"] modifier*
agg        := avg | sum | min | max | count | p50 | p75 | p90 | p95 | p99 | pNN.N
filters    := filter ("," filter)*            // AND; "*" = all
filter     := ["!"] key ":" value             // value: exact, wildcard "web-*", (a OR b), or
                                              // "quoted" (any characters, exactly; \" and \\ within)
modifier   := ".rollup(" fn ["," seconds | "," calendar] ")" | ".as_count()" | ".as_rate()"
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
| Arithmetic | `abs`, `round(q[, places])`, `ceil`, `floor`, `pow(a, b)` (each a query or a number), `log2`, `log10`, `cumsum`, `integral`; `+ - * /` between queries and numbers |
| Count | `count_nonzero`, `count_not_null` |
| Exclusion | `exclude_null`, `clamp_min(q, n)`, `clamp_max(q, n)`, `cutoff_min(q, n)`, `cutoff_max(q, n)`; kiln's `clamp(q, lo, hi)` |
| Interpolation | `.fill(null\|zero\|linear\|last[, seconds])`, `default_zero` |
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
log analytics `count() by {service}` over a search. A `-` that starts a term is NOT wherever it
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
after) and the store's operations need `observe.manage`
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
| `POST query` (or `GET query?query=&from=&to=&step=`) | metric query → series on a grid (`from`, `to` in ms, `step` or `points`); NaN is null; the answer states the index it was read at; a series with a band (`anomalies()`, `forecast()`) has `bounds` (`lower`, `upper`, `score`, `k`, `algorithm`, and a forecast's `forecast_from`, its first bucket forecast; `to` past now draws it). With `heatmap: <rows>`, a heatmap instead: `rows` (each `[lo, hi]`) and `counts` (each bucket of time, each row's): a distribution's merged buckets (made coarser until they fit), or another query's series' values in equal rows |
| `GET metrics?match=` , `GET tags?key=&match=` | metric names and tag values, sorted (autocomplete) |
| `GET metrics/summary?match=&limit=`, `GET metrics/summary/{name}` | what each metric is and costs, Datadog's Metrics Summary: its series (its cardinality) by kind, each tag's values and the series carrying it (the tag of most values first; a metric's own with each tag's ten values of most series), the samples applied since the server started and of late a second, its newest sample's time; the metrics of most series first (100 unless said, 1,000 at the most), and how many matched. From the index and the counts kept as samples are applied: nothing read from disk |
| `POST logs/search` | `{query, from, to, limit}` → records newest first, with the blocks pruned |
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
| `POST events`, `POST events/search` | an event (Datadog's v1 shape); events searched |
| `GET/POST monitors`, `GET/PUT/DELETE monitors/{id}`, `POST monitors/{id}/mute`, `…/unmute`, `POST monitors/validate` | monitors (§9), each with its state and its groups', a composite (`type: "composite"`) with what it reads (each monitor's letter, name and state, its condition in letters and in names); validate says what a query watches (the metric query or the log search, and its window), so a page draws what is evaluated, or, given `type`, whether a composite's monitors are there to read; a monitor a composite reads is not deleted (409) |
| `GET/POST downtimes`, `GET/PUT/DELETE downtimes/{id}` | downtimes (§9.3), Datadog's v2 shape: the monitors held (an id, or tags), the groups (a scope), once (times in ms or RFC 3339) or recurring (RRULEs in a timezone); each with its status (`active`, `scheduled`, `ended`, `canceled`) and its window now or next; DELETE cancels (kept); a monitor shows the downtimes that hold it now or will |
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
| `GET apm/map?from=&to=&env=` | the service map: nodes (each its requests and errors, whether only named) and edges (calls, errors, mean latency, whether inferred), and the share of traces read |
| `GET/POST span_metrics`, `GET/PUT/DELETE span_metrics/{name}`, `POST span_metric_test` | metrics from spans, as `log_metrics` are; the list says kiln's trace metrics too (`builtin`) |
| `GET log_sources`, `PUT/DELETE log_sources/{name}`, `POST log_sources/{name}/test` | logs machines collect (§8.1), each with how each machine that reads it goes (files open, records written and in flight, bytes behind; a syslog source's connections and datagrams dropped; the last error; counts since `since_ms`, when its process began reading); `test` reads a source on the server and writes nothing: the files it matches and the records of the newest one's last lines, or whether it may listen where it says |
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
  attributes and trace context, `service` and `host` from the resource. An explicit-bucket
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
  events; apps counted by health and sync.
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

**Each line once.** A line's place (a file's offset, the journal's cursor) is kept only once the
write that holds its record is applied by the group (`crate::observe::ingest::Ack`: the batcher
gives it when the write holding the last of an offer's records is answered), and written down a
second at most after. A refused offer (no room) is offered again, and nothing more is read until it
is taken. So a machine that stops (SIGTERM) waits a moment for its last writes and repeats nothing,
and one killed repeats at most what was written in its last second, and loses nothing it read
(Vector's end-to-end acknowledgements; Datadog's Agent and Fluent Bit keep their places the same
way). A runner keeps its loop up for those last writes, three seconds at the most: the supervisor is
told of a shutdown five seconds after the runner, so it does not drop the runner first.

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
not a library. Neither source has a machine of its own platform in the scenarios' Linux fleet:
the unified log's scenario runs on a Mac of the fleet, the Event Log's against a stand-in for
`wevtutil` that answers its two queries from a channel's file of events, as Windows' does (§13).

A source reads what kiln's process may read on each machine that reads it: a maintainer's to give.

### 8.2 Pipelines

How records are read as they are taken (`crate::observe::pipeline`), Datadog's log pipelines, so one
exported from Datadog reads here as it is: each pipeline a filter (a log search; empty, every
record) and processors run in order on each record it matches, the pipelines in order, each reading
what those before it made. The server keeps them (`<observe>/pipelines.json`) and tells every runner
the list; each node applies it where it takes a record (a request's, a log source's, a cloud's), so
what is stored is read already, on the machine that took it. A source's records are run through
once, as they are gathered: an offer refused comes back and is not run again.

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
its records fall in (`Touched`), taken under the same hold of the store as the count that follows,
so every record noted is in what that count reads. Records that come more than twenty minutes after
their window are not counted, as Datadog's ("aggregation only considers logs ingested with a
timestamp within the past 20 minutes"); they are stored and searched as any.

**When a window is counted.** Once its records have come: the latest any batch's newest record came
over the last five minutes, a quarter more, 2 to 30 s (learned as records come, so a fleet whose
records take longer waits longer, and in the steady state each window is counted once). A window
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
  sample rate, tags, a container and a timestamp; events and service checks. A message's context is
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
| `POST /api/v1/check_run` | service checks | records as DogStatsD's are (`source:service_check`) |
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
`client_drop_p0s` false), so a tracer computes no stats of its own and drops no trace for them; each
answer gives every service the rate 1 (`rate_by_service`), so a tracer keeps every trace its own
rules do not drop; its other products' paths are answered and passed over. Every span is counted
first by the runner's concentrator (`kiln_obs::ddtrace::Concentrator`), as the Agent's counts
them: entries, exits by peer, and spans of no kind that are top-level (marked so by the tracer, or
their parent none of their trace's, or another service's), in buckets of ten seconds of their
starts, each written once twenty seconds past its end (the Agent's buffer), a span later than that
in the oldest bucket still open (as the Agent shifts it), one more than a minute ahead in now's;
2,000 groups a bucket at the most, the rest in one of `N/A`. Then a trace its tracer dropped (its
priority below zero, `manual.drop`, a rule's rejection; or zero and no error in it) is not kept.
The series carry the runner's name as their `host`, the one writer of each, so each is written once
a bucket; the spans are `source:datadog-agent`, so no span is counted twice.

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
low 64 bits, mixed again), so a trace is read whole or not at all, and the counts are scaled by `k`.
Each service's requests and errors on it are its trace metrics' (every request, a trace an Agent
sampled away too; a service only of calls, a tracer's integration's, its calls'), the spans' only
where the range's metrics have none of it; a service of requests none of whose spans were kept is
on it still.

## 9. Monitors

A monitor is Datadog's (`kiln_obs::monitor`, `crate::observe::monitors`): a query, a message and
options, so a monitor written for Datadog reads here.

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
  are saved as they change.
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
notifications held for a while, for the monitors a downtime names (one by its id, or every one that
carries all of some tags) and the groups its scope holds (`env:prod host:(web-1 OR web-2) -region:eu`,
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

## 11. UI

Observe, in kiln's console (`ui/src/views/observe/`), built from the console's own parts (panels,
facts, the pill tabs, uPlot charts) and its rules: data through Suspense resources, no effects, the
time range and the search in the address (a page is a link).

- **Overview**: what is in trouble (monitors alerting, warning, without data), the newest events,
  and the store: series, bytes on disk, from when it keeps, each machine's replica, how far behind it
  is and whether it holds the same (its full digest at the server's index; "Compare replicas" asks
  for them).
- **Dashboards**: Datadog's model near enough. Widgets on a grid of twelve columns (one column on a
  phone): timeseries (lines, areas or bars), query value, top list, table, heatmap, distribution,
  log stream, event stream, monitors, note (Markdown). A heatmap is its query's values counted in
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
- **SLOs**: their states, in trouble first; each one's target and timeframe, SLI, budget left (a
  meter) and the hour's burn rate. An SLO's page: its standing and its four burn rates; each day's
  SLI against the target and the budget left through it, as lines (an SLI's days differ in their
  last places, so its axis begins just under the lowest of them and the target, which bars, measured
  from zero, cannot); and the monitors that alert on it, the workbook's two for its timeframe added
  in a click while none reads its burn rate. Made and edited in a form: good events over all, or
  time slices of a query meeting a condition.
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

Every chart is uPlot, already kiln's, but the heatmap and the distribution, drawn on a canvas. To
come: series reduced to the chart's width with largest-triangle-three-buckets (Steinarsson 2013) when a range holds more
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
samples is what would take that back. The rollups first came from a series' samples decoded whole
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
log queue then holds rows beside records, its acks counted across both.

Through kiln's nginx pipeline a line costs some ten plain ones (the same run: 1.8 M lines a second
plain, 160 k parsed, on a loaded machine): perf puts 18% in the regular expressions' backtracker
reading a matching rule's groups (and a date's again), 14% in their lazy DFAs, 8% setting
attributes, the rest allocation. A rule is told by the DFA alone before its groups are read, each
group found by its place (a name looked up hashed it every match: 2.5%), and dates in UTC or with an
offset reckoned without the zone table's lock; a set of every rule, one pass for all, was tried and
was no faster. Next: a date read from the rule's own groups, and fewer allocations.

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

A trace by its id, in a span store of 1,024,000 spans (341,334 traces) in 2,000 blocks: 1.6 ms, its
own block and a median of 20 others read (p99 31, the most 37): the 1% each filter is sized for,
whichever trace. Before the filters were salted by their block (§8.7), the median was 7 and the most
654: filters of one size set the same bits for the tokens every block holds (the service, `server`,
the names), so a query whose places fell on them passed most of them.

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
`file://` repository, its job's count, duration and event found; a monitor that alerts six
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
span's parent its own trace's) (2026-10-19: passes in Chromium and WebKit as a phone, on fedora).
Each makes its own records and checks through the API, not the page. 2026-10-18: all pass in Chromium at 1,440 px and in WebKit as a phone (390 px, touch);
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

**Under load** (`crates/kiln/tests/observe_load.rs`): the flood of §12.11, every point taken
stored exactly once, the replicas alike; and a runner flooded while its server is killed (SIGKILL)
and later started again: it refuses in time (0.5 s at full rate, 2.4 s at 100 k points a second),
its memory bounded, and everything it took is stored once the server is back.

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
check, over UDP and the Unix socket): every one as sent. What the API refuses (a TCP address, the
99.9th percentile, an unknown aggregate) is refused; removed, a listener closes where it was opened,
its socket file with it. 2026-10-19: passes (141 s). The flood beside it is §12.20's.

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
cart → postgresql (named) at 40 calls each. Spans with no ids refused, said as the partial
success (protobuf and JSON). 2026-10-19: passes (71 s, on fedora).

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
and cart → cart-db at the 20 kept calls each, each service's requests and errors its stats' 40
and 8; `trace.hits` of one host, the Agent's. Then the same services, their tracers sending straight
to a kiln runner's receiver on the Agent's port (no Agent anywhere): `/info` says `/v0.4/traces`
and `/v0.5/traces` and no client drops; the same 20 kept traces whole and none of the 20 dropped,
every request counted by the runner's concentrator (40 and 8, the database's 40 and 8), `host` the
runner's. (The services flush their tracers and say they are up on control ports of their own,
untraced, so no driver's call is a trace.) 2026-10-20: both pass (136 s, on fedora).

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
its lines JSON, each numbered, every tenth a health check a rule leaves out, a token in each a rule
masks; 500 lines there before it, read from the beginning as it says; then the file renamed
(logrotate's `create`) with lines written to both the old and the new, copied and truncated
(logrotate's `copytruncate`), and 4,000 lines written at 2,000 a second while the runner is stopped
(SIGTERM) and started, then 4,000 more while it is killed (SIGKILL) and started. Every line's records
counted by its number: none lost, none of those left out stored, none repeated through the renaming,
the copy and the stop; the kill repeats what was written in its last second (0 to 477 of the 4,000
over six runs). A record as stored: masked, its level, its host, its file's path, the source's tag.
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
(2 min). The Windows build is checked (clippy, `x86_64-pc-windows-gnu`); `wevtutil` itself is not
run by any scenario: the fleet has no Windows machine.

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
back; then the server stopped and started, 300 more records late for windows before it, and 15 s
more. Every window holds what was sent through all of it, and the records 25 minutes back are
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
attribute as one JSON text where Datadog's reads `@http.status_code`, now flattened as theirs.

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
2026-10-18: passes (18 s).

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
the six runs since, watched, answered every request within 3.84 ms. What held it is not yet known:
the next time, the stacks say. Where the server's anonymous memory lies is said at each third's
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
