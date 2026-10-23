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
