#!/usr/bin/env python3
"""The lab's packet capture (docs/browser-lab.md, "The three measures"; plan 01a1a832e9cc step 4):
every frame on an interface, both ways, its length and its time, written as a classic pcap (headers
only, the first 128 bytes) until SIGTERM. A raw packet socket in the client's own namespace, where
the lab is root; tcpdump would drop to a user the lab's namespace doesn't map.

    capture.py IFACE OUT.pcap
"""
import signal
import socket
import struct
import sys
import time

ETH_P_ALL = 0x0003
SNAP = 128


def main():
    iface, out = sys.argv[1], sys.argv[2]
    s = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(ETH_P_ALL))
    s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 8 << 20)
    s.bind((iface, 0))
    s.settimeout(0.2)
    done = []
    signal.signal(signal.SIGTERM, lambda *_: done.append(1))
    with open(out, "wb", buffering=1 << 16) as f:
        f.write(struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, SNAP, 1))
        while not done:
            try:
                frame, _ = s.recvfrom(65535)
            except socket.timeout:
                continue
            except InterruptedError:
                continue
            t = time.time_ns() // 1000
            head = frame[:SNAP]
            f.write(struct.pack("<IIII", t // 1_000_000, t % 1_000_000, len(head), len(frame)))
            f.write(head)


if __name__ == "__main__":
    main()
