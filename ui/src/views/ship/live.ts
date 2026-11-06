/**
 * Ship's lists, taking each change in place (W21-R15): the server sends a `ship` frame with each
 * repository's rows that changed, as `GET /api/ship/commits` renders them (crates/kiln/src/ship.rs,
 * `ShipCache::refresh`), and each resource puts them where its list has them (resource.ts's
 * `apply`), instead of loading the whole list again for every run, leg, review or comment event.
 * A commit's detail takes its row the same way, and its parts (checks, jobs, build ...) from the
 * frame's `parts` and `items` when they go over the `dv` it holds.
 *
 * What memory answered says whom its rows are for (`viewer`); data without it came from the store
 * or from a server that sends no frames, and loads again on the next event. The events asked for
 * follow what the server last said (`framed`): only `ship` while its answers carry `viewer`; the
 * run, leg, review ... events too, as before, while they don't. Only what a page can't place loads
 * it again: a frame saying more changed than its rows (`whole`), a row gone from a page that has
 * rows past it, the pace line moved (the repositories' list shows it), a detail whose parts moved
 * from another `dv` than its own.
 */
import { events } from "@/api/events";
import { shipApi, type ShipCommit, type ShipDetail, type ShipPage, type ShipQuery } from "@/api/ship";
import type { ServerEvent } from "@/api/types";
import type { ShipRow, ShipRows } from "@/api/wire";

/** The events a Ship list took as "load again" before `ship` frames: asked for while the server's answers don't say it sends them. */
const BEFORE_FRAMES = ["ship", "run", "job", "review", "comment", "conflict", "cd_apps", "cd_rollout", "updates", "update_job"] as const;
const FRAMES = ["ship"] as const;

/** Whether the server's last Ship answer said it sends `ship` frames (its `viewer`); unknown before the first. */
let framed: boolean | undefined;

/** An answer of Ship's came: the events asked for follow what it says of the frames. */
function heard<T extends { viewer?: string[] }>(data: T): T {
  const now = data.viewer !== undefined;
  if (framed !== now) {
    framed = now;
    events.changed();
  }
  return data;
}

/** The events a Ship resource needs now. */
export const shipOn = (): readonly string[] => (framed ? FRAMES : BEFORE_FRAMES);

/** `GET /api/ship/commits`, as Ship's resources read it. */
export const shipCommits = (q: ShipQuery, signal?: AbortSignal) => shipApi.commits(q, signal).then(heard);
/** `GET /api/ship/commits/:repo/:sha`, as Ship's resources read it. */
export const shipCommit = (repo: string, sha: string, signal?: AbortSignal) => shipApi.commit(repo, sha, signal).then(heard);

/** The server's frame of a repository's rows. */
export type ShipFrame = { type: "ship" } & ShipRows;

/** The row a viewer sees: `yours` when it holds any of `whose`, as the server picks a list's rows. */
export function rowFor(r: ShipRow, viewer: readonly string[]): ShipCommit {
  const yours = r.yours !== undefined && (r.whose ?? []).some((p) => viewer.includes(p));
  return (yours ? r.yours : r.row) as unknown as ShipCommit;
}

/** Whether a row is one a query's facets let through (ship.rs's `Facets::matches`). */
export function matches(q: ShipQuery, c: ShipCommit): boolean {
  const kind = c.kind ?? "commit";
  const word = (v?: string) => (v ?? "").trim().toLowerCase() || undefined;
  const k = word(q.kind);
  const h = word(q.host);
  return (k === undefined || k === kind) && (h === undefined || (c.host ?? undefined) === h) && (word(q.lens) !== "review" || kind !== "commit");
}

/** Newest first, as the server orders a page (`cursor` is a commit's place, newest highest). */
const newestFirst = (a: ShipCommit, b: ShipCommit) => b.cursor - a.cursor;

/**
 * A page of `GET /api/ship/commits` (`q`) brought up to date with `ev`; undefined when it can't be
 * (it loads again). The repositories' list (`by: "repo"`): each repository's newest commit that the
 * facets let through. A repository's page: those of `cursor`'s page, `limit` of them, newest first.
 */
export function applyShipPage(q: ShipQuery, data: ShipPage, ev: ServerEvent): ShipPage | undefined {
  const viewer = data.viewer;
  // Another event (a server that sends no frames): what it changed is the server's to say.
  if (ev.type !== "ship") return viewer === undefined ? undefined : data;
  if (q.repo && ev.repo !== q.repo) return data;
  if (viewer === undefined || ev.whole) return undefined;
  return q.by === "repo" ? heads(q, data, ev, viewer) : window(q, data, ev, viewer);
}

