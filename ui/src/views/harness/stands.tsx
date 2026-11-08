/**
 * How your agents stand, in a row of its own under Mission control's head: a rule with a dot for
 * each agent, the dots of each way they stand together in a band, each band's arrowhead fitting
 * the notch of the one before, and under it each one's badge and count (on a desktop its name
 * too). Each dot stirs as its state does and each band plays a game of life of its own, lighting
 * and dimming its dots (stands-dots.ts). It's drawn each frame while it's on screen and motion
 * plays; paused (Kiln's toggle, or reduced motion) it holds still. The badges keep their own
 * steady animations (stands.css), each on its own.
 */
import * as React from "react";
import "./stands.css";
import { useMotionPreference } from "@/components/shell/motion";
import type { MissionRow, MissionStand as Stand } from "./mission-rows";
import { between, dotsFrame, layout } from "./stands-dots";
import { cells, GROW_MS, LOOK, newFrame, paint, RING, type Shown } from "./stands-light";

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
  archived: (n) => `${n} archived`,
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
  archived: "Archived",
};

/** Where `.sd-word` hides (stands.css): a phone, its counts' digits at the start of their slots. */
const NARROW = "(max-width: 640px)";
const watchNarrow = (changed: () => void) => {
  const m = window.matchMedia(NARROW);
  m.addEventListener("change", changed);
  return () => m.removeEventListener("change", changed);
};
const useNarrow = () => React.useSyncExternalStore(watchNarrow, () => window.matchMedia(NARROW).matches, () => false);

const cleanId = (id: string) => id.replace(/[^a-zA-Z0-9_-]/g, "");

/** A digit's width as the counts' tabular figures set it (Geist's 1 is narrower than its other digits even so), on the row as `--sd-dw`: every cell is the widest, so no count's change moves what's beside it. Measured once the row is laid out (as its observer hears of it: measuring as it's put in laid out the whole page there and then), and again once the fonts are in. */
function measureDigits(el: HTMLDivElement | null) {
  if (!el) return;
  const measure = () => {
    const probe = document.createElement("span");
    probe.className = "sd-n sd-probe";
    el.append(probe);
    const width = (s: string) => {
      probe.textContent = s;
      return probe.getBoundingClientRect().width;
    };
    const widest = Math.max(...[..."0123456789"].map((d) => width(d + d) - width(d)));
    probe.remove();
    if (widest > 0) el.style.setProperty("--sd-dw", `${widest}px`);
  };
  const laid = new ResizeObserver(() => {
    laid.disconnect();
    measure();
    void document.fonts?.ready.then(measure);
  });
  laid.observe(el);
}

/**
 * Mission control counts its rows. Another page (Ship) brings its own `counts`, and with `every`
 * shows each of those stands always, a zero dimmed, so nothing moves as counts come and go.
 */
