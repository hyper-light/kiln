"""Raw records: one JSON object a line, every sample as measured.

Every record carries its kind, the host's wall clock (`t`, ns since the epoch, for ordering across
hosts within their clocks' skew, which each host's `host` record measures against the controller)
and its monotonic clock (`mono`, ns, for intervals on that host alone), the host's label and the
run's id. Nothing is aggregated here; the report reads these.
"""

from __future__ import annotations

import gzip
import json
import os
import sys
import threading
import time
from typing import IO, Any, Dict, Iterator, List, Optional


class Records:
    """Appends records to a file and, when asked, to stdout: the controller reads a host's stdout
    through `kiln run`, which streams it as it is written."""

    def __init__(self, path: str, host: str, run: str, echo: Optional[IO[str]] = None) -> None:
        self.host = host
        self.run = run
        self._echo = echo
        self._lock = threading.Lock()
        os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
        self._file = open(path, "a", encoding="utf-8")

    def write(self, kind: str, **fields: Any) -> Dict[str, Any]:
        rec: Dict[str, Any] = {"kind": kind, "t": time.time_ns(), "mono": time.monotonic_ns(), "host": self.host, "run": self.run}
        rec.update(fields)
        line = json.dumps(rec, sort_keys=True, separators=(",", ":"))
        with self._lock:
            self._file.write(line + "\n")
            self._file.flush()
            if self._echo is not None:
                try:
                    self._echo.write(line + "\n")
                    self._echo.flush()
                except (BrokenPipeError, OSError):
                    # The controller went away: the file keeps every record, and the agent ends
                    # on its own plan or signal.
                    self._echo = None
        return rec

    def close(self) -> None:
        with self._lock:
            self._file.close()


def read(path: str) -> Iterator[Dict[str, Any]]:
    """Every record in a file (gzipped where it ends `.gz`, as committed records are), in order; a
    line that isn't one is reported as `unparsed`."""
    with (gzip.open(path, "rt", encoding="utf-8") if path.endswith(".gz") else open(path, encoding="utf-8")) as f:
        for number, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except ValueError:
                yield {"kind": "unparsed", "line": number, "text": line[:200]}
                continue
            if isinstance(rec, dict) and isinstance(rec.get("kind"), str):
                yield rec
            else:
                yield {"kind": "unparsed", "line": number, "text": line[:200]}


def read_all(path: str) -> List[Dict[str, Any]]:
    return list(read(path))


def stdout() -> IO[str]:
    return sys.stdout