function heads(q: ShipQuery, data: ShipPage, ev: ShipFrame, viewer: readonly string[]): ShipPage | undefined {
  // The pace line counts every commit of the repositories in view, shown or not.
  if (ev.pace) return undefined;
  let commits = data.commits;
  const gone = new Set(ev.gone ?? []);
  for (const r of ev.rows) {
    const c = rowFor(r, viewer);
    const at = commits.findIndex((x) => x.repo === ev.repo);
    const head = commits[at];
    if (head === undefined) {
      // Its first commit the facets let through: its row.
      if (matches(q, c)) commits = [...commits, c];
      continue;
    }
    if (head.sha === c.sha) {
      // Its newest no longer matching: the one before it is the server's to say.
      if (!matches(q, c)) return undefined;
      commits = commits.map((x, i) => (i === at ? c : x));
    } else if (c.cursor > head.cursor && matches(q, c)) commits = commits.map((x, i) => (i === at ? c : x));
  }
  if (commits.some((c) => c.repo === ev.repo && gone.has(c.sha))) return undefined;
  if (commits === data.commits) return data;
  return { ...data, commits: [...commits].sort(newestFirst) };
}

function window(q: ShipQuery, data: ShipPage, ev: ShipFrame, viewer: readonly string[]): ShipPage | undefined {
  const limit = q.limit ?? 30;
  const rows = [...data.commits];
  let more = data.next_cursor !== null;
  let moved = false;
  // A row taken out: one from past this page would come up, which only the server knows.
  const out = (i: number) => {
    rows.splice(i, 1);
    moved = true;
    return !more;
  };
  for (const sha of ev.gone ?? []) {
    const i = rows.findIndex((c) => c.repo === ev.repo && c.sha === sha);
    // One this page doesn't show may have been the last past it: whether there are more is the server's.
    if (i < 0 ? more : !out(i)) return undefined;
  }
  for (const r of ev.rows) {
    const c = rowFor(r, viewer);
    const ok = matches(q, c) && (q.cursor === undefined || c.cursor < q.cursor);
    const i = rows.findIndex((x) => x.repo === c.repo && x.sha === c.sha);
    if (i >= 0) {
      if (!ok) {
        if (!out(i)) return undefined;
        continue;
      }
      rows[i] = c;
      moved = true;
      continue;
    }
    const last = rows.at(-1);
    const past = last !== undefined && c.cursor < last.cursor;
    if (!ok) {
      // Past the page, no longer one the facets let through: it may have been the last past it.
      if (more && past && (q.cursor === undefined || c.cursor < q.cursor)) return undefined;
      continue;
    }
    // Past the page's last, with more past it: another page's.
    if (more && past) continue;
    rows.push(c);
    rows.sort(newestFirst);
    if (rows.length > limit) {
      rows.pop();
      more = true;
    }
    moved = true;
  }
  if (!moved) return data;
  return { ...data, commits: rows, next_cursor: more ? (rows.at(-1)?.cursor ?? null) : null };
}

/**
 * A commit's detail (`GET /api/ship/commits/:repo/:sha`) brought up to date with `ev`; undefined
 * when it can't be. Its row's part is the frame's row; the rest (checks, jobs, lanes ...) holds when
 * the frame's `dv` is the detail's own, and is the frame's `parts` and `items` laid over it when
 * they go over that `dv` (`base`); any other `dv`, or none, loads it again.
 */
export function applyShipDetail(data: ShipDetail, ev: ServerEvent): ShipDetail | undefined {
  const viewer = data.viewer;
  if (ev.type !== "ship") return viewer === undefined ? undefined : data;
  if (ev.repo !== data.repo) return data;
  if (viewer === undefined || data.dv === undefined) return undefined;
  if (ev.whole || (ev.gone ?? []).includes(data.sha)) return undefined;
  const r = ev.rows.find((x) => (x.row as { sha?: unknown } | null)?.sha === data.sha);
  if (r === undefined) return data;
  if (r.dv === undefined) return undefined;
  const row = rowFor(r, viewer);
  if (r.dv === data.dv) return { ...data, ...row };
  if (r.base !== data.dv) return undefined;
  const next: Record<string, unknown> = { ...data, ...row, ...r.parts, dv: r.dv };
  for (const [k, it] of Object.entries(r.items ?? {})) {
    if (it === undefined) continue;
    const was: unknown[] = Array.isArray(next[k]) ? (next[k] as unknown[]) : [];
    const list = was.slice(0, it.len);
    for (const x of it.at) list[x.i] = x.v;
    next[k] = list;
  }
  return next as unknown as ShipDetail;
}

/** A commit's detail resource's options, one for every section that reads it (they share its key). */
export const DETAIL_OPTS = {
  pollMs: 15_000,
  get on() {
    return shipOn();
  },
  apply: applyShipDetail,
};

/** A page's options for query `q`. */
export const pageOpts = (q: ShipQuery) => ({
  pollMs: 15_000,
  get on() {
    return shipOn();
  },
  apply: (data: ShipPage, ev: ServerEvent) => applyShipPage(q, data, ev),
});