export function Stands({ rows = [], counts, every }: { rows?: readonly MissionRow[]; counts?: Partial<Record<Shown, number>>; every?: readonly Shown[] }) {
  const count = (stand: Shown) => (counts ? (counts[stand] ?? 0) : rows.filter((r) => r.stand === stand).length);
  const shown = (every ?? SHOWN).map((stand) => ({ stand, n: count(stand) }));
  const groups = shown.filter((g) => g.n > 0);
  const listed = every ? shown : groups;
  const narrow = useNarrow();
  if (!every && rows.length === 0) return <p className="sd-lead">Start coding agents on any of your machines and run them all from here.</p>;
  if (!every && groups.length === 0)
    return (
      <div className="sd">
        <span className="sd-quiet">All quiet</span>
      </div>
    );
  return (
    <div className="sd" ref={measureDigits}>
      <p className="sr-only">{listed.map((g) => SAYS[g.stand](g.n)).join(", ")}</p>
      <Rule counts={groups.map((g) => `${g.stand}:${g.n}`).join(",")} />
      <ul className="sd-row" aria-hidden="true">
        {listed.map((g) => (
          <li key={g.stand} className="sd-item" data-stand={g.stand} data-zero={g.n === 0 || undefined} title={SAYS[g.stand](g.n)}>
            <Badge stand={g.stand} />
            <Count n={g.n} align={narrow ? "start" : "end"} />
            <span className="sd-word">{WORD[g.stand]}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** The bands shown: each way of standing and how many, in the order they read. */
type Bands = readonly (readonly [Shown, number])[];
/** The bands it grows from and to, since when, and its width as last laid out (kept for the next counts). */
type Grow = { from: Bands; to: Bands; at: number; width: number };

/** `needs:3,working:9` as bands, in the order they read. */
const parseCounts = (counts: string): Bands => {
  const by = new Map(
    counts
      .split(",")
      .filter(Boolean)
      .map((pair) => {
        const [stand, n] = pair.split(":");
        return [stand as Shown, Number(n)] as const;
      }),
  );
  return SHOWN.map((s) => [s, by.get(s) ?? 0] as const);
};

/** The moment a still rule is drawn at: each band's game where it stands then. */
const STILL = 1.3;

/**
 * The rule (stands-dots.ts), wired to its canvas as it's attached: drawn each frame (at most every
 * 33 ms) while it's on screen and motion plays; still, it's drawn as it stands, and again only as
 * it's resized or its counts change. A change of `counts` moves each dot to its new place, the new
 * ones coming in and the gone ones going, and the canvas is as tall as its rows.
 */
function Rule({ counts }: { counts: string }) {
  const { paused } = useMotionPreference();
  const grow = React.useRef<Grow>({ from: [], to: [], at: -GROW_MS, width: 0 });
  const attach = React.useCallback(
    (canvas: HTMLCanvasElement | null) => {
      const ctx = canvas?.getContext("2d");
      if (!canvas || !ctx) return;
      const g = grow.current;
      const begun = performance.now();
      // The first counts, or any while motion is still, are drawn as they are.
      const first = g.to.length === 0;
      g.from = first ? parseCounts(counts) : g.to;
      g.to = parseCounts(counts);
      g.at = first || paused ? begun - GROW_MS : begun;
      let raf = 0;
      let last = -Infinity;
      let visible = true;
      // Its width from its observer, which hears once it's laid out: asked here, as it's put in, it
      // would lay out the whole page there and then (Mission control's rows just put in with it).
      let width = g.width;
      const kept = newFrame();
      const draw = (now: number) => {
        if (width <= 0) return;
        const to = layout(g.to, width);
        const { dots, height } = between(layout(g.from, width), to, Math.min(1, (now - g.at) / GROW_MS));
        const dpr = Math.min(3, window.devicePixelRatio || 1);
        const w = Math.round(width * dpr);
        const h = Math.round(height * dpr);
        if (canvas.width !== w || canvas.height !== h) {
          canvas.width = w;
          canvas.height = h;
          canvas.style.height = `${height}px`;
        }
        ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
        ctx.clearRect(0, 0, width, height);
        paint(ctx, dotsFrame(dots, to.rows, paused ? STILL : now / 1000, kept));
      };
      const tick = (now: number) => {
        raf = 0;
        if (!visible) return;
        if (now - last >= 33) {
          last = now;
          draw(now);
        }
        if (!paused || now - g.at < GROW_MS) raf = requestAnimationFrame(tick);
      };
      const start = () => {
        if (!raf) raf = requestAnimationFrame(tick);
      };
      const sized = new ResizeObserver((entries) => {
        width = g.width = entries[0]?.contentRect.width ?? width;
        draw(performance.now());
      });
      const seen = new IntersectionObserver((entries) => {
        visible = entries.some((e) => e.isIntersecting);
        if (visible) start();
      });
      sized.observe(canvas);
      seen.observe(canvas);
      draw(begun);
      start();
      return () => {
        cancelAnimationFrame(raf);
        sized.disconnect();
        seen.disconnect();
      };
    },
    [counts, paused],
  );
  return <canvas ref={attach} className="sd-rule" aria-hidden="true" />;
}

/** What each state is, plainly, in its colour (14 × 14). Working's arc turns, needs you's bubble brightens and dims, ready's light breathes; the rest are still (stands.css). */
function Badge({ stand }: { stand: Shown }) {
  const id = cleanId(React.useId());
  const c = LOOK[stand].colour;
  const line = { stroke: c, strokeLinecap: "round" as const, strokeLinejoin: "round" as const, fill: "none" };
  const box = { viewBox: "0 0 14 14", "aria-hidden": true as const };
  switch (stand) {
    case "working": // at work: an arc turning, in its ring's colours
      return (
        <svg className="sd-mark sd-turning" {...box}>
          <defs>
            <linearGradient id={id} x1="0" x2="1" y1="0" y2="1">
              {RING.map(([colour, at]) => (
                <stop key={at} offset={at} stopColor={colour} />
              ))}
            </linearGradient>
          </defs>
          <circle cx="7" cy="7" r="5" stroke={`url(#${id})`} strokeWidth="1.8" strokeLinecap="round" fill="none" strokeDasharray="23.6 7.8" />
        </svg>
      );
    case "ready": // ready for work: a lit ring
      return (
        <svg className="sd-mark" {...box}>
          <circle cx="7" cy="7" r="5" {...line} strokeWidth="1.4" />
          <circle className="sd-breathing" cx="7" cy="7" r="2.3" fill={c} />
        </svg>
      );
    case "needs": // it's asking you something: a speech bubble, "!"
      return (
        <svg className="sd-mark sd-calling" {...box}>
          <path d="M3.2 1.8 H10.8 Q12.6 1.8 12.6 3.6 V8.2 Q12.6 10 10.8 10 H6.6 L3.8 12.4 V10 H3.2 Q1.4 10 1.4 8.2 V3.6 Q1.4 1.8 3.2 1.8 Z" {...line} strokeWidth="1.4" />
          <path d="M7 4 V6.6" {...line} strokeWidth="1.6" />
          <circle cx="7" cy="8.4" r="0.85" fill={c} />
        </svg>
      );
    case "paused": // paused: the pause bars
      return (
        <svg className="sd-mark" {...box}>
          <path d="M5 3 V11 M9 3 V11" {...line} strokeWidth="2" />
        </svg>
      );
    case "finished": // done: a tick
      return (
        <svg className="sd-mark" {...box}>
          <path d="M2.6 7.4 L5.6 10.2 L11.4 3.8" {...line} strokeWidth="1.8" />
        </svg>
      );
    case "crashed": // stopped: the stop square
      return (
        <svg className="sd-mark" {...box}>
          <rect x="3" y="3" width="8" height="8" rx="1.6" fill={c} />
        </svg>
      );
    case "offline": // offline: an empty ring, broken
      return (
        <svg className="sd-mark" {...box}>
          <circle cx="7" cy="7" r="5" {...line} strokeWidth="1.4" strokeDasharray="2.2 2.1" />
        </svg>
      );
  }
}

/**
 * A count: a cell a digit wide for each place, two at least, so 9 going to 10 moves nothing beside
 * it. Only a place whose digit changes rolls, up as the count rises and down as it falls
 * (stands-light.ts `cells`): each change makes its digits anew, so the roll plays once, and the old
 * digit ends hidden (stands.css).
 */
function Count({ n, align }: { n: number; align: "start" | "end" }) {
  const [roll, setRoll] = React.useState({ n, from: n });
  if (roll.n !== n) setRoll({ n, from: roll.n });
  return (
    <span className="sd-n">
      {cells(roll.from, roll.n, align).map((c, i) => (
        <span key={`${roll.n}-${i}`} className="sd-d">
          {c.was === c.now ? (
            <b>{c.now}</b>
          ) : (
            <>
              <b className={`sd-out sd-${c.dir}`}>{c.was}</b>
              <b className={`sd-in sd-${c.dir}`}>{c.now}</b>
            </>
          )}
        </span>
      ))}
    </span>
  );
}
