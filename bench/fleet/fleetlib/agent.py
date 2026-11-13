"""The host agent: one per host, under `kiln run`, owning everything the run starts there.

Its phases, each recorded as it begins and ends:
1. setup: the host's facts, a scope of the run's own, the daemon, the image (pulled once into the
   run's home) and one run to its end (the first-run costs: the guest kernel, the template).
2. fill: waves of asks doubling from one, each wave's VMs started at once, until the measured room
   holds no more (`derive.density_limit`); start latency is each ask's spawn to its lane's first
   echo, the VM usable.
3. hold: on every VM at once, round trips of a real control message (100 samples a host at
   least), a recorded turn's stream, then transfers doubling in size until their median rate
   stops rising; the scope scanned after each batch.
4. chaos: each action of `--chaos`, `--rounds` times, each target drawn by the seeded generator;
   after each the scope settles, the fill tops it up again and the witnesses judge; then a second
   hold, at density after chaos.
5. drain: every lane's stdin closed, each run's end awaited, then what is left removed by name.
6. verify: the view empty and the scan holding no VM or network process.
7. teardown: the daemon stopped, then a last scan for the home's processes, which must be none.

Every wait is learned (`derive.Patience`); a wait that runs out is recorded, typed, and the thing
it waited for is still watched.
"""

from __future__ import annotations

import os
import random
import shutil
import signal
import subprocess
import time
from typing import Callable, Dict, List, Optional

from . import chaos, derive, hostfacts, lane, procs, witness
from .hex import Hex
from .kilnapi import Kiln
from .records import Records
from .shards import Shards

W = witness


class Stop(Exception):
    """A signal asked the agent to stop: it drains and tears down what it started."""


class Vm:
    __slots__ = ("token", "seq", "popen", "client_start", "asked_ns", "lane_ended", "wave")

    def __init__(self, token: str, seq: int, popen: subprocess.Popen, client_start: str, asked_ns: int, wave: int) -> None:
        self.token = token
        self.seq = seq
        self.popen = popen
        self.client_start = client_start
        self.asked_ns = asked_ns
        self.lane_ended: Optional[str] = None
        self.wave = wave


def driver_for(opts, scope: str):
    """The placer the run asks VMs of: Shards' daemon (`shards`, the direct driver), a Hex
    scheduler of the run's own (`hex`, sized by `--vm`), or kiln's balancer (`kiln`: isolated agent
    sessions, kiln's address and token from KILN_URL and KILN_TOKEN)."""
    driver = getattr(opts, "driver", "shards")
    if driver == "hex":
        return Hex(opts.hex, opts.shards, scope, needs_room(opts.vm), guest_platform())
    if driver == "kiln":
        return Kiln(os.environ.get("KILN_URL", ""), os.environ.get("KILN_TOKEN", ""), opts.kind, opts.shards, scope)
    return Shards(opts.shards, scope)


def guest_platform() -> str:
    """A Shards guest's OCI platform: Linux, of the host's architecture."""
    import platform

    machine = platform.machine().lower()
    return "linux/" + {"x86_64": "amd64", "amd64": "amd64", "aarch64": "arm64", "arm64": "arm64"}.get(machine, machine)


def needs_room(needs: str) -> Dict[str, Optional[int]]:
    """The room `kiln run --needs` asked for and kiln holds the run to (KILN_NEEDS): `mem=32g`,
    `cpus=8`. Memory in bytes, CPUs in thousandths."""
    room: Dict[str, Optional[int]] = {"mem": None, "cpu": None}
    units = {"k": 1 << 10, "m": 1 << 20, "g": 1 << 30, "t": 1 << 40}
    for part in needs.split(","):
        key, _, value = part.strip().partition("=")
        value = value.strip().lower()
        if not value:
            continue
        try:
            if key == "mem":
                scale = units.get(value[-1], 1)
                digits = value[:-1] if value[-1] in units else value
                room["mem"] = int(float(digits) * scale)
            elif key == "cpus":
                room["cpu"] = int(float(value) * 1000)
        except ValueError:
            continue
    return room


