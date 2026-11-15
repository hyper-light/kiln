/**
 * Mission control's attention snapshot, read lean (W21-R02; docs/agents.md, "The snapshot, lean"): the server names
 * each agent once (`ids`) and every list of agents by their places in that (`crates/kiln/src/harness/attention/lean.rs`),
 * writes the marks and the archived as columns (api/columns.ts), leaves out what the list's page doesn't read
 * (`without`), and carries the last words of the rows the list opens on when it's asked for them (`said`).
 * Read back here, at the API's edge: the snapshot as every reader of one knows it.
 */
import { fromColumn, fromColumns, type Column, type Columns } from "./columns";
import type { AttentionMark, AttentionSnapshot } from "./attention";
import type { AgentInfo } from "./harness";

/** A list of agents by place in `ids`: the first n of them in their order as n, else each one's place. */
type Places = number | number[];

export interface LeanSnapshot extends Omit<AttentionSnapshot, "agents" | "order" | "request_order" | "pair" | "archived"> {
  /** Every agent the snapshot names, once, the order's first: a column of its own, each id in its `v`, as they're all different. */
  ids: Column;
  order: Places;
  request_order: Places;
  /** The marks as columns; `at`: whose they are, when they aren't `ids`' first in their order. */
  agents: Columns & { at?: number[] };
  pair: (Omit<NonNullable<AttentionSnapshot["pair"]>, "order_a" | "order_b"> & { order_a: Places; order_b: Places }) | null;
  archived: Columns;
  /** The last words of the rows the list opens on (its first, and its last): whose (`at`) and what (`v`). */
  said?: { at: number[]; v: string[] };
  without: string[];
}

/** Whether `read` is a lean snapshot (a server from before them, and an action's answer, is the snapshot itself). */
export const isLean = (read: AttentionSnapshot | LeanSnapshot): read is LeanSnapshot => "ids" in read && "without" in read;

/** The snapshot `lean` holds, and the last words it carries, by agent. */
export function fromLean(lean: LeanSnapshot): { snapshot: AttentionSnapshot; said: Record<string, string> } {
  const { ids: listed, said: carried, order, request_order, agents, pair, archived, ...rest } = lean;
  const ids = fromColumn<string>(listed, listed.v?.length ?? 0);
  const named = (places: Places): string[] => (typeof places === "number" ? ids.slice(0, places) : places.map((at) => ids[at] ?? ""));
  const marks = fromColumns<AttentionMark>(agents);
  const whose = named(agents.at ?? marks.length);
  const said: Record<string, string> = {};
  carried?.at.forEach((at, i) => {
    const id = ids[at];
    if (id !== undefined) said[id] = carried.v[i] ?? "";
  });
  const snapshot: AttentionSnapshot & { without?: string[] } = {
    ...rest,
    order: named(order),
    request_order: named(request_order),
    agents: Object.fromEntries(marks.map((mark, i) => [whose[i] ?? "", mark])),
    pair: pair ? { ...pair, order_a: named(pair.order_a), order_b: named(pair.order_b) } : pair,
    archived: fromColumns<AgentInfo>(archived),
  };
  // What it left out is the lean form's to say, not the snapshot's.
  delete snapshot.without;
  return { snapshot, said };
}
