"""Light tests of the fleet harness (plan step 36): its statistics, derivations, witnesses, chaos
scope, lanes, process reader, scan and report, each read as "do X, expect Y". No microVM is started
here; the live campaign is ROOT's (README).

Run: python3 -m unittest discover -s bench/fleet/tests -v
"""

import json
import os
import random
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

from fleetlib import chaos, derive, lane, procs, records, report, stats, witness  # noqa: E402
from fleetlib.agent import needs_room  # noqa: E402
from fleetlib.shards import Shards  # noqa: E402

W = witness


class Stats(unittest.TestCase):
    def test_nearest_rank_reports_measured_samples(self):
        s = stats.summary(range(1, 101))
        self.assertEqual((s["n"], s["p50"], s["p90"], s["p99"], s["max"]), (100, 50, 90, 99, 100))
        self.assertFalse(s["p99_is_max"])

    def test_p99_under_100_samples_is_said_to_be_the_max(self):
        s = stats.summary([3, 1, 2])
        self.assertEqual(s["p99"], 3)
        self.assertTrue(s["p99_is_max"])

    def test_least_n_is_the_fewest_samples_whose_p99_is_not_the_max(self):
        self.assertEqual(stats.least_n(0.99), 100)
        self.assertEqual(stats.rank(0.99, 100), 99)
        self.assertEqual(stats.rank(0.99, 99), 99)
        self.assertIsNone(stats.summary([]))


class Derive(unittest.TestCase):
    def test_density_limit_is_the_tightest_dimension(self):
        self.assertEqual(derive.density_limit({"mem": 1000, "cpu": 8000}, {"mem": 300, "cpu": 1000}), 3)
        self.assertEqual(derive.density_limit({"mem": 1000, "cpu": None}, {"mem": 300, "cpu": 1000}), 3)
        self.assertIsNone(derive.density_limit({"mem": 1000}, {}))
        self.assertIsNone(derive.density_limit({"mem": 1000}, {"mem": 0}))

    def test_waves_double_from_one_to_the_room_left(self):
        waves, last, left = [], 0, 20
        while True:
            n = derive.next_wave(last, left)
            if n == 0:
                break
            waves.append(n)
            left -= n
            last = n
        self.assertEqual(waves, [1, 2, 4, 8, 5])

    def test_a_wait_is_seeded_then_learned_and_only_rises(self):
        first = derive.Patience("first_run")
        start = derive.Patience("start", first)
        self.assertIsNone(start.bound_ns())
        first.answered(1000)
        self.assertEqual(start.bound_ns(), 2000)
        start.answered(300)
        self.assertEqual(start.bound_ns(), 600)
        start.answered(100)
        self.assertEqual(start.bound_ns(), 600)
        start.ran_out(600)
        self.assertEqual(start.bound_ns(), 1200)

    def test_a_wait_kept_per_byte_scales_with_a_rounds_bytes(self):
        turn = derive.Patience("turn")
        bulk = derive.Patience("bulk", turn, 1 / 100)
        turn.answered(1000)
        # Seeded from the turn: 1000 ns for its 100 bytes, 10 ns a byte, twice over.
        self.assertEqual(bulk.bound_ns(200), 4000)
        bulk.answered(3000, 100)
        self.assertEqual(bulk.bound_ns(200), 12000)
        # A round of twice the bytes at the same rate fits its doubled bound.
        self.assertLessEqual(2 * 3000, bulk.bound_ns(200))

    def test_the_median_is_a_measured_sample(self):
        self.assertIsNone(derive.median([]))
        self.assertEqual(derive.median([3.0, 1.0, 2.0]), 2.0)
        self.assertEqual(derive.median([4.0, 1.0, 3.0, 2.0]), 2.0)

    def test_a_turns_stream_is_the_recorded_turns_size(self):
        self.assertEqual(derive.TURN_BYTES, 60783)

    def test_per_vm_gives_each_host_100_samples(self):
        for vms in (1, 3, 33, 100, 250):
            self.assertGreaterEqual(derive.per_vm(vms) * vms, 100)
            self.assertLess((derive.per_vm(vms) - 1) * vms, 100)

    def test_needs_room_reads_kiln_needs(self):
        self.assertEqual(needs_room("linux/amd64,mem=16g,cpus=8"), {"mem": 16 << 30, "cpu": 8000})
        self.assertEqual(needs_room("mem=512m"), {"mem": 512 << 20, "cpu": None})
        self.assertEqual(needs_room(""), {"mem": None, "cpu": None})
        self.assertEqual(needs_room("mem=lots"), {"mem": None, "cpu": None})


