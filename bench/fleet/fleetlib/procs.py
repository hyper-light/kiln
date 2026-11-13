"""The host's process table, read from the OS itself, independently of what any placer says.

What each OS gives, per process: its parent, its arguments, an identity for its start (so a pid
the OS reused is told apart from the process that had it), its CPU time and its memory:

- Linux: /proc. Memory is PSS (`smaps_rollup`), each shared page split among its sharers, as
  Shards' fleet measurements count a VM's host cost; VmRSS where PSS can't be read. CPU is
  utime + stime, which on Linux include a KVM guest's time (proc(5): guest_time is counted in
  utime). The start is the process's start in clock ticks since boot.
- macOS: `proc_pid_rusage(RUSAGE_INFO_V2)` through libproc: physical footprint (what Activity
  Monitor and jetsam count, as Shards' M49 fleet measurement reads it), user and system time, and
  the start in Mach absolute time; ps(1) for the parent and the arguments.
- Windows: Win32_Process through CIM (one PowerShell query a scan): the private working set is not
  in it, so memory is WorkingSetSize, which counts shared pages whole; the creation time is the
  start; kernel and user time.

A process another user owns may be unreadable: it is listed with `mem` None, never guessed.
"""

from __future__ import annotations

import ctypes
import json
import os
import signal
import subprocess
import sys
from typing import Dict, Iterable, List, Optional


class Proc:
    __slots__ = ("pid", "ppid", "name", "args", "start", "mem", "mem_kind", "cpu_ns", "zombie")

    def __init__(self, pid: int, ppid: int, name: str, args: List[str], start: str, mem: Optional[int], mem_kind: str, cpu_ns: Optional[int], zombie: bool = False) -> None:
        self.pid = pid
        self.ppid = ppid
        self.name = name
        self.args = args
        self.start = start
        self.mem = mem
        self.mem_kind = mem_kind
        self.cpu_ns = cpu_ns
        # Exited, its parent yet to reap it: it runs nothing and holds no memory.
        self.zombie = zombie

    def brief(self) -> Dict[str, object]:
        return {"pid": self.pid, "ppid": self.ppid, "name": self.name, "start": self.start, "mem": self.mem, "mem_kind": self.mem_kind, "cpu_ns": self.cpu_ns}


# ----------------------------------------------------------------------------------- Linux


def _linux_one(pid: int, tick_ns: int, with_mem: bool = True) -> Optional[Proc]:
    base = f"/proc/{pid}"
    try:
        with open(f"{base}/stat", "rb") as f:
            stat = f.read().decode("utf-8", "replace")
    except OSError:
        return None
    close = stat.rfind(")")
    if close < 0:
        return None
    name = stat[stat.find("(") + 1 : close]
    fields = stat[close + 2 :].split()
    # fields[0] is the state (field 3 of proc(5)); ppid is field 4, utime 14, stime 15, starttime 22.
    try:
        ppid = int(fields[1])
        cpu_ns = (int(fields[11]) + int(fields[12])) * tick_ns
        start = fields[19]
    except (IndexError, ValueError):
        return None
    try:
        with open(f"{base}/cmdline", "rb") as f:
            args = [a.decode("utf-8", "replace") for a in f.read().split(b"\0") if a]
    except OSError:
        args = []
    if not with_mem:
        return Proc(pid, ppid, name, args, start, None, "pss", cpu_ns, fields[0] == "Z")
    mem, kind = _linux_mem(pid)
    return Proc(pid, ppid, name, args, start, mem, kind, cpu_ns, fields[0] == "Z")


def _linux_mem(pid: int):
    base = f"/proc/{pid}"
    mem: Optional[int] = None
    kind = "pss"
    try:
        with open(f"{base}/smaps_rollup", encoding="utf-8", errors="replace") as f:
            for line in f:
                if line.startswith("Pss:"):
                    mem = int(line.split()[1]) * 1024
                    break
    except (OSError, ValueError, IndexError):
        mem = None
    if mem is None:
        kind = "rss"
        try:
            with open(f"{base}/status", encoding="utf-8", errors="replace") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        mem = int(line.split()[1]) * 1024
                        break
        except (OSError, ValueError, IndexError):
            mem = None
    return mem, kind


