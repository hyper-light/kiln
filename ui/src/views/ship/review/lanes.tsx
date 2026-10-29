/**
 * Ship's Lanes (W26-R14, W26-R15), drawn as f-ship's lanes: each group {group, label, state, note,
 * lanes[]} given to it — Review's marks first (reviewGroup), Ship's stage groups as they land —
 * every lane a bar on one horizon, at least 30 minutes wide and growing by 10 as marks need, its
 * label pinned left, the bar wearing its rim, "unsigned" on every seal until W27-R03, its key the
 * mark's id so a live update moves it, never remounts it. Fit squeezes rows; Detail gives each bar
 * its room and its who/when/where. Previous/Now/next walk the horizon. No effects: times tick on
 * the resource's poll; the horizon holds while the row is open, until Now catches it up.
 */
import { shipApi, type ShipCommit, type ShipMark } from "@/api/ship";
import { glyphOf, shipGroups } from "../model";
import { StatusGlyph } from "@/components/ui/status-glyph";
import { clockTime } from "@/lib/format";
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

/** One group's lanes on the shared horizon: its head glyph and label pinned, lanes sorted as given. */
function LaneGroup({ group, window, now }: { group: ShipLaneGroup; window: [number, number]; now: number }) {
  return (
    <div className="ship-lanegroup" data-group={group.group}>
      <p className="ship-lanegroup__head">
        <StatusGlyph state={glyphOf(group.state)} size={12} />
        <span>{group.label}</span>
        {group.note ? <em className="ship-lane__note">{group.note}</em> : null}
      </p>
      <ul className="ship-lanes">
        {group.lanes.map((l) => (
          <LaneBar key={l.id} lane={l} window={window} now={now} />
        ))}
      </ul>
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
      <div className="ship-lanesbox" data-fit={fit || undefined}>
        <span className="ship-lanesbox__span">
          {clockTime(window[0] * 1000)} – {clockTime(window[1] * 1000)}
        </span>
        {groups.map((g) => (
          <LaneGroup key={g.group} group={g} window={window} now={now} />
        ))}
      </div>
    </>
  );
}
