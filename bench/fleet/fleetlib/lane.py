"""A VM's control lane as a duplex byte stream, and what is timed on it.

The stub workload echoes every byte it reads (`cat`), so a round trip is bytes written and the
same bytes read back, and a transfer is a stream written while its echo is read. Every byte that
comes back is checked against what was written: a lane that returns other bytes is `corrupt`, a
forbidden effect, never a sample.

One event loop (selectors over non-blocking pipes) serves every VM's lane at once, so the host's
round trips are taken concurrently, one outstanding per VM, as a fleet of controllers would, with
no thread handoff inside a timed span. A lane's read end stays watched while it is open, so its
end (the client's exit) is an event, not a probe. The loop's own cost is measured beside the VMs'
as the floor: the same operations on a local `cat` process through the same loop.

Windows: its pipes can't be selected on (selectors waits on sockets alone there), so lanes there
are owed (README, Owed).
"""

from __future__ import annotations

import array
import json
import os
import selectors
import sys
import time
from typing import Callable, Dict, Optional

Done = Callable[[bool, int, str], None]

_PATTERNS: Dict[int, bytes] = {}
_WORD = "I" if array.array("I").itemsize == 4 else "L"


def pattern(size: int) -> bytes:
    """`size` bytes of the transfer pattern: each 4-byte word its own index, big-endian, so a
    dropped, doubled or reordered chunk of any length below 16 GiB shows at the first word it
    moves."""
    got = _PATTERNS.get(size)
    if got is None:
        words = array.array(_WORD, range((size + 3) // 4))
        if sys.byteorder == "little":
            words.byteswap()
        got = words.tobytes()[:size]
        _PATTERNS[size] = got
    return got


def control_message(seq: int) -> bytes:
    """The bytes kiln writes to interrupt a Claude Code turn, the control message its adapter
    sends most (crates/kiln/src/harness/claude.rs `request`, kiln d6ffa45d8: serde_json's compact
    form of {"type","request_id","request":{"subtype":"interrupt"}} and a newline). A round trip
    carries one, at its real size."""
    return (json.dumps({"request": {"subtype": "interrupt"}, "request_id": f"kiln_{seq}", "type": "control_request"}, separators=(",", ":"), sort_keys=True) + "\n").encode()


class Op:
    def __init__(self, out: bytes, done: Done) -> None:
        self.out = out
        self.done = done
        self.sent = 0
        self.got = 0
        self.began = time.perf_counter_ns()


class Stream:
    """One lane: what is written into (`wfd`) and what comes back (`rfd`)."""

    def __init__(self, name: str, wfd: int, rfd: int, ended: Callable[[str], None]) -> None:
        self.name = name
        self.wfd = wfd
        self.rfd = rfd
        self.ended = ended
        self.op: Optional[Op] = None
        self.open = True
        os.set_blocking(wfd, False)
        os.set_blocking(rfd, False)


class Lanes:
    def __init__(self) -> None:
        self.sel = selectors.DefaultSelector()
        self.streams: Dict[str, Stream] = {}

    def add(self, stream: Stream) -> None:
        self.streams[stream.name] = stream
        self.sel.register(stream.rfd, selectors.EVENT_READ, stream)

    def begin(self, name: str, out: bytes, done: Done) -> bool:
        """Starts writing `out` on lane `name`; False when the lane is gone, closed or busy."""
        s = self.streams.get(name)
        if s is None or not s.open or s.op is not None:
            return False
        s.op = Op(out, done)
        self._want_write(s, True)
        return True

    def close(self, name: str) -> None:
        """Ends what we write on `name` (the workload's stdin): `cat` then ends, and its VM's run."""
        s = self.streams.get(name)
        if s is None:
            return
        self._want_write(s, False)
        try:
            os.close(s.wfd)
        except OSError:
            pass
        s.wfd = -1

    def busy(self) -> int:
        return sum(1 for s in self.streams.values() if s.op is not None)

    def _want_write(self, s: Stream, on: bool) -> None:
        if s.wfd < 0:
            return
        try:
            self.sel.get_key(s.wfd)
            registered = True
        except KeyError:
            registered = False
        if on and not registered:
            self.sel.register(s.wfd, selectors.EVENT_WRITE, s)
        elif not on and registered:
            self.sel.unregister(s.wfd)

    def _end(self, s: Stream, why: str) -> None:
        if not s.open:
            return
        s.open = False
        try:
            self.sel.unregister(s.rfd)
        except (KeyError, ValueError):
            pass
        self._want_write(s, False)
        op, s.op = s.op, None
        if op is not None:
            op.done(False, time.perf_counter_ns() - op.began, why)
        s.ended(why)

    def run(self, until: Callable[[], bool], deadline_ns: Optional[int]) -> bool:
        """Serves every lane until `until()` holds (True), or the monotonic deadline passes or no
        event can come (False, unless `until()` holds then)."""
        while not until():
            timeout = None
            if deadline_ns is not None:
                left = deadline_ns - time.monotonic_ns()
                if left <= 0:
                    return until()
                timeout = left / 1e9
            if not self.sel.get_map():
                # No lane to serve: nothing can make `until` hold but time, so wait it out.
                if timeout is not None:
                    time.sleep(timeout)
                return until()
            for key, mask in self.sel.select(timeout):
                s: Stream = key.data
                if mask & selectors.EVENT_WRITE and key.fd == s.wfd and s.op is not None:
                    op = s.op
                    try:
                        op.sent += os.write(s.wfd, op.out[op.sent : op.sent + (1 << 20)])
                    except BlockingIOError:
                        pass
                    except OSError as e:
                        self._end(s, f"write: {e}")
                        continue
                    if op.sent >= len(op.out):
                        self._want_write(s, False)
                if mask & selectors.EVENT_READ and key.fd == s.rfd:
                    self._read(s)
        return True

    def _read(self, s: Stream) -> None:
        try:
            chunk = os.read(s.rfd, 1 << 20)
        except BlockingIOError:
            return
        except OSError as e:
            self._end(s, f"read: {e}")
            return
        if not chunk:
            self._end(s, "ended")
            return
        op = s.op
        if op is None:
            self._end(s, f"corrupt: {len(chunk)} bytes came back with nothing written")
            return
        if chunk != op.out[op.got : op.got + len(chunk)]:
            self._end(s, f"corrupt: {len(chunk)} bytes at offset {op.got} differ from what was written")
            return
        op.got += len(chunk)
        if op.got >= len(op.out):
            s.op = None
            op.done(True, time.perf_counter_ns() - op.began, "")