def live_ledger(*tokens):
    out = {}
    for t in tokens:
        a = W.Ask(t)
        a.move(W.STARTING)
        a.move(W.LIVE)
        out[t] = a
    return out


class Witnesses(unittest.TestCase):
    room = {"mem": 10_000}

    def clean(self):
        ledger = live_ledger("a", "b")
        scan = [
            {"pid": 10, "role": "vm", "mem": 1000},
            {"pid": 11, "role": "net", "mem": 100, "vm": 10},
            {"pid": 20, "role": "vm", "mem": 1000},
            {"pid": 21, "role": "net", "mem": 100, "vm": 20},
        ]
        view = {"a": [10], "b": [20]}
        return ledger, scan, view

    def kinds(self, found):
        return sorted(v["witness"] for v in found)

    def test_a_clean_scope_has_no_violation(self):
        ledger, scan, view = self.clean()
        self.assertEqual(W.check(ledger, scan, view, self.room, 4, True), [])

    def test_over_room_is_seen(self):
        ledger, scan, view = self.clean()
        self.assertEqual(self.kinds(W.check(ledger, scan, view, {"mem": 2199}, None, True)), ["over_room"])

    def test_over_limit_is_seen(self):
        ledger, scan, view = self.clean()
        self.assertEqual(self.kinds(W.check(ledger, scan, view, self.room, 1, True)), ["over_limit"])

    def test_doubled_is_seen_in_the_view_and_across_tokens(self):
        ledger, scan, view = self.clean()
        scan.append({"pid": 30, "role": "vm", "mem": 1})
        self.assertIn("doubled", self.kinds(W.check(ledger, scan, {"a": [10, 30], "b": [20]}, self.room, None, False)))
        self.assertIn("doubled", self.kinds(W.check(ledger, scan, {"a": [10], "b": [10]}, self.room, None, False)))

    def test_lost_is_seen_only_at_quiescence(self):
        ledger, scan, view = self.clean()
        scan = [p for p in scan if p["pid"] not in (20, 21)]
        self.assertEqual(self.kinds(W.check(ledger, scan, view, self.room, None, True)), ["lost"])
        self.assertEqual(W.check(ledger, scan, view, self.room, None, False), [])

    def test_leaked_is_seen_for_a_vm_no_live_ask_holds(self):
        ledger, scan, view = self.clean()
        scan.append({"pid": 40, "role": "vm", "mem": 1})
        self.assertEqual(self.kinds(W.check(ledger, scan, view, self.room, None, True)), ["leaked"])
        ledger["b"].move(W.STOPPING)
        ledger["b"].move(W.ENDED)
        got = self.kinds(W.check(ledger, scan, view, self.room, None, True))
        self.assertEqual(got, ["leaked", "leaked", "unasked"])

    def test_orphan_is_a_network_process_without_its_vm(self):
        ledger, scan, view = self.clean()
        scan.append({"pid": 51, "role": "net", "mem": 1, "vm": 50})
        self.assertEqual(self.kinds(W.check(ledger, scan, view, self.room, None, True)), ["orphan"])

    def test_unasked_is_a_vm_for_an_ask_never_made(self):
        ledger, scan, view = self.clean()
        scan.append({"pid": 60, "role": "vm", "mem": 1})
        view["zz"] = [60]
        self.assertEqual(self.kinds(W.check(ledger, scan, view, self.room, None, True)), ["leaked", "unasked"])

    def test_doubled_across_hosts(self):
        self.assertEqual(W.across_hosts({"h1": ["x", "y"], "h2": ["z"]}), [])
        got = W.across_hosts({"h1": ["x"], "h2": ["x"]})
        self.assertEqual([v["witness"] for v in got], ["doubled"])

    def test_an_ask_moves_only_as_its_state_machine_allows(self):
        a = W.Ask("t")
        with self.assertRaises(W.IllegalMove):
            a.move(W.LIVE)
        a.move(W.STARTING)
        a.move(W.LIVE)
        a.move(W.KILLED)
        with self.assertRaises(W.IllegalMove):
            a.move(W.LIVE)
        a.move(W.ENDED)
        self.assertEqual(a.history, [W.ASKED, W.STARTING, W.LIVE, W.KILLED, W.ENDED])


