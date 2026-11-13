"""Chaos: each action kills one thing the harness started, in its own scope, and says what became
of it; the agent then settles, refills and lets the witnesses judge.

Scope is by construction: a target is a process the harness started (the daemon, a client) or a
VM process the scope's scan found under them, named by its pid and its start, and killed only if
that pid still has that start (`procs.kill`: a reused pid is refused, `reused`). Nothing is picked
by command line, and nothing outside the run's scope is reachable from here.

Actions (direct driver):
- `vm`: SIGKILL a live ask's VM process (a VM dies under its run).
- `daemon`: SIGKILL the home's daemon (its node dies; the agent starts it again in the same home).
- `client`: SIGKILL a live ask's client, the far end of its lane (its link dies; the run is then
  removed by name, as a controller that lost its link would).

Targets are drawn by a seeded generator (`--seed`, recorded), so a run's actions repeat.
"""

from __future__ import annotations

import random
from typing import Dict, List, Optional, Sequence

from . import procs

ACTIONS = ("vm", "daemon", "client")


def pick(rng: random.Random, candidates: Sequence[str]) -> Optional[str]:
    ordered = sorted(candidates)
    return rng.choice(ordered) if ordered else None


def kill_in_scope(pid: int, start: str, scope_pids: Dict[int, str]) -> str:
    """Kills `pid` only if the scope holds it with that start; else `out_of_scope`."""
    if scope_pids.get(pid) != start:
        return "out_of_scope"
    return procs.kill(pid, start)


def parse(spec: str) -> List[str]:
    """`--chaos vm,daemon,client` into its actions, each known; `none` is no chaos."""
    if spec.strip() in ("", "none"):
        return []
    out = [a.strip() for a in spec.split(",") if a.strip()]
    unknown = [a for a in out if a not in ACTIONS]
    if unknown:
        raise ValueError(f"unknown chaos actions {unknown}: the actions are {', '.join(ACTIONS)}")
    return out
