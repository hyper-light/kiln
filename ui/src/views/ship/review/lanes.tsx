/**
 * Ship's Lanes (W26-R14, W26-R15), drawn as f-ship's lanes: each group {group, label, state, note,
 * lanes[]} given to it — Review's marks first (reviewGroup), Ship's stage groups as they land —
 * every lane a bar on one horizon, at least 30 minutes wide and growing by 10 as marks need, its
 * key the mark's id so a live update moves it, never remounts it. In the opened commit (plan
 * 01a1856fe81375b1bd1d1d86f270a45b, step 10) as f-ship draws them: a label column pinned left (each
 * group's name, each lane's glyph and name) beside a timeline with its axis, a bar for each lane
 * from its start to its end (to now while under way), a point for an instant, and the now line.
 * Fit draws the horizon in the room there is; Detail at 140 px a minute, scrolling sideways in its
 * card, each lane's time and seal ("unsigned" until W27-R03; W26-R14) beside its name.
 * Previous/Now/next walk the horizon. No effects: times tick on the resource's poll; the horizon
 * holds while the row is open, until Now catches it up.
 */
import { shipApi, type ShipCommit, type ShipMark } from "@/api/ship";
import { glyphOf, shipGroups } from "../model";
import { StatusGlyph } from "@/components/ui/status-glyph";
import { clockTime, formatDuration } from "@/lib/format";
import { GLYPHS } from "@/components/ui/status-glyph-spec";
import { useSuspenseResource } from "@/lib/resource";
import { Button } from "@/components/ui/button";
import { Segmented } from "@/components/ui/tabs";
import { ChevronLeft, ChevronRight } from "lucide-react";
import { SectionHead } from "../head";
import * as React from "react";
import "./review.css";

/** One lane of a Ship lane group (f-ship): a span, or a point when end = start. */
export interface ShipLane {
  id: string;
  kind?: string;
  name: string;
  state: string;
  start: number | null;
  end: number | null;
  by?: string;
  note?: string;
  /** The mark's signer; null = unsigned until W27-R03, a name once signed. */
  seal?: string | null;
  /** The job a Checks leg ran, once it has: what Output shows when it's picked (plan
   * 01a1856fe81375b1bd1d1d86f270a45b, step 8). */
  job?: string;
}

export interface ShipLaneGroup {
  group: string;
  label: string;
  state: string;
  note: string;
  lanes: ShipLane[];
}

/** A principal's name as a lane shows it (`user:kim` is kim, `agent:x for user:y` is x). */
const shown = (p: string) => p.split(" for ")[0]?.split(":").pop() ?? p;

/** A mark's lane name ("v5 · pushed", "kim · comment", "approval · kim"). */
function nameOf(m: ShipMark): string {
  const v = m.revision != null ? `v${m.revision} · ` : "";
  switch (m.kind) {
    case "push":
      return m.revision != null ? `${v}${m.note === "rebase only" ? "rebased" : "pushed"}` : "pushed";
    case "replay":
      return `${v}replayed${m.note ? ` ${m.note}` : ""}`;
    case "comment": {
      const role = m.id.endsWith(":resolved") ? "resolved" : m.note === "answered" ? "reply" : "comment";
      return `${shown(m.by)} · ${role}`;
    }
    case "fix":
      return `${v}${m.note ?? "fix"}`;
    case "resolution":
      return `${v}conflict resolved`;
    case "approval":
      return `approval · ${shown(m.by)}${m.revision != null ? ` (v${m.revision})` : ""}`;
    default:
      return `${v}${m.kind}`;
  }
}

/** The marks as the Review group for Lanes: one lane per mark, oldest first; the group's state the
 * change's review state (a commit with no change reads passed). */
export function reviewGroup(detail: { change?: { state?: string | null } | null; review?: ShipMark[] }): ShipLaneGroup | null {
  const marks = detail.review ?? [];
  if (marks.length === 0 && !detail.change) return null;
  return {
    group: "review",
    label: "Review",
    state: detail.change?.state ?? "passed",
    note: "",
    lanes: marks.map((m) => ({ id: m.id, kind: m.kind, name: nameOf(m), state: m.state, start: m.at, end: m.end, by: m.by, note: m.note, seal: m.signer })),
  };
}

const MIN_HORIZON = 30 * 60;
const GROW_BY = 10 * 60;

/** The window the lanes draw on: from the earliest start, at least 30 minutes wide and 10 more at a
 * time as now or a mark passes its end, so the bars keep their scale as time passes but for a
 * 10-minute step (the opened commit's step 2). `now` is the caller's — no clock inside. */