@unittest.skipIf(sys.platform == "win32", "lanes select on pipes, which Windows can't (README, Owed)")
class Lanes(unittest.TestCase):
    def setUp(self):
        self.ps = []
        self.lanes = lane.Lanes()
        self.ended = {}

    def tearDown(self):
        for p in self.ps:
            if p.poll() is None:
                p.kill()
            p.wait()

    def spawn(self, name, argv):
        p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
        self.ps.append(p)
        self.lanes.add(lane.Stream(name, p.stdin.fileno(), p.stdout.fileno(), lambda why, n=name: self.ended.__setitem__(n, why)))
        return p

    def test_round_trips_and_transfers_come_back_whole_on_every_lane_at_once(self):
        for i in range(4):
            self.spawn(f"c{i}", ["cat"])
        got = []
        for i in range(4):
            self.assertTrue(self.lanes.begin(f"c{i}", lane.control_message(i), lambda ok, took, why: got.append((ok, took, why))))
        self.assertTrue(self.lanes.run(lambda: len(got) == 4, time.monotonic_ns() + 10_000_000_000))
        self.assertTrue(all(ok and took > 0 for ok, took, _ in got), got)
        big = []
        self.assertTrue(self.lanes.begin("c0", lane.pattern(4 << 20), lambda ok, took, why: big.append((ok, why))))
        self.assertTrue(self.lanes.run(lambda: bool(big), time.monotonic_ns() + 30_000_000_000))
        self.assertEqual(big, [(True, "")])

    def test_bytes_that_differ_from_what_was_written_are_corrupt(self):
        # A lane that changes what it carries, line by line, unbuffered.
        self.spawn("tr", [sys.executable, "-u", "-c", "import sys\nfor line in sys.stdin: sys.stdout.write(line.replace('a', 'b'))"])
        got = []
        self.lanes.begin("tr", b"aaaa\n", lambda ok, took, why: got.append((ok, why)))
        self.lanes.run(lambda: bool(got), time.monotonic_ns() + 10_000_000_000)
        self.assertFalse(got[0][0])
        self.assertIn("corrupt", got[0][1])

    def test_a_lane_whose_process_ends_says_so_as_an_event(self):
        p = self.spawn("c", ["cat"])
        self.lanes.close("c")
        self.lanes.run(lambda: "c" in self.ended, time.monotonic_ns() + 10_000_000_000)
        self.assertEqual(self.ended.get("c"), "ended")
        p.wait()

    def test_the_pattern_shows_a_dropped_word(self):
        p = lane.pattern(1 << 16)
        self.assertEqual(len(p), 1 << 16)
        self.assertNotEqual(p[4:8], p[8:12])
        self.assertNotEqual(p[:4096], p[4096:8192])


class Procs(unittest.TestCase):
    def test_the_table_holds_this_process_and_a_childs_tree(self):
        child = subprocess.Popen([sys.executable, "-c", "import subprocess,sys,time; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); time.sleep(30)"])
        try:
            deadline = time.monotonic() + 10
            while True:
                t = procs.table()
                under = procs.descendants(t, [child.pid])
                if len(under) >= 2 or time.monotonic() > deadline:
                    break
                time.sleep(0.05)
            self.assertIn(os.getpid(), t)
            self.assertEqual(under[0], child.pid)
            self.assertGreaterEqual(len(under), 2)
            me = t[os.getpid()]
            self.assertTrue(me.start)
            if sys.platform != "win32":
                self.assertIsNotNone(me.mem)
        finally:
            for pid in procs.descendants(procs.table(), [child.pid])[::-1]:
                try:
                    os.kill(pid, 9)
                except OSError:
                    pass
            child.wait()

    def test_kill_refuses_a_pid_whose_start_differs(self):
        p = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
        try:
            me = procs.one(p.pid)
            self.assertIsNotNone(me)
            self.assertEqual(procs.kill(p.pid, me.start + "0"), "reused")
            self.assertIsNone(p.poll())
            self.assertEqual(procs.kill(p.pid, me.start), "killed")
            p.wait()
            self.assertEqual(procs.kill(p.pid, me.start), "gone")
        finally:
            if p.poll() is None:
                p.kill()
                p.wait()


