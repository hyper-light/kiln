"""A VM's control lane as a duplex byte stream, and what is timed on it.

The stub workload echoes every byte it reads (`cat`), so a round trip is bytes written and the
same bytes read back, and a transfer is a stream written while its echo is read. Every byte that
comes back is checked against what was written: a lane that returns other bytes is `corrupt`, a
forbidden effect, never a sample.

On Linux and macOS one event loop (selectors over non-blocking pipes, `Selected`) serves every
VM's lane at once, so the host's round trips are taken concurrently, one outstanding per VM, as a
fleet of controllers would, with no thread handoff inside a timed span. A lane's read end stays
watched while it is open, so its end (the client's exit) is an event, not a probe. The loop's own
cost is measured beside the VMs' as the floor: the same operations on a local `cat` process
through the same loop.

On Windows select() takes sockets alone (Python's select docs), so each lane's reader and writer
block in threads of their own (`Threaded`): the reader checks each byte as it comes and times an
operation where its last byte is read, in that thread, and hands the agent's thread only the
outcome, so no handoff sits inside a timed span there either. `Lanes` is the host's.
"""

from __future__ import annotations

import array
import json
import os
import queue
import selectors
import sys
import threading
import time
from typing import Callable, Dict, Optional, Tuple

Done = Callable[[bool, int, str], None]

# The most one read or write moves: no pipe an unprivileged process makes holds more (pipe(7):
# /proc/sys/fs/pipe-max-size, 1 MiB by default; 64 KiB on macOS, a few KiB on Windows), so a call
# never moves less than it could for want of this bound, which only caps the buffer a read takes.
IO_MOST = 1 << 20

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

    def __init__(self, name: str, wfd: int, rfd: int, ended: Callable[[str], None], closer: Optional[Callable[[], None]] = None) -> None:
        self.name = name
        self.wfd = wfd
        self.rfd = rfd
        self.ended = ended
        # What ends our side of the lane: the file object that owns `wfd` where there is one, so it
        # isn't closed twice.
        self.closer = closer
        self.op: Optional[Op] = None
        self.open = True
        # Threaded's: the writer's work.
        self.jobs: "queue.SimpleQueue[object]" = queue.SimpleQueue()


class _Lanes:
    """What both kinds of lanes share: the lanes by name and their operations in flight."""

    def __init__(self) -> None:
        self.streams: Dict[str, Stream] = {}

    def _start(self, name: str, out: bytes, done: Done) -> Optional[Stream]:
        s = self.streams.get(name)
        if s is None or not s.open or s.op is not None:
            return None
        s.op = Op(out, done)
        return s

    def busy(self) -> int:
        return sum(1 for s in self.streams.values() if s.op is not None)

    def outstanding(self) -> Dict[str, int]:
        """Each lane's operation in flight, and how long it has been (ns)."""
        now = time.perf_counter_ns()
        return {name: now - s.op.began for name, s in sorted(self.streams.items()) if s.op is not None}


class Selected(_Lanes):
    def __init__(self) -> None:
        super().__init__()
        self.sel = selectors.DefaultSelector()

    def add(self, stream: Stream) -> None:
        os.set_blocking(stream.wfd, False)
        os.set_blocking(stream.rfd, False)
        self.streams[stream.name] = stream
        self.sel.register(stream.rfd, selectors.EVENT_READ, stream)

    def begin(self, name: str, out: bytes, done: Done) -> bool:
        """Starts writing `out` on lane `name`; False when the lane is gone, closed or busy."""
        s = self._start(name, out, done)
        if s is None:
            return False
        self._want_write(s, True)
        return True

    def close(self, name: str) -> None:
        """Ends what we write on `name` (the workload's stdin): `cat` then ends, and its VM's run."""
        s = self.streams.get(name)
        if s is None:
            return
        self._want_write(s, False)
        try:
            if s.closer is not None:
                s.closer()
            else:
                os.close(s.wfd)
        except OSError:
            pass
        s.wfd = -1

    def drop(self, name: str) -> None:
        """Forgets lane `name`: neither of its ends is watched any more (its owner closes them,
        and a descriptor number the OS gives out again is never found still registered)."""
        s = self.streams.pop(name, None)
        if s is None:
            return
        self._want_write(s, False)
        if s.open:
            s.open = False
            try:
                self.sel.unregister(s.rfd)
            except (KeyError, ValueError):
                pass

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
                self._serve(key, mask)
        return True

    def poll(self) -> None:
        """Serves what is ready now, without waiting: a wave's lanes are served between its
        spawns, so an echo that came is never charged the spawns after it."""
        if self.sel.get_map():
            for key, mask in self.sel.select(0):
                self._serve(key, mask)

    def _serve(self, key: selectors.SelectorKey, mask: int) -> None:
        s: Stream = key.data
        if mask & selectors.EVENT_WRITE and key.fd == s.wfd and s.op is not None:
            op = s.op
            try:
                op.sent += os.write(s.wfd, op.out[op.sent : op.sent + IO_MOST])
            except BlockingIOError:
                pass
            except OSError as e:
                self._end(s, f"write: {e}")
                return
            if op.sent >= len(op.out):
                self._want_write(s, False)
        if mask & selectors.EVENT_READ and key.fd == s.rfd:
            self._read(s)

    def _read(self, s: Stream) -> None:
        try:
            chunk = os.read(s.rfd, IO_MOST)
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


