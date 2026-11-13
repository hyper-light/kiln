"""Forbidden-effect witnesses (Shards R15): each names an effect the acceptance forbids and looks
for it in what the OS shows, beside what the placer says.

Three sources meet here, each read on its own:
- the harness's ledger: every ask it made and the state it is in (`Ask`);
- the placer's view: which VM processes it says run each ask (Shards' inspect, Hex's records);
- the scan: the VM and network processes the OS shows in the harness's scope.

The effects:
- `over_room`: what the scope's VM processes hold passes the room the host gave the run.
- `over_limit`: more VMs run than the derived limit lets in.
- `doubled`: one ask has two VM processes, in the view or across hosts.
- `lost`: an ask the ledger holds live has no VM process.
- `leaked`: a VM process in scope that no live ask holds.
- `orphan`: a network process in scope whose VM process is gone.
- `unasked`: the placer runs a VM for an ask the ledger never made, or one it ended.

`lost`, `leaked`, `orphan` and `unasked` are judged only at quiescent points (no start or stop
in flight), when a transition can't be mistaken for the effect; the room and `doubled` always.
"""

from __future__ import annotations

from typing import Dict, Iterable, List, Optional, Set

# The ask's states (Shards R04): legal transitions only.
ASKED, STARTING, LIVE, STOPPING, ENDED, REFUSED, FAILED, KILLED = ("asked", "starting", "live", "stopping", "ended", "refused", "failed", "killed")
MOVES = {
    ASKED: {STARTING, REFUSED},
    STARTING: {LIVE, FAILED, KILLED, STOPPING},
    LIVE: {STOPPING, KILLED, FAILED},
    STOPPING: {ENDED, FAILED},
    KILLED: {ENDED},
    FAILED: {ENDED},
    REFUSED: set(),
    ENDED: set(),
}
# An ask in these may have a VM process the witnesses expect to see.
HOLDS_VM = {LIVE}
# An ask in these may still have one, coming or going.
IN_FLIGHT = {STARTING, STOPPING, KILLED, FAILED}


class IllegalMove(Exception):
    pass


class Ask:
    __slots__ = ("token", "state", "history")

    def __init__(self, token: str) -> None:
        self.token = token
        self.state = ASKED
        self.history: List[str] = [ASKED]

    def move(self, to: str) -> None:
        if to not in MOVES.get(self.state, set()):
            raise IllegalMove(f"{self.token}: {self.state} -> {to}")
        self.state = to
        self.history.append(to)


def violation(witness: str, **detail: object) -> Dict[str, object]:
    return {"witness": witness, "detail": detail}


def check(
    ledger: Dict[str, Ask],
    scan: Iterable[Dict[str, object]],
    view: Dict[str, List[int]],
    room: Dict[str, Optional[int]],
    limit: Optional[int],
    quiescent: bool,
) -> List[Dict[str, object]]:
    """Every forbidden effect in one scan.

    `scan`: the scope's processes, each {"pid", "role": "vm"|"net", "mem", "vm": the pid of the
    VM a network process serves, where known}. `view`: the placer's VM pids by ask token.
    """
    procs = list(scan)
    out: List[Dict[str, object]] = []
    vms = [p for p in procs if p.get("role") == "vm"]
    vm_pids: Set[int] = {int(p["pid"]) for p in vms}

    held = sum(int(p["mem"]) for p in procs if p.get("mem") is not None)
    if room.get("mem") is not None and held > int(room["mem"]):
        out.append(violation("over_room", dim="mem", held=held, room=room["mem"]))
    if limit is not None and len(vms) > limit:
        out.append(violation("over_limit", vms=len(vms), limit=limit))

    owner: Dict[int, str] = {}
    for token, pids in view.items():
        if len(set(pids)) > 1:
            out.append(violation("doubled", token=token, pids=sorted(set(pids))))
        for pid in pids:
            if pid in owner and owner[pid] != token:
                out.append(violation("doubled", token=token, pids=[pid], also=owner[pid]))
            owner[pid] = token

    if not quiescent:
        return out

    for token, ask in ledger.items():
        if ask.state in HOLDS_VM and not any(pid in vm_pids for pid in view.get(token, [])):
            out.append(violation("lost", token=token, state=ask.state, view=view.get(token, [])))
    for p in vms:
        token = owner.get(int(p["pid"]))
        ask = ledger.get(token) if token is not None else None
        if ask is None or ask.state not in HOLDS_VM:
            out.append(violation("leaked", pid=p["pid"], token=token, state=ask.state if ask else None))
    for token in view:
        ask = ledger.get(token)
        if ask is None or ask.state not in HOLDS_VM | IN_FLIGHT:
            if any(pid in vm_pids for pid in view[token]):
                out.append(violation("unasked", token=token, state=ask.state if ask else None))
    for p in procs:
        if p.get("role") == "net" and p.get("vm") is not None and int(p["vm"]) not in vm_pids:
            out.append(violation("orphan", pid=p["pid"], vm=p["vm"]))
    return out


def across_hosts(live: Dict[str, List[str]]) -> List[Dict[str, object]]:
    """`doubled` across hosts: `live` is each host's ask tokens with a VM process at its last
    quiescent scan; a token on two hosts is one ask placed twice."""
    seen: Dict[str, str] = {}
    out: List[Dict[str, object]] = []
    for host, tokens in sorted(live.items()):
        for token in tokens:
            if token in seen and seen[token] != host:
                out.append(violation("doubled", token=token, hosts=[seen[token], host]))
            seen.setdefault(token, host)
    return out
