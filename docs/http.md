# kiln's HTTP

kiln speaks HTTP through `crates/kiln-http`: HTTP/1.1 and HTTP/2, server and client, on hyper-rt
sockets, and HTTP/3 for the WebTransport door alone (below). There is no tokio, hyper or h2 in kiln's tree. `crate::http` is kiln's side of it: the
listener (TLS sniffed from the first byte, HTTP/2 by ALPN or by its preface), the client, WebSockets
(async-tungstenite over kiln-http's upgrades) and TLS (hyper-tls). Messages are the `http` crate's
types and bodies are `http-body`'s, so handlers are written as they were on hyper.

## How a connection runs

- **One task per connection.** An HTTP/1.1 connection's handler is called and polled in the
  connection's own task. An HTTP/2 connection's handlers all run side by side in its one task:
  a stream costs a slot in a table, not a task. A request whose handler answers at once (most do)
  never waits, and an HTTP/2 stream like that never gets a place in the table at all.
- **Answers gather and leave together.** A connection takes every request it has read before it
  writes. Pipelined HTTP/1.1 requests, or a burst of HTTP/2 streams, are answered in one write.
  hyper's HTTP/1.1 writes once per response unless `pipeline_flush` is set.
- **An idle connection holds no buffer.** Reads go through one 64 KiB scratch buffer per thread.
  A connection keeps only what was read and not yet taken (a request's head shares those bytes:
  header values are slices of them, not copies). Write buffers come from a per-thread pool and go
  back to it when a connection has nothing to send. Between requests, an HTTP/1.1 connection
  drops its read buffer, its handler slot and its spare header map.
- **No speculative reads.** A read that took less than it had room for emptied the socket, so the
  connection next waits for readiness instead of asking the kernel first: one system call fewer
  for every request a connection waits for.

## HTTP/1.1

- Bodies by `Content-Length` or chunked (trailers included), both ways. A request with both, with
  lengths that disagree, or with a transfer coding other than a final `chunked` is refused `400`.
  These are the shapes request smuggling uses.
- Keep-alive, and pipelining. `Expect: 100-continue` is answered when the handler first reads the
  body. Upgrades (`101`, and `CONNECT`'s `2xx`) hand the connection over through `kiln_http::upgrade`.
- A request head must arrive whole within 30 seconds (kiln's setting), counted from when the
  connection is ready for it, so an idle keep-alive connection also closes after 30 seconds, as
  with hyper. A head over 1 MiB (kiln's setting; 64 KiB by default) is answered `431`.
- A client that closes its sending side ends its request, as hyper's default does: a handler that
  waits (a long poll, an event stream) is dropped when its browser goes away.

## HTTP/2

- Settings advertised: 256 concurrent streams, a 1 MiB stream window, a 16 MiB connection window,
  and a 64 KiB header list (kiln's settings). Windows are given back as each body is read, so a slow
  reader holds its sender back instead of filling memory. Until the client acknowledges the
  settings, it may send what the default 64 KiB window allows.
- HPACK with the dynamic table both ways. The decoder holds to the 4 KiB table we advertise. The
  encoder indexes only headers whose values repeat (a type, a date, a server), and never
  `authorization`, `cookie` or `set-cookie`.
- Limits against the protocol's known attacks:
  - A header list past the limit is answered `431`.
  - A header block (HEADERS plus CONTINUATION) larger than four times the limit ends the connection
    with `ENHANCE_YOUR_CALM` (CVE-2024-27316).
  - A client that resets streams before their answers, more than twice the concurrency limit in a
    second, gets `ENHANCE_YOUR_CALM` (CVE-2023-44487).
  - Streams past the concurrency limit are refused (`REFUSED_STREAM`).
  - A request with uppercase or connection-specific headers, or pseudo-headers out of order, is
    reset (`PROTOCOL_ERROR`).
- Keep-alive pings: a connection quiet for 30 seconds is pinged and closed if no answer comes
  within 20 (kiln's settings).
- No server push, and no extended CONNECT: browsers open HTTP/1.1 connections for WebSockets.
  (HTTP/3 has extended CONNECT for WebTransport alone: below.)

## HTTP/3, for WebTransport alone

`kiln_http::h3` is HTTP/3 (RFC 9114) for one purpose: the door browsers open a WebTransport
session through (goal 4 of the browser protocol program, plan `01a1a832eb597110a6dcfda227981604`).
It accepts one extended CONNECT with `:protocol webtransport` per connection and refuses everything
else rather than supporting it, so its parser surface is small. It is sans-IO in layers: QPACK
(`h3::qpack`), one connection's HTTP/3 and its session (`h3::conn`), capsules (`h3::capsule`), the
QUIC endpoint (`h3::door`, a raw hyper-quic endpoint) and a hyper-rt driver for a real UDP socket
(`h3::driver`). In kiln it is off unless `[server] h3_addr` is set (docs/browser-protocol.md, "The
HTTP/3 door").

### What it accepts and refuses

- It sends one control stream; its SETTINGS carry every handshake browsers speak today, at once:
  `QPACK_MAX_TABLE_CAPACITY` 0, `QPACK_BLOCKED_STREAMS` 0, `MAX_FIELD_SECTION_SIZE` 4,608 (the
  budget below), `ENABLE_CONNECT_PROTOCOL` 1, `H3_DATAGRAM` 1 under both 0x33 and draft-04's
  0xffd277, and the WebTransport dialects draft-02's `0x2b603742` = 1 (Chrome, Edge, Firefox),
  drafts 07-12's `0xc671706a` = 1 and drafts 13-14's `0x14e9cd29` = 1 (Safari). Each at one
  session: no WebTransport flow control (draft-14 §5.1). Captured from Chromium 156, Firefox 157 and
  Safari 26.4 against these settings (`crates/kiln-http/tests/vectors/webtransport/`): Chromium
  and Firefox speak draft-02 and need `0x2b603742` and `H3_DATAGRAM` before they send their CONNECT;
  Safari speaks draft-14 (it sends `0x14e9cd29` and `0xc671706a`, and draft-14's flow-control
  settings, which stay off because the door sends none). The answer to a draft-02 CONNECT carries
  `sec-webtransport-http3-draft: draft02`, as Chrome needs. All three opened sessions on the door
  in the lab and echoed a bidi stream, a uni stream and a datagram (Chromium and Firefox at desktop
  and phone widths, Safari in its own window);
  Firefox and Safari saw GOAWAY with WT_DRAIN_SESSION as `draining` (Chromium 156 has no `draining`
  to see it by). Playwright's own WebKit build (27.2) has no WebTransport at all.
- No request is read before the client's SETTINGS arrive (draft-14 §3.1); the dialect comes from
  them. One request is decoded at a time. Only a CONNECT with `:protocol webtransport`, `:scheme
  https`, an `:authority` and a `:path`, from a client whose settings speak WebTransport, reaches the
  caller's admit function, which answers 200, 403 or 404. Another method is reset with
  `H3_REQUEST_REJECTED`; a classic CONNECT likewise; another `:protocol` gets 501; a malformed
  request (uppercase names, pseudo-headers after headers or twice, connection-specific headers)
  `H3_MESSAGE_ERROR`; every request once a session is up is reset unread.
- Connection errors: a push stream from the client, a second control, QPACK encoder or decoder
  stream (nginx's CVE-2026-42530 reopened one), a critical stream closed or reset, SETTINGS not
  first or twice, a reserved HTTP/2 frame or setting, DATA before HEADERS, a session id that can't be
  one (`H3_ID_ERROR`). Unknown stream types are stopped (`H3_STREAM_CREATION_ERROR`) and forgotten;
  unknown frames and settings are skipped by their length without being buffered.
- Peer resets past twice the stream limit in a second (264) end the connection with
  `H3_EXCESSIVE_LOAD`, as the HTTP/2 server meets Rapid Reset (CVE-2023-44487).
- GOAWAY on shutdown, and WT_DRAIN_SESSION to ask a session to end.

### QPACK with no dynamic table, and a budget counted field by field

The table capacity is 0, so a field section's Required Insert Count must be 0; a nonzero one, a
dynamic or post-base reference, or a table capacity above 0 on the peer's encoder stream is a
connection error (`QPACK_DECOMPRESSION_FAILED`, `QPACK_ENCODER_STREAM_ERROR`). That removes the
dynamic-table "header accordion" (FoxIO, 2026-10-20: one inserted entry referenced by 1-byte lines,
4064:1). The peer's decoder stream may carry only Stream Cancellations. The door's encoder uses
static references and literals and opens no encoder or decoder stream.

What's left (many literal fields, repeated 1-byte references to a 101-byte static entry, Huffman's
8:5) is what the budget bounds: 4,608 bytes and 72 fields, from the capture (Firefox's CONNECT was
the largest, 1,020 bytes counted in 17 fields, against Chromium's 414 and Safari's 405; four times
it plus 214 for a ticket and a principal, rounded up). The decoder's peak while it refuses a 1 MiB
accordion is 4,573 bytes, and it keeps none of it. Each field's name, value and 32 bytes are added to the running total before the field
is materialised, a Huffman string is decoded only as far as the budget left allows, and the first
field past the budget refuses the section (431) with nothing of it kept. A HEADERS frame longer
than the budget is refused before its payload is read. The same constant is advertised and
enforced. FoxIO's five mistakes are a test each (`h3::qpack::tests::foxio_*`).

### The memory bound

hyper-quic's defaults hold far more than a fixed bound (a connection window of 2^62, 100 + 100
streams of 1.25 MB each), so the door sets every transport parameter that sizes memory
(`DoorConfig::transport`, and a test fails if a default leaks through). A connection with no
session gets a 16 KiB receive window, 5 bidi and 7 uni streams, a 4 KiB send window; once its
CONNECT is admitted they're raised (1 MiB, 65 and 67, 1 MiB: goal 5 tunes these). Datagrams
buffer 8 KiB received, 64 KiB to send; the handshake 8 KiB; each stream the limit allows, the
state hyper-quic keeps for it (512 bytes: 368 measured). B, what a connection may hold beyond its
idle state, is computed from these (`DoorConfig::bound`): 56,320 bytes before a session, 2,259,968
after. Connections with no session are capped endpoint-wide (1,000); when the cap is full
the oldest such connection is closed for the new one, never an admitted session (Slowloris, with
no new timer); past 64 handshakes at once an unvalidated address gets a Retry. The endpoint's own
per-address memories (loss bursts, Careful Resume) hold 256 addresses each.

Measured with hyper-measure's allocator, counting the door's code alone (`h3::attacks`, every attack
at 1x, 10x and 100x against one connection and against 1,000 at once; the cells print as `attack:`):

| attack | source | bytes a connection, most of any cell | B |
|---|---|---|---|
| QPACK count, static and Huffman accordions | FoxIO | 3,512 | 56,320 |
| many HEADERS frames on one stream | FoxIO on lsquic | 2,496 | 56,320 |
| encoder-stream inserts, duplicate critical streams | CVE-2026-42530 | ≤ 0 (the connection ends) | 56,320 |
| trailers, an oversized WT_CLOSE_SESSION | RFC 9114 §4.1, draft-14 §6 | ≤ 0 (the session ends) | 2,259,968 |
| one unknown capsule of 2^62 bytes, streamed | CVE-2026-57497 | 108 | 2,259,968 |
| many tiny capsules | RFC 9297 §3.2 | 45 | 2,259,968 |
| unknown uni streams, pre-session WT streams | RFC 9114 §6.2, draft-14 §4.6 | 4,721 | 56,320 |
| Rapid Reset, bidi and uni | CVE-2023-44487 | 10,240 | 2,259,968 |
| every stream the limit allows, held open | RFC 9000 §21.8 | 126,976 | 2,259,968 |
| STOP_SENDING on the door's streams | RFC 9000 §19.5 | 26,352 | 2,259,968 |
| one-byte STREAM frames, reordered | RFC 9000 §21.7 | 4,144 | 2,259,968 |
| datagrams for no session | RFC 9297 §2 | 128 | 56,320 |
| a CONNECT a byte at a time; sessions kept by PINGs alone | RFC 9000 §21.6 | 471; 0 | 56,320 |

No cell grew by more than 4 KiB from 10x to 100x (what streams held open cost stops at the
stream limit). The bytes the door sends to an address not yet validated were at most 0.967 of
what it received from it (RFC 9000 §8 allows 3). An idle connection with no session costs 31,634
bytes, an idle admitted session 40,711 (hyper-quic's connection state, mostly), beside kiln-http's
868 bytes an HTTP/1.1 connection and 2,487 an HTTP/2 one. Two QUIC attacks need packets a client
library doesn't make, and stay with hyper-quic's own defences and tests: the NEW_CONNECTION_ID
retirement flood with ACKs withheld (CVE-2024-22189: pending retirements are capped) and optimistic
ACKs (skipped packet numbers).

On a real socket (`examples/door_lab` under hyper-rt, `examples/door_soak` beside it over loopback
on a Linux machine shared with CI; the Macs on the lab's Wi-Fi couldn't carry it, and macOS holds
a new binary's LAN traffic for a person's consent): 10,000 admitted sessions echoing a stream every
30 seconds and 1,000 attackers in step 8's attacks in rotation (each coming back when the door
ends it: 415,000 and 623,000 times), 30 minutes each:

| | 0% loss | 5% loss (the generator drops 5% each way) |
|---|---|---|
| resident memory, minute 1 / 5 / 30 | 365 / 433 / 540 MB | 371 / 440 / 648 MB |
| end / minute 5 | 1.25 | 1.47 |
| live heap at the end (the allocator's count) | | 612 MB |
| sessions' echoes answered, missed | 516,436, 4,067 | 589,835, 0 |
| door CPU per attack byte | 53 ns | 91 ns |

Resident memory is the live heap (the allocator's count tracks it within 6%), and what it grows by
is each session's stream state settling at the stream limit: hyper-quic keeps 368 bytes for each
stream a connection has had open, up to the 132 the limit allows, and a session that opens a stream
every 30 seconds reaches the limit after about half an hour, so a 30-minute soak ends still on the
way there. The same sessions in memory (`attacks::long_lived_sessions_settle_to_a_steady_size`,
four streams a minute) grow 1,472 bytes a minute and stop at 23,866 bytes a session once the
limit is reached: that's in B. The 0% run's generator, one thread for 11,000 connections, fell
behind and let 3,974 sessions time out on its side.

### Capsules, streams and the session

The CONNECT stream's capsules are read as they come (`h3::capsule`): an unknown one is skipped by
its length holding only its header (CVE-2026-57497 read them whole), WT_CLOSE_SESSION's message is
at most 1,024 bytes, WT_DRAIN_SESSION is empty, a DATAGRAM capsule past 1,200 bytes is discarded
unbuffered and at most four wait. WebTransport streams (uni type 0x54, bidi signal 0x41, then the
session id) that arrive before their session are held unread, four at most; past that they're
refused with `WT_BUFFERED_STREAM_REJECTED`. Datagrams carry the quarter stream id; those for no
session are dropped. Application error codes map into WebTransport's range (draft-14 §4.4). A close
never waits on the peer (webtransport-go's did, CVE-2026-21435): WT_CLOSE_SESSION goes out in the
same call if the credit already granted covers it, else the CONNECT stream is reset at once.

### Fuzzing

`h3::fuzz` runs seeded targets inside `cargo test -p kiln-http`: the QPACK decoder (6,000 seeds),
HTTP/3 streams of every type into the connection core (3,000), capsules (1,500), and whole
connections of hyper-quic peers over a lossy, reordering network (60), 10,560 seeds a run. Each
checks no panic, every error a value, and what's held within its bound at every step. A failure
names its seed; `KILN_H3_FUZZ_SEED=<seed>` replays it alone, `KILN_H3_FUZZ_FROM` and
`KILN_H3_FUZZ_SEEDS` move the budget. `.kiln/h3-fuzz.yaml` runs a large budget from seeds no run
before tried, when asked (its nightly schedule is declared off).

## What the browser receives, compressed

kiln's API answers (`crates/kiln/src/http/compress.rs`, after every route in one place) and its
WebSockets (`crates/kiln/src/http/deflate.rs`, under tungstenite) go compressed when the client can
take it, and exactly as before when it can't:

- **Answers.** A whole body of a textual type (JSON, text, JavaScript, SVG, XML) of 1 KiB or more goes
  brotli-compressed to a client that takes `br`, else gzipped, with `Vary: Accept-Encoding`. Bodies
  already encoded, ranges, streams (event streams, log tails, downloads: no length known whole),
  answers with an `ETag` of their own (files: strong, for that body), `no-transform` and `HEAD` are
  left alone, as is every request without `Accept-Encoding` (kiln's
  own clients send none).
- **Validators.** A `GET` answered `200` gets a weak `ETag` from its body's SHA-256 and, unless the
  handler set one, `Cache-Control: private, no-cache`: the browser asks again every time with
  `If-None-Match`, and an unchanged answer is a `304` without a body.
- **WebSockets.** A client that offers `permessage-deflate` (RFC 7692; browsers do) gets it, with
  the compressor's window carried from message to message (unless it asks
  `server_no_context_takeover`); offers asking for a window under 15 bits are declined. Messages it
  sends compressed are inflated as they arrive, bounded by the socket's message limit after
  inflating (past it: closed 1009, as an oversized message always was). A client offering nothing
  (runners, kiln's own client) gets the same frames as before.
- Compression runs on the shard when it fits a step's quantum, else on the compute pool.

## TLS sessions

A TLS listener (`server.https`, and each runner's front door) lets a browser that comes back
resume its session: the server sends no certificate and no signature again. Sessions are
stateless: nothing is kept on the server. After each handshake TLS 1.3 sends 2 tickets, each
holding its session sealed with the listener's ticket key (AES-256-GCM), and a ticket resumes
for 24 hours after it was issued, the lifetime clients are told. The key is one immutable copy
that every connection reads without a lock, so a ticket made on one shard resumes on any other.
A key seals tickets for 24 hours; then a new one takes over, and the old one still opens its
tickets until the last of them is 24 hours old, then goes. The keys live in memory: a restart
makes a new one, and every browser's next connect is a full handshake.

Because nothing is kept, a ticket isn't used up: the same ticket can resume more than once. That
is safe because 0-RTT is off (no early data is accepted), so a replayed ClientHello carries no
request, and its handshake still needs the client's own key exchange. Resumption exchanges keys
(browsers resume with `psk_dhe_ke` only), so it saves bytes, not a round trip.
`kiln_tls_handshakes_total{door,kind}` counts handshakes by door (`server`, `front_door`) and kind
(`full`, `full_hello_retry`, `resumed`).

## The UI's files

kiln serves the web UI's static files itself (`crates/kiln/src/ui.rs`); goal 8 of the browser
protocol program (plan `01a1a832ebfd`) set how they are compressed, cached and revalidated, so a
page loads in bounded time on a 50 kbit/s link (docs/browser-lab/ui-loads.md).

- **Compressed once, at build time.** kiln's build packs every compressible file of the UI's
  build (text, scripts, JSON, SVG and WebAssembly over 1 KiB: `crates/kiln/src/ui_pack.rs`) with
  Brotli 11 into the binary (`crates/kiln/build.rs`; goal 11 of the API runtime program), and a
  request that takes `br` gets that copy, so none goes out uncompressed after a restart. A client
  that takes gzip alone (none of the browsers) gets the file as it is the first time, and a gzip
  copy, made on the compute pool and kept, after. The grammars are plain WebAssembly the browser
  decodes, not gzip files inflated in the page.
- **The compression policy.** Brotli encodes only kiln's own static files, made at build time; a
  dynamic response is never compressed in a context that holds a secret beside text an attacker
  can choose (BREACH). kiln's `brotli` is 9.1 or later (it refuses the streams behind
  CVE-2026-111497 in the C decoder browsers use).
- **Names that change only with content.** Hashed assets (`assets/`) are `public,
  max-age=31536000, immutable`. After the build, `ui/scripts/stable-names.mjs` names each chunk by
  its own content and has chunks name each other by keys an import map in every page resolves, so
  a deploy renames only the chunks that changed (and the shell, which carries the map).
- **Revalidated, not downloaded again.** Every response carries a strong ETag of the bytes it
  sends (each coding its own); `If-None-Match` naming it gets a 304 with no body. The shell and the
  other unhashed files (the manifest, icons, `sw.js`) are `no-cache`, so a repeat visit costs a
  revalidation for each. A page that renders only in the browser (every page but those the build
  lists as rendered on the server, `_server-pages.json`: today `/shared/plans/`) gets kiln's own
  shell, packed by the build and revalidated, rather than the UI server's render of the same
  document.
- **Deltas for what a deploy changed** (Compression Dictionary Transport, RFC 9842). Every hashed
  script and stylesheet goes with `Use-As-Dictionary: match="/assets/<name>-*.<ext>"` and `Vary:
  Accept-Encoding, Available-Dictionary`. A request whose `Available-Dictionary` names a version of
  the same name kiln has (this build's or a kept earlier one's, by its SHA-256) and whose
  Accept-Encoding takes `dcb` gets Brotli against that version: the magic, the dictionary's SHA-256,
  then the stream. A delta is made once per pair on the compute pool and kept with the kept assets
  (`<data>/ui-assets/.deltas`, aged out with them); until it's made, the plain Brotli copy goes, so
  no request waits. Only Chromium asks for it; Firefox and Safari get the plain copies.
  Whether Chromium uses dictionaries over a connection signed by kiln's own root is for the page-load
  lab to measure (goal 1's lab, not yet run for page loads).
- **Earlier builds' assets** are kept for 14 days after the last build that had them, each with
  the Brotli copy its build packed (on disk), so a page loaded before a deploy can still load what
  it hasn't yet, compressed.

`ci/ui/serve_check.sh` checks all of it against a real server.

## Clients

`kiln_http::client::http1` sends one request at a time over a kept-alive connection, mirroring
hyper's `SendRequest`. Callers pool it (the Kubernetes API's connections, opened apps), and upgrades
hand the connection over. `kiln_http::client::http2` multiplexes requests on one connection, each
on a stream of its own, with flow control both ways.

## Checked against hyper

`bench/http/src/bin/interop.rs` runs hyper's HTTP/1.1 and HTTP/2 clients (on tokio) against
kiln-http's servers, and kiln-http's clients against hyper's servers. Each pairing covers bodies past
every flow-control window both ways, chunked bodies, statuses (204 and 304 included), headers both
ways, and 100 to 200 streams at once. `cargo test -p kiln-http` covers the rest, down to frames
written by hand for what a peer gets wrong.

## How fast

`bench/http` compares kiln-http with hyper on tokio, the solution it replaces, using one load
generator for every server and four threads each. bench/http/README.md has the method, every run
and why each gap is what it is. The headline figures, from 2026-10-22:

| | kiln-http | hyper on tokio |
|---|---|---|
| HTTP/2, 64 connections × 100 streams | 13.2M requests/s, 273 ns of CPU each, p99 0.64 ms | 1.32M requests/s (10.0× fewer), 2,494 ns each, p99 27 ms |
| memory per idle HTTP/1.1 connection | 868 B | 11,377 B (13.1× more) |
| memory per idle HTTP/2 connection | 2,487 B | 19,989 B (8.0× more) |
| HTTP/1.1 pipelined 16 deep | 6.7M requests/s | 1.07M (6.3× fewer; 5.9M with `pipeline_flush`) |
| HTTP/1.1, one request at a time | 450k requests/s | 468k: level, as the kernel bounds both |
