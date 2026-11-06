"""A lab program (never a kiln dependency): what a browser sends to open a WebTransport session.

Goal 4 step 1 (plan 01a1a832eb597110a6dcfda227981604). A QUIC server on aioquic's QUIC layer
alone (no HTTP/3 of its own) that answers with the bytes kiln's door sends: one control stream
whose SETTINGS carry every WebTransport dialect at once, a QPACK-static 200 to the CONNECT, and
echoes. Every byte the browser sends on every stream, and every datagram, is recorded decrypted,
in arrival order, to a JSON file a test vector is made from.

  python3 server.py --out DIR [--http 8080] [--quic 4433]

Serves the test page on http://127.0.0.1:<http>/ (a secure context: no certificate needed for
the page) and WebTransport on UDP <quic>, with a fresh self-signed ECDSA P-256 certificate valid
for 10 days whose SHA-256 the page fetches from /info (serverCertificateHashes).
"""

import argparse
import asyncio
import datetime
import hashlib
import http.server
import json
import os
import threading
import time

from aioquic.asyncio import QuicConnectionProtocol, serve
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import (
    ConnectionTerminated,
    DatagramFrameReceived,
    HandshakeCompleted,
    StreamDataReceived,
    StreamReset,
)
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID
import ipaddress

HERE = os.path.dirname(os.path.abspath(__file__))


def varint(n):
    if n < 0x40:
        return bytes([n])
    if n < 0x4000:
        return (0x4000 | n).to_bytes(2, "big")
    if n < 0x40000000:
        return (0x80000000 | n).to_bytes(4, "big")
    return (0xC000000000000000 | n).to_bytes(8, "big")


def read_varint(buf, pos):
    if pos >= len(buf):
        return None
    first = buf[pos]
    size = 1 << (first >> 6)
    if pos + size > len(buf):
        return None
    v = first & 0x3F
    for b in buf[pos + 1 : pos + size]:
        v = (v << 8) | b
    return v, pos + size


# The door's SETTINGS (goal 4 step 3): QPACK table 0, no blocked streams, the field-section budget,
# extended CONNECT, HTTP datagrams under both ids, and every WebTransport dialect.
SETTINGS = [
    (0x01, 0),  # SETTINGS_QPACK_MAX_TABLE_CAPACITY
    (0x07, 0),  # SETTINGS_QPACK_BLOCKED_STREAMS
    (0x06, 16384),  # SETTINGS_MAX_FIELD_SECTION_SIZE (the cap; step 1 fixes the real one)
    (0x08, 1),  # SETTINGS_ENABLE_CONNECT_PROTOCOL
    (0x33, 1),  # SETTINGS_H3_DATAGRAM (RFC 9297)
    (0xFFD277, 1),  # SETTINGS_H3_DATAGRAM, draft-04 id
    (0x2B603742, 1),  # SETTINGS_ENABLE_WEBTRANSPORT (draft-02)
    (0xC671706A, 1),  # SETTINGS_WEBTRANSPORT_MAX_SESSIONS (drafts 07-12)
    (0x14E9CD29, 1),  # SETTINGS_WT_MAX_SESSIONS (drafts 13-14)
]


def settings_frame():
    body = b"".join(varint(k) + varint(v) for k, v in SETTINGS)
    return varint(0x04) + varint(len(body)) + body


def response_headers():
    # Field section prefix: Required Insert Count 0, Delta Base 0. ':status 200' is static index 25
    # (indexed field line, T=1: 0b11xxxxxx). 'sec-webtransport-http3-draft: draft02' as a literal
    # with a literal name (0b001N H len(3)), neither Huffman-coded.
    name = b"sec-webtransport-http3-draft"
    value = b"draft02"
    lit = bytes([0x27, len(name) - 7]) + name + bytes([len(value)]) + value
    block = b"\x00\x00" + bytes([0xC0 | 25]) + lit
    return varint(0x01) + varint(len(block)) + block