class Chaos(unittest.TestCase):
    def test_a_target_outside_the_scope_is_never_killed(self):
        p = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
        try:
            start = procs.one(p.pid).start
            self.assertEqual(chaos.kill_in_scope(p.pid, start, {}), "out_of_scope")
            self.assertEqual(chaos.kill_in_scope(p.pid, start, {p.pid: start + "x"}), "out_of_scope")
            self.assertIsNone(p.poll())
            self.assertEqual(chaos.kill_in_scope(p.pid, start, {p.pid: start}), "killed")
            p.wait()
        finally:
            if p.poll() is None:
                p.kill()
                p.wait()

    def test_targets_repeat_for_a_seed(self):
        a = [chaos.pick(random.Random(36), ["x", "y", "z", "w"]) for _ in range(3)]
        b = [chaos.pick(random.Random(36), ["w", "z", "y", "x"]) for _ in range(3)]
        self.assertEqual(a, b)
        self.assertIsNone(chaos.pick(random.Random(1), []))

    def test_actions_are_known_ones(self):
        self.assertEqual(chaos.parse("vm,daemon,client"), ["vm", "daemon", "client"])
        self.assertEqual(chaos.parse("none"), [])
        with self.assertRaises(ValueError):
            chaos.parse("vm,host")


class Scan(unittest.TestCase):
    def test_vm_and_network_processes_pair_by_mac_and_orphans_are_found_by_home(self):
        home_scope = "/tmp/fleet-x/s"
        sh = Shards.__new__(Shards)
        sh.home = home_scope + "/h"
        P = procs.Proc
        t = {
            100: P(100, 1, "shards", ["shards", "run", "daemon"], "1", 10, "pss", 0),
            101: P(101, 100, "shards", ["shards", "spawner"], "2", 10, "pss", 0),
            102: P(102, 101, "shards-net", ["shards-net", "--mac", "0a:01"], "3", 20, "pss", 0),
            103: P(103, 101, "shards-vm", ["shards-vm", "run", "--net", "5,6,7,0a:01", "--kernel", sh.home + "/guest/k"], "4", 300, "pss", 0),
            # A VM a dead daemon left behind, and its network process.
            200: P(200, 1, "shards-vm", ["shards-vm", "run", "--net", "5,6,7,0a:02", "--kernel", sh.home + "/guest/k"], "5", 300, "pss", 0),
            201: P(201, 1, "shards-net", ["shards-net", "--mac", "0a:02"], "6", 20, "pss", 0),
            # A VM process that exited, its parent yet to reap it: no VM.
            400: P(400, 101, "shards-vm", ["shards-vm", "run", "--net", "5,6,7,0a:04", "--kernel", sh.home + "/guest/k"], "9", None, "pss", 0, True),
            # Someone else's VM: never ours.
            300: P(300, 1, "shards-vm", ["shards-vm", "run", "--net", "5,6,7,0a:03", "--kernel", "/other/guest/k"], "7", 300, "pss", 0),
            301: P(301, 1, "shards-net", ["shards-net", "--mac", "0a:03"], "8", 20, "pss", 0),
        }
        got = {p["pid"]: p for p in sh.scan(t, [100])}
        self.assertEqual(sorted(got), [101, 102, 103, 200, 201])
        self.assertEqual(got[102]["vm"], 103)
        self.assertEqual(got[201]["vm"], 200)
        self.assertEqual((got[103]["role"], got[101]["role"]), ("vm", "shards"))


