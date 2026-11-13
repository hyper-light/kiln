"""What a host is and what it has free, as its OS says, for every record's header.

The report names host, OS, kernel, CPU and revision beside every number (Shards R14; Hex's
bench rule), so each host agent reads them itself: no value here comes from the placer under test.
"""

from __future__ import annotations

import json
import os
import platform
import socket
import subprocess
import sys
from typing import Dict, List, Optional


def _run(args, cwd: Optional[str] = None) -> str:
    try:
        return subprocess.run(args, capture_output=True, text=True, errors="replace", cwd=cwd, check=False, timeout=None).stdout.strip()
    except OSError:
        return ""


def memory() -> Dict[str, Optional[int]]:
    """Total and available memory in bytes: what the OS would give a new process now."""
    if sys.platform.startswith("linux"):
        got: Dict[str, int] = {}
        try:
            with open("/proc/meminfo", encoding="utf-8") as f:
                for line in f:
                    key, _, rest = line.partition(":")
                    if key in ("MemTotal", "MemAvailable"):
                        got[key] = int(rest.split()[0]) * 1024
        except (OSError, ValueError, IndexError):
            pass
        return {"total": got.get("MemTotal"), "available": got.get("MemAvailable")}
    if sys.platform == "darwin":
        total = _run(["sysctl", "-n", "hw.memsize"])
        page = _run(["sysctl", "-n", "hw.pagesize"])
        free = _run(["sysctl", "-n", "vm.page_free_count"])
        spec = _run(["sysctl", "-n", "vm.page_speculative_count"])
        try:
            # Free and speculative pages, as Hex's node facts count macOS's free memory (hex-node facts).
            avail = (int(free) + int(spec)) * int(page)
        except ValueError:
            avail = None
        return {"total": int(total) if total.isdigit() else None, "available": avail}
    if sys.platform in ("win32", "cygwin", "msys"):
        out = _run(["powershell", "-NoProfile", "-NonInteractive", "-Command", "Get-CimInstance Win32_OperatingSystem | Select-Object TotalVisibleMemorySize,FreePhysicalMemory | ConvertTo-Json -Compress"])
        try:
            row = json.loads(out)
            return {"total": int(row["TotalVisibleMemorySize"]) * 1024, "available": int(row["FreePhysicalMemory"]) * 1024}
        except (ValueError, KeyError, TypeError):
            return {"total": None, "available": None}
    return {"total": None, "available": None}


def raise_nofile() -> Optional[List[int]]:
    """Lifts this process's open-file limit to the most the OS lets it hold, its hard limit (on
    macOS, kern.maxfilesperproc where the hard limit is unlimited): each live VM's lane holds two
    descriptors, so a host's hundreds need more than a shell's soft limit (macOS's is 256).
    Returns [soft, hard] as they are after; None where there are none (Windows)."""
    try:
        import resource
    except ImportError:
        return None
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    want = hard
    if sys.platform == "darwin":
        cap = _run(["sysctl", "-n", "kern.maxfilesperproc"])
        if cap.isdigit() and (hard == resource.RLIM_INFINITY or int(cap) < hard):
            want = int(cap)
    if want != resource.RLIM_INFINITY and want > soft:
        try:
            resource.setrlimit(resource.RLIMIT_NOFILE, (want, hard))
        except (ValueError, OSError):
            pass
    return [int(x) for x in resource.getrlimit(resource.RLIMIT_NOFILE)]


def cpu_model() -> str:
    if sys.platform.startswith("linux"):
        try:
            with open("/proc/cpuinfo", encoding="utf-8", errors="replace") as f:
                for line in f:
                    if line.startswith("model name") or line.startswith("Model"):
                        return line.split(":", 1)[1].strip()
        except OSError:
            return ""
    if sys.platform == "darwin":
        return _run(["sysctl", "-n", "machdep.cpu.brand_string"])
    return platform.processor()


def load() -> Optional[list]:
    try:
        return [round(x, 2) for x in os.getloadavg()]
    except (AttributeError, OSError):
        return None


def facts(repo: Optional[str]) -> Dict[str, object]:
    mem = memory()
    rev = _run(["git", "rev-parse", "HEAD"], cwd=repo) if repo else ""
    dirty = bool(_run(["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo)) if repo else None
    return {
        "hostname": socket.gethostname(),
        "os": platform.system(),
        "os_release": platform.release(),
        "os_version": platform.mac_ver()[0] if sys.platform == "darwin" else platform.version(),
        "arch": platform.machine(),
        "cpu": cpu_model(),
        "cpus": os.cpu_count(),
        "mem_total": mem["total"],
        "mem_available": mem["available"],
        "load": load(),
        "python": platform.python_version(),
        "kiln_rev": rev,
        "kiln_dirty": dirty,
    }