export function horizonOf(groups: ShipLaneGroup[], now: number): [number, number] {
  let lo = now;
  for (const g of groups) for (const l of g.lanes) if (l.start != null && l.start < lo) lo = l.start;
  let hi = now;
  for (const g of groups) for (const l of g.lanes) if (l.end != null && l.end > hi) hi = l.end;
  return [lo, lo + Math.max(MIN_HORIZON, Math.ceil((hi - lo) / GROW_BY) * GROW_BY)];
}

/** One lane's bar: its span on the horizon, or a tick for a point. `pos` is 0..1. */
const pos = (t: number, [lo, hi]: [number, number]) => Math.min(1, Math.max(0, (t - lo) / (hi - lo)));

/** A lane's words: name and, in Detail, who did it and when (and its place when it has one). */
export function LaneBar({ lane, window, now }: { lane: ShipLane; window: [number, number]; now: number }) {
  const [lo, hi] = window;
  const a = lane.start == null ? null : pos(lane.start, [lo, hi]);
  // A bar under way runs to now, not to the window's end.
  const b = pos(lane.end ?? now, [lo, hi]);
  return (
    <li className="ship-lane" data-kind={lane.kind}>
      <StatusGlyph state={glyphOf(lane.state)} size={12} />
      <span className="ship-lane__name" title={lane.note}>
        {lane.name}
        {lane.note && !lane.name.includes(lane.note) ? <em className="ship-lane__note"> {lane.note}</em> : null}
      </span>
      <span className="ship-lane__track">
        {a == null ? (
          <i className="ship-lane__now" style={{ left: `${pos(now, [lo, hi]) * 100}%` }} />
        ) : b - a < 0.004 ? (
          <i className="ship-lane__bar ship-lane__bar--point" data-state={lane.state} style={{ left: `${a * 100}%` }} />
        ) : (
          <i className="ship-lane__bar" data-state={lane.state} style={{ left: `${a * 100}%`, width: `${(b - a) * 100}%` }} />
        )}
      </span>
      <span className="ship-lane__who">{lane.by ? shown(lane.by) : ""}</span>
      <span className="ship-lane__when">
        {lane.start == null ? "—" : clockTime(lane.start * 1000)}
        {lane.end != null && lane.end !== lane.start ? ` – ${clockTime(lane.end * 1000)}` : ""}
      </span>
      <span className="ship-lane__seal">{lane.seal ?? "unsigned"}</span>
    </li>
  );
}

/** Px a second in Detail (f-ship's: 140 a minute), the track at least 480 px. */
const PXS = 140 / 60;

/** The axis's ticks from the horizon's start: in Fit, a step that makes six or fewer; in Detail, each minute. */
function ticksOf(span: number, detail: boolean): number[] {
  const step = detail ? 60 : ([300, 600, 900, 1800, 3600].find((x) => span / x <= 6) ?? 3600);
  const out: number[] = [];
  for (let t = 0; t <= span; t += step) out.push(t);
  return out;
}

/** A tick's word: "+0s", "+5m", "+1h 00m". */
const plus = (s: number) => `+${formatDuration(s).replace(" 00s", "")}`;

/** One lane's row: its glyph and name pinned (in Detail its time and seal too), its bar on the track. */
function LaneRow({ group, lane, at, span, detail, now }: { group: string; lane: ShipLane; at: (t: number) => string; span: (a: number, b: number) => string; detail: boolean; now: number }) {
  const a = lane.start;
  // A bar under way runs to now, not to the horizon's end; one not begun has none.
  const b = Math.max(a ?? 0, lane.end != null && lane.end <= now ? lane.end : now);
  const when = a == null ? "—" : `${clockTime(a * 1000)}${lane.end != null && lane.end !== a ? ` – ${clockTime(lane.end * 1000)}` : ""}`;
  const word = [lane.name, GLYPHS[glyphOf(lane.state)].word, a == null ? null : when, lane.kind ? (lane.seal ?? "unsigned") : null].filter(Boolean).join(" · ");
  return (
    <div className="ship-lrow" data-group={group} data-lane={lane.id} data-kind={lane.kind} data-start={a ?? undefined} data-end={lane.end ?? undefined}>
      <span className="ship-ll" title={lane.note}>
        <StatusGlyph state={glyphOf(lane.state)} size={12} />
        <span className="ship-ll__name">{lane.name}</span>
        {detail ? (
          <>
            <span className="ship-ll__when">{when}</span>
            {lane.kind ? <span className="ship-ll__seal">{lane.seal ?? "unsigned"}</span> : null}
          </>
        ) : null}
      </span>
      <span className="ship-lt">
        {a != null && a <= now ? (
          <i className="ship-lbar" data-state={glyphOf(lane.state)} data-done={(lane.end != null && lane.end <= now) || undefined} style={{ left: at(a), width: span(a, b) }} title={word} />
        ) : null}
      </span>
    </div>
  );
}

