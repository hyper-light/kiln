# The browser protocol

kiln's own protocol between a browser and the server (goal 2 of the browser protocol program,
plan `01a1a832eb04756a9f5a6c3fef8cf63a`). One connection per browser carries every request,
answer and live subscription a page has, in small binary frames, with changes sent as deltas,
latest values coalesced while they wait, priorities decided when bytes leave, and credit per
subscription and per connection. This page is normative: `crates/kiln-bp` (Rust) and
`ui/src/api/bp/` (TypeScript) implement it, and a test holds the frame table below to the code.

It has no carrier of its own. The WebSocket carrier is goal 3's, WebTransport goal 5's. Both drive
the same sans-IO core: the carrier hands it the bytes that came (`receive`) and asks it for bytes to
send when it has room (`poll_transmit(room)`), so the core decides what goes next at the moment
bytes leave, never when they're queued.

Where the bytes come from (research `01a1a832eb0976cdaeb4c389474087ad`): postcard alone is worth
about 1.4x against compressed JSON (f14). What wins is not sending whole records again (f8, f11),
not fetching again after "changed" or a reconnect (f7), joining what waits (f11, f21, f43), and
putting several small frames in one carrier message (f12).

## Encoding

Every body is postcard (f15), with the protocol's own wire types (f16): no `serde_json::Value`,
no skipped, flattened, untagged or internally tagged fields. The types:

| Type | On the wire |
|---|---|
| `u8` | one byte |
| `u16`, `u32` | an unsigned varint (7 bits a byte, low first, high bit says more follows), refused past the type |
| `u53` | an unsigned varint, refused past 2^53 - 1 (JavaScript's exact integers) |
| `i53` | a zigzag varint, refused outside ±(2^53 - 1) |
| `bits` | an unsigned varint of 64 bits: capability bits, field masks, schema hashes (a `bigint` in TypeScript) |
| `bool` | `0` or `1`; anything else is refused |
| `str` | a varint length, then that many bytes of UTF-8 |
| `bytes` | a varint length, then that many bytes |
| `opt<T>` | `0`, or `1` then a `T` |
| `vec<T>` | a varint count, then that many `T` |
| an enum | a varint variant index, then that variant's fields in order |
| a struct | its fields in order, no names, no length |

A varint is at most 10 bytes. There are no floats: what would be one is an integer in stated units
(per mille, hundredths), so both languages read and write the same bits.

## Frames

A carrier message holds one or more frames back to back. A frame is:

```
kind: u32 varint | len: u32 varint | body: len bytes
```

Several small due frames share one message (f12): a 19-byte delta alone costs about 95 bytes on
the wire, and packed, a few.

### Reading, so builds of different ages work together

- A frame is read whole or refused, never half-applied, and never with a panic or an exception
  that escapes.
- `len` is checked against the bytes left in the message before anything is read or allocated;
  past them, the rest of the message is refused (`cut_short`).
- A kind this build doesn't know is skipped by its `len` and counted (`unknown`); the next frame is
  read.
- Bytes after the fields a build knows are a newer build's trailing fields: counted, not refused.
- Fields missing at a body's end read as their defaults (0, empty, false, none, an enum's first
  variant), so an older sender's frame reads.
- Inside a body, every length and count is checked against the bytes left before it is allocated.
- Only a frame's own body grows this way. A struct or enum inside a body is read strictly: it never
  grows; what a frame needs more of goes as a new trailing field of the frame.

### Refusals

A frame that can't be read is refused with a code (docs/wire.md, "What can't be read") and why:

| Why | Index | Code | When |
|---|---|---|---|
| `cut_short` | 0 | `malformed` | The bytes ran out inside a field, or a frame's `len` is past the message. |
| `too_large` | 1 | `malformed` | A frame, a string, bytes or a count past its limit. |
| `bad_varint` | 2 | `malformed` | A varint past 10 bytes, or past its type (`u16`, `u32`, `u53`, `i53`). |
| `bad_value` | 3 | `malformed` | A `bool` or `opt` tag not 0 or 1, a `str` that isn't UTF-8. |
| `too_deep` | 4 | `malformed` | A fragment or compressed batch holding another. |
| `unknown_kind` | 5 | `unknown` | A frame kind this build doesn't know (skipped by its length). |
| `unknown_variant` | 6 | `unknown` | An enum variant this build doesn't know, inside a known kind. |
| `left_over` | 7 | `malformed` | Bytes after the one frame a reassembled fragment holds. |
| `out_of_order` | 8 | `malformed` | A frame the connection's state doesn't take: anything before `Open`, a second `Open`, a server's frame sent up. |