class Settle(unittest.TestCase):
    """The agent's scan judges a lagging effect again until it passes or outlasts its settle."""

    def agent(self, d, observations):
        from types import SimpleNamespace

        from fleetlib.agent import Agent

        o = SimpleNamespace(deadline_s=30, out=d, shards="/bin/true", seed=1, needs="mem=1g", label="h", image="i@sha256:00",
                            ballast=0, chaos="none", rounds=0, repo=d, rev="", controller_t=0, keep_home=False)
        rec = records.Records(os.path.join(d, "r.jsonl"), "h", "run")
        a = Agent(o, rec)
        a.waits["start"].answered(50_000_000)
        a.ledger = live_ledger("t1")
        seq = iter(observations)
        last = [None]

        def observe():
            last[0] = next(seq, last[0])
            return last[0]

        a.observe = observe
        a.sweep_ended = lambda: None
        return a, rec

    vm = {"pid": 10, "role": "vm", "mem": 100, "start": "1"}
    stray = {"pid": 11, "role": "vm", "mem": 100, "start": "2"}

    def kinds(self, path):
        return [r["kind"] for r in records.read_all(path)]

    def test_a_vm_the_view_catches_up_with_is_a_lag_not_a_leak(self):
        with tempfile.TemporaryDirectory() as d:
            lagging = ([self.vm, self.stray], {"t1": [10]}, "")
            caught_up = ([self.vm], {"t1": [10]}, "")
            a, rec = self.agent(d, [lagging, lagging, caught_up])
            a.scan("test")
            rec.close()
            kinds = self.kinds(os.path.join(d, "r.jsonl"))
            self.assertIn("view_lag", kinds)
            self.assertNotIn("witness", kinds)

    def test_a_vm_no_ask_ever_holds_is_leaked_once_it_outlasts_the_settle(self):
        with tempfile.TemporaryDirectory() as d:
            lagging = ([self.vm, self.stray], {"t1": [10]}, "")
            a, rec = self.agent(d, [lagging])
            a.scan("test")
            rec.close()
            got = records.read_all(os.path.join(d, "r.jsonl"))
            self.assertEqual([r["witness"] for r in got if r["kind"] == "witness"], ["leaked"])
            lag = [r for r in got if r["kind"] == "view_lag"]
            self.assertEqual((len(lag), lag[0]["resolved"]), (1, False))


def lane_agent(d, deadline_s, needs, workloads):
    """An agent whose live asks' lanes are local processes (`workloads`: argv by token)."""
    from types import SimpleNamespace

    from fleetlib.agent import Agent, Vm

    o = SimpleNamespace(deadline_s=deadline_s, out=d, shards="/bin/true", seed=1, needs=needs, label="h", image="i@sha256:00",
                        ballast=0, chaos="none", rounds=0, repo=d, rev="", controller_t=0, keep_home=False)
    rec = records.Records(os.path.join(d, "r.jsonl"), "h", "run")
    a = Agent(o, rec)
    a.ledger = live_ledger(*workloads)
    a.scan = lambda why: {}
    a.scope_procs = lambda: []
    ps = []
    for i, (t, argv) in enumerate(sorted(workloads.items())):
        p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
        ps.append(p)
        a.vms[t] = Vm(t, i, p, "", 0, 1)
        a.lanes.add(lane.Stream(t, p.stdin.fileno(), p.stdout.fileno(), lambda why: None, p.stdin.close))
    return a, rec, ps


@unittest.skipIf(sys.platform == "win32", "lanes select on pipes, which Windows can't (README, Owed)")
class Hold(unittest.TestCase):
    """The hold on live lanes: round trips, a turn's stream, then bulk rounds to a converged size."""

    def finish(self, ps):
        for p in ps:
            if p.poll() is None:
                p.kill()
            p.wait()

    def test_a_hold_takes_every_kind_of_sample_on_every_lane(self):
        with tempfile.TemporaryDirectory() as d:
            # A room of 1 MiB bounds the bulk rounds at a few hundred KiB, so the test stays light.
            a, rec, ps = lane_agent(d, 60, "mem=1m", {"t1": ["cat"], "t2": ["cat"]})
            a.waits["start"].answered(1_000_000_000)
            try:
                a.hold("test")
            finally:
                self.finish(ps)
            rec.close()
            got = records.read_all(os.path.join(d, "r.jsonl"))
            count = lambda kind: sum(1 for r in got if r["kind"] == kind)  # noqa: E731
            # A stall may be recorded on a loaded host (`ran_out`); no sample is ever dropped for it.
            self.assertEqual([r for r in got if r["kind"] == "failed"], [])
            self.assertEqual((count("rtt"), count("turn")), (100, 100))
            self.assertGreaterEqual(count("bulk_round"), 1)
            size = [r["bytes"] for r in got if r["kind"] == "bulk_size"]
            self.assertEqual(len(size), 1)
            self.assertEqual(sum(1 for r in got if r["kind"] == "bulk" and r["size"] == size[0]), 100)

    def test_a_stall_is_recorded_and_backed_off_but_never_cuts_the_batch_short(self):
        # The lane echoes its first message, then reads on and answers nothing.
        mute = [sys.executable, "-u", "-c", "import sys, time\nsys.stdout.write(sys.stdin.readline())\ntime.sleep(60)"]
        with tempfile.TemporaryDirectory() as d:
            a, rec, ps = lane_agent(d, 1.5, "mem=1g", {"m": mute})
            a.waits["start"].answered(10_000_000)
            began = time.monotonic_ns()
            try:
                a.ops({"m": 3}, lane.control_message, "rtt")
            finally:
                self.finish(ps)
            rec.close()
            got = records.read_all(os.path.join(d, "r.jsonl"))
            outs = [r for r in got if r["kind"] == "ran_out"]
            stalls = [r for r in outs if "at" not in r]
            self.assertEqual(sum(1 for r in got if r["kind"] == "rtt"), 1)
            self.assertGreaterEqual(len(stalls), 2)
            self.assertIn("m", stalls[0]["stalled"])
            # Backed off: each stall's wait at least doubles the one before.
            self.assertTrue(all(b["after_ns"] >= 2 * x["after_ns"] * 0.99 for x, b in zip(stalls, stalls[1:])), [s["after_ns"] for s in stalls])
            # Only the run's deadline ended it.
            self.assertEqual([r.get("at") for r in outs if "at" in r], ["the run's deadline"])
            self.assertGreaterEqual(time.monotonic_ns(), a.deadline_ns)
            self.assertGreater(a.deadline_ns, began)