/** The dropdown's Lanes: the item's groups on one horizon — Review's marks first, Ship's own as
 * they arrive — with Previous / Now / next walking it and Fit/Detail picking the room. */
export function ShipLanes({ commit }: { commit: ShipCommit }) {
  const detail = useSuspenseResource(`ship/commit/${commit.repo}/${commit.sha}`, (s) => shipApi.commit(commit.repo, commit.sha, s), { pollMs: 15_000, on: ["run", "job"] });
  const [walk, setWalk] = React.useState(0);
  const [fit, setFit] = React.useState(true);
  const groups = React.useMemo(() => [reviewGroup(detail.data), ...shipGroups(detail.data)].filter((g): g is ShipLaneGroup => g != null && g.lanes.length > 0), [detail.data]);
  const now = Math.floor(Date.now() / 1000);
  const fresh = horizonOf(groups, now);
  // The horizon holds while the row is open, as the list holds its rows: what arrives moves no bar
  // (Agent pace and checks, step 2). A bar under way runs on to the edge; Now takes the horizon as it
  // is now, and a step walks a third of it.
  const [base, setBase] = React.useState(fresh);
  const behind = base[0] !== fresh[0] || base[1] !== fresh[1];
  const step = Math.max(1, Math.round((base[1] - base[0]) / 3));
  const window: [number, number] = walk === 0 ? base : [base[0] + walk * step, base[1] + walk * step];
  if (groups.length === 0) return <SectionHead label="Lanes" />;
  const [lo, hi] = window;
  const width = Math.max(1, hi - lo);
  const W = Math.max(480, width * PXS);
  // Where a time falls on the track, and how long a span is: a share of it in Fit, px in Detail.
  const at = (t: number) => (fit ? `${(((t - lo) / width) * 100).toFixed(3)}%` : `${((t - lo) * PXS).toFixed(1)}px`);
  const span = (a: number, b: number) => (fit ? `${Math.max(0.4, ((b - a) / width) * 100).toFixed(3)}%` : `${Math.max(4, (b - a) * PXS).toFixed(1)}px`);
  const live = groups.some((g) => g.lanes.some((l) => l.start != null && l.start <= now && (l.end == null || l.end > now)));
  return (
    <>
      <SectionHead label="Lanes">
        <Segmented
          size="xs"
          label="Lanes' scale"
          value={fit ? "fit" : "detail"}
          onChange={(v) => setFit(v === "fit")}
          options={[
            { value: "fit", label: "Fit" },
            { value: "detail", label: "Detail" },
          ]}
        />
        <Button variant="secondary" size="icon-sm" aria-label="Earlier" onClick={() => setWalk(walk - 1)}>
          <ChevronLeft />
        </Button>
        <Button
          variant="secondary"
          size="xs"
          aria-label="Now"
          disabled={walk === 0 && !behind}
          onClick={() => {
            setWalk(0);
            setBase(fresh);
          }}
        >
          Now
        </Button>
        <Button variant="secondary" size="icon-sm" aria-label="Later" onClick={() => setWalk(walk + 1)}>
          <ChevronRight />
        </Button>
      </SectionHead>
      <div className="ship-lanesbox" data-fit={fit || undefined} data-horizon={`${lo}-${hi}`}>
        <div className="ship-lscroll">
          <div className="ship-lin" style={fit ? undefined : ({ "--track": `${Math.round(W)}px` } as React.CSSProperties)}>
            <div className="ship-lrow ship-lrow--axis" aria-hidden="true">
              <span className="ship-ll" />
              <span className="ship-lt">
                {ticksOf(width, !fit).map((t) => (
                  <span key={t} style={{ left: at(lo + t) }}>
                    {plus(t)}
                  </span>
                ))}
              </span>
            </div>
            {groups.map((g) => (
              <React.Fragment key={g.group}>
                <div className="ship-lrow ship-lrow--g" data-group={g.group}>
                  <span className="ship-ll">{g.label}</span>
                  <span className="ship-lt" />
                </div>
                {g.lanes.map((l) => (
                  <LaneRow key={l.id} group={g.group} lane={l} at={at} span={span} detail={!fit} now={now} />
                ))}
              </React.Fragment>
            ))}
            {/* The now line while any lane is under way: moved by transform, so nothing it passes shifts. */}
            {live ? (
              <span className="ship-lnow" aria-hidden="true">
                <i style={{ transform: `translateX(${(Math.min(1, Math.max(0, (now - lo) / width)) * 100).toFixed(3)}%)` }} />
              </span>
            ) : null}
          </div>
        </div>
      </div>
    </>
  );
}
