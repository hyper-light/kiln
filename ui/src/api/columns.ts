/**
 * A list written as columns, read back as the list (docs/wire.md, "Lists as columns"; W21-R02). A list of objects
 * writes every key in every row; as columns each key is written once, then its values in the rows' order, in
 * whichever of a few forms is shortest (the server finds it, `crates/kiln/src/wire/columns.rs`):
 *
 * - `{ v }`: its values, one a row;
 * - `{ of, v }`: its distinct values once, and for each row which of them;
 * - `{ d, x, v }`: the value most rows have, and the rows that have another, with theirs;
 * - `{ pre, v }`: texts that begin alike, the beginning once and each one's rest;
 * - `{ cols }`: objects, as columns of their own;
 *
 * and `at` when only some rows have the key: those rows, among which its values then count. What's read back is the
 * rows as they were written, each its keys with their values and no other.
 */
export interface Columns {
  rows: number;
  cols: [key: string, column: Column][];
}

export interface Column {
  at?: number[];
  v?: unknown[];
  of?: unknown[];
  d?: unknown;
  x?: number[];
  pre?: string;
  cols?: Columns["cols"];
}

/** Whether `read` is a list as columns (a server from before them answers the list itself). */
export function isColumns(read: unknown): read is Columns {
  const c = read as Partial<Columns> | null;
  return typeof c === "object" && c !== null && !Array.isArray(c) && typeof c.rows === "number" && Array.isArray(c.cols);
}

/** A value said once for several rows, as each row's own: an object or a list is copied, so no two rows share one. */
const own = (value: unknown): unknown => (typeof value === "object" && value !== null ? structuredClone(value) : value);

/** A column's `n` values, in its rows' order. */
function values(column: Column, n: number): unknown[] {
  if (column.cols) {
    const inner: Record<string, unknown>[] = Array.from({ length: n }, () => ({}));
    fill(inner, column.cols);
    return inner;
  }
  const v = column.v ?? [];
  if (column.of) {
    const of = column.of;
    return v.map((which) => own(of[which as number]));
  }
  if ("d" in column) {
    const all = Array.from({ length: n }, () => own(column.d));
    (column.x ?? []).forEach((x, i) => {
      all[x] = v[i];
    });
    return all;
  }
  if (column.pre !== undefined) {
    const pre = column.pre;
    return v.map((rest) => pre + String(rest));
  }
  return v;
}

/** Gives `rows` what `cols` say of them. */
function fill(rows: Record<string, unknown>[], cols: Columns["cols"]) {
  for (const [key, column] of cols) {
    const at = column.at;
    const given = values(column, at ? at.length : rows.length);
    given.forEach((value, i) => {
      const row = rows[at ? (at[i] ?? -1) : i];
      if (row) row[key] = value;
    });
  }
}

/** The `n` values `column` holds, in order: a list that isn't of objects (ids, say), written as a column of its own. */
export function fromColumn<T>(column: Column, n: number): T[] {
  return values(column, n) as T[];
}

/** The rows `columns` hold, as they were written. */
export function fromColumns<T>(columns: Columns): T[] {
  const rows: Record<string, unknown>[] = Array.from({ length: columns.rows }, () => ({}));
  fill(rows, columns.cols);
  return rows as T[];
}