# The writer's word that the lane's input ends (CLOSE) or that the lane was dropped (STOP).
_CLOSE = object()
_STOP = object()

# What a lane's thread hands the agent's thread: its lane, the operation done (with its time) or
# None, and the time (ns) or why the lane ended.
Event = Tuple[Stream, Optional[Op], object]


class Threaded(_Lanes):
    """Windows' lanes: a reader and a writer thread a lane, each blocking on a duplicate of its
    descriptor that it alone closes, so a descriptor the agent closes and the OS numbers again is
    never one a lane thread reads or writes. What the threads learn reaches the agent's thread as
    events (`events`), served by `run` and `poll` as the selector's readiness is; the agent's
    callbacks run there alone."""

    def __init__(self) -> None:
        super().__init__()
        self.events: "queue.SimpleQueue[Event]" = queue.SimpleQueue()

    def add(self, stream: Stream) -> None:
        """Starts the lane's threads. An OSError when the OS gives no descriptor or thread for it:
        the lane is never served, and what was started for it ends."""
        rd = os.dup(stream.rfd)
        try:
            wd = os.dup(stream.wfd)
        except OSError:
            os.close(rd)
            raise
        self.streams[stream.name] = stream
        mine = [wd, rd]
        try:
            for role, run, fd in (("w", self._writer, wd), ("r", self._reader, rd)):
                threading.Thread(target=run, args=(stream, fd), name=f"lane-{stream.name}-{role}", daemon=True).start()
                mine.remove(fd)
        except RuntimeError as e:
            for fd in mine:
                os.close(fd)
            self.drop(stream.name)
            raise OSError(f"no thread for lane {stream.name}: {e}") from e

    def begin(self, name: str, out: bytes, done: Done) -> bool:
        """Starts writing `out` on lane `name`; False when the lane is gone, closed or busy."""
        s = self._start(name, out, done)
        if s is None:
            return False
        s.jobs.put(s.op)
        return True

    def close(self, name: str) -> None:
        """Ends what we write on `name`: the agent's own end now, the writer's once what it holds
        is written; then `cat` ends, and its VM's run."""
        s = self.streams.get(name)
        if s is None or s.wfd < 0:
            return
        try:
            if s.closer is not None:
                s.closer()
            else:
                os.close(s.wfd)
        except OSError:
            pass
        s.wfd = -1
        s.jobs.put(_CLOSE)

    def drop(self, name: str) -> None:
        """Forgets lane `name`: its threads say nothing more and end with their pipes."""
        s = self.streams.pop(name, None)
        if s is None:
            return
        s.open = False
        s.jobs.put(_STOP)

    def _writer(self, s: Stream, fd: int) -> None:
        why: Optional[str] = None
        try:
            while True:
                job = s.jobs.get()
                if not isinstance(job, Op):
                    return
                out = memoryview(job.out)
                sent = 0
                while sent < len(out):
                    try:
                        sent += os.write(fd, out[sent:])
                    except OSError as e:
                        why = f"write: {e}"
                        return
        finally:
            os.close(fd)
            if why is not None:
                self.events.put((s, None, why))

    def _reader(self, s: Stream, fd: int) -> None:
        why: Optional[str] = None
        try:
            while why is None:
                try:
                    chunk = os.read(fd, IO_MOST)
                except OSError as e:
                    why = f"read: {e}"
                    break
                now = time.perf_counter_ns()
                if not chunk:
                    why = "ended"
                    break
                op = s.op
                if op is None:
                    why = f"corrupt: {len(chunk)} bytes came back with nothing written"
                elif chunk != op.out[op.got : op.got + len(chunk)]:
                    why = f"corrupt: {len(chunk)} bytes at offset {op.got} differ from what was written"
                else:
                    op.got += len(chunk)
                    if op.got >= len(op.out):
                        # Bytes past it before the agent's thread hears of it differ from the
                        # empty rest of `out`: corrupt, as with nothing written.
                        self.events.put((s, op, now - op.began))
        finally:
            os.close(fd)
            if why is not None:
                self.events.put((s, None, why))

    def _serve(self, event: Event) -> None:
        s, op, what = event
        # A dropped lane's late word reaches no one: its ask is over.
        if self.streams.get(s.name) is not s:
            return
        if op is not None:
            if s.op is op:
                s.op = None
                op.done(True, int(what), "")  # type: ignore[call-overload]
            return
        if not s.open:
            return
        s.open = False
        s.jobs.put(_STOP)
        op, s.op = s.op, None
        if op is not None:
            op.done(False, time.perf_counter_ns() - op.began, str(what))
        s.ended(str(what))

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
            if not self.streams and self.events.empty():
                # No lane to serve: nothing can make `until` hold but time, so wait it out.
                if timeout is not None:
                    time.sleep(timeout)
                return until()
            try:
                self._serve(self.events.get(timeout=timeout))
            except queue.Empty:
                continue
        return True

    def poll(self) -> None:
        """Serves what has come, without waiting."""
        while True:
            try:
                event = self.events.get_nowait()
            except queue.Empty:
                return
            self._serve(event)


Lanes = Threaded if sys.platform == "win32" else Selected
