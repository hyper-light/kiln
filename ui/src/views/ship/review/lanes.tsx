/**
 * The Review lane group drawn inside Ship's Lanes (W26-R14), as agreed with the Ship agent
 * (2026-10-26, NOTES.md): the item's marks (`ShipDetail.review`, oldest first) shaped as Ship's
 * generic lanes {group, label, state, note, lanes: [{id, name, state, start, end, note?, seal?}]},
 * `group` the stage key `review` — the first of STAGES — and drawn like Ship's own groups: the
 * group's glyph and name, then a lane per mark with its time, who did it and its seal — "unsigned"
 * while `signer` is null (until W27-R03). Data through Suspense, the same detail read Output makes.
 */
import { shipApi, type ShipCommit, type ShipMark } from "@/api/ship";
import { StatusGlyph } from "@/components/ui/status-glyph";
import type { GlyphState } from "@/components/ui/status-glyph-spec";
import { clockTime } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import "./review.css";

/** One lane of a Ship lane group (Fit/Detail): a span, or a point when end = start. */
export interface ShipLane {
  id: string;
  kind: ShipMark["kind"];
  name: string;
  state: string;
  start: number | null;
  end: number | null;
  by: string;
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

/** A mark's lane name, as the mock's ("v5 · pushed", "kim · comment", "approval · kim"). */
function nameOf(m: ShipMark): string {
  const v = m.revision != null ? `v${m.revision} · ` : "";
  switch (m.kind) {
    case "push":
      return m.revision != null ? `${v}${m.note === "rebase only" ? "rebased" : "pushed"}` : "pushed";
    case "replay":
      return `${v}replayed${m.note ? ` ${m.note}` : ""}`;
    case "comment": {
      // The server names a thread's marks `{thread}:open`, `{thread}:resolved`, `comment:{id}`.
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

/**
 * The marks as a group for Ship's Lanes: one lane per mark, the group's state the change's review
 * state (a commit with no change reads passed). The horizon starts at the earliest mark — Ship's.
 */
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

/** The Review lane group in the item's Lanes: its marks in order, each with time, who and seal. */
export function ReviewLanes({ commit }: { commit: ShipCommit }) {
  const detail = useSuspenseResource(`ship/commit/${commit.repo}/${commit.sha}`, (s) => shipApi.commit(commit.repo, commit.sha, s), { pollMs: 15_000, on: ["run", "job"] });
  const group = reviewGroup(detail.data);
  if (!group) return null;
  return (
    <div className="ship-lanegroup" data-group={group.group}>
      <p className="ship-lanegroup__head">
        <StatusGlyph state={group.state as GlyphState} size={12} />
        <span>{group.label}</span>
      </p>
      <ul className="ship-lanes">
        {group.lanes.map((l) => (
          <li key={l.id} className="ship-lane" data-kind={l.kind}>
            <StatusGlyph state={l.state as GlyphState} size={12} />
            <span className="ship-lane__name" title={l.note}>
              {l.name}
              {l.note && !l.name.includes(l.note) ? <em className="ship-lane__note"> {l.note}</em> : null}
            </span>
            <span className="ship-lane__by">{shown(l.by)}</span>
            <span className="ship-lane__when">
              {l.start == null ? "—" : clockTime(l.start * 1000)}
              {l.end != null && l.end !== l.start ? ` – ${clockTime(l.end * 1000)}` : ""}
            </span>
            <span className="ship-lane__seal">{l.seal ?? "unsigned"}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
