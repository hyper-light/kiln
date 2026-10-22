# kiln-http against hyper on tokio

A workspace of its own, so the baseline (hyper on tokio) never reaches kiln's lockfile. Every server
answers `200 OK` with `Hello, World!` (13 bytes, plus `date`), and every server meets the same load
generator.

| binary | what |
|---|---|
| `kiln-http` | kiln-http on hyper-rt: one shard per `--threads`, each accepting on its own `SO_REUSEPORT` listener |
| `hyper-tokio` | hyper 1.x on tokio's multi-threaded runtime, as a hyper or axum service is usually run; `--pipeline-flush` turns on hyper's batching of pipelined answers (off by default) |
| `raw-epoll` | no HTTP stack and no runtime: an epoll loop per thread answering every request it read in one write, the kernel's ceiling for HTTP/1.1 |
| `load` | the load generator: HTTP/1.1 (pipelined to `--depth`), a new connection per request (`churn`), HTTP/2 with `--streams` per connection, or `--conns` idle connections weighed in the server's resident memory |
| `micro` | each stack's HTTP/1.1 server over an in-memory transport, polled on one thread: its cost per request in user space alone |
| `interop` | kiln-http against hyper's clients and servers, both protocols, both ways |

```sh
cargo build --release
./matrix.sh results.jsonl 3        # every workload, every server, three interleaved rounds
python3 summarize.py --medians < results.jsonl
./target/release/interop
```

`run.sh` runs a server on CPUs 0–3 (four threads for every server) and the load generator on CPUs
8–15, with eight threads pinned one to a CPU. It reads the server's user and system CPU time from
`/proc` around the run, so each figure also comes as server CPU per request. That figure doesn't
depend on how much the load generator could push. Each run takes one warm-up second and five
measured. The rounds are interleaved so a shared machine's noise falls on every server alike, and
the tables give each figure's median over the rounds.

## Results

Measured on 2026-10-22, on a 16-core (32-thread) AMD Ryzen 9 9950X3D2 shared with other work: the
load average ran from 15 to 37 during the runs. Every server had four threads. The raw lines are
in `results/`.

### Where the gap is an order of magnitude

**HTTP/2, 64 connections of 100 streams each.** This is how a browser uses kiln, with many requests
over one connection. Three rounds, kiln-http alternating with hyper (`results/2026-10-22-h2.jsonl`):

| server | requests/s | server CPU per request | p50 | p99 |
|---|---:|---:|---:|---:|
| kiln-http | 13,189,360 (10.0×) | 273 ns (9.2× less) | 475 µs | 639 µs |
| hyper on tokio | 1,319,132 | 2,494 ns | 934 µs | 26,739 µs |

hyper spends 1.8 µs of user time on each request (a task per stream, the `h2` crate's shared stream
store) and kiln-http 0.21 µs (the stream's handler polled in the connection's task, HPACK fields
built straight into the request). Under the machine's contention it's starker. In two of the
matrix's three rounds, hyper's HTTP/2 fell onto about one core, at 268,158 and 331,101
requests/s, while kiln-http, also slowed, still answered 6.7M and 14.8M. In the round without
that collapse, hyper answered 1,597,924 to kiln-http's 14,472,140 (9.1×).

**Idle connections.** Each connection made one request and then stayed open. The figure is the
server's resident memory per connection:

| | kiln-http | hyper on tokio | |
|---|---:|---:|---|
| HTTP/1.1, 50 000 connections | 868 B | 11,377 B | 13.1× less |
| HTTP/2, 20 000 connections | 2,487 B | 19,989 B | 8.0× less |

An idle kiln-http connection holds no buffer. hyper keeps an 8 KiB read buffer and a write buffer
per connection. kiln-http's runtime also preallocates its task slots (about 700 B each, 64 Ki slots
here), so its total resident memory at 50 000 connections was 85 MiB against hyper's 546 MiB:
6.4× less.

### Where the kernel sets the pace

The whole matrix, the median of three rounds (`results/2026-10-22-matrix.jsonl`):

| workload | kiln-http | hyper on tokio | hyper, `pipeline_flush` | raw epoll |
|---|---:|---:|---:|---:|
| HTTP/1.1, 256 connections, pipelined 16 deep | 6,707,344 (6.3×) | 1,071,399 | 5,921,299 | 12,017,389 |
| server CPU per request | 434 ns (4.4× less) | 1,892 ns | 486 ns | 178 ns |
| HTTP/1.1, 256 connections, one request at a time | 450,018 | 467,605 | | 892,606 |
| HTTP/2, 256 connections × 10 streams | 3,656,440 (3.7×) | 976,687 | | |
| HTTP/1.1, a new connection per request | 116,268 (2.1×) | 56,515 | | 19,997 |

- **Pipelined HTTP/1.1.** By default hyper writes once per response. kiln-http answers everything
  it read in one write, which is 6.3× hyper's default. With `pipeline_flush` hyper batches too,
  and kiln-http's lead is 1.13×: both are then bound by the kernel's work for a write, and raw
  epoll, which does nothing but that, is 1.8× kiln-http.
- **One request at a time.** Each request costs a read, a write and a wakeup, nearly all of it in
  the kernel. Even raw epoll, with no stack at all, is only 1.9× these servers here; no HTTP
  implementation can be ten times faster. kiln-http and hyper are level (in an earlier matrix,
  659,405 to 638,103).
- **A new connection per request.** The kernel's accept and close dominate here too; kiln-http's
  2.1× comes from closing its side first. Raw epoll leaves the close to the client, whose ports
  then wait out TIME_WAIT.

