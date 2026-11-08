/**
 * Mission control's list as its sections and lines show it (attention plan 01a199e1b177752e84dad5dc990e6a15):
 * Needs you, your pins in a section of their own, the rest; and the one-line Undo where a row was.
 */
import type { AttentionCommand, AttentionUndo } from "@/api/attention";
import { intervention } from "./list-tools";
import type { MissionRow } from "./mission-rows";
import { untilWords } from "./attention-state";

/** One of the list's lines: a row, the Pinned section's head, the space that ends it, or the Undo where a row was. */
export type Entry = { key: string; row?: MissionRow; pinned?: number };

/**
 * The list's rows as their sections show them (when it's ordered for you, `sectioned`): Needs you,
 * then your pins in a section of their own (in your order), then the rest.
 */
export function entriesOf(rows: readonly MissionRow[], sectioned: boolean): Entry[] {
  const pinned = (r: MissionRow) => sectioned && r.pinned !== undefined && r.stand !== "archived" && intervention(r) === 2;
  const start = rows.findIndex(pinned);
  let end = start;
  if (start >= 0) while (end < rows.length && rows[end] && pinned(rows[end] as MissionRow)) end++;
  const out: Entry[] = [];
  rows.forEach((row, i) => {
    if (i === start) out.push({ key: "sep:pinned", pinned: end - start });
    if (start >= 0 && i === end) out.push({ key: "sep:rest" });
    out.push({ key: `row:${row.a.id}`, row });
  });
  return out;
}

/** Where a row was when you acted on it (the list it was in, what was before and after it, its height): its Undo stands there. */
export interface Anchor {
  list: string;
  prev: string | null;
  next: string | null;
  height: number;
}
/** The Undo where the row was: shown while nothing else was done since (`seq`), in this visit, as the list was filtered and sorted then (`shown`). */
export interface UndoState extends Anchor {
  seq: number;
  visit: string;
  shown: string;
  text: string;
  failed?: string;
  pending: boolean;
  undo?: () => void;
}

/** `entries` with the Undo line where its row was, when it's in this list: after what was before it, else before what was after it. */
export function withUndo(entries: Entry[], undo: UndoState | null, list: string): Entry[] {
  if (!undo || undo.list !== list) return entries;
  const keys = entries.map((e) => e.key);
  const after = undo.prev === null ? 0 : keys.indexOf(undo.prev) + 1;
  const before = undo.next === null ? -1 : keys.indexOf(undo.next);
  const at = after > 0 || undo.prev === null ? after : before >= 0 ? before : entries.length;
  return [...entries.slice(0, at), { key: "undo" }, ...entries.slice(at)];
}

/** What was done, said in the Undo's line; and what undoes it (the server's Undo, or the opposite action). */
export function described(commands: readonly AttentionCommand[], name: (id: string) => string, now: number): { done: string; doing: string } | null {
  const first = commands[0];
  if (!first || !("agent" in first)) return null;
  const who = commands.length === 1 ? `\u201c${name(first.agent)}\u201d` : `${commands.length} agents`;
  switch (first.action) {
    case "park":
      return { done: `${who} parked until ${untilWords(first.until, now)}`, doing: `park ${who}` };
    case "pin":
      return { done: `${who} pinned`, doing: `pin ${who}` };
    case "unpin":
      return { done: `${who} unpinned`, doing: `unpin ${who}` };
    case "unpark":
      return { done: `${who} unparked`, doing: `unpark ${who}` };
    case "archive":
      return { done: `${who} archived`, doing: `archive ${who}` };
    case "restore":
    case "restore_copy":
      return { done: `${who} restored`, doing: `restore ${who}` };
    default:
      return null;
  }
}
export function inverseOf(commands: readonly AttentionCommand[], undo: readonly AttentionUndo[], failed: ReadonlySet<string>): AttentionCommand[] {
  const went = commands.flatMap((c) => ("agent" in c && !failed.has(c.agent) ? [c.agent] : []));
  const first = commands[0];
  if (first?.action === "archive") return went.map((agent) => ({ action: "restore", agent }));
  if (first?.action === "restore" || first?.action === "restore_copy") return went.map((agent) => ({ action: "archive", agent }));
  return undo.map((u) => ({ action: "undo", agent: u.agent, revision: u.revision }));
}
