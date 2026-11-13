"""Order statistics for the report: n, p50, p90, p99 and max, by nearest rank.

Nearest rank is Hyndman and Fan's definition 1 ("Sample quantiles in statistical packages", The
American Statistician 50(4), 1996): the p-quantile of n sorted samples is the one at rank
ceil(p * n). Every value it reports is a sample that was measured, never an interpolation
between two.
"""

from __future__ import annotations

import math
from typing import Dict, Iterable, List, Optional

QUANTILES = (("p50", 0.50), ("p90", 0.90), ("p99", 0.99))


def rank(p: float, n: int) -> int:
    """The 1-based nearest rank of the p-quantile among n samples."""
    if n <= 0:
        raise ValueError("no samples")
    if not 0.0 < p <= 1.0:
        raise ValueError(f"quantile {p} is outside (0, 1]")
    return max(1, math.ceil(p * n))


def least_n(p: float) -> int:
    """The fewest samples whose nearest-rank p-quantile is not their maximum.

    ceil(p * n) <= n - 1 holds exactly when n >= 1 / (1 - p): for p99, 100 samples. A host whose
    p99 rests on fewer is reported as its max, and the report says so (`p99_is_max`).
    """
    if not 0.0 < p < 1.0:
        raise ValueError(f"quantile {p} is outside (0, 1)")
    return math.ceil(round(1.0 / (1.0 - p), 9))


def summary(samples: Iterable[float]) -> Optional[Dict[str, float]]:
    """n, min, p50, p90, p99 and max of the samples, or None when there are none."""
    ordered: List[float] = sorted(samples)
    n = len(ordered)
    if n == 0:
        return None
    out: Dict[str, float] = {"n": n, "min": ordered[0]}
    for name, p in QUANTILES:
        out[name] = ordered[rank(p, n) - 1]
    out["max"] = ordered[-1]
    out["p99_is_max"] = n < least_n(0.99)
    return out
