/**
 * The lean answers of the real server (W21-R02), as the mock gives them: the sessions' list without what a list
 * doesn't read, as columns (`crates/kiln/src/api.rs` `lean_sessions`), and the attention snapshot with each agent
 * named once and the last words of the rows the list shows first (`crates/kiln/src/harness/attention/lean.rs`).
 * Each column is written plainly here (`v`, with `at` where only some rows have the key): one of the forms a list
 * as columns can take (src/api/columns.ts), where the server takes whichever is shortest.
 */
import type { Columns } from "../src/api/columns.ts";
import type { AttentionMark, AttentionSnapshot } from "../src/api/attention.ts";
import type { LeanSnapshot } from "../src/api/attention-lean.ts";
import type { AgentInfo } from "../src/api/harness.ts";

/** `rows` as columns, each written plainly. */
export function plainColumns(rows: readonly object[]): Columns {
  const keys: string[] = [];
  for (const row of rows) for (const key of Object.keys(row)) if (!keys.includes(key)) keys.push(key);
  return {
    rows: rows.length,
    cols: keys.map((key) => {
      const at = rows.flatMap((row, i) => (key in row ? [i] : []));
      const v = at.map((i) => (rows[i] as Record<string, unknown>)[key]);
      return [key, at.length === rows.length ? { v } : { v, at }];
    }),
  };
}

/** What a lean list leaves out of each session: where it started, and its last words when it has some. */
const WITHOUT = ["summary", "starts", "runs"];

/** `GET /api/harness/sessions?lean=1`. */
export function leanSessions(list: readonly AgentInfo[]): Columns & { without: string[] } {
  const rows = list.map((info) => {
    const row: Record<string, unknown> = { ...info };
    delete row.starts;
    delete row.runs;
    if (info.summary) delete row.summary;
    return row;
  });
  return { ...plainColumns(rows), without: WITHOUT };
}

/** `GET /api/harness/sessions?summaries=<id>,<id>…`: the last words of those of them there are. */
export function summariesOf(list: readonly AgentInfo[], ids: string): Record<string, string> {
  const said: Record<string, string> = {};
  for (const id of ids.split(",")) {
    const info = list.find((a) => a.id === id);
    if (info) said[id] = info.summary ?? "";
  }
  return said;
}

/**
 * `GET /api/harness/attention?lean=1&said=<rows>`: `snapshot` lean, with the last words of the rows the list opens
 * on: its first `rows` (your pins, then the order's, without the parked, the archived and the ended), a row that
 * has said nothing taking its place among them, and its last.
 */
export function leanSnapshot(snapshot: AttentionSnapshot, rows: number, now: number, list: readonly AgentInfo[]): LeanSnapshot {
  const ids: string[] = [];
  const place = (id: string) => {
    const at = ids.indexOf(id);
    return at >= 0 ? at : ids.push(id) - 1;
  };
  const written = (places: number[]) => (places.every((at, i) => at === i) ? places.length : places);
  const order = snapshot.order.map(place);
  const marked = Object.entries(snapshot.agents)
    .map(([id, mark]): [number, AttentionMark] => {
      const rest = { ...mark };
      delete rest.rank;
      return [place(id), rest];
    })
    .sort(([x], [y]) => x - y);
  const whose = marked.map(([at]) => at);
  const rest = { ...snapshot };
  delete rest.model;
  const lean: LeanSnapshot = {
    ...rest,
    order: written(order),
    agents: { ...plainColumns(marked.map(([, mark]) => mark)), ...(whose.every((at, i) => at === i) ? {} : { at: whose }) },
    request_order: written(snapshot.request_order.map(place)),
    pair: snapshot.pair ? { ...snapshot.pair, order_a: written(snapshot.pair.order_a.map(place)), order_b: written(snapshot.pair.order_b.map(place)) } : null,
    archived: plainColumns(snapshot.archived),
    ids: { v: ids },
    without: ["model", "examination", "urgency", "agents.rank"],
  };
  if (rows > 0) {
    const mark = (at: number) => snapshot.agents[ids[at] ?? ""];
    const away = (at: number) => mark(at)?.archived === true || (mark(at)?.parked_until ?? 0) > now;
    // A row the list shows as it opens, with what its agent last said: not one that ended; nor a subagent kiln started
    // for an agent that's listed, which is on its agent's row (the server's order doesn't have those: this mock's has every agent).
    const shown = (at: number): string | undefined => {
      const info = list.find((a) => a.id === ids[at]);
      if (away(at) || !info || info.status === "exited" || (info.subagent && info.parent && list.some((a) => a.id === info.parent))) return undefined;
      return info.summary ?? "";
    };
    const pinned = ids.flatMap((id, at) => (mark(at)?.pinned === undefined ? [] : [{ pin: mark(at)!.pinned!, id, at }])).sort((x, y) => x.pin - y.pin || x.id.localeCompare(y.id));
    const met = new Set<number>();
    const said: { at: number[]; v: string[] } = { at: [], v: [] };
    const say = (at: number, words: string) => {
      if (!words) return;
      said.at.push(at);
      said.v.push(words);
    };
    let taken = 0;
    for (const at of [...pinned.map((p) => p.at), ...order]) {
      if (taken >= rows) break;
      if (met.has(at)) continue;
      met.add(at);
      const words = shown(at);
      if (words === undefined) continue;
      taken++;
      say(at, words);
    }
    // The list's last row too (the page keeps the row Tab ends on in it): the order's last that isn't one of your pins.
    const last = [...order].reverse().find((at) => mark(at)?.pinned === undefined && shown(at) !== undefined);
    if (last !== undefined && !met.has(last)) say(last, shown(last) ?? "");
    lean.said = said;
  }
  // Every id is listed once the places are all taken: the column is made last.
  lean.ids = { v: ids };
  return lean;
}