def _linux_table() -> Dict[int, Proc]:
    # Memory is read for the scope's processes alone (`fill_mem`): smaps_rollup walks a process's
    # mappings, and a host runs thousands of processes the scan needs only the parents of.
    tick_ns = 1_000_000_000 // os.sysconf("SC_CLK_TCK")
    out: Dict[int, Proc] = {}
    for entry in os.listdir("/proc"):
        if entry.isdigit():
            p = _linux_one(int(entry), tick_ns, with_mem=False)
            if p is not None:
                out[p.pid] = p
    return out


# ----------------------------------------------------------------------------------- macOS


class _RusageV2(ctypes.Structure):
    # <sys/resource.h>, struct rusage_info_v2.
    _fields_ = [
        ("uuid", ctypes.c_uint8 * 16),
        ("user_time", ctypes.c_uint64),
        ("system_time", ctypes.c_uint64),
        ("pkg_idle_wkups", ctypes.c_uint64),
        ("interrupt_wkups", ctypes.c_uint64),
        ("pageins", ctypes.c_uint64),
        ("wired_size", ctypes.c_uint64),
        ("resident_size", ctypes.c_uint64),
        ("phys_footprint", ctypes.c_uint64),
        ("proc_start_abstime", ctypes.c_uint64),
        ("proc_exit_abstime", ctypes.c_uint64),
        ("child_user_time", ctypes.c_uint64),
        ("child_system_time", ctypes.c_uint64),
        ("child_pkg_idle_wkups", ctypes.c_uint64),
        ("child_interrupt_wkups", ctypes.c_uint64),
        ("child_pageins", ctypes.c_uint64),
        ("child_elapsed_abstime", ctypes.c_uint64),
        ("ri_diskio_bytesread", ctypes.c_uint64),
        ("ri_diskio_byteswritten", ctypes.c_uint64),
    ]


class _Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_MAC: Dict[str, object] = {}


def _mac_lib():
    if "lib" not in _MAC:
        lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
        lib.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.POINTER(_RusageV2)]
        lib.proc_pid_rusage.restype = ctypes.c_int
        tb = _Timebase()
        lib.mach_timebase_info(ctypes.byref(tb))
        _MAC["lib"] = lib
        _MAC["numer"] = tb.numer or 1
        _MAC["denom"] = tb.denom or 1
    return _MAC["lib"]


def _mac_rusage(pid: int):
    lib = _mac_lib()
    info = _RusageV2()
    if lib.proc_pid_rusage(pid, 2, ctypes.byref(info)) != 0:  # RUSAGE_INFO_V2
        return None
    numer, denom = int(_MAC["numer"]), int(_MAC["denom"])
    cpu_ns = (info.user_time + info.system_time) * numer // denom
    return int(info.phys_footprint), cpu_ns, str(info.proc_start_abstime)


def _mac_ps(*select: str) -> Dict[int, Proc]:
    out = subprocess.run(["ps", *select, "-ww", "-o", "pid=,ppid=,stat=,args="], capture_output=True, text=True, errors="replace", check=False).stdout
    table: Dict[int, Proc] = {}
    for line in out.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 3 or not parts[0].isdigit() or not parts[1].isdigit():
            continue
        pid, ppid, zombie = int(parts[0]), int(parts[1]), parts[2].startswith("Z")
        # Arguments split at spaces: enough to know a Shards helper by its path, which has none.
        args = parts[3].split() if len(parts) > 3 else []
        name = os.path.basename(args[0]) if args else ""
        got = _mac_rusage(pid)
        if got is None:
            table[pid] = Proc(pid, ppid, name, args, "", None, "footprint", None, zombie)
        else:
            mem, cpu_ns, start = got
            table[pid] = Proc(pid, ppid, name, args, start, mem, "footprint", cpu_ns, zombie)
    return table


