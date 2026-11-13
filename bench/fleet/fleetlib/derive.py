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
  on the lane, so a round trip past twice every start is past any). Setup's own first waits
  have no record before them and are bounded by the run's deadline alone, which the person
  running it gives, as Hex's image_real bounds its first waits (HEX_IMAGE_WAIT_MS). An answer
  that comes after its wait ran out is still recorded.
- **Samples.** A host's p99 is a measured sample below its max only from 100 samples
  (`stats.least_n`), so each VM is asked ceil(100 / VMs) round trips and transfers.
- **Ramp.** Waves of starts double from one (1, 2, 4, ...) up to the room left: every
  concurrency from one to the limit is met within a factor of two, in log2(limit) waves.
- **Transfer size.** A stream's size doubles from one vsock packet (64 KiB,
  VIRTIO_VSOCK_MAX_PKT_BUF_SIZE in linux include/linux/virtio_vsock.h, as Shards'
  managed-engine-relay-throughput-v1 took it) until doubling it no longer raises the rate past
  the spread of the samples at the size before: the rate has converged. Each size is measured
  three times, the fewest whose median is neither their min nor their max.
"""

from __future__ import annotations

import math
import threading
from typing import Dict, List, Optional, Sequence

from . import stats

TIMES_SLOWEST = 2
FIRST_CHUNK = 64 * 1024
PER_SIZE = 3


class Patience:
    """One kind of wait's bound, learned from its answers, seeded from another kind's."""

    def __init__(self, name: str, seed: Optional["Patience"] = None) -> None:
        self.name = name
        self.seed = seed
        self._slowest_ns = 0
        self._lock = threading.Lock()

    def slowest_ns(self) -> int:
        with self._lock:
            own = self._slowest_ns
        if own == 0 and self.seed is not None:
            return self.seed.slowest_ns()
        return own

    def bound_ns(self) -> Optional[int]:
        """The wait's bound, or None while neither it nor its seed has an answer (unarmed)."""
        slowest = self.slowest_ns()
        return slowest * TIMES_SLOWEST if slowest else None

    def answered(self, took_ns: int) -> None:
        with self._lock:
            self._slowest_ns = max(self._slowest_ns, max(1, took_ns))

    def ran_out(self, after_ns: int) -> None:
        self.answered(after_ns)


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


def converged(before: Sequence[float], after: Sequence[float]) -> bool:
    """Whether rates at twice the size (`after`) gain nothing past the spread at the size
    before: the median after is no higher than the highest before."""
    if not before or not after:
        return False
    ordered = sorted(after)
    return ordered[(len(ordered) - 1) // 2] <= max(before)
