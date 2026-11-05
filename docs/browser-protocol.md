# The browser protocol

(Goal 2 writes this page's protocol; goal 4 adds the door below. When the two land, this section
goes after goal 2's.)

## The HTTP/3 door

Browsers reach kiln over WebTransport on the HTTP/3 door (goal 4, plan
`01a1a832eb597110a6dcfda227981604`; the HTTP/3 itself is docs/http.md, "HTTP/3, for WebTransport
alone"). It's off until declared:

```toml
[server]
h3_addr = "0.0.0.0:7443"        # its own UDP address; unset, there's no door and no socket
# h3_advertise = "kiln.example:7443"  # where pages reach it, when not public_url's host at that port
# h3_cert = "/path/fullchain.pem"     # a publicly trusted certificate instead of pinned ones
# h3_key = "/path/privkey.pem"
```

`KILN_H3_ADDR` and `KILN_H3_ADVERTISE` set the first two. The door shares no port: not the runner
QUIC listener's (it requires client certificates browsers can't give), and not the front door's
(which carries no UDP).

**Certificates.** Chrome takes no privately rooted certificate for QUIC, so unless `h3_cert` and
`h3_key` name a publicly trusted one, the door serves self-signed ECDSA P-256 certificates that
pages pin by their SHA-256 (`serverCertificateHashes`, which accepts only certificates valid for
at most two weeks, and needs `allowPooling: false`). Certificate *k* is served from *k* weeks after
the epoch for one week and is valid from an hour before that for 13 days (`h3door::Rotation`). A
page is given the hashes of the current certificate and the next, so the one served is always in a
list a page fetched within its ticket's life (a test checks every second of a simulated 60 days).
Session tickets are on (TLS 1.3, one a handshake); early data is off. Goal 7 owns the rest of the
door's TLS: chain size, compression, resumption policy, the post-quantum setting and 0-RTT.

**Who.** WebTransport sends no cookies. A page asks `POST /api/me/door` (authenticated by its
`kiln_session` cookie, as every API call), and gets the door's URL with a single-use ticket and
the principal in its query, and the hashes:

```json
{ "on": true, "url": "https://kiln.example:7443/wt?ticket=…&u=alice", "hashes": ["<base64 SHA-256>", "…"], "ticket_life_ms": 30000 }
```

or `{ "on": false }` when there's no door. The page opens `new WebTransport(url, {
serverCertificateHashes: [{ algorithm: "sha-256", value }], allowPooling: false })`. The door
admits a CONNECT only from one of the server's own origins (`public_url`'s, and `https://` with each
of `tls_names`), at a path it serves (`/wt`), with a ticket not used before, within its 30
seconds, for the principal the URL names; anything else is 403 (or 404 for a path), and nothing of
it is kept. The tickets sit behind `h3door::DoorTickets` with goal 2's rules (32 random bytes, once,
30 seconds); goal 2's own (`kiln-bp`'s `Redeem`) takes its place when it lands.

**The session API** (`kiln_http::h3::door`) is what goal 5's carrier uses, without HTTP/3's
frames: `Door::session(conn, now)` gives a `Session` with `connect()` (path, origin, dialect),
`open(bidi)`, `read`, `write`, `finish`, `reset` and `stop` (WebTransport application codes),
`send_datagram`, `recv_datagram`, `max_datagram`, `close(code, message)` and `drain(goaway)`, and
`quic()` for the connection's knobs (congestion control, windows within the bound B). Events come
from `Door::poll_event`: `Accepted`, `Stream`, `Readable`, `Writable`, `Datagram`, `StreamReset`,
`StreamStopped`, `Draining`, `Closed`. `/wt` sessions have no server behind them yet (goal 5's);
`/wt/echo` echoes, for the lab's browser checks.