Each refusal is told to the sender in an `Unread` frame (kind, code, why) and is an event the
server notes on its wires `bp_up` (what a browser sent) and `bp_down` (what a browser says it
couldn't read): `kiln_wire_refused_total{wire,code}` and the audit log, as today's sockets are
noted. A refusal never ends the connection; the next valid frame is read. A connection that breaks
the protocol's rules (a fragment past its limit, credit taken back) is closed with `Close`
(`protocol`).

## The frame kinds

Kinds are numbered once and never reused: a new kind goes after every one here, and a test pins
each index. "Up" is browser to server, "down" server to browser. The class is docs/wire.md's and
decides what coalescing may do: `state` and `telemetry` keep their newest value while queued,
`stream` text appended to the same place is joined, and commands, asks, answers and acks are never
merged or dropped.

<!-- bp:kinds -->
| Kind | Index | Way | Fields, in order | Class | Priority | Coalesced | Compressed | Urgent | Lane |
|---|---|---|---|---|---|---|---|---|---|
| `Open` | 0 | up | `version: u32, oldest: u32, caps: bits, schema: bits, ticket: bytes, idle_ms: u32, window: u53, build: str` | handshake | control | never | never (a secret: the ticket) | yes | control |
| `Accepted` | 1 | down | `version: u32, caps: bits, idle_ms: u32, born: u53, fragment: u32, build: str` | handshake | control | never | never | yes | control |
| `Refused` | 2 | down | `code: OpenRefusal, why: str, build: str` | handshake | control | never | never | yes | control |
| `Request` | 3 | up | `id: u53, call: Call` | command or ask, as its call says | control | never | never | yes | control |
| `Answer` | 4 | down | `id: u53, outcome: Outcome, at: opt<Mark>` | answer | control | never | only when its route says its trust class | yes | control |
| `Subscribe` | 5 | up | `sub: u32, topic: Topic, cursor: opt<Cursor>, credit: u53` | state | control | never | never | no | control |
| `Unsubscribe` | 6 | up | `sub: u32` | state | control | never | never | no | control |
| `Snapshot` | 7 | down | `sub: u32, cursor: Cursor` | stream | bulk | never | its topic's trust class | no | sub |
| `Resumed` | 8 | down | `sub: u32, cursor: Cursor` | stream | its topic's | never | its topic's trust class | no | sub |
| `Change` | 9 | down | `sub: u32, rev: u53, step: u53, delta: Delta` | its topic's (state, telemetry or stream) | its topic's | state, telemetry: newest per key; stream: appends joined | its topic's trust class | when its topic is interactive | sub |
| `Tail` | 10 | down | `sub: u32, data: bytes` | stream | its topic's | joined | its topic's trust class | when its topic is interactive | sub |
| `Ready` | 11 | down | `sub: u32, cursor: Cursor` | stream | its topic's | never | its topic's trust class | no | sub |
| `End` | 12 | down | `sub: u32, code: EndCode, why: str` | answer | control | never | never | no | sub |
| `Credit` | 13 | up | `sub: u32, upto: u53` | state | control | newest per `sub` | never | no | control |
| `Fragment` | 14 | both | `lane: u32, last: bool, data: bytes` | carried | its frame's | never | never | its frame's | its frame's |
| `Idle` | 15 | both | `got: u53` | telemetry | control | newest | never | no | datagram |
| `Probe` | 16 | both | `nonce: u32` | ask | control, ahead of all | never | never | yes | datagram |
| `ProbeAck` | 17 | both | `nonce: u32, got: u53` | answer | control | never | never | yes | datagram |
| `Unread` | 18 | both | `kind: u32, code: UnreadCode, why: UnreadWhy` | ack | control | never | never | no | control |
| `Compressed` | 19 | down | `ctx: u8, len: u32, data: bytes` | carried | its frames' | never | (is compression) | no | its frames' |
| `Close` | 20 | both | `code: CloseCode, why: str, build: str` | handshake | control | never | never | yes | control |
| `Input` | 21 | up | `sub: u32, data: bytes` | command, refused while away | interactive | never | never | yes | control |
| `SecretAnswer` | 22 | down | `id: u53, outcome: Outcome, at: opt<Mark>` | answer | control | never | never (a secret) | yes | control |
| `Resize` | 23 | up | `sub: u32, cols: u16, rows: u16` | state | interactive | newest per `sub` | never | yes | control |
<!-- /bp:kinds -->

The types inside them:

| Type | Fields or variants, in order |
|---|---|
| `Topic` | `kind: u32, key: str` |
| `Cursor` | `born: u53, rev: u53` |
| `Mark` | `topic: Topic, cursor: Cursor` |
| `Call` | `0 Http { method: Method, path: str, body: bytes }`, `1 Command { session: str, command: bytes }` |
| `Method` | `0 Get`, `1 Post`, `2 Put`, `3 Patch`, `4 Delete` |
| `Outcome` | `0 Taken`, `1 Accepted { epoch: u53, seq: u53, at: i53, repeat: bool }`, `2 Unchecked`, `3 Pending`, `4 Parked`, `5 Nack { code: str, why: str, again: bool }`, `6 Reply { status: u16, body: bytes }` |
| `Delta` | `0 Set { mask: bits, fields: vec<bytes> }`, `1 Put { key: Key, fields: vec<bytes> }`, `2 Patch { key: Key, mask: bits, fields: vec<bytes> }`, `3 Remove { key: Key }`, `4 Append { at: u53, data: bytes }`, `5 Extend { key: Key, field: u32, at: u53, data: bytes }` |
| `Key` | `0 Num(u53)`, `1 Str(str)`, `2 Ref(u32)` |
| `OpenRefusal` | `0 BadTicket`, `1 Expired`, `2 Reused`, `3 TooOld`, `4 TooNew`, `5 Schema`, `6 Busy`, `7 Malformed` |
| `EndCode` | `0 Done`, `1 Gone`, `2 Denied`, `3 Unknown`, `4 Limit` |
| `CloseCode` | `0 Bye`, `1 Expired`, `2 SwitchBuild`, `3 Revoked`, `4 Shutdown`, `5 Protocol` |
| `UnreadCode` | `0 Unknown`, `1 Malformed` |
| `UnreadWhy` | the refusals above, by index (`0 CutShort` to `8 OutOfOrder`) |

A delta that carries a record's fields (`Set`, `Put`, `Patch`) holds each field as its own
`bytes`, encoded as the topic's record type says, so the generic core moves, merges and keeps them
field by field without knowing the type, and the page's typed code reads them. A field's own
length costs a byte; it buys coalescing that needs no type.

## Opening

The browser opens with `Open`: the newest protocol version it speaks (`version`), the oldest
(`oldest`), its capability bits (`caps`, a `u64`, append-only: each new capability a new bit), the
hash of the codec schema it was built with (`schema`), a ticket, the idle period it promises to keep
(`idle_ms`), the bytes it lets the server send before it grants more (`window`), and its build id.

The server answers `Accepted`, with the version both speak (the newer of the two `oldest`s up to the
older of the two `version`s), the capabilities it takes of those offered, its own idle period, its
incarnation (`born`), the fragment bound it starts with, and its build; or `Refused`, with why:

| Refusal | When |
|---|---|
| `BadTicket` | The ticket isn't one the server made (forged, or unknown to it). |
| `Expired` | The ticket was made too long ago. |
| `Reused` | The ticket was taken already: each opens one connection, once. |
| `TooOld` | The browser's newest version is older than the server's oldest: `build` names the build to switch to. |
| `TooNew` | The browser's oldest version is newer than the server's newest (a server rolled back). |
| `Schema` | The same version with a different schema hash: the builds disagree; switch builds. |
| `Busy` | The server takes no more connections now. |
| `Malformed` | The first frame wasn't an `Open` that reads. |

Each refusal is noted as today's sockets note theirs: an event the server writes to
`kiln_wire_refused_total` (wire `bp_up`) and the audit log (`security.wire_refused`).

### The ticket

WebTransport sends no cookies (its session request has credentials mode "omit", f30), so the
protocol authenticates inside itself, the same way on every carrier. A ticket is 32 random bytes
the server mints for a fetch that carries the `kiln_session` cookie (the route is goal 3's), good
for one open, once, within 30 s. The server keeps what it minted (a hash of each, who it's for,
when it was made); a ticket it never made is `BadTicket`, one taken already `Reused`, one past its
life `Expired`. `crates/kiln-bp` decides this as pure code (`Tickets`, the clock and randomness
passed in); the server provides the minting route and the bytes.

The connection belongs to the browser session the ticket was minted for. It is cut (`Close`,
`Expired`) when that session expires, as plan `01a19fa90f92` has it, and the `Idle`, `Probe` and
`ProbeAck` frames never count as activity: the core says when the browser last did something
(`activity`), from every other frame.

### Version skew

A page keeps running the build it loaded after the server moves to a new one, until the tab is
next hidden (f40). So a newer server serves older pages for a while after every deploy. The server
speaks the version `Accepted` names: it sends only kinds whose version is at most that one, and in
each frame only the fields that version knows (it stops before newer trailing fields). Version N
serves N-1; a page older than that is refused `TooOld` and told the build to switch to. Version 1
is the first; the skew rules are held by a test schema with a second version's fields
(`crates/kiln-bp/tests/codec.rs`, `older_and_newer_layouts_read_both_ways`) and by opens across
version pairs (`tests/conn.rs`, builds speaking 3 down to 2 and 2 down to 1), so the first real
change finds them working. The core takes the versions it speaks from its settings
(`ServerConfig::version`, `oldest`), and each kind carries the version it came in (`Kind::since`).

## Requests and answers

A `Request` carries an id the page makes, unique to the browser (its tabs share the connection's
numbering), and a call: an HTTP request for the server's route table (goal 9 runs it through
`authorized()` as REST does), or a command to an agent session (its `Command` as JSON, which
either side can grow, f3). Its answer carries today's command answers (f44, docs/wire.md "The UI's
sockets"): `Taken`, `Accepted` (with its inbox's `epoch`, `seq`, `at`, `repeat`), `Unchecked`,
`Pending`, `Parked`, `Nack` (with `code`, `why`, `again`), or an HTTP `Reply` (status and body);
and, when the write's effects show at a position, that position (`at`: a topic and cursor), so the
page drops its overlay when its data reaches it.

Each answer is final, for now, or not this time, exactly as docs/wire.md says:
- **Final:** every outcome but `Parked` and `Pending`, and `Nack` with `again: false`. It's kept
  under its id, and a resend is answered as the first was.
- **For now:** `Parked`. A resend is answered `Parked` until the real answer takes its place.
- **Not this time:** `Pending`, and `Nack` with `again: true`. Not kept: a resend runs again.

A request resent under an id while its first is still running isn't run twice: it's answered when
the first is. The server keeps the newest 1,024 final answers per caller for 10 minutes; a server
that restarts lets them go, as the session socket's ids do. A `SecretAnswer` is an answer whose body
carries a secret (a sign-in code, a token shown once, a credential): it reads as an `Answer` and is
never compressed.

## Subscriptions

`Subscribe` names a topic (its kind and key) under an id the connection's client chooses
(`sub`, never 0: lane 0 is the connection's own), with the cursor the page has, if any, and the
credit it grants that subscription. The ids are the connection's: a shared worker maps them to its
tabs. The server checks access once, at subscribe, and again when the caller's access changes,
never by reading each event (f6). Refused, it answers `End` (`Denied`, or `Unknown` for a topic
kind it doesn't know).

A topic has a cursor: the server's incarnation (`born`) and a revision (`rev`), counted per topic,
and a bounded change log. The server answers a subscribe with either:
- `Resumed` (from the cursor), then only the changes since it, then `Ready`, when the cursor is of
  this incarnation and the log still holds every change after it; no snapshot goes;
- or `Snapshot` (at the topic's cursor), the topic's state as changes (`Change` with `step` 0),
  then `Ready`, when it can't.

Then the topic's changes, as they come, until `Unsubscribe` (answered `End`, `Done`) or the topic
ends (`End`, `Gone`).

### Changes

A `Change` carries the topic's revision after it (`rev`) and how far it moves the subscription
(`step`: `rev` less the `rev` of the frame before it on this subscription; 0 inside a snapshot).
The page holds a cursor per subscription: a change whose `rev - step` isn't its cursor is a gap,
and the page subscribes again from its cursor; a change at or below its cursor is a repeat, and is
dropped. So a page can move to another carrier, or reconnect, without losing or repeating a change
(goal 5). The deltas:

- `Set { mask, fields }`: the topic is one record; `mask` has a bit for each field present, and
  `fields` holds them in field order.
- `Put { key, fields }` (every field), `Patch { key, mask, fields }`, `Remove { key }`: the topic is a collection
  of records by key.
- `Append { at, data }`: the topic is text or bytes; `data` goes at offset `at`.
- `Extend { key, field, at, data }`: text appended to one field (a `str` or `bytes`) of one item,
  at offset `at` (a streaming event's text).
- `Tail { sub, data }`, a kind of its own: the next revision, appending `data` where the
  subscription's last `Append`, `Extend` or `Tail` left off. A streamed token costs 4 bytes and
  its text.

Keys: `Num` and `Str` name an item; `Ref(n)` is the n-th `Str` key (from 0) the subscription has
been sent since its last `Snapshot` or `Resumed`, as MQTT's topic aliases are (f28), so a runner's
name goes once. A subscription's table holds at most 4,096 names; past that a `Str` goes whole and
isn't added, on both sides alike.

A record type has at most 64 fields, numbered by order. Its delta carries only the fields that
changed from what the subscription was last sent.

### Coalescing

What waits to be sent is decided again each time the carrier has room (`poll_transmit`):
- a state or telemetry topic's change to a key replaces, field by field, one to the same key still
  waiting (a newer value for the same field wins; a `Remove` wins over all; a `Put` after a
  `Remove` stands);
- text appended to the same place joins what's waiting there;
- commands, asks, answers and acks are never merged or dropped.

Changes of a subscription wait in the order of the oldest change each holds, and each frame says
the revision through which the page then has everything (`rev`): so a page that drops the link
mid-way resumes from a cursor that is exact.

## Priorities

Five classes, served in strict order; within a class, subscriptions take turns (round robin), as
RFC 9218's incremental urgency shares bandwidth (f24):

| Priority | What |
|---|---|
| `control` | the connection's own frames and answers; `Probe` ahead of everything |
| `interactive` | a terminal's output, keystrokes, the composer |
| `state` | live state: runs, legs, runners, a session's state |
| `stream` | logs, timelines |
| `bulk` | snapshots, history, files |

A subscription takes `bulk` while its snapshot goes, then its topic's own.

## Fragmentation

A frame larger than the fragment bound is cut into `Fragment`s of its lane, each at most the bound,
so an urgent frame waits behind at most one fragment (f42: at 50 kbit/s a 1,500-byte frame takes
240 ms to send). The bound is the carrier's to set from the rate it measures (the core starts at
1,200 bytes and takes 256 bytes at the least); the core decides the next fragment by priority each
time. A lane's fragments go in its order, and the page joins each lane's fragments until `last`,
then reads the one frame they hold. A fragment never holds a fragment (`too_deep`). A frame is at
most 1 MiB whole.

## Credits

The page grants credit in bytes (RSocket's REQUEST_N in bytes, HTTP/2's windows, f23, f24): at open
for the connection (`window`), at subscribe for the subscription (`credit`), and after that with
`Credit { sub, upto }`, the total bytes the server may have sent (on that subscription, or with
`sub` 0 on the connection) since it began. `upto` only grows (a smaller one closes the connection,
`protocol`). The connection's credit covers every frame but the connection's own small ones
(`Accepted`, `Refused`, `Idle`, `Probe`, `ProbeAck`, `Unread`, `Close`, `End`); a subscription's
covers its frames.

## Memory per client

What waits for a client is bounded (1 MiB by default). Past the bound, the largest waiting
subscription lets its waiting changes go and is resumed from what it was last sent (`Resumed` and
the change log, or a `Snapshot`) once it has room: never an unbounded queue, never a change dropped
without the page being brought back to the topic's state (f41). A snapshot under way goes whole,
as the channel's do (docs/wire.md): the bound holds what waits after it, and a topic's own bounds
(its change log, the text it keeps for snapshots) hold a snapshot's size. A subscription granted
credit 0 is held by the connection's credit alone.

## Compression

Off unless the page offers it (`CAP_DEFLATE_RAW`) and the server's settings allow it (they do by
default). The one format is deflate-raw with a sync flush after each batch, which every browser can
inflate through one long-lived `DecompressionStream("deflate-raw")` (f31, f32; brotli is missing in
Chrome, zstd in every shipping browser). A `Compressed { ctx, len, data }` frame holds a batch of whole
frames, `len` bytes of them, compressed in context `ctx`, which lives as long as the connection;
the browser's stream inflater reads until it has `len` bytes, so each batch is whole before the
next comes. The carriers add none:
goal 3 leaves permessage-deflate off (RFC 7692's own warning).

Contexts are per trust class (f34): attacker-influenced data never shares a history with a secret.

| ctx | Trust class | What |
|---|---|---|
| — | `secret` | never compressed: `Open` (the ticket), `SecretAnswer`; an `Answer` the server doesn't class |
| 1 | `records` | kiln's own numbers: samples, the queue, resources changed |
| 2 | `text` | attacker-influenced text: runs, legs, steps and runners (commit messages, branch and step names are a repository's), transcripts, logs, terminal output, notices |

The core's types keep a secret frame away from every compressor: `compress::Compressible::of` is
the only way into one, and it refuses `Open`, `SecretAnswer` and every frame of the secret class
(`tests/compress.rs`; a BREACH-style probe's compressed sizes are the same with and without the
secret sent, and the same probe against a shared context finds it, 15 B against 18 to 22 B). On a carrier with a stream per
subscription (WebTransport), a context is per stream as well, since streams don't keep one order.

## Topics

kiln's live data as topics (`crates/kiln/src/bp/`; numbers on the wire, append-only). Access is
decided once, when a page subscribes, from the topic alone (`kiln::bp::allowed`): each topic is
scoped to what may see it, so no event is read per connection to decide who may have it.

| Kind | Name | Key | Items | Class | Priority | Trust |
|---|---|---|---|---|---|---|
| 1 | `runs` | a repository's name | runs by id (`records::Run`) | state | state | text |
| 2 | `legs` | a repository's name | legs by id, without steps (`Leg`) | state | state | text |
| 3 | `steps` | a repository's name | steps by `<leg>.<idx>` (`Step`) | state | state | text |
| 4 | `runners` | empty | runners by name (`Runner`) | state | state | text |
| 5 | `samples` | empty | host samples by machine (`Sample`) | telemetry | state | records |
| 6 | `queue` | empty | one record (`QueueCounts`) | telemetry | state | records |
| 7 | `changed` | empty, a repository's name, or `person:<name>` | how often each resource changed (`Changed`) | state | state | records |
| 8 | `events` | empty or a repository's name | events carried as their JSON by number, the newest 256 kept (`Event`) | stream | stream | text |
| 9 | `session` | an agent session's id | `info`, `meta`, `state.<field>`, `change.<tree>\|<path>`, `files`, events by number (`SessionItem`, streamed text by `Extend`/`Tail`) | stream | interactive | text |
| 10 | `step_log` | `<leg>/<step>` | the log, by offset | stream | stream | text |
| 11 | `terminal` | a terminal's id | its output, by offset | stream | interactive | text |
| 12 | `cd_log` | `<app>/<resource>` | a deployment's log, by offset | stream | stream | text |

Runs, legs, steps and the repository-scoped `events` and `changed` are a person's when they may see
the repository; `session` when they may see the session; `changed` under `person:<name>` is theirs;
the rest are the owner's, as on today's events socket (`Narrow`). Every `ServerEvent` kind maps to
one of these (`kiln::bp::Feed::event`, a match without a catch-all): runs, legs and steps, runners
and samples, and the queue's counts as typed deltas of only the fields that changed; the "read it
again" events (`terminals`, `updates`, `places`, `discovery`, `join_requests`, `tailscale`, `push`,
`peers`, `keep`, `ai_sessions`, `plan_stack`, `inbox`, `program`, `front_door`, `agent_limit`,
`headroom`, `cd_rollout`) as a counter per resource, merged while they wait; and the events with
records of their own but no protocol record yet (comments, reviews, diff runs, landings, images,
verdicts, experiments, previews, conflicts, notices, incidents, deploy plans and the rest) carried as
their JSON, which either side can grow, until goal 9 gives each its record. `resync` and `refused`
have none: nothing falls behind into a resync, and an unreadable frame is the protocol's `Unread`.

A record goes field by field (`kiln_bp::wire::Fields`): each field its own encoding, a change only
the fields that differ (`wire::diff`). The records are kiln-bp's (`crates/kiln-bp/src/records.rs`),
so the TypeScript codec has readers for them (`fromFields<Name>`, `toFields<Name>` in `gen.ts`).
Times are Unix milliseconds; a percentage is per mille, a load in hundredths: no floats. A session
viewed through the topic converges to what its socket's viewer holds for the same frames
(`kiln::bp::tests`, `a_session_converges_as_its_socket_would`).

## Liveness

Liveness is judged by bytes arriving, not by complete messages or timer ticks (goal 9's rule, coded
in the core, used by goals 3 and 5):
- Each side declares an idle period at open and sends an `Idle` (with the bytes it has received,
  `got`) when it has sent nothing else for that long.
- The receiver notes when each chunk of bytes arrives, and learns the gaps between arrivals beyond
  the declared period as TCP learns round trips (RFC 6298: a smoothed mean and a mean deviation).
  The next bytes are expected by the last arrival, plus the declared period, plus the learned
  spread (mean plus four deviations).
- Past that the link is suspect, and a `Probe` goes ahead of everything else.
- It is dead only if it stays silent past the probe through `2^backoff - 1` of its measured round
  trips (from probes and the open, smoothed the same way; `backoff` 5 by default). The carrier below
  may be retransmitting with doubling timers meanwhile: over TCP a lost packet holds everything after
  it, and a probe's answer waits behind every byte already queued, so a link silent for one round trip
  is often only stalled (goal 9's traces at 50 kbit/s, 5% loss and a 6 s queue: one window gave 231
  false dead verdicts in 300). What's in flight can't shorten this for the receiver: on one ordered
  stream, a position the sender declares arrives after every byte it declares, so the receiver never
  learns what's queued behind its last arrival; only the sender knows its own unacknowledged bytes.
- A page that ran again after a freeze (shown, `pageshow`, resume) says so (`resumed(now)`) before it
  reads what came meanwhile: the gap was its own, so it's neither learned as the link's lateness nor
  judged as silence.
- All of this is worked out from the arrival times the core was given, whenever it's asked, so a
  page that was frozen judges from what arrived, not from timers that didn't fire.

## Limits

| Limit | Default |
|---|---|
| a frame, whole | 1 MiB |
| a fragment | 1,200 bytes to start (the carrier sets it; at least 256) |
| subscriptions per connection | 256 |
| what waits per client | 1 MiB |
| a topic's change log | 1,024 changes or 256 KiB |
| final answers kept | 1,024 per caller, 10 minutes |
| a ticket's life | 30 s |
| idle period | 15 s (a page says its own) |

## Every message the UI's sockets carry today, and its counterpart

From docs/wire.md, "The UI's sockets":

| Socket | Way | Message | Counterpart here |
|---|---|---|---|
| `/api/harness/sessions/:id/ws` | up | `cmd` | `Request` with `Call::Command` |
| `/api/harness/sessions/:id/ws` | up | `unread` | `Unread` |
| `/api/harness/sessions/:id/ws` | down | `ack` | `Answer` (`Taken`, `Accepted`, `Unchecked`, `Pending`, `Parked`) |
| `/api/harness/sessions/:id/ws` | down | `nack` | `Answer` (`Nack`) |
| `/api/harness/sessions/:id/ws` | down | `open` | `Snapshot` of the session topic |
| `/api/harness/sessions/:id/ws` | down | `resumed` | `Resumed` |
| `/api/harness/sessions/:id/ws` | down | `meta`, `state`, `changes`, `files` | `Change` `Put` of the session topic's `meta`, `state`, `changes`, `files` items |
| `/api/harness/sessions/:id/ws` | down | `state_patch`, `changes_patch` | `Change` `Patch` of those items |
| `/api/harness/sessions/:id/ws` | down | `events` | `Change` `Put` of event items by number |
| `/api/harness/sessions/:id/ws` | down | `stream` | `Change` `Extend` of the event's text, then `Tail` |
| `/api/harness/sessions/:id/ws` | down | `synced` | `Ready` |
| `/api/harness/sessions/:id/ws` | down | `closed` | `End` (`Gone`) |
| `/api/harness/sessions/:id/ws` | down | `error` | none: it says a bare command's refusals to clients from before ids, and every `Request` has an id; its refusal is the `Answer`'s `Nack` |
| `/api/events` | up | `unread` | `Unread` |
| `/api/events` | down | `event` | `Change` of the topic the event is about (crates/kiln/src/bp/, "Topics") |
| `/api/events` | down | `refused` | `Unread` |
| `/api/terminal/ws`, `/api/ci/legs/:id/terminal` | up | `input` | `Input` |
| `/api/terminal/ws`, `/api/ci/legs/:id/terminal` | up | `resize` | `Resize` |
| `/api/terminal/ws`, `/api/ci/legs/:id/terminal` | up | `unread` | `Unread` |
| `/api/terminal/ws`, `/api/ci/legs/:id/terminal` | down | `output` | `Change` `Append` of the terminal topic, then `Tail` |
| `/api/terminal/ws`, `/api/ci/legs/:id/terminal` | down | `refused` | `Unread` |
| `/api/jobs/:id/logs/ws` | up | `unread` | `Unread` |
| `/api/jobs/:id/logs/ws` | down | `log` | `Change` `Append` of the step log topic, then `Tail` |
| `/api/jobs/:id/logs/ws` | down | `refused` | `Unread` |
| `/api/harness/sessions/:id/streams/:sid/ws` | up | `input` | none yet: a live page's video and its input stay on their socket (media: a lost frame is overtaken, never sent again, and its bytes dwarf the rest; goal 9 names what stays on HTTP) |
| `/api/harness/sessions/:id/streams/:sid/ws` | up | `unread` | none yet, as `input` |
| `/api/harness/sessions/:id/streams/:sid/ws` | down | `frame` | none: media, as `input` |
| `/api/harness/sessions/:id/streams/:sid/ws` | down | `event` | none yet, as `input` |
| `/api/harness/sessions/:id/streams/:sid/ws` | down | `refused` | none yet, as `input` |
| `/api/cd/apps/:app/resources/:key/logs/ws` | up | `unread` | `Unread` |
| `/api/cd/apps/:app/resources/:key/logs/ws` | down | `log` | `Change` `Append` of the deployment log topic, then `Tail` |
| `/api/cd/apps/:app/resources/:key/logs/ws` | down | `refused` | `Unread` |
| `/api/agents/tunnel` | up, down | `datagram`, `unread`, `refused` | none: a runner's tunnel, not a browser's |

## Byte budgets

Each common message's bytes today and its budget here. Today's are measured from kiln's real types
(the research's script, f11), or worked out from them; the budget is the protocol frame, envelope
included, before the carrier's own bytes. Step 10 measures them all on recorded sequences.

| Message | Today (JSON) | Budget here | How |
|---|---|---|---|
| A host sample | 264 B (`sample` event) | ≤ 32 B | `Change` + `Patch` of the runner's item by `Ref` key, the changed fields only: envelope 2, sub 1, rev 3, step 1, delta 1, key 2, mask 1, length 1, fields ≤ 20 |
| A leg's step changing | 1,400 B (the whole `job` event) | ≤ 32 B | `Change` + `Patch` of the step's item: status 1, a time 6, an exit code 1, beside the same 13 of framing |
| A streamed token | 80 B (a `stream` frame) | ≤ 4 B + its text | `Tail`: envelope 2, sub 1, length 1 |
| 40 tokens while the link is busy | 3,200 B (40 frames) | ≤ 16 B + their text | one `Change` + `Extend`, joined while they waited |
| A command's ack | ~40 B (`{"t":"ack","id":"…","outcome":"taken"}`) | ≤ 8 B | `Answer`: envelope 2, id ≤ 3, outcome 1, `at` none 1 |
| A resume after a reconnect | every key fetched again (`resync`, `onReopen`) | the missed changes + ≤ 64 B | `Subscribe` with its cursor, `Resumed`, the changes since, `Ready`; no snapshot while the log holds them |
| An idle link | nothing (a dead link goes unseen for minutes) | 3 B per idle period each way | `Idle` |

## Measured (offline, no carrier)

Five sequences built from kiln's real types (goal 1 recorded operations' sizes, not event
traces, so these are made from `store::Run`, `store::Job`, `hub::AgentLive`, a session's
`SessionFrame`s and a build's log lines): each through the protocol and as today's endpoints send
it. Bytes are above the carrier (no WebSocket, TLS or TCP bytes on either side), per operation as
it happens on a quiet link, and deterministic (`bp_g2_s10_measure` asserts a second run gives the
same counts). `cargo test -p kiln --lib bp::measure -- --nocapture` (2026-11-05, bp-g2 on
adacachy).

| Sequence (operations) | Form | p50 B | p99 B | p99.9 B | total B |
|---|---|---|---|---|---|
| a leg's life (15) | protocol | 23 | 325 | 325 | 679 |
| | protocol, compressed | 21 | 185 | 185 | 582 |
| | today, JSON | 991 | 1,023 | 1,023 | 13,663 |
| | today, gzip each | 317 | 325 | 325 | 4,547 |
| | today, kept deflate | 35 | 229 | 229 | 897 |
| | protocol on a busy link (merged, packed, compressed) | | | | 305 |
| a busy runners page (1,200) | protocol | 61 | 204 | 204 | 75,756 |
| | protocol, compressed | 43 | 77 | 83 | 53,180 |
| | today, JSON | 577 | 577 | 577 | 692,400 |
| | today, gzip each | 356 | 359 | 359 | 427,336 |
| | today, kept deflate | 35 | 42 | 47 | 42,314 |
| | protocol on a busy link | | | | 1,691 |
| an agent session streaming (303) | protocol | 10 | 15 | 92 | 2,996 |
| | protocol, compressed | 13 | 25 | 107 | 4,185 |
| | today, JSON | 58 | 61 | 114 | 17,447 |
| | today, gzip each | 81 | 84 | 117 | 24,396 |
| | today, kept deflate | 24 | 43 | 98 | 7,538 |
| | protocol on a busy link | | | | 260 |
| a log flood (2,000) | protocol | 91 | 92 | 92 | 182,464 |
| | protocol, compressed | 31 | 35 | 39 | 63,128 |
| | today, JSON (raw lines) | 87 | 88 | 88 | 174,460 |
| | today, gzip each | 103 | 103 | 104 | 205,005 |
| | today, kept deflate | 26 | 29 | 31 | 52,693 |
| | protocol on a busy link | | | | 10,543 |
| a reconnect (40 changes missed) | protocol: resume from the cursors | | | | 1,337 |
| | today: every key fetched again (goal 1's recorded bodies) | | | | 9,206 |

Server CPU per operation, release, publish and fan out to N subscribers with every frame made,
against today's work for the same event (its JSON made once and parsed per socket, as the events
socket's `Narrow` does): `cargo test -p kiln-bp --release --test bench -- --nocapture`, µs at
p50 / p99 / p99.9.

| Sequence | N | protocol | today |
|---|---|---|---|
| a leg's life | 1 | 0.78 / 9.40 / 9.40 | 5.69 / 8.90 / 8.90 |
| | 10 | 3.51 / 39.16 / 39.16 | 33.66 / 39.09 / 39.09 |
| | 100 | 31.96 / 402.54 / 402.54 | 312.90 / 317.19 / 317.19 |
| a busy runners page | 1 | 1.44 / 3.51 / 4.37 | 2.81 / 4.22 / 7.19 |
| | 10 | 8.69 / 15.11 / 19.67 | 16.75 / 19.50 / 23.98 |
| | 100 | 77.54 / 142.73 / 195.22 | 159.67 / 181.73 / 188.35 |
| an agent session streaming | 1 | 0.53 / 1.39 / 2.05 | 0.45 / 0.76 / 1.62 |
| | 10 | 2.26 / 3.38 / 4.84 | 2.80 / 3.40 / 4.51 |
| | 100 | 18.79 / 24.03 / 39.45 | 18.80 / 26.70 / 29.72 |
| a log flood | 1 | 0.46 / 0.80 / 1.63 | 0.09 / 0.23 / 0.26 |
| | 10 | 2.62 / 3.22 / 4.01 | 0.15 / 0.18 / 0.21 |
| | 100 | 24.57 / 41.08 / 43.06 | 1.11 / 1.95 / 2.10 |

Compression by class (step 8), the same sequences: compression saves 13% of a leg's life and 30% of
a runners page (records), 65% of a log flood (text), and costs 40% on a streaming session, whose
frames are tokens too short for deflate's sync flush to pay. A trained dictionary (the first half
of each sequence's frames, primed into the context) would save a further 0 to 13% on the second
half (leg 141 → 122 B, runners 3,809 → 3,638, session 870 → 800, log 2,932 → 2,639); the browser's
DecompressionStream takes no dictionary, so it isn't shipped (Discord found the same, f22).

Where the protocol is not at 1/10 or better of the best JSON form (goal 10 makes the bar's call):
- **A leg's life, per operation:** 23 B against kept deflate's 35 B at p50; the whole leg 679 B
  against 897. A kept deflate context already strips the repetition the delta strips; the 1/10 is
  against today's API as it is (13,663 B, no compression), not against a tuned baseline. On a busy
  link, merged and packed, the leg is 305 B (1/3 of kept deflate).
- **A busy runners page:** 43 B compressed against kept deflate's 35 B per sample: samples change
  in many fields at once, so the delta isn't small; packed on a busy link the minute is 1,691 B
  (1/25 of kept deflate's 42,314), because waiting samples merge to the newest.
- **A session streaming:** 10 B a token against kept deflate's 24 B (1/2.4); joined while the link
  is busy, 260 B for the turn (1/29).
- **A log flood:** the text itself dominates; compressed 31 B a line against kept deflate's 26 B
  (the protocol's frame per append and its separate contexts per class cost a few bytes). Packed on
  a busy link, appends join and the flood is 10,543 B (1/5).
- **Server CPU:** at 1 to 10 subscribers the protocol is as fast as or faster than today's
  per-socket parse, but on the log flood today's path (raw bytes, no parse) is cheaper: the
  protocol makes each subscriber's frame (sub id, revision, aliases, tails) per connection. At
  100 subscribers of a leg's life the p99 is 403 µs against 317. Sharing the encoded delta across
  subscribers (only the sub id differs) is the next thing to measure.
- Browser decode timings (`ui/e2e/bp-decode.spec.ts`, Chromium and WebKit) are below.

The browser's decode cost per frame against `JSON.parse` of today's frame for the same operation,
over each sequence's first 200 operations, 200 repetitions each (`npx playwright test -c
playwright.bp.config.ts bp-decode`, adacachy, Chromium 1248 and WebKit 2370), µs at p50 / p99 /
p99.9. The browsers' clocks are coarse (WebKit's to 5 µs over the 200 repetitions), so these say
the two are within a few microseconds of each other at every percentile, not which is faster:

| Sequence | Chromium: protocol | Chromium: JSON.parse | WebKit: protocol | WebKit: JSON.parse | WebKit, phone: protocol | WebKit, phone: JSON.parse |
|---|---|---|---|---|---|---|
| a leg's life | 0.5 / 9.0 / 9.0 | 1.0 / 1.5 / 1.5 | 0 / 5 / 5 | 0 / 5 / 5 | 0 / 10 / 10 | 0 / 5 / 5 |
| a busy runners page | 0.5 / 1.5 / 1.5 | 1.0 / 1.5 / 5.5 | 0 / 5 / 5 | 0 / 5 / 5 | 0 / 5 / 15 | 0 / 5 / 5 |
| an agent session streaming | 0 / 0.5 / 0.5 | 0 / 0.5 / 0.5 | 0 / 5 / 5 | 0 / 0 / 5 | 0 / 5 / 5 | 0 / 5 / 5 |
| a log flood | 0.5 / 1.0 / 1.0 | 0 / 0 / 0 | 0 / 5 / 5 | 0 / 0 / 5 | 0 / 5 / 5 | 0 / 0 / 5 |

So TypeScript decoding costs nothing a page would notice, and WASM isn't called for (Ada's call
stays at its default).

## Where it lives and how it's held

| Part | Where |
|---|---|
| the frames, their codec and schema | `crates/kiln-bp/src/{wire,schema,macros,frames,records}.rs` |
| the connection's halves, sans-IO | `crates/kiln-bp/src/{server,client}.rs`, with `liveness`, `tickets`, `answers`, `topic`, `state`, `join`, `compress` |
| the browser's codec, generated | `ui/src/api/bp/gen.ts` (`KILN_WRITE_BP=1 cargo test -p kiln-bp --test ts`) |
| the browser's runtime and client core | `ui/src/api/bp/{runtime,frames,join,inflate,state,liveness,client,caps,index}.ts` |
| kiln's topics | `crates/kiln/src/bp/` (`Feed`, `SessionFeed`, `Streams`, `allowed`, `unread`) |

A carrier drives a `Server` (and a page a `Client`) with `receive(bytes, now, ..)` and
`poll_transmit(room, now)`, which returns one message of frames (`Transmit`), each named with its
lane, priority and urgency (`Sent`), so WebTransport can give each subscription a stream and put
small urgent frames on datagrams; `poll_timeout(now)` says when to look again. The server's code
hears `Event`s (`Request`, `Subscribe`, `Resume`, `Input`, refusals to note) and answers with
`answer`, `answer_secret`, `start` (from `TopicLog::start`) and `push`. A browser passes each
message through `Unpacker.unpack` (compressed batches inflated in order) into `Client.receive`.
Neither half has a lock, a clock or randomness of its own; both are on the lockless list
(`scripts/locks/hot.toml`).

The tests, each named for the plan's step it holds:

| What | Where |
|---|---|
| every kind round-trips, indexes pinned, layouts of two versions read both ways, hostile frames refused and the next read, a million seeded inputs without a panic, a refused decode allocating nothing | `crates/kiln-bp/tests/codec.rs`, `tests/alloc.rs` |
| the generated codec and the vectors are this build's | `tests/ts.rs`, `tests/vectors.rs` |
| the vectors, the joins, the inflate batches, a million seeded inputs to Rust's digests, the liveness traces and a Rust server's conversation, in Node | `ui/tests/bp-vectors.test.ts` |
| the same in Chromium and WebKit, desktop and phone | `ui/e2e/bp-vectors.spec.ts` (`npx playwright test -c playwright.bp.config.ts`) |
| open, tickets, version pairs, requests once per id, liveness, expiry | `tests/conn.rs` |
| resume, budgets, convergence over seeds | `tests/subs.rs` |
| the scheduler at 50 kbit/s | `tests/sched.rs` |
| compression by trust class, the BREACH probe | `tests/compress.rs` |
| this page's frame table | `tests/doc.rs` |
| kiln's topics against the core | `crates/kiln/src/bp/tests.rs` |
| the measurements | `crates/kiln/src/bp/measure.rs`, `crates/kiln-bp/tests/bench.rs`, `ui/e2e/bp-decode.spec.ts` |

## Ada's calls, and what's taken meanwhile

- The crate's name: `kiln-bp`.
- Version skew: version N serves N-1; older pages are told the build to switch to.
- Compression for attacker-influenced content: on, by trust class, when the page offers deflate-raw
  (`ServerConfig::compress`). Within the `text` context a page's own visible data shares a history
  with attacker-influenced text; no secret ever does.
- Default limits: as the table above.
- The browser's codec: TypeScript, no WASM (the measurements below say whether that holds).
