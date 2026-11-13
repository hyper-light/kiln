"""The hex driver: microVMs placed by a Hex scheduler of the run's own, on this host (Hex's laptop
deployment: one node, its messages drained locally by the same driver that serves peers; Hex's
docs/scheduler-live.md). Peers across hosts link over SSH with a forced command per peer, an
administrator's setup (README, The hex driver).

The agent starts `hex node --socket N --state D` and `hex schedule serve --config C` as its children,
each serving once it says `ready <socket>` on stdout (an event). An ask is a `Submit` of a
`ShardsImage` box (Hex's spec: the image pinned by digest, the run's command) through
`hex schedule call`, sized as the run asks (`--vm`: the VM Hex admits is the VM, never Shards' own
sizing, Hex's docs/design/shards-image-class.md §1); its key is kept, `Status` follows it until Hex
says it was created or ended, and `Delete` removes it. Hex refuses a create typed, in its own words,
where its image class can't boot one (`Awaits(...)`: macOS's and Windows' run hosts, its waits'
records outside the census): the refusal is recorded as said.

The view is Hex's records: the boxes it holds created and not ended. A box's run host (`shards
api-run` under the node) names no box, and Hex's records name no process, so the view names none
(witness.check judges such asks by count). The scan is the OS's: the node's and the scheduler's
descendants and every process whose arguments name the scope; each run host is one VM, its memory its
whole tree's (the run host's own, its VM's and its network process's). The node's guest comes from
the environment the agent is given (Hex's GuestConfig::from_env: HEX_GUEST_KERNEL, ...), with
SHARDS_BIN the binary under test and SHARDS_HOME in the scope, where the image is pulled.
"""

from __future__ import annotations

import json
import os
import queue
import subprocess
import threading
import time
from typing import Dict, List, Optional, Set, Tuple

from . import procs

ROLES = ("node", "scheduler")


