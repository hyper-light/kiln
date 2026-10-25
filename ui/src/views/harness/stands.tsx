/**
 * How your agents stand, in a row of its own under Mission control's head: a rule with a stretch
 * for each way they stand, as long as its share of them, and under it each one's badge and count
 * (on a desktop its name too). The rule is one wave across every stretch, as lively in each as its
 * agents, each stretch in its own colour and mark and reaching for the next across the gap between
 * them (stands-wave.ts). It's drawn each frame while it's on screen and motion plays; paused (Kiln's
 * toggle, or reduced motion) it holds still.
 */
import * as React from "react";
import "./stands.css";
import { useMotionPreference } from "@/components/shell/motion";
import type { MissionRow, MissionStand as Stand } from "./mission-rows";
import { cells, draw, easeOut, GROW_MS, HEIGHT, LOOK, PRISM, stretches, type Shown } from "./stands-wave";

/** The ways counted, in the order they read (ended agents aren't). */
const SHOWN: readonly Shown[] = ["needs", "crashed", "paused", "working", "finished", "ready", "offline"];

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

/** Where `.st-word` hides (stands.css): a phone, its counts' digits at the start of their slots. */
const NARROW = "(max-width: 640px)";
const watchNarrow = (changed: () => void) => {
  const m = window.matchMedia(NARROW);
  m.addEventListener("change", changed);
  return () => m.removeEventListener("change", changed);
};
const useNarrow = () => React.useSyncExternalStore(watchNarrow, () => window.matchMedia(NARROW).matches, () => false);

const cleanId = (id: string) => id.replace(/[^a-zA-Z0-9_-]/g, "");

/** A digit's width as the counts' tabular figures set it (Geist's 1 is narrower than its other digits even so), on the row as `--st-dw`: every cell is the widest, so no count's change moves what's beside it. Measured again once the fonts are in. */
function measureDigits(el: HTMLDivElement | null) {
  if (!el) return;
  const measure = () => {
    const probe = document.createElement("span");
    probe.className = "st-n st-probe";
    el.append(probe);
    const width = (s: string) => {
      probe.textContent = s;
      return probe.getBoundingClientRect().width;
    };
    const widest = Math.max(...[..."0123456789"].map((d) => width(d + d) - width(d)));
    probe.remove();
    if (widest > 0) el.style.setProperty("--st-dw", `${widest}px`);
  };
  measure();
  void document.fonts?.ready.then(measure);
}

