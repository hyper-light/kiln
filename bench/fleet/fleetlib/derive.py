"""Every limit the harness keeps, derived from what it measures (Shards R03): none is picked.

- **Density.** A host takes VMs while the measured room covers the measured cost of one more in
  every dimension: limit = min over dimensions of floor(room / cost), the cost the most any VM on
  the host was measured holding (so a VM that grew is charged as it grew, and the limit never
  rests on the cheapest). Under a placer (Hex, kiln's balancer) the placer's own refusal is the
  limit, and this one is the witness's check of it.
- **Waits.** Each kind of wait lasts twice the slowest answer of that kind seen so far, Hex's
  failure-bound rule (`hex_core::patience`, TIMES_SLOWEST, the lead's rule of 2026-11-06), and
  only rises; a wait that ran out proves the answer slower than its bound, so the next is twice
  that (Karn's point, RFC 6298 §5.5). Before a kind has an answer of its own it is seeded from a
  measured answer that bounds it from above (Hex seeds each wait from a record): a VM's start
  from the run of the image the setup makes (`first_run`), and a lane's round trip, a transfer's
  first chunk and a chaos action's settle from the slowest start (a start ends with a round trip
  on the lane, so a round trip past twice every start is past any). A batch of operations on the
  lanes (round trips, turns, transfers) is bounded on the wait for its next operation to finish,
  a stall, never on its length: twice the slowest single operation of its kind, which each
  operation teaches (RFC 6298's timer is a segment's). A transfer's is kept per byte, since each
  bulk round's transfers are twice the last's, and seeded from a turn's time per byte (the same
  stream, smaller). Setup's own first waits
  have no record before them and are bounded by the run's deadline alone, which the person
  running it gives, as Hex's image_real bounds its first waits (HEX_IMAGE_WAIT_MS). An answer
  that comes after its wait ran out is still recorded.
- **Samples.** A host's p99 is a measured sample below its max only from 100 samples
  (`stats.least_n`), so each VM is asked ceil(100 / VMs) round trips and transfers.
- **Ramp.** Waves of starts double from one (1, 2, 4, ...) up to the room left: every
  concurrency from one to the limit is met within a factor of two, in log2(limit) waves.
- **Transfer size.** Two streams are timed. A turn's: the bytes a real harness turn wrote (Claude
  Code's signed-in turn, `TURN_BYTES`, from its committed record). And a bulk one, whose size
  doubles from one vsock packet (64 KiB, VIRTIO_VSOCK_MAX_PKT_BUF_SIZE in linux
  include/linux/virtio_vsock.h, as Shards' managed-engine-relay-throughput-v1 took it), a full
  round of 100 samples a size on every lane at once, until the VMs' median rate no longer rises:
  the size before is the converged one, and its round is the bulk distribution. A median over
  every lane's samples holds under a loaded host where three samples on one lane did not (the
  light test, 2026-11-12: rates on one lane at one size spread fourfold).
"""

from __future__ import annotations

import math
import threading
from typing import Dict, List, Optional, Sequence

from . import stats

TIMES_SLOWEST = 2
FIRST_CHUNK = 64 * 1024
# A real turn's stream: Claude Code 2.1.322's stream-json stdout of one signed-in turn, the largest
# turn recorded (Shards e7825cb32, docs/research/measurements/harness-bringup-v2/records/
# claude-signed-in/stdout.bin: 60,783 bytes; Codex 0.167.2's turn there wrote 765).
TURN_BYTES = 60783


class Patience:
    """One kind of wait's bound, learned from its answers, seeded from another kind's.

    A wait is kept per unit of its work (`units`: one, or a transfer's bytes), and `per_seed`
    turns its seed's unit into its own (a turn's round into a byte of it)."""

    def __init__(self, name: str, seed: Optional["Patience"] = None, per_seed: float = 1.0) -> None:
        self.name = name
        self.seed = seed
        self.per_seed = per_seed
        self._slowest = 0.0
        self._lock = threading.Lock()

    def slowest(self) -> float:
        """The slowest answer of a unit, its own or its seed's in its units; 0 while unarmed."""
        with self._lock:
            own = self._slowest
        if own == 0 and self.seed is not None:
            return self.seed.slowest() * self.per_seed
        return own

    def bound_ns(self, units: float = 1) -> Optional[int]:
        """The wait's bound for `units` of work, or None while neither it nor its seed has an
        answer (unarmed)."""
        slowest = self.slowest()
        return math.ceil(slowest * units * TIMES_SLOWEST) if slowest else None

    def answered(self, took_ns: int, units: float = 1) -> None:
        with self._lock:
            self._slowest = max(self._slowest, max(1, took_ns) / units)

    def ran_out(self, after_ns: int, units: float = 1) -> None:
        self.answered(after_ns, units)


def density_limit(room: Dict[str, Optional[int]], cost: Dict[str, int]) -> Optional[int]:
    """How many VMs of `cost` the room holds, or None when no dimension's cost is known."""
    limits: List[int] = []
    for dim, per in cost.items():
        have = room.get(dim)
        if per and per > 0 and have is not None:
            limits.append(max(0, have // per))
    return min(limits) if limits else None


def per_vm(vms: int) -> int:
    """Operations each VM is asked so that its host has a p99 below its max."""
    return max(1, math.ceil(stats.least_n(0.99) / max(1, vms)))


def next_wave(last: int, room_left: int) -> int:
    return max(0, min(room_left, 1 if last <= 0 else last * 2))


def median(xs: Sequence[float]) -> Optional[float]:
    """The lower median (a measured sample), or None for no samples."""
    if not xs:
        return None
    ordered = sorted(xs)
    return ordered[(len(ordered) - 1) // 2]
