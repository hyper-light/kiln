#!/usr/bin/env python3
"""The kernel's numbers of the server's sockets to a peer, every 10 ms (plan 01a1a832eb2f711f9c6f51d88effe89d
step 3: "TCP_INFO unsent bytes, sampled every 10 ms"): Linux's sock_diag (NETLINK_SOCK_DIAG, the interface
`ss -i` reads), each established TCP socket's `struct tcp_info`, from the server's own namespace. One line a
socket a sample, until SIGTERM:

    t_us sport dport notsent_bytes unacked_segs snd_mss rtt_us min_rtt_us delivery_rate bytes_sent bytes_retrans

    diag.py PORT PEER_IP EVERY_MS OUT
"""
import ipaddress
import signal
import socket
import struct
import sys
import time

NETLINK_SOCK_DIAG = 4
SOCK_DIAG_BY_FAMILY = 20
NLM_F_REQUEST, NLM_F_DUMP = 0x1, 0x300
NLMSG_DONE, NLMSG_ERROR = 3, 2
INET_DIAG_INFO = 2
TCP_ESTABLISHED = 1


def request(seq):
    # struct inet_diag_req_v2: family, protocol, ext (INET_DIAG_INFO's bit), pad, states; an empty sockid.
    req = struct.pack("=BBBBI", socket.AF_INET, socket.IPPROTO_TCP, 1 << (INET_DIAG_INFO - 1), 0, 1 << TCP_ESTABLISHED) + bytes(48)
    return struct.pack("=IHHII", 16 + len(req), SOCK_DIAG_BY_FAMILY, NLM_F_REQUEST | NLM_F_DUMP, seq, 0) + req


def sockets(nl, seq):
    """Each established IPv4 TCP socket: (sport, dport, dst, tcp_info bytes)."""
    nl.send(request(seq))
    out = []
    while True:
        data = nl.recv(1 << 20)
        at = 0
        while at + 16 <= len(data):
            length, kind = struct.unpack_from("=IH", data, at)
            if kind == NLMSG_DONE:
                return out
            if kind == NLMSG_ERROR:
                raise OSError("sock_diag refused the dump")
            msg = data[at + 16 : at + length]
            # struct inet_diag_msg: family, state, timer, retrans, then the sockid (ports big-endian).
            sport, dport = struct.unpack_from(">HH", msg, 4)
            dst = ipaddress.IPv4Address(msg[24:28])
            info = None
            a = 72
            while a + 4 <= len(msg):
                alen, atype = struct.unpack_from("=HH", msg, a)
                if alen < 4:
                    break
                if atype == INET_DIAG_INFO:
                    info = msg[a + 4 : a + alen]
                a += (alen + 3) & ~3
            if info is not None:
                out.append((sport, dport, dst, info))
            at += (length + 3) & ~3


def fields(info):
    """struct tcp_info's fields this reads (include/uapi/linux/tcp.h), 0 where the kernel's is shorter."""
    info = info + bytes(max(0, 216 - len(info)))
    unacked, = struct.unpack_from("=I", info, 24)
    snd_mss, = struct.unpack_from("=I", info, 16)
    rtt, = struct.unpack_from("=I", info, 68)
    notsent, min_rtt = struct.unpack_from("=II", info, 144)
    delivery_rate, = struct.unpack_from("=Q", info, 160)
    bytes_sent, bytes_retrans = struct.unpack_from("=QQ", info, 200)
    return notsent, unacked, snd_mss, rtt, min_rtt, delivery_rate, bytes_sent, bytes_retrans


def main():
    port, peer, every_ms, out = int(sys.argv[1]), ipaddress.IPv4Address(sys.argv[2]), float(sys.argv[3]), sys.argv[4]
    done = []
    signal.signal(signal.SIGTERM, lambda *_: done.append(1))
    nl = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, NETLINK_SOCK_DIAG)
    step = every_ms / 1000
    nxt = time.monotonic()
    seq = 0
    with open(out, "w", buffering=1 << 16) as f:
        while not done:
            seq += 1
            t = time.time_ns() // 1000
            for sport, dport, dst, info in sockets(nl, seq):
                if sport == port and dst == peer:
                    f.write(f"{t} {sport} {dport} " + " ".join(str(v) for v in fields(info)) + "\n")
            nxt += step
            time.sleep(max(0.0, nxt - time.monotonic()))


if __name__ == "__main__":
    main()
