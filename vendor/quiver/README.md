# quiver
A simple queuing service designed for agentic scale.

Quiver is the topic store: the cornerstone for an event-driven world. Topics, partitions, placement, replication, consumer groups and SQS-style queues, built on Focal's
bus with its gaps fixed and offered back. One binary from a laptop to global multi-region. Kiln
moves to publish and subscribe on it.

**Status:** topics of the ephemeral class on one node, in process (`quiver::Node`, reached through
`quiver-api`), on Focal's bus; kiln's live feed of every session rides it. The `quiver` binary says
what it is, and nothing more yet. `STATUS.md` says what works and what's next; `docs/design/` is the
design; `bench/` the benchmarks.

```sh
cargo build --release      # target/release/quiver
bash scripts/gates.sh      # every gate, as kiln CI runs them
```

MIT licensed (`LICENSE`).
