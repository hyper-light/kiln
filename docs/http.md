# kiln's HTTP

kiln speaks HTTP through `crates/kiln-http`: HTTP/1.1 and HTTP/2, server and client, on hyper-rt
sockets. There is no tokio, hyper or h2 in kiln's tree. `crate::http` is kiln's side of it: the
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
