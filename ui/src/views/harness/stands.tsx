/**
 * How your agents stand, in a row of its own under Mission control's head: a spectral rule with a
 * length for each way they stand, as long as its share of them, and under it each one's count (on a
 * desktop its name too). Only what goes on moves (stands.css): working's length carries the prism
 * flowing along it, and needing you's brightens and dims.
 */
import "./stands.css";
import type { MissionRow, MissionStand as Stand } from "./mission-rows";

/** The ways counted, in the order they read (ended agents aren't). */
const SHOWN: readonly Stand[] = ["needs", "crashed", "paused", "working", "finished", "ready", "offline"];

const SAYS: Record<Stand, (n: number) => string> = {
  needs: (n) => `${n} need${n === 1 ? "s" : ""} you`,
  crashed: (n) => `${n} stopped`,
  paused: (n) => `${n} paused`,
  working: (n) => `${n} working`,
  finished: (n) => `${n} finished`,
  ready: (n) => `${n} ready`,
  offline: (n) => `${n} offline`,
  ended: (n) => `${n} ended`,
};

const WORD: Record<Stand, string> = {
  needs: "Needs you",
  crashed: "Stopped",
  paused: "Paused",
  working: "Working",
  finished: "Finished",
  ready: "Ready",
  offline: "Offline",
  ended: "Ended",
};

export function Stands({ rows }: { rows: readonly MissionRow[] }) {
  const groups = SHOWN.map((stand) => ({ stand, n: rows.filter((r) => r.stand === stand).length })).filter((g) => g.n > 0);
  if (rows.length === 0) return <p className="st-lead">Start coding agents on any of your machines and run them all from here.</p>;
  if (groups.length === 0)
    return (
      <div className="st">
        <span className="st-quiet">All quiet</span>
      </div>
    );
  return (
    <div className="st">
      <p className="sr-only">{groups.map((g) => SAYS[g.stand](g.n)).join(", ")}</p>
      <span className="st-rule" aria-hidden="true">
        {groups.map((g) => (
          <i key={g.stand} data-stand={g.stand} style={{ flexGrow: g.n }} />
        ))}
      </span>
      <ul className="st-row" aria-hidden="true">
        {groups.map((g) => (
          <li key={g.stand} className="st-item" data-stand={g.stand} title={SAYS[g.stand](g.n)}>
            <i className="st-tick" />
            <span className="st-n">{g.n}</span>
            <span className="st-word">{WORD[g.stand]}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
