#!/usr/bin/env python3
"""Guard-removal mutations (Shards R10): each guard the harness keeps is removed in a copy of
fleetlib, and the light tests are run against the copy; every mutation must fail them.

Run: python3 bench/fleet/tests/mutate.py   (prints each mutation and whether the tests caught it;
exits 1 if any survived)
"""

import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

# (file, the guard as written, what it becomes with the guard removed, what it guards)
MUTATIONS = [
    ("witness.py", 'if room.get("mem") is not None and held > int(room["mem"]):', "if False:", "over_room"),
    ("witness.py", "if limit is not None and len(vms) > limit:", "if False:", "over_limit"),
    ("witness.py", "if len(set(pids)) > 1:", "if False:", "doubled within a token"),
    ("witness.py", "if pid in owner and owner[pid] != token:", "if False:", "doubled across tokens"),
    ("witness.py", "if not quiescent:\n        return out", "if False:\n        return out", "quiescence gating"),
    ("witness.py", "if ask.state in HOLDS_VM and not any(pid in vm_pids for pid in view.get(token, [])):", "if False:", "lost"),
    ("witness.py", "if ask is None or ask.state not in HOLDS_VM:\n            out.append(violation(\"leaked\"", "if False:\n            out.append(violation(\"leaked\"", "leaked"),
    ("witness.py", "if p.get(\"role\") == \"net\" and p.get(\"vm\") is not None and int(p[\"vm\"]) not in vm_pids:", "if False:", "orphan"),
    ("witness.py", "if token in seen and seen[token] != host:", "if False:", "doubled across hosts"),
    ("witness.py", "if to not in MOVES.get(self.state, set()):", "if False:", "the ask's state machine"),
    ("chaos.py", "if scope_pids.get(pid) != start:", "if False:", "chaos scope"),
    ("procs.py", "if start and now.start != start:", "if False:", "pid reuse"),
    ("lane.py", "if chunk != op.out[op.got : op.got + len(chunk)]:", "if False:", "lane corruption"),
    ("derive.py", "self._slowest = max(self._slowest, max(1, took_ns) / units)", "self._slowest = max(1, took_ns) / units", "waits only rise"),
    ("derive.py", "self._slowest = max(self._slowest, max(1, took_ns) / units)", "self._slowest = max(self._slowest, max(1, took_ns))", "a wait is kept per unit"),
    ("derive.py", "limits.append(max(0, have // per))", "limits.append(max(0, have // per) + 1)", "density limit"),
    ("stats.py", "return max(1, math.ceil(p * n))", "return max(1, round(p * n))", "nearest rank"),
    ("agent.py", "                last_t = now\n        return True", "                return False\n        return True", "a stall never cuts a batch short"),
    ("agent.py", "            if first:\n", "            if False:\n", "a lag settles before it is judged"),
    ("shards.py", "if p is None or p.zombie or", "if p is None or", "zombies are no VMs"),
    ("shards.py", "        for pid in under | named:\n            p = table.get(pid)", "        self.macs = {}\n        for pid in under | named:\n            p = table.get(pid)", "a network process is the run's while it lives"),
    ("agent.py", "            self.lanes.poll()\n", "", "a wave's echoes are read between its spawns"),
    ("agent.py", "        for f in (v.popen.stdin, v.popen.stdout):", "        for f in ():", "an ended ask's descriptors close"),
    ("agent.py", "        except OSError as e:\n            # The host gave the harness no client", "        except ValueError as e:\n            # The host gave the harness no client", "a refused spawn is typed"),
    ("shards.py", "rec[\"vm\"] = self.macs.get(_flag(p.args, \"--mac\") or \"\", 0)", "rec[\"vm\"] = self.macs.get(_flag(p.args, \"--mac\") or \"\")", "a network process with no VM ever is an orphan"),
]


def main() -> int:
    survived = 0
    for name, guard, removed, what in MUTATIONS:
        with tempfile.TemporaryDirectory() as d:
            shutil.copytree(os.path.join(ROOT, "fleetlib"), os.path.join(d, "fleetlib"))
            shutil.copytree(HERE, os.path.join(d, "tests"))
            path = os.path.join(d, "fleetlib", name)
            with open(path, encoding="utf-8") as f:
                text = f.read()
            if text.count(guard) != 1:
                print(f"BROKEN  {what}: the guard isn't in {name} once ({text.count(guard)} times)")
                survived += 1
                continue
            with open(path, "w", encoding="utf-8") as f:
                f.write(text.replace(guard, removed))
            r = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", os.path.join(d, "tests")], cwd=d, capture_output=True, text=True, check=False)
            caught = r.returncode != 0
            print(f"{'caught ' if caught else 'SURVIVED'} {what} ({name})")
            survived += 0 if caught else 1
    print(f"{len(MUTATIONS) - survived} of {len(MUTATIONS)} mutations caught")
    return 1 if survived else 0


if __name__ == "__main__":
    sys.exit(main())
