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
