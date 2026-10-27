/**
 * The Review lane group (W26-R14), as agreed with the Ship agent (2026-10-26, NOTES.md): the
 * item's marks (`ShipDetail.review`, oldest first) shaped as Ship's generic lanes
 * {group, label, state, note, lanes: [{id, name, state, start, end, note?, seal?}]}. `group` is
 * the stage key `review`, the first of STAGES. Pure; Ship's Lanes draws it.
 */
import type { ReviewMark } from "@/api/review";

/** One lane of a Ship lane group (Fit/Detail): a span, or a point when end = start. */
export interface ShipLane {
  id: string;
  name: string;
  state: string;
  start: number | null;
  end: number | null;
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
function nameOf(m: ReviewMark): string {
  const v = m.revision != null ? `v${m.revision} · ` : "";
  switch (m.kind) {
    case "push":
      return m.revision != null ? `${v}${m.note === "rebase only" ? "rebased" : "pushed"}` : "pushed";
    case "replay":
      return `${v}replayed${m.note ? ` ${m.note}` : ""}`;
    case "comment":
      return `${shown(m.by)} · ${m.note === "answered" ? "reply" : m.note === "resolved" || m.state === "passed" ? "resolved" : "comment"}`;
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
export function reviewGroup(detail: { change?: { state?: string | null } | null; review?: ReviewMark[] }): ShipLaneGroup | null {
  const marks = detail.review ?? [];
  if (marks.length === 0 && !detail.change) return null;
  return {
    group: "review",
    label: "Review",
    state: detail.change?.state ?? "passed",
    note: "",
    lanes: marks.map((m) => ({ id: m.id, name: nameOf(m), state: m.state, start: m.at, end: m.end, note: m.note, seal: m.signer })),
  };
}