class Agent:
    def __init__(self, opts, rec: Records) -> None:
        self.o = opts
        self.rec = rec
        self.deadline_ns = time.monotonic_ns() + int(opts.deadline_s * 1e9)
        self.scope = os.path.join(opts.out, "s")
        self.logs = os.path.join(opts.out, "logs")
        self.driver = driver_for(opts, self.scope)
        self.ledger: Dict[str, W.Ask] = {}
        self.vms: Dict[str, Vm] = {}
        self.lanes = lane.Lanes()
        self.rng = random.Random(opts.seed)
        self.seq = 0
        self.wave = 0
        self.room: Dict[str, Optional[int]] = needs_room(opts.needs)
        self.cost: Dict[str, int] = {}
        self.limit: Optional[int] = None
        self.found: Dict[str, int] = {}
        self.scans = 0
        self.floor: Optional[subprocess.Popen] = None
        self.stopping = False
        # Failures recorded (`failed`): any fails the run, as a witness does.
        self.failures = 0
        first = derive.Patience("first_run")
        daemon = derive.Patience("daemon")
        start = derive.Patience("start", first)
        self.waits: Dict[str, derive.Patience] = {"first_run": first, "daemon": daemon, "start": start}
        for kind in ("rtt", "turn", "reap", "drain.lanes", "drain.gone", "chaos.vm.lane", "chaos.vm.gone", "chaos.client.lane", "chaos.client.gone"):
            self.waits[kind] = derive.Patience(kind, start)
        for role in ("daemon", "node", "scheduler"):
            self.waits[f"chaos.{role}.settle"] = derive.Patience(f"chaos.{role}.settle", daemon)
        self.asked_ns: Dict[str, int] = {}
        # Per byte: each bulk round's transfers are twice the last's (derive's docstring).
        self.waits["bulk"] = derive.Patience("bulk", self.waits["turn"], 1 / derive.TURN_BYTES)
        self.waits["view.settle"] = derive.Patience("view.settle", start)
        self.waits["first_run.gone"] = derive.Patience("first_run.gone", first)

    # ------------------------------------------------------------------ plumbing

    def until(self, kind: str) -> Optional[int]:
        """A wait of `kind`'s monotonic deadline: its learned bound from now, never past the run's."""
        bound = self.waits[kind].bound_ns()
        if bound is None:
            return self.deadline_ns
        return min(self.deadline_ns, time.monotonic_ns() + bound)

    def phase(self, name: str, at: str, **more: object) -> None:
        self.rec.write("phase", phase=name, at=at, **more)

    def failed(self, what: str, why: str, **more: object) -> None:
        self.failures += 1
        self.rec.write("failed", what=what, why=why, **more)

    def check_stop(self) -> None:
        if self.stopping:
            raise Stop()

    def serve(self, done: Callable[[], bool], kind: str) -> bool:
        """Serves the lanes until `done()` or `kind`'s learned bound; records a run-out."""
        began = time.monotonic_ns()
        self.lanes.run(lambda: done() or self.stopping, self.until(kind))
        took = time.monotonic_ns() - began
        self.check_stop()
        if done():
            self.waits[kind].answered(took)
            return True
        self.waits[kind].ran_out(took)
        self.rec.write("ran_out", wait=kind, after_ns=took)
        return False

    def serve_ops(self, done: Callable[[], bool], finished: Callable[[], int], kind: str, units: float) -> bool:
        """Serves a batch of operations until `done()`: each has finished or its lane ended.

        Its bound is on the wait for the next operation to finish, never on the batch's length:
        twice the slowest operation of `kind` for `units`, which each operation teaches (`ops`;
        RFC 6298's timer is a segment's, not a transfer's). A stall past it is recorded (`ran_out`,
        `stalled`: each lane's operation in flight and for how long) and the bound doubled (RFC
        6298 §5.5); it never cuts the batch short, since what that drops is the slowest operations,
        the tail being measured. Only a signal or the run's deadline ends it unfinished (False)."""
        last_n, last_t = finished(), time.monotonic_ns()
        while not done():
            bound = self.waits[kind].bound_ns(units)
            deadline = self.deadline_ns if bound is None else min(self.deadline_ns, last_t + bound)
            self.lanes.run(lambda n=last_n: done() or self.stopping or finished() != n, deadline)
            self.check_stop()
            now = time.monotonic_ns()
            if finished() != last_n:
                last_n, last_t = finished(), now
            elif not done() and now >= self.deadline_ns:
                self.rec.write("ran_out", wait=kind, after_ns=now - last_t, units=units, at="the run's deadline", stalled=self.lanes.outstanding())
                return False
            elif not done() and now >= deadline:
                self.waits[kind].ran_out(now - last_t, units)
                self.rec.write("ran_out", wait=kind, after_ns=now - last_t, units=units, stalled=self.lanes.outstanding())
                last_t = now
        return True

    def settle(self, done: Callable[[], bool], kind: str) -> bool:
        """A probe of the process table at doubling intervals from 1 ms until `done()` or
        `kind`'s bound: O(log T) probes for a settle of T, the lanes served meanwhile (R05: a
        probe, said as one, where the OS gives no event)."""
        began = time.monotonic_ns()
        deadline = self.until(kind)
        wait_ns = 1_000_000
        while not done():
            self.check_stop()
            now = time.monotonic_ns()
            if deadline is not None and now > deadline:
                self.waits[kind].ran_out(now - began)
                self.rec.write("ran_out", wait=kind, after_ns=now - began)
                return False
            self.lanes.run(lambda: self.stopping, min(deadline, now + wait_ns) if deadline is not None else now + wait_ns)
            wait_ns *= 2
        self.waits[kind].answered(time.monotonic_ns() - began)
        return True

    # ------------------------------------------------------------------ scans

    def roots(self) -> List[int]:
        return self.driver.roots()

    def scope_procs(self) -> List[Dict[str, object]]:
        return self.driver.scan(procs.table(), self.roots())

    def quiescent(self) -> bool:
        return not any(a.state in W.IN_FLIGHT for a in self.ledger.values())

    def ended_on_its_own(self, token: str) -> Optional[str]:
        """Why a live ask's run ended with nobody ending it: its lane's end, or the placer's word."""
        if self.driver.lanes:
            return self.vms[token].lane_ended
        return "the placer says it ended" if token in self.driver.ended else None

    def sweep_ended(self) -> None:
        """A live ask whose run ended on its own (its lane's end, or the placer's word). Typed
        (`ended_unasked`), never left to look lost."""
        for t in self.live():
            why = self.ended_on_its_own(t)
            if why is not None:
                self.ledger[t].move(W.FAILED)
                self.rec.write("ended_unasked", token=t, lane=why)
                self.found["ended_unasked"] = self.found.get("ended_unasked", 0) + 1
                self.finish(t)

    def observe(self):
        found = self.scope_procs()
        view, view_err = self.driver.view()
        return found, view, view_err

    def scan(self, why: str) -> Dict[str, object]:
        """The scope as the OS shows it and as the daemon says, judged by every witness.

        At a quiescent point, an effect that a lag could make (`leaked`, `lost`, `unasked`,
        `orphan`: the daemon's records behind its VMs, a VM still exiting) is judged again until
        it is gone or a VM's start has passed twice over (`view.settle`, seeded from the slowest
        start): it is a violation only if it outlasts that, and the lag is recorded either way."""
        self.sweep_ended()
        self.scans += 1
        found, view, view_err = self.observe()
        quiet = self.quiescent()
        if quiet:
            lagging = lambda vs: [v for v in vs if v["witness"] in W.LAGGING]  # noqa: E731
            first = lagging(W.check(self.ledger, found, view, self.room, self.limit, True, self.driver.scans))
            if first:
                began = time.monotonic_ns()
                latest = {"found": found, "view": view, "err": view_err}

                def settled() -> bool:
                    latest["found"], latest["view"], latest["err"] = self.observe()
                    return not lagging(W.check(self.ledger, latest["found"], latest["view"], self.room, self.limit, True, self.driver.scans))

                ok = self.settle(settled, "view.settle")
                found, view, view_err = latest["found"], latest["view"], latest["err"]
                self.rec.write("view_lag", scan=self.scans, ns=time.monotonic_ns() - began, resolved=ok, first=[v["witness"] for v in first], detail=first[:20])
        vms = [p for p in found if p["role"] == "vm"]
        nets = {p["vm"]: p for p in found if p["role"] == "net" and p.get("vm") is not None}
        per_vm = [int(p["mem"] or 0) + int((nets.get(p["pid"]) or {}).get("mem") or 0) for p in vms]
        if per_vm and max(per_vm) > 0:
            self.cost["mem"] = max(self.cost.get("mem", 0), max(per_vm))
            self.limit = derive.density_limit(self.room, self.cost)
        violations = W.check(self.ledger, found, view, self.room, self.limit, quiet, self.driver.scans)
        mem = hostfacts.memory()
        rec = self.rec.write(
            "scan", n=self.scans, why=why, quiescent=quiet, vms=len(vms), per_vm_mem=per_vm,
            held=sum(int(p["mem"] or 0) for p in found), procs=found, view=view, view_error=view_err,
            live=self.live(), cost=dict(self.cost), room=self.room, limit=self.limit,
            mem_available=mem["available"], load=hostfacts.load(),
        )
        for v in violations:
            self.found[v["witness"]] = self.found.get(v["witness"], 0) + 1
            self.rec.write("witness", scan=self.scans, quiescent=quiet, **v)
        return rec

    # ------------------------------------------------------------------ asks

    def live(self) -> List[str]:
        return sorted(t for t, a in self.ledger.items() if a.state == W.LIVE)

    def starting(self) -> List[str]:
        return sorted(t for t, a in self.ledger.items() if a.state == W.STARTING)

    def ask(self) -> str:
        self.seq += 1
        token = f"{self.o.label}-{self.seq}"
        a = W.Ask(token)
        self.ledger[token] = a
        asked = time.perf_counter_ns()
        if not self.driver.lanes:
            return self.place(token, a, asked)
        try:
            popen = self.driver.start_vm(token, self.o.image, self.o.ballast, os.path.join(self.logs, f"vm-{token}.err"))
        except OSError as e:
            # The host gave the harness no client (no descriptor or process left): refused here,
            # typed, and the fill stops at it.
            a.move(W.REFUSED)
            self.rec.write("start_failed", token=token, why=f"spawn: {e}", wave=self.wave, refused=True)
            return token
        spawned = time.perf_counter_ns() - asked
        client = procs.one(popen.pid)
        v = Vm(token, self.seq, popen, client.start if client else "", asked, self.wave)
        self.vms[token] = v
        a.move(W.STARTING)
        self.rec.write("ask", token=token, wave=self.wave, client_pid=popen.pid, spawn_ns=spawned)
        if popen.stdin is None or popen.stdout is None:
            a.move(W.FAILED)
            self.rec.write("start_failed", token=token, why="the client has no pipes", wave=self.wave)
            return token

        def ended(why: str, t: str = token) -> None:
            self.vms[t].lane_ended = why

        try:
            self.lanes.add(lane.Stream(token, popen.stdin.fileno(), popen.stdout.fileno(), ended, popen.stdin.close))
        except OSError as e:
            # No descriptor or thread for its lane: its client is let go (its stdin closed, `cat`
            # and its run end), the ask is a typed failure, and the fill stops at it.
            popen.stdin.close()
            a.move(W.FAILED)
            self.rec.write("start_failed", token=token, why=f"lane: {e}", wave=self.wave)
            return token

        def started(ok: bool, _took: int, why: str, t: str = token) -> None:
            vm = self.vms[t]
            took = time.perf_counter_ns() - vm.asked_ns
            if self.ledger[t].state != W.STARTING:
                self.rec.write("late", token=t, ok=ok, start_ns=took, why=why)
            elif ok:
                self.ledger[t].move(W.LIVE)
                self.waits["start"].answered(took)
                self.rec.write("started", token=t, start_ns=took, wave=vm.wave, concurrency=self.wave_size)
            else:
                self.ledger[t].move(W.FAILED)
                self.rec.write("start_failed", token=t, why=why, after_ns=took, wave=vm.wave)

        self.lanes.begin(token, lane.control_message(self.seq), started)
        return token

    def place(self, token: str, a: W.Ask, asked: int) -> str:
        """An ask of a placer that gives no lane: its box submitted. Refused, typed in the placer's
        words, the fill stops at it; taken, it is starting until the placer says it was made."""
        why = self.driver.place(token, self.o.image, self.o.ballast)
        took = time.perf_counter_ns() - asked
        if why is not None:
            a.move(W.REFUSED)
            self.rec.write("start_failed", token=token, why=why, wave=self.wave, refused=True, after_ns=took)
            return token
        a.move(W.STARTING)
        self.asked_ns[token] = asked
        self.rec.write("ask", token=token, wave=self.wave, place_ns=took)
        return token

    def await_placed(self, tokens: List[str]) -> None:
        """Follows each starting ask's box (the wave's, and any earlier one still starting) until
        the placer says it was made (`started`: asked to made, its start; the placer gives no lane,
        so no workload's answer is in it), ended or refused, or its learned wait passes. A probe at
        doubling intervals from 1 ms, as the placer gives no event (R05: a probe, said as one)."""
        tokens = sorted(set(tokens) | set(self.starting()))

        def settled() -> bool:
            for t in tokens:
                if self.ledger[t].state != W.STARTING:
                    continue
                state = self.driver.placed(t)
                took = time.perf_counter_ns() - self.asked_ns[t]
                if state == "live":
                    self.ledger[t].move(W.LIVE)
                    self.waits["start"].answered(took)
                    self.rec.write("started", token=t, start_ns=took, wave=self.wave, concurrency=self.wave_size, by="made", on=self.driver.where(t))
                elif state != "starting":
                    self.ledger[t].move(W.FAILED)
                    self.rec.write("start_failed", token=t, why=state, after_ns=took, wave=self.wave)
            return all(self.ledger[t].state != W.STARTING for t in tokens)

        self.settle(settled, "start")

    def finish(self, token: str) -> None:
        """An ask whose run is over: its client reaped (within its learned wait), its lane
        dropped, its state ENDED. A placer's ask has no client: its state alone moves."""
        a = self.ledger[token]
        if not self.driver.lanes:
            if a.state in (W.LIVE, W.STARTING):
                a.move(W.STOPPING)
            if a.state in (W.STOPPING, W.KILLED, W.FAILED):
                a.move(W.ENDED)
            self.rec.write("ended", token=token)
            return
        v = self.vms[token]
        code = v.popen.poll()
        if code is None:
            began = time.monotonic_ns()
            bound = self.waits["reap"].bound_ns()
            try:
                code = v.popen.wait(timeout=None if bound is None else bound / 1e9)
                self.waits["reap"].answered(time.monotonic_ns() - began)
            except subprocess.TimeoutExpired:
                self.waits["reap"].ran_out(time.monotonic_ns() - began)
                self.rec.write("ran_out", wait="reap", token=token, after_ns=time.monotonic_ns() - began)
        self.lanes.drop(token)
        # Its descriptors go with it: a host's hundreds of asks would run the agent out of them.
        for f in (v.popen.stdin, v.popen.stdout):
            if f is not None:
                try:
                    f.close()
                except OSError:
                    pass
        if a.state in (W.LIVE, W.STARTING):
            a.move(W.STOPPING)
        if a.state in (W.STOPPING, W.KILLED, W.FAILED):
            a.move(W.ENDED)
        self.rec.write("ended", token=token, client_exit=code, lane=v.lane_ended)

    # ------------------------------------------------------------------ phases

    def setup(self) -> bool:
        self.phase("setup", "begin")
        os.makedirs(self.logs, exist_ok=True)
        if os.path.exists(self.scope):
            self.failed("setup", f"{self.scope} exists: each run takes a scope of its own")
            return False
        os.makedirs(self.scope)
        nofile = hostfacts.raise_nofile()
        facts = hostfacts.facts(self.o.repo)
        if self.o.rev:
            facts["kiln_rev"], facts["kiln_dirty"] = self.o.rev, False
        identity = self.driver.identity()
        self.rec.write(
            "host", facts=facts, shards=self.o.shards, shards_sha256=identity.get("shards"), identity=identity, image=self.o.image,
            ballast=self.o.ballast, needs=self.o.needs, room=self.room, seed=self.o.seed, chaos=self.o.chaos, rounds=self.o.rounds,
            deadline_s=self.o.deadline_s, driver=self.driver.name, vm=getattr(self.o, "vm", None), controller_t=self.o.controller_t, nofile=nofile,
        )
        if self.room.get("mem") is None:
            self.failed("setup", "no memory room: run the agent through `kiln run --needs mem=…`, which holds it to that room (KILN_NEEDS), or give --needs")
            return False
        began = time.monotonic_ns()
        ok, why = self.driver.start(os.path.join(self.logs, "daemon.log"), self.deadline_ns)
        if not ok:
            self.failed("daemon", why)
            return False
        self.waits["daemon"].answered(time.monotonic_ns() - began)
        self.rec.write("daemon", took_ns=time.monotonic_ns() - began, version=self.driver.version(), **self.driver.placer())
        began = time.monotonic_ns()
        ok, said = self.driver.pull(self.o.image)
        self.rec.write("pulled", ok=ok, took_ns=time.monotonic_ns() - began, said=said[-300:])
        if not ok:
            self.failed("pull", said)
            return False
        began = time.monotonic_ns()
        ok, said = self.driver.run_once(self.o.image, self.deadline_ns)
        took = time.monotonic_ns() - began
        self.rec.write("first_run", ok=ok, took_ns=took, said=said[-300:])
        if not ok:
            self.failed("first_run", said)
            return False
        self.waits["first_run"].answered(took)
        # The first run's VM may still be exiting when its client returns: its linger is recorded,
        # and nothing is measured until it is gone.
        began = time.monotonic_ns()
        gone = self.settle(lambda: not any(p["role"] in ("vm", "net") for p in self.scope_procs()), "first_run.gone")
        self.rec.write("first_run_gone", gone=gone, ns=time.monotonic_ns() - began)
        if shutil.which("cat"):
            self.floor = subprocess.Popen(["cat"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
            if self.floor.stdin is not None and self.floor.stdout is not None:
                self.lanes.add(lane.Stream("floor", self.floor.stdin.fileno(), self.floor.stdout.fileno(), lambda why: None, self.floor.stdin.close))
        self.scan("setup")
        self.phase("setup", "end")
        return True

    wave_size = 0

    def fill(self, why: str) -> None:
        """Waves doubling from one until the measured room holds no more VMs."""
        self.phase("fill", "begin", why=why)
        last = 0
        while True:
            self.check_stop()
            if not self.driver.scans:
                # A placer whose VMs run elsewhere admits them itself: the run asks up to the
                # person's number (`--asks`), and its own refusal or queue is where it is full.
                room_left = self.o.asks - len(self.live()) - len(self.starting())
            elif self.limit is None and self.ledger and any(a.state == W.LIVE for a in self.ledger.values()):
                self.failed("fill", "no VM's memory could be read: no limit can be derived, so nothing more is started")
                break
            else:
                room_left = 1 if self.limit is None else self.limit - len(self.live()) - len(self.starting())
            n = derive.next_wave(last, room_left)
            if n <= 0:
                break
            self.wave += 1
            self.wave_size = n
            tokens = self.ask_wave(n)
            if self.driver.lanes:
                self.serve(lambda ts=tokens: all(self.ledger[t].state != W.STARTING for t in ts), "start")
            else:
                self.await_placed(tokens)
            failed = [t for t in tokens if self.ledger[t].state == W.FAILED]
            refused = failed + [t for t in tokens if self.ledger[t].state == W.REFUSED]
            for t in failed:
                self.finish(t)
            self.scan(f"wave {self.wave}")
            if refused:
                # The host refused a start, whatever the derived limit says: the fill stops there.
                self.rec.write("fill_stopped", wave=self.wave, refused=refused)
                break
            last = n
        self.phase("fill", "end", live=len(self.live()), limit=self.limit, cost=dict(self.cost))

    def ask_wave(self, n: int) -> List[str]:
        """`n` asks at once, the lanes served between their spawns (`Lanes.poll`): an echo that
        came while the next VM was being asked is read as that spawn returns, so a start is
        charged at most the one spawn under way as it came (each ask's `spawn_ns`), never the
        rest of its wave's. A refused ask ends the wave."""
        tokens: List[str] = []
        for _ in range(n):
            t = self.ask()
            tokens.append(t)
            self.lanes.poll()
            if self.ledger[t].state == W.REFUSED:
                break
        return tokens

    def ops(self, counts: Dict[str, int], make: Callable[[int], bytes], kind: str, units: float = 1, **tags: object) -> List[float]:
        """`counts[lane]` operations on each lane, one outstanding per lane, every lane at once;
        the VMs' (not the floor's) rates in bytes a nanosecond. Ended unfinished (a signal, the
        run's deadline), it starts nothing more: what is in flight still lands as its sample."""
        rates: List[float] = []
        left = dict(counts)
        seq = [0]
        finished = [0]

        def next_on(t: str) -> None:
            if left.get(t, 0) <= 0:
                return
            left[t] -= 1
            seq[0] += 1
            out = make(seq[0])

            def done(ok: bool, took: int, why: str, t: str = t, size: int = len(out)) -> None:
                finished[0] += 1
                if ok:
                    self.waits[kind].answered(took, units)
                    self.rec.write(kind, token=t, ns=took, bytes=size, floor=(t == "floor"), vms=len(self.live()), **tags)
                    if t != "floor" and took > 0:
                        rates.append(size / took)
                    next_on(t)
                else:
                    left[t] = 0
                    self.rec.write(kind + "_failed", token=t, ns=took, bytes=size, why=why)

            if not self.lanes.begin(t, out, done):
                left[t] = 0

        for t in list(counts):
            next_on(t)
        if not self.serve_ops(lambda: all(v <= 0 for v in left.values()) and self.lanes.busy() == 0, lambda: finished[0], kind, units):
            for t in left:
                left[t] = 0
        return rates

    def counts(self, live: List[str]) -> Dict[str, int]:
        k = derive.per_vm(len(live))
        out = {t: k for t in live}
        if self.floor is not None:
            out["floor"] = derive.per_vm(1)
        return out

    def hold(self, why: str) -> None:
        """At density: round trips, a recorded turn's stream, then transfers doubling in size until
        the VMs' median rate stops rising (`derive.median`); every lane at once in each."""
        self.phase("hold", "begin", why=why)
        if not self.live():
            self.phase("hold", "end", live=0)
            return
        if not self.driver.lanes:
            # Round trips and streams need a lane into each VM's workload, which this placer
            # doesn't give yet (Hex: member exec, Shards' step 11): owed, never measured here.
            self.rec.write("hold_owed", hold=why, why=f"the {self.driver.name} driver gives no lane into a VM", live=len(self.live()))
            self.phase("hold", "end", live=len(self.live()), owed=True)
            return
        self.ops(self.counts(self.live()), lane.control_message, "rtt", hold=why)
        self.scan(f"after round trips ({why})")
        self.ops(self.counts(self.live()), lambda _s: lane.pattern(derive.TURN_BYTES), "turn", hold=why)
        self.scan(f"after turns ({why})")
        size, best, prev = derive.FIRST_CHUNK, None, None
        while self.live():
            self.check_stop()
            rates = self.ops(self.counts(self.live()), lambda _s, n=size: lane.pattern(n), "bulk", size, hold=why, size=size)
            med = derive.median(rates)
            self.rec.write("bulk_round", hold=why, bytes=size, median_bytes_per_ns=med, n=len(rates))
            if med is None or (prev is not None and med <= prev):
                break
            best, prev = size, med
            # The harness holds every size's pattern it has written (a geometric sum under twice
            # the largest) and the next, twice this one: four times this size, from the room the
            # VMs leave.
            spare = (self.room["mem"] or 0) - sum(int(p.get("mem") or 0) for p in self.scope_procs())
            if size * 4 > spare:
                self.rec.write("bulk_bounded", bytes=size, spare=spare)
                break
            size *= 2
        self.rec.write("bulk_size", hold=why, bytes=best)
        self.scan(f"after transfers ({why})")
        self.phase("hold", "end", live=len(self.live()), bulk_bytes=best)

    def gone(self, pids: List[int]) -> Callable[[], bool]:
        want = set(pids)
        return lambda: not any(int(p["pid"]) in want for p in self.scope_procs())

    def chaos_round(self, action: str, round_no: int) -> None:
        self.check_stop()
        if not self.driver.lanes:
            self.chaos_placer(action, round_no)
            return
        began = time.monotonic_ns()
        before = self.scan(f"before chaos {action} {round_no}")
        scope_pids = {int(p["pid"]): str(p["start"]) for p in before["procs"]}
        view_before: Dict[str, List[int]] = before["view"]
        result: Dict[str, object] = {"action": action, "round": round_no}
        if action == "vm":
            token = chaos.pick(self.rng, [t for t in self.live() if view_before.get(t)])
            if token is None:
                self.rec.write("chaos", skipped="no live VM", **result)
                return
            pid = view_before[token][0]
            nets = [int(p["pid"]) for p in before["procs"] if p.get("role") == "net" and p.get("vm") == pid]
            self.ledger[token].move(W.KILLED)
            result.update(token=token, pid=pid, kill=chaos.kill_in_scope(pid, scope_pids.get(pid, ""), scope_pids))
            self.serve(lambda: self.vms[token].lane_ended is not None, "chaos.vm.lane")
            result["gone"] = self.settle(self.gone([pid] + nets), "chaos.vm.gone")
            self.finish(token)
            view, _ = self.driver.view()
            if view.get(token):
                result["left_listed"] = view[token]
                self.driver.remove([token])
        elif action == "client":
            token = chaos.pick(self.rng, self.live())
            if token is None:
                self.rec.write("chaos", skipped="no live VM", **result)
                return
            v = self.vms[token]
            result.update(token=token, client_pid=v.popen.pid, kill=procs.kill(v.popen.pid, v.client_start))
            self.serve(lambda: v.lane_ended is not None, "chaos.client.lane")
            view, _ = self.driver.view()
            vm_pids = list(view.get(token, []))
            result["run_after_client"] = "running" if vm_pids else "gone"
            self.ledger[token].move(W.STOPPING)
            code, said = self.driver.remove([token])
            result["removed"] = {"code": code, "said": said[-200:]}
            nets = [int(p["pid"]) for p in before["procs"] if p.get("role") == "net" and p.get("vm") in vm_pids]
            result["gone"] = self.settle(self.gone(vm_pids + nets), "chaos.client.gone")
            self.finish(token)
        elif action == "daemon":
            d = self.driver.daemon
            if d is None:
                self.rec.write("chaos", skipped="no daemon", **result)
                return
            result.update(pid=d.pid, kill=procs.kill(d.pid, self.driver.daemon_start))
            try:
                d.wait(timeout=None)
            except subprocess.SubprocessError:
                pass
            live_before = self.live()
            restarted = time.monotonic_ns()
            ok, why = self.driver.start(os.path.join(self.logs, "daemon.log"), self.deadline_ns)
            result["restarted"] = ok
            if not ok:
                self.failed("daemon restart", why)
                self.stopping = True
                raise Stop()
            self.waits["daemon"].answered(time.monotonic_ns() - restarted)
            self.rec.write("daemon", restarted=True, **self.driver.placer())

            def resolved() -> bool:
                view, _ = self.driver.view()
                return all(self.vms[t].lane_ended is not None or view.get(t) for t in live_before)

            result["settled"] = self.settle(resolved, "chaos.daemon.settle")
            view, _ = self.driver.view()
            ended = [t for t in live_before if self.vms[t].lane_ended is not None]
            survived = [t for t in live_before if t not in ended and view.get(t)]
            for t in ended:
                self.ledger[t].move(W.KILLED)
                self.finish(t)
            result.update(ended=len(ended), survived=len(survived), unresolved=sorted(set(live_before) - set(ended) - set(survived)))
        result["settle_ns"] = time.monotonic_ns() - began
        self.rec.write("chaos", **result)
        self.scan(f"after chaos {action} {round_no}")
        self.fill(f"after chaos {action} {round_no}")

    def chaos_placer(self, action: str, round_no: int) -> None:
        """Chaos on a placer that gives no lane: one VM's run host (`vm`), or the placer's own node or
        scheduler, killed by its pid and start (a pid the OS gave another process refused), the
        placer started again where it was one of its own. The placer names no VM's ask, so the asks
        a kill ended are those it then says ended (`killed`); one it still holds with no VM is
        `lost`, by count, at the next quiescent scan."""
        began = time.monotonic_ns()
        before = self.scan(f"before chaos {action} {round_no}")
        scope_pids = {int(p["pid"]): str(p["start"]) for p in before["procs"]}
        result: Dict[str, object] = {"action": action, "round": round_no}
        live_before = self.live()
        if action == "vm":
            hosts = {str(p["pid"]): p for p in before["procs"] if p.get("role") == "vm"}
            pick = chaos.pick(self.rng, sorted(hosts))
            if pick is None:
                self.rec.write("chaos", skipped="no VM", **result)
                return
            host = hosts[pick]
            pid = int(host["pid"])
            result.update(pid=pid, kill=chaos.kill_in_scope(pid, scope_pids.get(pid, ""), scope_pids))
            result["gone"] = self.settle(self.gone(list(host.get("tree") or [pid])), "chaos.vm.gone")
        else:
            result.update(self.driver.placer().get(action, {}), kill=self.driver.kill(action))
            restarted = time.monotonic_ns()
            ok, why = self.driver.start_one(action, os.path.join(self.logs, "daemon.log"), self.deadline_ns)
            result["restarted"] = ok
            if not ok:
                self.failed(f"{action} restart", why)
                self.stopping = True
                raise Stop()
            self.waits["daemon"].answered(time.monotonic_ns() - restarted)
            self.rec.write("daemon", restarted=action, **self.driver.placer())

            def resolved() -> bool:
                view, _ = self.driver.view()
                return all(t in view or t in self.driver.ended for t in live_before)

            result["settled"] = self.settle(resolved, f"chaos.{action}.settle")
        self.driver.view()
        ended = [t for t in live_before if t in self.driver.ended]
        for t in ended:
            self.ledger[t].move(W.KILLED)
            self.finish(t)
        if ended:
            self.driver.remove(ended)
        result.update(ended=len(ended), survived=len(live_before) - len(ended), settle_ns=time.monotonic_ns() - began)
        self.rec.write("chaos", **result)
        self.scan(f"after chaos {action} {round_no}")
        self.fill(f"after chaos {action} {round_no}")

    def drain(self) -> None:
        if not self.driver.lanes:
            self.drain_placer()
            return
        self.phase("drain", "begin")
        for t in self.live() + self.starting():
            self.ledger[t].move(W.STOPPING)
            self.lanes.close(t)
        stopping = sorted(t for t, a in self.ledger.items() if a.state == W.STOPPING)
        self.serve(lambda: all(self.vms[t].lane_ended is not None for t in stopping), "drain.lanes")
        for t in stopping:
            if self.vms[t].lane_ended is not None:
                self.finish(t)
        view, _ = self.driver.view()
        left = sorted(view)
        if left:
            self.rec.write("drain_left", tokens=left)
            self.driver.remove(left)
        self.settle(lambda: not any(p["role"] in ("vm", "net") for p in self.scope_procs()), "drain.gone")
        for t, a in sorted(self.ledger.items()):
            if a.state not in (W.ENDED, W.REFUSED):
                if a.state in (W.ASKED,):
                    a.move(W.REFUSED)
                    continue
                self.finish(t)
        self.phase("drain", "end")

    def drain_placer(self) -> None:
        """Every ask's box removed (the placer's own remove), and anything it still holds; then the
        scope awaited until no VM is left in it."""
        self.phase("drain", "begin")
        stopping = sorted(t for t, a in self.ledger.items() if a.state in (W.LIVE, W.STARTING))
        for t in stopping:
            self.ledger[t].move(W.STOPPING)
        # Every ask, refused ones too: a placer that queued one may have started it since.
        code, said = self.driver.remove(sorted(self.ledger))
        view, _ = self.driver.view()
        if view:
            self.rec.write("drain_left", tokens=sorted(view), removed={"code": code, "said": said[-200:]})
        self.settle(lambda: not any(p["role"] == "vm" for p in self.scope_procs()) and (self.driver.scans or not self.driver.view()[0]), "drain.gone")
        for t, a in sorted(self.ledger.items()):
            if a.state not in (W.ENDED, W.REFUSED):
                if a.state == W.ASKED:
                    a.move(W.REFUSED)
                    continue
                self.finish(t)
        self.phase("drain", "end")

    def states(self) -> Dict[str, int]:
        out: Dict[str, int] = {}
        for a in self.ledger.values():
            out[a.state] = out.get(a.state, 0) + 1
        return out

    def verify(self) -> None:
        self.phase("verify", "begin")
        final = self.scan("verify")
        stray = [p for p in final["procs"] if p["role"] in ("vm", "net")]
        for p in stray:
            self.found["stray"] = self.found.get("stray", 0) + 1
        self.rec.write("verdict", at="before teardown", stray=stray, listed=final["view"], witnesses=dict(self.found), asks=len(self.ledger), states=self.states())
        self.phase("verify", "end")

    def teardown(self) -> None:
        self.phase("teardown", "begin")
        for v in self.vms.values():
            if v.popen.poll() is None:
                self.rec.write("teardown_kill", token=v.token, client_pid=v.popen.pid, kill=procs.kill(v.popen.pid, v.client_start))
        if self.floor is not None and self.floor.poll() is None:
            self.floor.kill()
            self.floor.wait()
        code = self.driver.stop()
        left = [p.brief() for p in procs.table().values() if any(self.driver.mark in a for a in p.args)]
        if left:
            self.found["left_after_teardown"] = self.found.get("left_after_teardown", 0) + len(left)
        self.rec.write("verdict", at="after teardown", daemon_exit=code, left=left, witnesses=dict(self.found), states=self.states())
        if not self.o.keep_home and os.path.isdir(self.scope):
            shutil.rmtree(self.scope, ignore_errors=True)
        self.phase("teardown", "end")

    def run(self) -> int:
        def on_signal(_signum, _frame) -> None:
            self.stopping = True

        for s in (signal.SIGTERM, signal.SIGINT, getattr(signal, "SIGHUP", None)):
            if s is not None:
                signal.signal(s, on_signal)
        actions = chaos.parse(self.o.chaos, self.driver.actions)
        try:
            if self.setup():
                self.fill("initial")
                self.hold("at density")
                for r in range(1, self.o.rounds + 1):
                    for action in actions:
                        self.chaos_round(action, r)
                if actions and self.o.rounds > 0:
                    self.hold("after chaos")
        except Stop:
            self.rec.write("stopped", why="a signal or a failed restart")
        except Exception as e:  # the harness's own defect: recorded, then everything cleaned up
            self.failed("agent", f"{type(e).__name__}: {e}")
        finally:
            self.stopping = False
            try:
                self.drain()
                self.verify()
            except Exception as e:
                self.failed("drain", f"{type(e).__name__}: {e}")
            self.teardown()
        return 0 if not self.found and not self.failures else 3
