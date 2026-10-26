/**
 * What benchmark gates said on a commit (docs/ci.md W16-R09): on a diff, beside its checks, and on
 * a land queue entry's candidate. Each with its level (as the run page marks annotations) and its
 * job, which opens the run.
 */
import type { BenchGate } from "@/api/review";
import { Link } from "@/lib/router";

export function Gates({ gates }: { gates: BenchGate[] }) {
  return (
    <ul className="rv-gates" aria-label="Benchmark gates">
      {gates.map((g, i) => (
        <li key={`${g.leg}\u0000${i}`}>
          <i className="dot" data-status={g.level === "error" ? "failed" : "timed_out"} aria-hidden="true" />
          <span className="rv-gate-level">{g.level}</span>
          <Link to={`/runs/${encodeURIComponent(g.run)}`}>{g.job}</Link>
          <span className="rv-sub">{g.message}</span>
        </li>
      ))}
    </ul>
  );
}
