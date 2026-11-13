"""The direct driver: microVMs started by a Shards daemon of the harness's own, on one host, with no
placer between (the baseline every placer is held to, and the harness's own light test).

The daemon runs in the foreground as the agent's child (`shards run daemon`, `SHARDS_POOL=0`, so
each run's microVM is its own and every VM process belongs to an ask), in a home of the run's own,
with a Docker config of its own and no local image store, as Shards' control-end harness runs it.
Each VM is `shards run vm -i --rm --name fleet-<token> IMAGE CMD`: its client holds the run's
stdin and stdout, which are the lane; closing its stdin ends `cat` and so the run.

The view (the placer's word on which VM process runs each ask) is `shards inspect vm`'s
`.State.Pid`. The scan is the OS's: the daemon's descendants, and, for what a daemon that died
left behind, every process whose arguments name the run's home, with each `shards-net` paired to
its `shards-vm` by the MAC address both are given.
"""

from __future__ import annotations

import os
import subprocess
import time
from typing import Dict, List, Optional, Tuple

from . import procs

NAME_PREFIX = "fleet-"


class Shards:
    def __init__(self, binary: str, scope: str) -> None:
        self.binary = binary
        self.scope = scope
        self.home = os.path.join(scope, "h")
        self.env = dict(os.environ)
        for drop in ("DOCKER_AUTH_CONFIG", "SHARDS_DEFAULT_PLATFORM", "DOCKER_DEFAULT_PLATFORM", "DOCKER_HOST", "SHARDS_HOST"):
            self.env.pop(drop, None)
        self.config = os.path.join(scope, "docker-config")
        self.env.update({"SHARDS_HOME": self.home, "DOCKER_CONFIG": self.config, "SHARDS_LOCAL_STORE": "none", "SHARDS_POOL": "0"})
        self.daemon: Optional[subprocess.Popen] = None
        self.daemon_start = ""

    def command(self, *args: str, timeout_s: Optional[float] = None) -> Tuple[int, str, str]:
        p = subprocess.run([self.binary, *args], env=self.env, stdin=subprocess.DEVNULL, capture_output=True, text=True, errors="replace", timeout=timeout_s, check=False)
        return p.returncode, p.stdout, p.stderr

    def version(self) -> str:
        _, out, _ = self.command("version")
        return " ".join(line.strip() for line in out.splitlines()[:5])

    def identity(self) -> str:
        """The binary's SHA-256: what was measured, exactly (its version is asked of the daemon
        once it serves, since a client with no daemon would start one)."""
        import hashlib

        h = hashlib.sha256()
        with open(self.binary, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        return h.hexdigest()

    def start_daemon(self, log_path: str, deadline_ns: Optional[int]) -> Tuple[bool, str]:
        """Starts the home's daemon as this process's child and waits for it to serve: its pid file
        naming it and its request slots made (D185), then a client answered by it."""
        os.makedirs(self.config, exist_ok=True)
        log = open(log_path, "ab")
        self.daemon = subprocess.Popen([self.binary, "run", "daemon"], env=self.env, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        me = str(self.daemon.pid)
        pidfile = os.path.join(self.home, "daemon.pid")
        # A one-time probe at doubling intervals from 1 ms: O(log T) reads for a wait of T, none
        # picked (R05: a probe, said as one).
        wait = 0.001
        while True:
            try:
                with open(pidfile, encoding="utf-8") as f:
                    named = f.read().split()
            except OSError:
                named = []
            if named[:1] == [me] and os.path.isdir(os.path.join(self.home, "requests")):
                break
            if self.daemon.poll() is not None:
                return False, f"the daemon ended at once ({self.daemon.returncode}): another serves {self.home}"
            if deadline_ns is not None and time.monotonic_ns() > deadline_ns:
                return False, "the daemon named itself in no pid file within the run's deadline"
            time.sleep(wait)
            wait *= 2
        found = procs.one(self.daemon.pid)
        self.daemon_start = found.start if found else ""
        code, _, err = self.command("list", "vm", "-q")
        if code != 0:
            return False, f"the daemon answered no list: {err.strip()[:300]}"
        return True, ""

    def stop_daemon(self) -> Optional[int]:
        if self.daemon is None:
            return None
        if self.daemon.poll() is None:
            self.command("stop", "daemon")
            try:
                self.daemon.wait(timeout=None)
            except subprocess.SubprocessError:
                pass
        return self.daemon.returncode

    def pull(self, image: str) -> Tuple[bool, str]:
        code, out, err = self.command("pull", image)
        return code == 0, (out + err).strip()[-500:]

    def run_once(self, image: str) -> Tuple[bool, str]:
        """One run to its end: the guest kernel fetched and the image's template made, the
        first-run costs no fleet start pays again."""
        code, out, err = self.command("run", "vm", "--rm", image, "true")
        return code == 0, (out + err).strip()[-500:]

    def start_vm(self, token: str, image: str, ballast: int, stderr_path: str) -> subprocess.Popen:
        argv = [self.binary, "run", "vm", "-i", "--rm", "--name", NAME_PREFIX + token, image]
        if ballast > 0:
            # The workload holds `ballast` bytes resident (tmpfs pages) before it answers, as a
            # recorded harness held them (README, the profile).
            argv += ["sh", "-c", f"head -c {ballast} /dev/zero > /tmp/.ballast && exec cat"]
        else:
            argv += ["cat"]
        err = open(stderr_path, "ab")
        return subprocess.Popen(argv, env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, bufsize=0)

    def remove(self, tokens: List[str]) -> Tuple[int, str]:
        if not tokens:
            return 0, ""
        code, out, err = self.command("remove", "vm", "-f", *[NAME_PREFIX + t for t in tokens])
        return code, (out + err).strip()[-500:]

    def view(self) -> Tuple[Dict[str, List[int]], str]:
        """Each listed run's token and the VM process its daemon says runs it."""
        code, out, err = self.command("list", "vm", "-a", "--format", "{{.Names}}")
        if code != 0:
            return {}, f"list: {err.strip()[:300]}"
        names = [n.strip() for n in out.split() if n.strip().startswith(NAME_PREFIX)]
        if not names:
            return {}, ""
        code, out, err = self.command("inspect", "vm", "--format", "{{.Name}} {{.State.Pid}} {{.State.Running}}", *names)
        view: Dict[str, List[int]] = {}
        for line in out.splitlines():
            parts = line.split()
            if len(parts) < 2:
                continue
            token = parts[0].lstrip("/")[len(NAME_PREFIX) :]
            try:
                pid = int(parts[1])
            except ValueError:
                continue
            view.setdefault(token, [])
            if pid > 0 and (len(parts) < 3 or parts[2] == "true"):
                view[token].append(pid)
        return view, "" if code == 0 else f"inspect: {err.strip()[:300]}"

    def scan(self, table: Dict[int, procs.Proc], roots: List[int]) -> List[Dict[str, object]]:
        """The scope's Shards processes: VM processes (`shards-vm`), network processes
        (`shards-net`, each with the VM it serves) and the rest (`shards`: spawners, launchers)."""
        under = set(procs.descendants(table, roots))
        named = {p.pid for p in table.values() if any(self.home in a for a in p.args)}
        procs.fill_mem(table, under | named)
        vm_by_mac: Dict[str, int] = {}
        for pid in under | named:
            p = table.get(pid)
            if p is not None and p.name == "shards-vm":
                mac = _flag(p.args, "--net")
                if mac:
                    vm_by_mac[mac.split(",")[-1]] = pid
        nets = {p.pid for p in table.values() if p.name == "shards-net" and _flag(p.args, "--mac") in vm_by_mac}
        procs.fill_mem(table, nets)
        out: List[Dict[str, object]] = []
        for pid in sorted(under | named | nets):
            p = table.get(pid)
            if p is None or (pid in roots and p.name not in ("shards-vm", "shards-net")):
                continue
            role = "vm" if p.name == "shards-vm" else "net" if p.name == "shards-net" else "shards"
            rec: Dict[str, object] = {"pid": pid, "start": p.start, "role": role, "mem": p.mem, "cpu_ns": p.cpu_ns}
            if role == "net":
                rec["vm"] = vm_by_mac.get(_flag(p.args, "--mac") or "")
            out.append(rec)
        return out


def _flag(args: List[str], name: str) -> Optional[str]:
    for i, a in enumerate(args):
        if a == name and i + 1 < len(args):
            return args[i + 1]
        if a.startswith(name + "="):
            return a[len(name) + 1 :]
    return None