def _mac_table() -> Dict[int, Proc]:
    return _mac_ps("-ax")


# --------------------------------------------------------------------------------- Windows

_WIN_QUERY = (
    "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name,CommandLine,"
    "WorkingSetSize,KernelModeTime,UserModeTime,@{n='Created';e={$_.CreationDate.ToFileTimeUtc()}} "
    "| ConvertTo-Json -Compress"
)


def _win_table() -> Dict[int, Proc]:
    out = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", _WIN_QUERY], capture_output=True, text=True, errors="replace", check=False).stdout
    try:
        rows = json.loads(out) if out.strip() else []
    except ValueError:
        rows = []
    if isinstance(rows, dict):
        rows = [rows]
    table: Dict[int, Proc] = {}
    for r in rows:
        try:
            pid = int(r.get("ProcessId"))
            ppid = int(r.get("ParentProcessId") or 0)
        except (TypeError, ValueError):
            continue
        ws = r.get("WorkingSetSize")
        k, u = r.get("KernelModeTime"), r.get("UserModeTime")
        cpu = (int(k) + int(u)) * 100 if k is not None and u is not None else None
        cmd = r.get("CommandLine") or ""
        table[pid] = Proc(pid, ppid, r.get("Name") or "", cmd.split(), str(r.get("Created") or ""), int(ws) if ws is not None else None, "working_set", cpu)
    return table


# ---------------------------------------------------------------------------------- shared


def table() -> Dict[int, Proc]:
    if sys.platform.startswith("linux"):
        return _linux_table()
    if sys.platform == "darwin":
        return _mac_table()
    if sys.platform in ("win32", "cygwin", "msys"):
        return _win_table()
    raise OSError(f"no process reader for {sys.platform}")


def fill_mem(procs_: Dict[int, Proc], pids: Iterable[int]) -> None:
    """Reads the memory of `pids` where the table left it out (Linux reads it only for these)."""
    if not sys.platform.startswith("linux"):
        return
    for pid in pids:
        p = procs_.get(pid)
        if p is not None and p.mem is None:
            p.mem, p.mem_kind = _linux_mem(pid)


def one(pid: int) -> Optional[Proc]:
    """One process, read alone (an ask reads its client's start this way, so a wave's asks
    never read the whole table each)."""
    if sys.platform.startswith("linux"):
        return _linux_one(pid, 1_000_000_000 // os.sysconf("SC_CLK_TCK"))
    if sys.platform == "darwin":
        return _mac_ps("-p", str(pid)).get(pid)
    return table().get(pid)


def descendants(procs: Dict[int, Proc], roots: Iterable[int]) -> List[int]:
    """The roots that exist and every process under them, breadth first (no recursion)."""
    children: Dict[int, List[int]] = {}
    for p in procs.values():
        children.setdefault(p.ppid, []).append(p.pid)
    found = [r for r in roots if r in procs]
    seen = set(found)
    at = 0
    while at < len(found):
        for c in children.get(found[at], ()):
            if c not in seen:
                seen.add(c)
                found.append(c)
        at += 1
    return found


def kill(pid: int, start: str) -> str:
    """Kills the process `pid` only if it is still the one that started at `start`.

    Answers `killed`, `gone` (no such process), `reused` (another process has the pid: refused)
    or `denied`.
    """
    now = one(pid)
    if now is None:
        return "gone"
    if start and now.start != start:
        return "reused"
    try:
        os.kill(pid, signal.SIGKILL if hasattr(signal, "SIGKILL") else signal.SIGTERM)
    except ProcessLookupError:
        return "gone"
    except PermissionError:
        return "denied"
    return "killed"
