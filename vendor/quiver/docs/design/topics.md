# Topics: the contract every implementation keeps

`quiver-api` is what other code reaches Quiver through (CLAUDE.md §9): the `Topics` trait, its
simulator (`quiver_api::sim::SimTopics`) and its conformance suite (`quiver_api::conformance`).
Quiver's node passes the suite (`crates/quiver/tests/topics.rs`), and so does the simulator; kiln
reaches Quiver through its own seam, kiln-seams' `Bus`, whose suite Quiver passes in kiln. This
page states the contract, clause by clause, under the ids the suite's cases carry; a doc-truth
test (`crates/quiver-api/tests/doc_truth.rs`) holds the two together.

## What a topic is

A topic holds messages, each given the topic's next offset (0 for the first) in the order it was
published, each a key (optional) and a payload, stamped with when it was published. Anyone reads
a topic from any offset it still keeps, or follows it with a subscription that takes each message
in order as it comes. A topic keeps its newest messages, up to its retention in messages and in
bytes (keys and payloads); past it the oldest are let go. A reader asking for what's gone is
refused `Behind`, naming where what's kept starts; a subscription that falls behind takes a lag
notice (`Next::Lag { from, to }`: the offsets it missed) and goes on from the oldest kept.

Publishing never waits for a reader or a subscription: one that stalls holds up no publish and no
other reader (kiln's CP-T2.1, AC-T2.1.5). Each topic states its class (Focal's doc 33):
ephemeral, replicated, persisted or windowed. An implementation serves the classes it can and
refuses the others, typed (`Refusal::Unsupported`), never serving one class as another. Quiver's
node serves the ephemeral class today (docs/design/focal-bus.md).

Methods of `Topics`: `create`, `delete`, `publish`, `read`, `wait`, `bounds`, `subscribe`.
Methods of `Subscription`: `poll_ready`, `take`, `position`.

## Bounds

Topic names are 1 to 128 of `A-Z a-z 0-9 _ - . /`; keys to 1024 bytes; payloads to 1 MiB; 1000
messages a read; a topic retains 1 to a million messages and 1 byte to 4 GiB.

## Clauses

- **QUIVER-1** a topic's messages get offsets 0, 1, 2… in the order they were published
- **QUIVER-2** a read from an offset gives the messages from it in publish order, keys and payloads intact, and where to go on
- **QUIVER-3** every reader and every subscription takes every message kept: none takes them from another
- **QUIVER-4** a topic keeps its newest messages up to its retention in messages; a read from before them is refused Behind, naming where they start
- **QUIVER-5** a topic keeps its newest messages up to its retention in bytes (keys and payloads); a message it can't keep alone is refused TooLarge
- **QUIVER-6** a wait completes once a message at its offset or later is there, at once when one is
- **QUIVER-7** every request about a topic that doesn't exist is refused NoTopic
- **QUIVER-8** creating a topic again with the same settings changes nothing; with others it's refused Exists; bad names and retentions are refused Invalid and make nothing
- **QUIVER-9** a payload or key over its limit is refused TooLarge and publishes nothing; the largest allowed is kept whole
- **QUIVER-10** a read gives at most its limit and says where to go on; at or past the end it's empty; a limit of 0 is refused
- **QUIVER-11** topics are apart: each has its own messages, offsets and subscriptions
- **QUIVER-12** a subscription takes every message from its start, in order, and each one published after, as published
- **QUIVER-13** a subscription starts at the oldest kept, at the latest, or at an offset (past the end: the end)
- **QUIVER-14** a subscription that falls behind what's kept takes a lag notice naming the offsets it missed, then goes on from the oldest kept
- **QUIVER-15** a stalled subscription holds up no publish and no other subscription; it takes a lag notice when it goes on
- **QUIVER-16** a subscription is ready once a take has something, and its waker is woken when a publish gives it something
- **QUIVER-17** a deleted topic's subscriptions end, its waits are refused NoTopic, and created again its offsets start at 0
- **QUIVER-18** each class an implementation serves takes topics; one it doesn't is refused Unsupported, never served as another
- **QUIVER-19** each message is stamped with when it was published, in offset order, the same in reads and subscriptions
