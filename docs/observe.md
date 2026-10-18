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
| O-4 | Query language: metrics (parser without recursion, evaluator, §12.5; answers agree to the bit on every replica), log search and analytics, events; the API (§7); Datadog's whole function catalog, calendar rollups in any timezone (the tz database in the binary), `weighted()` (§6) | done |
| O-5 | Ingestion (§8): OTLP/HTTP metrics and logs (protobuf, JSON, gzip) on the server and every runner; every node's host, container and process; kiln's registry; kiln CI's jobs and runs, kiln CD's deploys and status; GitHub's webhooks; Datadog-style series, logs and events | done |
| O-6 | Logs: LZ4 (§12.6), blocks with summaries and token filters (§12.7), Datadog's search syntax, the store's log head and files, OTLP logs, analytics (count, cardinality, sum, avg, min, max by facets, in time buckets; every facet's counts in one pass), allocating a group, not a row (§12.8); patterns (Drain, 0.887 on Loghub, §12.13); live tail (each record once as the replica applies it, a uniform sample past the rate asked, §6, §12.14) | done |
| O-7 | Monitors (§9): metric, change, log, anomaly; thresholds with recovery, warnings, NO DATA, renotify, mute, evaluation delay; Datadog's templates and mentions; anomaly detection (§9.1): Datadog's three algorithms, seasons learned, deviations calibrated per series, scored on NAB 52.6 (§12.12), `anomalies()` in queries with its band drawn; composites (Datadog's algebra and group matching, its table a test); forecasts (`forecast()` and forecast monitors; chosen and scored on M4, §12.15); outliers (`outliers()` and outlier monitors; DBSCAN's constant from a simulation, §9.1) | done |
| O-8 | The UI (§11): Observe's overview, dashboards (kiln's own and yours, template variables, edited in place), the metrics explorer, the logs explorer and event stream, monitors (composites among them) and integrations; checked by scenarios in Chromium and WebKit (§13); next: distribution and heatmap widgets | done |
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