export function Stands({ rows }: { rows: readonly MissionRow[] }) {
  const groups = SHOWN.map((stand) => ({ stand, n: rows.filter((r) => r.stand === stand).length })).filter((g) => g.n > 0);
  const narrow = useNarrow();
  if (rows.length === 0) return <p className="st-lead">Start coding agents on any of your machines and run them all from here.</p>;
  if (groups.length === 0)
    return (
      <div className="st">
        <span className="st-quiet">All quiet</span>
      </div>
    );
  return (
    <div className="st" ref={measureDigits}>
      <p className="sr-only">{groups.map((g) => SAYS[g.stand](g.n)).join(", ")}</p>
      <Rule counts={groups.map((g) => `${g.stand}:${g.n}`).join(",")} />
      <ul className="st-row" aria-hidden="true">
        {groups.map((g) => (
          <li key={g.stand} className="st-item" data-stand={g.stand} title={SAYS[g.stand](g.n)}>
            <Badge stand={g.stand} />
            <Count n={g.n} align={narrow ? "start" : "end"} />
            <span className="st-word">{WORD[g.stand]}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

type Grow = { from: Map<Shown, number>; to: Map<Shown, number>; at: number };

/** The counts drawn at `now`: easing from those drawn when they last changed to these. */
function countsAt(g: Grow, now: number): Map<Shown, number> {
  const k = easeOut((now - g.at) / GROW_MS);
  const out = new Map<Shown, number>();
  for (const s of SHOWN) {
    const a = g.from.get(s) ?? 0;
    const b = g.to.get(s) ?? 0;
    const v = a + (b - a) * k;
    if (v > 0.001) out.set(s, v);
  }
  return out;
}

/** `needs:3,working:9` as counts by way of standing. */
const parseCounts = (counts: string) =>
  new Map(
    counts
      .split(",")
      .filter(Boolean)
      .map((pair): [Shown, number] => {
        const [stand, n] = pair.split(":");
        return [stand as Shown, Number(n)];
      }),
  );

/**
 * The rule (stands-wave.ts), wired to its SVG as it's attached: drawn each frame (at most every
 * 25 ms) while it's on screen and motion plays; still, it's drawn at rest, and again only as it's
 * resized or its counts change. A change of `counts` eases its stretches to their new lengths.
 */
function Rule({ counts }: { counts: string }) {
  const id = cleanId(React.useId());
  const { paused } = useMotionPreference();
  const grow = React.useRef<Grow>({ from: new Map(), to: new Map(), at: -GROW_MS });
  const attach = React.useCallback(
    (svg: SVGSVGElement | null) => {
      if (!svg) return;
      const g = grow.current;
      const begun = performance.now();
      g.from = countsAt(g, begun);
      g.to = parseCounts(counts);
      // The first counts, or any while motion is still, are drawn as they are.
      g.at = g.from.size === 0 || paused ? begun - GROW_MS : begun;
      let raf = 0;
      let last = -Infinity;
      let visible = true;
      let width = svg.getBoundingClientRect().width;
      const frame = (now: number) => {
        const shown = countsAt(g, now);
        const stands = SHOWN.filter((s) => shown.has(s));
        if (width <= 0 || stands.length === 0) return;
        draw(svg, stretches(stands, stands.map((s) => shown.get(s) ?? 0), width), width, paused ? 0 : now / 1000, id);
      };
      const tick = (now: number) => {
        raf = 0;
        if (!visible) return;
        if (now - last >= 25) {
          last = now;
          frame(now);
        }
        if (!paused || now - g.at < GROW_MS) raf = requestAnimationFrame(tick);
      };
      const start = () => {
        if (!raf) raf = requestAnimationFrame(tick);
      };
      const sized = new ResizeObserver((entries) => {
        width = entries[0]?.contentRect.width ?? width;
        frame(performance.now());
      });
      const seen = new IntersectionObserver((entries) => {
        visible = entries.some((e) => e.isIntersecting);
        if (visible) start();
      });
      sized.observe(svg);
      seen.observe(svg);
      frame(begun);
      start();
      return () => {
        cancelAnimationFrame(raf);
        sized.disconnect();
        seen.disconnect();
      };
    },
    [counts, paused, id],
  );
  return <svg ref={attach} className="st-rule" viewBox={`0 0 100 ${HEIGHT}`} preserveAspectRatio="none" aria-hidden="true" />;
}

/** What each state is, plainly, in its colour (14 × 14). Working's arc turns, needs you's bubble brightens and dims, ready's light breathes; the rest are still (stands.css). */
function Badge({ stand }: { stand: Shown }) {
  const id = cleanId(React.useId());
  const c = LOOK[stand].colour;
  const line = { stroke: c, strokeLinecap: "round" as const, strokeLinejoin: "round" as const, fill: "none" };
  const box = { viewBox: "0 0 14 14", "aria-hidden": true as const };
  switch (stand) {
    case "working": // at work: an arc turning, in the prism
      return (
        <svg className="st-mark st-turning" {...box}>
          <defs>
            <linearGradient id={id} x1="0" x2="1" y1="0" y2="1">
              {PRISM.map((p, k) => (
                <stop key={p} offset={k / (PRISM.length - 1)} stopColor={p} />
              ))}
            </linearGradient>
          </defs>
          <circle cx="7" cy="7" r="5" stroke={`url(#${id})`} strokeWidth="1.8" strokeLinecap="round" fill="none" strokeDasharray="23.6 7.8" />
        </svg>
      );
    case "ready": // ready for work: a lit ring
      return (
        <svg className="st-mark" {...box}>
          <circle cx="7" cy="7" r="5" {...line} strokeWidth="1.4" />
          <circle className="st-breathing" cx="7" cy="7" r="2.3" fill={c} />
        </svg>
      );
    case "needs": // it's asking you something: a speech bubble, "!"
      return (
        <svg className="st-mark st-calling" {...box}>
          <path d="M3.2 1.8 H10.8 Q12.6 1.8 12.6 3.6 V8.2 Q12.6 10 10.8 10 H6.6 L3.8 12.4 V10 H3.2 Q1.4 10 1.4 8.2 V3.6 Q1.4 1.8 3.2 1.8 Z" {...line} strokeWidth="1.4" />
          <path d="M7 4 V6.6" {...line} strokeWidth="1.6" />
          <circle cx="7" cy="8.4" r="0.85" fill={c} />
        </svg>
      );
    case "paused": // paused: the pause bars
      return (
        <svg className="st-mark" {...box}>
          <path d="M5 3 V11 M9 3 V11" {...line} strokeWidth="2" />
        </svg>
      );
    case "finished": // done: a tick
      return (
        <svg className="st-mark" {...box}>
          <path d="M2.6 7.4 L5.6 10.2 L11.4 3.8" {...line} strokeWidth="1.8" />
        </svg>
      );
    case "crashed": // stopped: the stop square
      return (
        <svg className="st-mark" {...box}>
          <rect x="3" y="3" width="8" height="8" rx="1.6" fill={c} />
        </svg>
      );
    case "offline": // offline: an empty ring, broken
      return (
        <svg className="st-mark" {...box}>
          <circle cx="7" cy="7" r="5" {...line} strokeWidth="1.4" strokeDasharray="2.2 2.1" />
        </svg>
      );
  }
}

/**
 * A count: a cell a digit wide for each place, two at least, so 9 going to 10 moves nothing beside
 * it. Only a place whose digit changes rolls, up as the count rises and down as it falls
 * (stands-wave.ts `cells`): each change makes its digits anew, so the roll plays once, and the old
 * digit ends hidden (stands.css).
 */
function Count({ n, align }: { n: number; align: "start" | "end" }) {
  const [roll, setRoll] = React.useState({ n, from: n });
  if (roll.n !== n) setRoll({ n, from: roll.n });
  return (
    <span className="st-n">
      {cells(roll.from, roll.n, align).map((c, i) => (
        <span key={`${roll.n}-${i}`} className="st-d">
          {c.was === c.now ? (
            <b>{c.now}</b>
          ) : (
            <>
              <b className={`st-out st-${c.dir}`}>{c.was}</b>
              <b className={`st-in st-${c.dir}`}>{c.now}</b>
            </>
          )}
        </span>
      ))}
    </span>
  );
}