class Capture(QuicConnectionProtocol):
    out_dir = "."
    counter = 0

    def __init__(self, *a, **k):
        super().__init__(*a, **k)
        Capture.counter += 1
        self.n = Capture.counter
        self.started = time.time()
        self.log = {"streams": {}, "datagrams": [], "events": []}
        self.bufs = {}
        self.answered = set()
        self.wt_bidi = {}
        self.control = None

    def save(self):
        path = os.path.join(self.out_dir, "conn-%d-%d.json" % (int(self.started), self.n))
        with open(path, "w") as f:
            json.dump(self.log, f, indent=1)

    def event(self, what, **kw):
        kw["what"] = what
        kw["t"] = round(time.time() - self.started, 4)
        self.log["events"].append(kw)

    def quic_event_received(self, ev):
        if isinstance(ev, HandshakeCompleted):
            self.event("handshake", alpn=ev.alpn_protocol, resumed=ev.session_resumed, early=ev.early_data_accepted)
            self.control = self._quic.get_next_available_stream_id(is_unidirectional=True)
            self._quic.send_stream_data(self.control, varint(0x00) + settings_frame(), end_stream=False)
            self.transmit()
        elif isinstance(ev, StreamDataReceived):
            s = self.log["streams"].setdefault(str(ev.stream_id), [])
            s.append({"hex": ev.data.hex(), "fin": ev.end_stream, "t": round(time.time() - self.started, 4)})
            buf = self.bufs.get(ev.stream_id, b"") + ev.data
            self.bufs[ev.stream_id] = buf
            self.on_stream(ev.stream_id, buf, ev.end_stream)
            self.save()
        elif isinstance(ev, DatagramFrameReceived):
            self.log["datagrams"].append({"hex": ev.data.hex(), "t": round(time.time() - self.started, 4)})
            # Echo it as it came: the same quarter stream id, the same payload.
            self._quic.send_datagram_frame(ev.data)
            self.transmit()
            self.save()
        elif isinstance(ev, StreamReset):
            self.event("reset", stream=ev.stream_id, code=ev.error_code)
            self.save()
        elif isinstance(ev, ConnectionTerminated):
            self.event("closed", code=ev.error_code, frame=ev.frame_type, reason=ev.reason_phrase)
            self.save()

    def on_stream(self, sid, buf, fin):
        client_bidi = sid % 4 == 0
        if not client_bidi:
            # A uni stream: 0x54 + session id + payload is WebTransport; echo the payload on a uni
            # stream of ours once it ends.
            t = read_varint(buf, 0)
            if t and t[0] == 0x54 and fin:
                sess = read_varint(buf, t[1])
                if sess:
                    out = self._quic.get_next_available_stream_id(is_unidirectional=True)
                    self._quic.send_stream_data(out, varint(0x54) + varint(sess[0]) + buf[sess[1] :], end_stream=True)
                    self.transmit()
            return
        t = read_varint(buf, 0)
        if not t:
            return
        if t[0] == 0x41:
            # A WebTransport bidi stream: echo what follows the header back on it.
            sess = read_varint(buf, t[1])
            if not sess:
                return
            sent = self.wt_bidi.get(sid, sess[1])
            data = buf[sent:]
            if data or fin:
                self._quic.send_stream_data(sid, data, end_stream=fin)
                self.wt_bidi[sid] = len(buf)
                self.transmit()
            return
        if t[0] == 0x01 and sid not in self.answered:
            ln = read_varint(buf, t[1])
            if ln and len(buf) >= ln[1] + ln[0]:
                self.answered.add(sid)
                self.event("connect", stream=sid, headers_len=ln[0])
                self._quic.send_stream_data(sid, response_headers(), end_stream=False)
                self.transmit()


def make_cert(dir_):
    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "kiln-wt-capture")])
    now = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=5)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now)
        .not_valid_after(now + datetime.timedelta(days=10))
        .add_extension(
            x509.SubjectAlternativeName([x509.DNSName("localhost"), x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]),
            critical=False,
        )
        .sign(key, hashes.SHA256())
    )
    der = cert.public_bytes(serialization.Encoding.DER)
    cp, kp = os.path.join(dir_, "cert.pem"), os.path.join(dir_, "key.pem")
    with open(cp, "wb") as f:
        f.write(cert.public_bytes(serialization.Encoding.PEM))
    with open(kp, "wb") as f:
        f.write(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
    return cp, kp, hashlib.sha256(der).hexdigest()


def http_thread(port, quic_port, digest, results):
    class H(http.server.BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_GET(self):
            if self.path.startswith("/info"):
                body = json.dumps({"port": quic_port, "hash": digest}).encode()
                ctype = "application/json"
            else:
                with open(os.path.join(HERE, "page.html"), "rb") as f:
                    body = f.read()
                ctype = "text/html"
            self.send_response(200)
            self.send_header("content-type", ctype)
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            n = int(self.headers.get("content-length", "0"))
            data = self.rfile.read(n)
            with open(os.path.join(results, "page-%d.json" % int(time.time() * 1000)), "wb") as f:
                f.write(data)
            self.send_response(204)
            self.end_headers()

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--http", type=int, default=18480)
    ap.add_argument("--quic", type=int, default=18443)
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    cp, kp, digest = make_cert(a.out)
    cfg = QuicConfiguration(is_client=False, alpn_protocols=["h3"], max_datagram_frame_size=65536)
    cfg.load_cert_chain(cp, kp)
    Capture.out_dir = a.out
    http_thread(a.http, a.quic, digest, a.out)
    await serve("127.0.0.1", a.quic, configuration=cfg, create_protocol=Capture)
    print("ready http=%d quic=%d hash=%s" % (a.http, a.quic, digest), flush=True)
    await asyncio.Future()


if __name__ == "__main__":
    asyncio.run(main())