A replica publishes the log records it applies to a **feed** (`kiln_obs::logs::tail`) while anyone
follows them (live tail, §6): each batch's rows copied once into a ring bounded at 32 MiB, kept until
every follower has read them or the bound passes them by (a follower that fell that far behind is
told how many rows it missed; none holds the replica up). With no one following, applying costs a
load. What a replica applies again is not published again: opening, it applies the entries after its
last cut once more, and the feed skips every entry its log held as it opened.

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
fills gaps of at most `seconds` (300 unless said, Datadog's default; larger is allowed).
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
same search syntax.

## 7. API

Under `/api/observe/v1/` (all JSON; the UI uses nothing else), each route under a capability of
kiln's (docs/access.md): every GET, and every query, search and check asked by POST, needs
`observe.read` (a viewer's); dashboards, monitors and data posted need `observe.write` (a
developer's); integrations, clouds and the store's operations need `observe.manage` (a
maintainer's). `/api/openapi.json` says each route's. Built:

| | |
| --- | --- |
| `POST query` (or `GET query?query=&from=&to=&step=`) | metric query → series on a grid (`from`, `to` in ms, `step` or `points`); NaN is null; the answer states the index it was read at; a series with a band (`anomalies()`, `forecast()`) has `bounds` (`lower`, `upper`, `score`, `k`, `algorithm`, and a forecast's `forecast_from`, its first bucket forecast; `to` past now draws it) |
| `GET metrics?match=` , `GET tags?key=&match=` | metric names and tag values, sorted (autocomplete) |
| `POST logs/search` | `{query, from, to, limit}` → records newest first, with the blocks pruned |
| `POST series` | ingest points as Datadog's v1 series API takes them (`metric`, `type`, `tags`, `points` of `[seconds, value]`) |
| `POST logs` | ingest records as Datadog's log intake takes them (`message`, `timestamp`, `status`, `service`, `hostname`, `ddsource`, `attributes`) |
| `GET status` | the group: members, leader, term, applied and durable index, series, blocks, bytes on disk (observe's folder, hard-linked snapshot files once), cutoff, this node's writes, each machine's replica, digests (also in hex: a u64 is more than JavaScript's numbers hold) |
| `POST compact`, `POST verify` | cut and compact the log now; take every replica's full digest (an operator's, or a test's, check that replicas agree) |
| `POST logs/aggregate` | log analytics: `{query, from, to, by: [facets], measure, of}` → groups; with `step` (ms), each group's value a bucket on a grid (the explorer's histogram) |
| `POST logs/facets` | `{query, from, to, facets, top}` → each facet's most common values and how many it has, every facet in one pass |
| `POST logs/patterns` | `{query, from, to}` → the search's newest records (10,000 at the most) grouped by their template (Drain, §12.13): each its count, first and last seen, newest message and its records by status, the most common first |
| `GET logs/tail?query=&rate=` | live tail (§6), a WebSocket: each 200 ms, `{type: "records", records, read, matched, missed, ms}`, the tick's records (all the search matched, or a uniform sample past `rate` a second: 50 unless said, 1 to 1,000) and how many rows it read, matched and missed (missed: it fell more than the feed's bound behind); idle, one a second, its counts 0. `{type: "error", message}`, then closed, for a search that does not parse, a rate out of bounds, or a node that 16 tails follow already |
| `GET/POST dashboards`, `GET/PUT/DELETE dashboards/{id}` | dashboards (§11): kiln's own (`builtin:hosts`, `builtin:ci`, `builtin:cd`, read-only) and yours, kept in `<observe>/dashboards.json` |
| `POST events`, `POST events/search` | an event (Datadog's v1 shape); events searched |
| `GET/POST monitors`, `GET/PUT/DELETE monitors/{id}`, `POST monitors/{id}/mute`, `…/unmute`, `POST monitors/validate` | monitors (§9), each with its state and its groups', a composite (`type: "composite"`) with what it reads (each monitor's letter, name and state, its condition in letters and in names); validate says what a query watches (the metric query or the log search, and its window), so a page draws what is evaluated, or, given `type`, whether a composite's monitors are there to read; a monitor a composite reads is not deleted (409) |
| `GET integrations`, `PUT/DELETE integrations/{name}`, `POST integrations/{name}/test` | where monitors say things (§10) |
| `GET clouds`, `PUT/DELETE clouds/{name}`, `POST clouds/{name}/pull`, `POST clouds/{name}/test` | cloud integrations (§10.1), each with its status; a secret left out of a `PUT` (or `"…"`, as shown) is the one kept; `test` pulls a configuration once and writes nothing, saying what it would read |
| `GET snapshot/{point}/{file}?machine=` | a snapshot's file, for the runner of `machine` with its token |


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
- **kiln's own spans** (`crate::observe::spans`): each closed `tracing` span of a target observe
  keeps (`kiln::upgrade`: runners' blue/green upgrades, `upgrade`, `upgrade.start`,
  `upgrade.healthy`, `upgrade.move`, `upgrade.retire`, `upgrade.rollback`) is
  `kiln.<span>.duration` (seconds of wall time, a gauge a close) and `kiln.<span>.count`, tagged
  with the fields low in cardinality (`runner`, `from`, `to`, `what`, `result`, `reason`, …; a
  child's taken from its ancestors), and an event (`service:kiln.upgrade @event.type:<span>`)
  holding every field, ids too (`@upgrade_id:…` finds one upgrade's story), an error when its
  `result` failed or rolled back. A layer of kiln's subscriber with its own filter: `KILN_LOG`
  does not hide these spans from observe, nor do they reach the log.

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
  `change(avg(last_5m),last_1h):…` and `pct_change(…)`; anomaly monitors
  `avg(last_15m):anomalies(<metric query>, 'robust', 3) >= 0.5`, whose value a group is the share of
  its window's buckets past the band on the side asked (Datadog's: `>= 1`, all of them); log monitors
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
  into a monitor, in a step. An `anomalies()` series is drawn with its band beneath it, in its color,
  its points past the band marked (every chart: a dashboard's, a monitor's). A chart of a
  `forecast()` reaches half its range past now (a forecast monitor's, its horizon): each series
  solid to its last point, then dashed in its color with its band; its figures (last, average, least,
  most; a widget's value) are its data's alone. An `outliers()` chart draws its outliers in their
  colors and every other member grey. A calendar rollup's buckets are drawn at their own starts.
- **Patterns** (the logs explorer's and the event stream's other view, `?view=patterns`): the
  search's records grouped by their template (Drain), the most common first, each its count, its
  worst status, its template (variables a quiet `*`) and when last seen; a click narrows the search
  to the pattern's fixed text and shows its records.
- **Live tail** (their third view, `?view=live`): the search's records as they come, the newest on
  top, as the records list shows them; the line above says how many a second match and, when they
  come faster than 50 a second, that a sample of them is shown. Pause holds what comes (counted)
  until Resume; opening a record pauses.
- **Logs explorer and event stream**: a search, a histogram of its records by status (stacked, a
  bucket of a round size, about 60 to the range), each facet's most common values (a click narrows
  the search to one, again widens it), the newest 100 records (and 100 more on asking), each opened
  to its fields and attributes, any one a filter a click away.
- **Monitors**: their states, in trouble first; each one's groups, what it watches drawn over the
  range with its thresholds, what it said (its events), mute and unmute; made and edited in a form
  that checks its query. A composite (the form's other kind) is its monitors chosen by letter and a
  condition over the letters; its page lists what it reads, each one's state a link away.
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
reads, and one of them refused deletion from its page while the composite reads it; a signed webhook
added, tested (the receiver checks the signature) and removed. Each makes its own records and checks through the API,
not the page. 2026-10-18: all pass in Chromium at 1,440 px and in WebKit as a phone (390 px, touch);
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
