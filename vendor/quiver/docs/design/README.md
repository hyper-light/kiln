# Quiver's design

The target, from track T2 of kiln's critical path (kiln plan `01a157fdefc37396957ea10adb51a19f`). Each decision below is
made by the goal named beside it, recorded here with its sources, its evidence and what would
falsify it (CLAUDE.md §5).

## What it is

Quiver is the topic store, and the SQS-style work queues it was first described as are its share
groups. It holds:
- `quiver-api`: the traits other code uses, with a simulator implementation;
- `quiver`: topics, partitions, placement, groups, retention and transactions;
- `quiver-sqs`: the SQS-compatible API;
- the `quiver` binary.

It vendors Focal's bus (branch `bus-replicated`) and hyper-raft at the revision the bus takes,
Amphora for its catalog and offsets, and Mantle's client for tiering. Fixes to Focal's bus are
made in the vendored copy, recorded in its `UPSTREAM.md` and offered to Focal as branches.
Kiln vendors Quiver at `vendor/quiver` and reaches it only through kiln-seams' `Bus`.

## The target

- **One code path.** One binary on a laptop runs one-voter parts, a fleet parts of three;
  region-local partitions run under a global catalog. Nobody picks part counts, leaders or
  placement to set it up.
- **The write-ahead log is mandatory.** A persisted publish is acknowledged only once a quorum
  fsynced it; `GroupStore` sits on a real device with the full flush; `RamStore` is test-only.
- **Fate classes** from Focal's doc 33: ephemeral, replicated, persisted and windowed, each with
  its stated loss bound.
- **Topics** with versioned schemas and compatibility rules (as Confluent's registry has them), a
  fate class, retention by time, size or key compaction, and access rules; the catalog in Amphora.
- **Partitions** keyed by hash or range, ordered per key, placed by a driver that spreads leaders
  and follows locality and load; reassignments fenced, nothing lost or doubled across one.
- **Groups:** consumer groups with KIP-848 cooperative rebalancing; share groups as KIP-932 queues,
  each message acknowledged on its own, with delivery counts, lock timeouts and dead letters;
  FIFO message groups and deduplication ids as SQS's FIFO queues.
- **An SQS-compatible API** the AWS CLI, boto3 and the SDKs use unchanged.
- **Exactly once in effect:** idempotent producers with epochs, transactions across partitions and
  offsets, read-committed consumers.
- **Compared class for class** with Kafka (KRaft), Redpanda, NATS JetStream and Pulsar, and SQS.

## Decisions

| Decision | Made by | Status |
|---|---|---|
| Vendoring Focal's bus behind `quiver-api` (`focal-bus.md`), the contract (`topics.md`) | CP-T2.1 | decided |
| The persisted tail: liveness apart from disk progress, leases, per-device I/O | CP-T2.2 | open |
| Typed acknowledgements and loss bounds | CP-T2.3 | open |
| The windowed class, calibration and accrual suspicion | CP-T2.4 | open |
| The topic catalog and schema compatibility | CP-T2.5 | open |
| Partition placement after Slicer and Shard Manager | CP-T2.6 | open |
| Consumer and share groups | CP-T2.7 | open |
| The SQS protocol and SigV4 from Mantle's `mantle-s3` | CP-T2.8 | open |
| Retention, compaction and tiering to Mantle | CP-T2.9 | open |
| Transactions | CP-T2.10 | open |