class Report(unittest.TestCase):
    def records(self, extra=()):
        recs = [
            {"kind": "host", "host": "h1", "facts": {"hostname": "h1", "os": "Linux", "cpus": 4}, "driver": "shards"},
            {"kind": "started", "host": "h1", "start_ns": 2_000_000, "t": 1},
            {"kind": "started", "host": "h1", "start_ns": 4_000_000, "t": 2},
            {"kind": "rtt", "host": "h1", "ns": 50_000, "bytes": 83, "floor": False},
            {"kind": "rtt", "host": "h1", "ns": 5_000, "bytes": 83, "floor": True},
            {"kind": "bulk", "host": "h1", "ns": 1_000_000_000, "bytes": 1 << 20, "floor": False, "hold": "at density", "size": 1 << 20},
            {"kind": "bulk", "host": "h1", "ns": 1_000_000_000, "bytes": 2 << 20, "floor": False, "hold": "at density", "size": 2 << 20},
            {"kind": "bulk_size", "host": "h1", "hold": "at density", "bytes": 1 << 20},
            {"kind": "turn", "host": "h1", "ns": 1_000_000, "bytes": 60783, "floor": False},
            {"kind": "scan", "host": "h1", "why": "after round trips (at density)", "vms": 2, "per_vm_mem": [100 << 20, 120 << 20], "limit": 2, "cost": {"mem": 120 << 20}},
            {"kind": "verdict", "host": "h1", "at": "after teardown", "left": []},
        ]
        return recs + list(extra)

    def test_a_clean_run_passes_and_says_its_numbers(self):
        text = report.render(self.records())
        self.assertIn("verdict: PASS", text)
        self.assertIn("start (ask to first echo): n=2 p50=2.000", text)
        self.assertIn("control round trip: n=1 p50=50.000", text)
        self.assertIn("bulk at the converged [1048576] B: n=1 p50=1.000 p90=1.000 p99=1.000 max=1.000 MiB/s", text)
        self.assertIn("a turn's 60783 B: n=1 p50=57.967", text)
        self.assertIn("limit 2 VMs", text)

    def test_any_witness_or_leftover_fails_it(self):
        text = report.render(self.records([{"kind": "witness", "host": "h1", "witness": "leaked", "detail": {}}]))
        self.assertIn("FAIL: witnesses {'leaked': 1}", text)
        text = report.render(self.records([{"kind": "verdict", "host": "h1", "at": "after teardown", "left": [{"pid": 1}]}]))
        self.assertIn("1 processes left after teardown", text)

    def test_records_round_trip_through_a_file(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "r.jsonl")
            rec = records.Records(path, "h1", "run1")
            rec.write("rtt", ns=5)
            rec.close()
            with open(path, "a") as f:
                f.write("not json\n")
            got = records.read_all(path)
            self.assertEqual([r["kind"] for r in got], ["rtt", "unparsed"])
            self.assertEqual((got[0]["host"], got[0]["run"], got[0]["ns"]), ("h1", "run1", 5))
            json.dumps(got)


if __name__ == "__main__":
    unittest.main()