class Hex:
    lanes = False
    actions = ("vm", "node", "scheduler")
    name = "hex"

    def __init__(self, binary: str, shards: str, scope: str, vm: Dict[str, Optional[int]], platform: str) -> None:
        self.binary = binary
        self.shards_binary = shards
        self.scope = scope
        self.home = os.path.join(scope, "h")
        self.vm = vm
        self.platform = platform
        self.sockets = {"node": os.path.join(scope, "n.sock"), "scheduler": os.path.join(scope, "x.sock")}
        self.config = os.path.join(scope, "scheduler.json")
        self.env = dict(os.environ)
        for drop in ("DOCKER_AUTH_CONFIG", "DOCKER_HOST", "SHARDS_HOST"):
            self.env.pop(drop, None)
        self.env.update({
            "SHARDS_BIN": shards, "SHARDS_HOME": self.home, "DOCKER_CONFIG": os.path.join(scope, "docker-config"),
            "SHARDS_LOCAL_STORE": "none", "XDG_RUNTIME_DIR": os.path.join(scope, "run"),
        })
        # What a process of the run's names in its arguments (teardown's last scan).
        self.mark = scope
        self.procs: Dict[str, subprocess.Popen] = {}
        self.starts: Dict[str, str] = {}
        self.keys: Dict[str, dict] = {}
        # Asks whose boxes Hex says ended: an ended box stays ended, so it is asked of no more.
        self.ended: Set[str] = set()
        self.calls = 0

    # ---------------------------------------------------------------- the node and the scheduler

    def argv(self, role: str) -> List[str]:
        if role == "node":
            return [self.binary, "node", "--socket", self.sockets["node"], "--state", os.path.join(self.scope, "node")]
        return [self.binary, "schedule", "serve", "--config", self.config]

    def start(self, log_path: str, deadline_ns: Optional[int]) -> Tuple[bool, str]:
        for d in ("node", "scheduler", "run", "docker-config", "calls"):
            os.makedirs(os.path.join(self.scope, d), exist_ok=True)
        with open(self.config, "w", encoding="utf-8") as f:
            json.dump({
                "cluster": f"fleet-{os.path.basename(os.path.dirname(self.scope))}",
                "state": os.path.join(self.scope, "scheduler"),
                "socket": self.sockets["scheduler"],
                "backends": [{"class": "shards", "socket": self.sockets["node"]}],
            }, f)
        for role in ROLES:
            ok, why = self.start_one(role, log_path, deadline_ns)
            if not ok:
                return False, why
        return True, ""

    def start_one(self, role: str, log_path: str, deadline_ns: Optional[int]) -> Tuple[bool, str]:
        """Starts `role` as this process's child and waits for its `ready` line (an event: its
        stdout read on a thread of its own, so the run's deadline bounds the wait)."""
        with open(log_path, "ab") as log:
            p = subprocess.Popen(self.argv(role), env=self.env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=log)
        self.procs[role] = p
        lines: "queue.SimpleQueue[Optional[str]]" = queue.SimpleQueue()

        def read() -> None:
            out = p.stdout
            if out is None:
                lines.put(None)
                return
            for raw in out:
                lines.put(raw.decode("utf-8", "replace").strip())
            lines.put(None)

        threading.Thread(target=read, name=f"hex-{role}-stdout", daemon=True).start()
        while True:
            left = None if deadline_ns is None else (deadline_ns - time.monotonic_ns()) / 1e9
            if left is not None and left <= 0:
                return False, f"hex {role} said no ready within the run's deadline"
            try:
                line = lines.get(timeout=left)
            except queue.Empty:
                continue
            if line is None:
                return False, f"hex {role} ended at once ({p.wait()}): see its log"
            if line.startswith("ready "):
                found = procs.one(p.pid)
                self.starts[role] = found.start if found else ""
                return True, ""

    def stop(self) -> Optional[int]:
        """Ends the scheduler, then the node (SIGTERM: the node leaves its boxes for the next node's
        recovery, Hex's node.rs); the node's exit."""
        code = None
        for role in ("scheduler", "node"):
            p = self.procs.get(role)
            if p is None:
                continue
            if p.poll() is None:
                p.terminate()
            code = p.wait()
        return code

    def roots(self) -> List[int]:
        return [p.pid for p in self.procs.values() if p.poll() is None]

    def placer(self) -> Dict[str, object]:
        return {role: {"pid": p.pid, "start": self.starts.get(role, "")} for role, p in self.procs.items()}

    def kill(self, role: str) -> str:
        p = self.procs.get(role)
        if p is None:
            return "gone"
        said = procs.kill(p.pid, self.starts.get(role, ""))
        p.wait()
        return said

    # ---------------------------------------------------------------- identity and the image

    def identity(self) -> Dict[str, str]:
        import hashlib

        out = {}
        for what, path in (("shards", self.shards_binary), ("hex", self.binary)):
            h = hashlib.sha256()
            with open(path, "rb") as f:
                for chunk in iter(lambda: f.read(1 << 20), b""):
                    h.update(chunk)
            out[what] = h.hexdigest()
        return out

    def version(self) -> str:
        p = subprocess.run([self.shards_binary, "--version"], env=self.env, stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace", check=False)
        return p.stdout.strip()

    def pull(self, image: str) -> Tuple[bool, str]:
        """Pulls the image into the node's Shards home, whose store an image box boots from."""
        p = subprocess.run([self.shards_binary, "pull", image], env=self.env, stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace", check=False)
        return p.returncode == 0, (p.stdout + p.stderr).strip()[-500:]

    def run_once(self, image: str, deadline_ns: Optional[int]) -> Tuple[bool, str]:
        """One box run to its end through Hex (`true`), then removed: the guest kernel fetched and
        the image's template made, the first-run costs no fleet start pays again, and a time that
        bounds a start from above (a start ends once its box is made). Followed by Status at
        doubling intervals from 1 ms (Hex gives no event), bounded by the run's deadline alone, as
        setup's first waits are."""
        token = "first-run"
        why = self.place(token, image, 0, ["true"])
        if why is not None:
            return False, why
        wait = 0.001
        try:
            while True:
                state = self.placed(token)
                if state == "ended":
                    return True, "ended"
                if state.startswith("refused"):
                    return False, state
                if deadline_ns is not None and time.monotonic_ns() > deadline_ns:
                    return False, "the first box didn't end within the run's deadline"
                time.sleep(wait)
                wait *= 2
        finally:
            self.remove([token])
            self.keys.pop(token, None)

    # ---------------------------------------------------------------- asks

    def call(self, request: dict) -> Tuple[int, Optional[dict], str]:
        """One `hex schedule call`: its exit, its reply (JSON) and what it said on stderr."""
        self.calls += 1
        path = os.path.join(self.scope, "calls", f"{self.calls}.json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(request, f)
        p = subprocess.run([self.binary, "schedule", "call", "--socket", self.sockets["scheduler"], "--request", path], env=self.env, stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace", check=False)
        os.remove(path)
        try:
            reply = json.loads(p.stdout) if p.stdout.strip() else None
        except ValueError:
            reply = None
        return p.returncode, reply if isinstance(reply, dict) else None, p.stderr.strip()[-500:]

    def place(self, token: str, image: str, ballast: int, command: Optional[List[str]] = None) -> Optional[str]:
        """Submits the ask's box (the stub's `cat`, holding `ballast` bytes first); None once Hex
        took it, else why not (Hex's own words)."""
        if command is None:
            command = ["cat"] if ballast <= 0 else ["sh", "-c", f"head -c {ballast} /dev/zero > /tmp/.ballast && exec cat"]
        cpus, mem = self.vm["cpu"], self.vm["mem"]
        demand = f"cpus={cpus / 1000:g},mem={mem}b,class:shards,{self.platform}"
        code, reply, err = self.call({"Submit": {"demand": demand, "payload": {
            "spec": {"id": token, "bounds": {"cpu_max_milli": cpus, "memory_max": mem}, "class": {"shardsimage": {"image": image, "command": command}}, "owner": None},
            "start": True,
        }}})
        if code != 0 or reply is None:
            return err or f"hex schedule call exited {code} with no reply"
        if reply.get("refusals"):
            return f"refused: {refusals(reply)}"
        if reply.get("key") is None:
            return "Hex answered no key"
        self.keys[token] = reply["key"]
        return None

    def placed(self, token: str) -> str:
        """Where Hex has the ask's box: `starting` (no confirmed creation yet), `live`, `ended`, or
        `refused: ...` with Hex's words."""
        key = self.keys.get(token)
        if key is None:
            return "refused: no key"
        code, reply, err = self.call({"Status": {"key": key}})
        if code != 0 or reply is None:
            return f"refused: {err or code}"
        if reply.get("refusals"):
            return f"refused: {refusals(reply)}"
        if reply.get("ended") or not reply.get("tracked", True):
            return "ended"
        return "live" if reply.get("created") else "starting"

    def view(self) -> Tuple[Dict[str, Optional[List[int]]], str]:
        """The asks whose boxes Hex holds created and not ended, each naming no process."""
        view: Dict[str, Optional[List[int]]] = {}
        errs = []
        for token in sorted(set(self.keys) - self.ended):
            state = self.placed(token)
            if state == "live":
                view[token] = None
            elif state == "ended":
                self.ended.add(token)
            elif state.startswith("refused"):
                errs.append(f"{token}: {state}")
        return view, "; ".join(errs)[:300]

    def remove(self, tokens: List[str]) -> Tuple[int, str]:
        worst, said = 0, []
        for token in tokens:
            key = self.keys.get(token)
            if key is None:
                continue
            code, reply, err = self.call({"Delete": {"key": key}})
            if code != 0 or reply is None or reply.get("refusals"):
                worst = code or 1
                said.append(f"{token}: {err or refusals(reply or {})}")
        return worst, "; ".join(said)[-500:]

    # ---------------------------------------------------------------- the scan

    def scan(self, table: Dict[int, procs.Proc], roots: List[int]) -> List[Dict[str, object]]:
        """The scope's processes: each run host (`shards api-run`) one VM, its memory and CPU its
        whole tree's; the node, the scheduler and the rest as `hex`."""
        under = set(procs.descendants(table, roots))
        named = {p.pid for p in table.values() if any(self.scope in a for a in p.args)}
        # A run host's first arguments say `api-run` (`shards api-run`, or an interpreter's script).
        hosts = [pid for pid in sorted(under | named) if pid in table and "api-run" in table[pid].args[1:3] and not table[pid].zombie]
        trees = {h: procs.descendants(table, [h]) for h in hosts}
        procs.fill_mem(table, {pid for tree in trees.values() for pid in tree})
        out: List[Dict[str, object]] = []
        in_trees = set()
        for h, tree in trees.items():
            live = [table[pid] for pid in tree if pid in table and not table[pid].zombie]
            in_trees.update(tree)
            mems = [p.mem for p in live]
            cpus = [p.cpu_ns for p in live]
            out.append({
                "pid": h, "start": table[h].start, "role": "vm",
                "mem": sum(m for m in mems if m is not None) if all(m is not None for m in mems) else None,
                "cpu_ns": sum(c for c in cpus if c is not None) if all(c is not None for c in cpus) else None,
                "tree": sorted(tree),
            })
        for pid in sorted((under | named) - in_trees):
            p = table.get(pid)
            if p is None or p.zombie or pid in roots:
                continue
            out.append({"pid": pid, "start": p.start, "role": "hex", "mem": p.mem, "cpu_ns": p.cpu_ns})
        return out


def refusals(reply: dict) -> str:
    """Hex's refusals as its codec carried them: their bytes as text where they are text, else hex."""
    said = []
    for raw in reply.get("refusals") or []:
        data = bytes(raw) if isinstance(raw, list) else str(raw).encode()
        text = data.decode("utf-8", "replace")
        said.append(text if text.isprintable() else data.hex())
    return "; ".join(said)
