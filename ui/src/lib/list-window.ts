/**
 * Rows of differing heights on the page's own scroll (W21-R02): Mission control's lists, each row
 * as tall as what it says (and as what's open under it), a separator or a tree of subagents a row
 * among them. Only the rows near the viewport are mounted; the room of the rest is kept by gaps, so
 * the page is as tall, and every mounted row where, as with all of them mounted. Heights are
 * measured from the page and kept by row; a row never measured takes the measured ones' typical
 * height (`typicalHeight`). The arithmetic is pure (below, tests/list-window.test.ts); `ListWindow`
 * wires it to the page and `useWindowed` to React.
 *
 * Exact for what was measured: a gap is the sum of its rows' last measured heights. A row that
 * changes while it's unmounted is measured again when it's next mounted, and what's in view stays
 * where it is, and on the page, as that happens (the browser's scroll anchoring; by hand where it
 * has none). The window is always taken from where the viewport is over the list as it's asked
 * for, and, while gaps are still to be drawn again from heights that changed, from where that will
 * leave it.
 *
 * Apart from lib/virtual.ts (rows of one fixed height), so a page that uses only that doesn't download this.
 */
import * as React from "react";

/**
 * One piece of a windowed list as it's drawn, in order: a mounted row (`row`: its place among the
 * keys), or the room (`room`, px) of the rows between that aren't (`gap`: which gap, counted from
 * the top, for its element's key).
 */
export type Part = { key: string; row: number } | { gap: number; room: number };

/**
 * What a row never measured is taken to be: the middle one of the heights measured (between the
 * middle two, half-way). Half the measured rows are no taller and half no shorter, so the few
 * unlike the rest (a separator, a row with a hundred subagents open under it) don't pull every
 * unmeasured row's height their way, as their mean would. None measured: undefined.
 */
export function typicalHeight(heights: Iterable<number>): number | undefined {
  const tally = new Map<number, number>();
  for (const h of heights) tally.set(h, (tally.get(h) ?? 0) + 1);
  return middle(tally);
}

/** The middle of heights counted by value (`tally`: how many rows are each height). A list's rows are of few heights, so it's found among those, not among the rows. */
function middle(tally: ReadonlyMap<number, number>): number | undefined {
  let n = 0;
  for (const count of tally.values()) n += count;
  if (n === 0) return undefined;
  // The middle one (an odd number of rows), or the middle two.
  const lower = (n - 1) >> 1;
  const upper = n >> 1;
  let seen = 0;
  let first: number | undefined;
  for (const height of [...tally.keys()].sort((a, b) => a - b)) {
    seen += tally.get(height) ?? 0;
    if (first === undefined && seen > lower) first = height;
    if (first !== undefined && seen > upper) return (first + height) / 2;
  }
  return first;
}

/** Where each row's top is from the list's: `tops[i]` row i's, and one more, the height of them all. A row not in `heights` takes `typical`. */
export function rowTops(keys: readonly string[], heights: ReadonlyMap<string, number>, typical: number): Float64Array {
  const tops = new Float64Array(keys.length + 1);
  let y = 0;
  for (let i = 0; i < keys.length; i++) {
    tops[i] = y;
    y += heights.get(keys[i] ?? "") ?? typical;
  }
  tops[keys.length] = y;
  return tops;
}

/** The row at `y` (from the list's top): the first whose bottom is past it, found by halving. As many as there are rows when `y` is past them all. */
export function rowAt(tops: Float64Array, y: number): number {
  let lo = 0;
  let hi = tops.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if ((tops[mid + 1] ?? 0) > y) hi = mid;
    else lo = mid + 1;
  }
  return lo;
}

/** How many rows start before `y`. */
function startBefore(tops: Float64Array, y: number): number {
  let lo = 0;
  let hi = tops.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if ((tops[mid] ?? 0) < y) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * The rows near the viewport, `[from, to)`: those any part of which is in the viewport (`viewTop`
 * from the list's top, `viewHeight` tall) or within one viewport above or below it. One viewport,
 * because that's as far as the page moves at once other than under a finger or a wheel (Page Up and
 * Down, Space, a press in the scroll bar's track): wherever one of those lands is already mounted,
 * and a drag or a fling has a viewport's travel before it shows a gap.
 */
export function nearRows(tops: Float64Array, viewTop: number, viewHeight: number): [from: number, to: number] {
  const from = rowAt(tops, viewTop - viewHeight);
  return [from, Math.max(from, startBefore(tops, viewTop + 2 * viewHeight))];
}

/** The rows mounted, as runs `[from, to)` in order: those near the viewport (`near`) and those kept wherever they are (`keep`: their places among the keys). */
export function mountedRuns(near: readonly [number, number], keep: Iterable<number> = []): [number, number][] {
  const [from, to] = near;
  const runs: [number, number][] = [];
  const push = (a: number, b: number) => {
    const last = runs.at(-1);
    if (last && last[1] === a) last[1] = b;
    else runs.push([a, b]);
  };
  let placed = from >= to;
  for (const i of [...new Set(keep)].filter((i) => i < from || i >= to).sort((a, b) => a - b)) {
    if (!placed && i >= to) {
      push(from, to);
      placed = true;
    }
    push(i, i + 1);
  }
  if (!placed) push(from, to);
  return runs;
}

/** The list as drawn: each mounted row, and between the runs a gap as tall as the rows it stands for (none where they take no room). */
export function windowParts(keys: readonly string[], tops: Float64Array, runs: readonly (readonly [number, number])[]): Part[] {
  const parts: Part[] = [];
  let gaps = 0;
  let end = 0;
  const gap = (from: number, to: number) => {
    const room = (tops[to] ?? 0) - (tops[from] ?? 0);
    if (room > 0) parts.push({ gap: gaps++, room });
  };
  for (const [from, to] of runs) {
    gap(end, from);
    for (let i = from; i < to; i++) {
      const key = keys[i];
      if (key !== undefined) parts.push({ key, row: i });
    }
    end = to;
  }
  gap(end, keys.length);
  return parts;
}

/**
 * How far the rows in view move down the page as rows' heights change (their tops `was`, then
 * `now`, the same rows in the same order): by what the rows wholly above the viewport's top edge
 * (`y`, from the list's top) gained between them. A browser with scroll anchoring scrolls by as much
 * itself, so nothing in view moves; one without (WebKit) is scrolled by it here.
 */
export function shiftAbove(was: Float64Array, now: Float64Array, y: number): number {
  const at = rowAt(was, y);
  return (now[at] ?? 0) - (was[at] ?? 0);
}

const sameKeys = (a: readonly string[], b: readonly string[]) => a === b || (a.length === b.length && a.every((k, i) => k === b[i]));
const sameParts = (a: readonly Part[], b: readonly Part[]) =>
  a.length === b.length &&
  a.every((p, i) => {
    const q = b[i];
    if (!q) return false;
    return "gap" in p ? "gap" in q && p.gap === q.gap && p.room === q.room : "row" in q && p.key === q.key && p.row === q.row;
  });

/** A row's height as the rows after it feel it: its box and its margins (a separator's room above it is a margin). */
function outerHeight(el: Element, box = el.getBoundingClientRect().height): number {
  const style = getComputedStyle(el);
  return box + (parseFloat(style.marginTop) || 0) + (parseFloat(style.marginBottom) || 0);
}

let anchoring: boolean | undefined;
/** Whether the browser itself keeps what's in view where it is as what's above it changes height (CSS scroll anchoring). */
const anchors = () => (anchoring ??= typeof CSS !== "undefined" && typeof CSS.supports === "function" && CSS.supports("overflow-anchor", "auto"));

export interface WindowOptions {
  /** The attribute each row's element says its key in. The rows' elements are the list element's children. */
  attr: string;
  /** Rows mounted wherever they are. */
  pinned?: ReadonlySet<string>;
  /**
   * Whether Tab stops in a row (none given: in every row). The first and last such rows are always
   * mounted, and so are those either side of the row the focus is in, so Tab and Shift+Tab go where
   * they would with every row mounted.
   */
  tabStop?: (key: string) => boolean;
  /** Within a row: while one of its elements (or it) matches, the row stays mounted (something open in it, a request of its on its way). */
  keep?: string;
  /** Anywhere on the page: while an element matches (a menu or a dialog a row opened, drawn outside it), no row is unmounted. */
  open?: string;
  /**
   * The order the list glides to (views/harness/list-tools.tsx `useGlide`): as it changes, the rows
   * near the viewport stay mounted until that glide is over (`settle`), so one leaving glides out.
   * Only with a glide to end it.
   */
  order?: string;
}

const NO_PARTS: readonly Part[] = [];

/**
 * A list's window, kept outside React (`listWindow`): what it has measured stays while its list is
 * off the page, so a list that comes back is as tall as it was. `useWindowed` hands it the rows as
 * they render and the list's element once they're on the page.
 */
export class ListWindow {
  /** Each measured row's height, at the list's `width` (a row is as tall as its words wrap). */
  private heights = new Map<string, number>();
  /** How many of them are each height: their typical height without sorting them all at each measure. */
  private tally = new Map<number, number>();
  private width: number | undefined;
  private typical: number | undefined;
  private keys: readonly string[] = [];
  private index = new Map<string, number>();
  private opts: WindowOptions = { attr: "" };
  /** The first and last rows Tab stops in. */
  private ends: number[] = [];
  private tops: Float64Array = new Float64Array(1);
  /** `tops` is behind the heights or the keys. */
  private stale = false;
  /** The viewport's top from the list's, and its height. */
  private viewTop = 0;
  private viewHeight = 0;
  /** The list's top from the page's, as last seen: where the viewport is over a list not on the page yet. */
  private pageTop = 0;
  private el: HTMLElement | null = null;
  private unwire: (() => void) | null = null;
  private sizes: ResizeObserver | null = null;
  /** The rows' elements on the page, each with its key. */
  private rows = new Map<Element, string>();
  private focused: Element | null = null;
  private pressed: string | null = null;
  private revealing: { key: string; block: ScrollLogicalPosition } | null = null;
  /** Rows kept through a glide, by the glide. */
  private holds = new Map<object, ReadonlySet<string>>();
  private held: object | null = null;
  private claimed = false;
  /** The list as it stood before its order last changed. */
  private before: { index: ReadonlyMap<string, number>; tops: Float64Array } | null = null;
  /** The parts on the page. */
  private drawn: readonly Part[] = NO_PARTS;
  /** Every mounted row is to be measured again (the rows changed places: a margin may have). */
  private all = false;
  /**
   * What the page is still to be scrolled by, once the gaps it's for are on the page, for what's in
   * view to be where it is: by the browser's scroll anchoring as it lays those out, by hand without it.
   * Until then the window is taken from where that leaves the viewport.
   */
  private owed = 0;
  private frame = 0;
  private view: { parts: readonly Part[] } = { parts: NO_PARTS };
  private listeners = new Set<() => void>();

  subscribe = (l: () => void) => {
    this.listeners.add(l);
    return () => {
      this.listeners.delete(l);
    };
  };
  get = () => this.view;

  /** The rows as they render (the same keys in the same order cost a comparison). */
  sync(keys: readonly string[], opts: WindowOptions) {
    const was = this.opts;
    if (opts.order !== undefined && was.order !== undefined && opts.order !== was.order) this.leave();
    let changed = opts.pinned !== was.pinned || opts.tabStop !== was.tabStop;
    this.opts = opts;
    if (!sameKeys(keys, this.keys)) {
      this.keys = keys;
      const index = new Map<string, number>();
      for (let i = 0; i < keys.length; i++) index.set(keys[i] ?? "", i);
      this.index = index;
      // A row that left is forgotten, and measured again if it comes back.
      for (const [key, height] of this.heights) {
        if (index.has(key)) continue;
        this.heights.delete(key);
        this.count(height, -1);
      }
      this.stale = true;
      this.all = true;
      changed = true;
    }
    if (changed) {
      const stop = opts.tabStop;
      const first = stop ? keys.findIndex(stop) : 0;
      const last = stop ? keys.findLastIndex(stop) : keys.length - 1;
      this.ends = [first, last].filter((i) => i >= 0);
    }
    // Not on the page yet: the viewport as the page has it now, over the list where it last was.
    if (!this.el && typeof window !== "undefined") {
      const viewTop = window.scrollY - this.pageTop;
      if (viewTop !== this.viewTop || window.innerHeight !== this.viewHeight) changed = true;
      this.viewTop = viewTop;
      this.viewHeight = window.innerHeight;
    }
    if (changed) this.view = this.compute();
  }

  /**
   * The order is about to change: the list as it stands is kept (`wasAt`), and the rows on the page
   * within a viewport of what shows stay mounted through the glide (`settle`), wherever they go.
   */
  private leave() {
    this.before = { index: this.index, tops: this.fresh() };
    if (this.held && !this.claimed) this.holds.delete(this.held);
    this.held = null;
    if (!this.el) return;
    const reach = window.innerHeight;
    const near = new Set<string>();
    for (const [row, key] of this.rows) {
      const box = row.getBoundingClientRect();
      if (box.bottom > -reach && box.top < 2 * reach) near.add(key);
    }
    this.held = {};
    this.claimed = false;
    this.holds.set(this.held, near);
  }

  /** Where the row `el` is was, from the list's top, before the order last changed: undefined when it wasn't in the list. */
  wasAt = (el: Element): number | undefined => {
    const key = el.getAttribute(this.opts.attr);
    const i = key === null ? undefined : this.before?.index.get(key);
    return i === undefined ? undefined : this.before?.tops[i];
  };
  /** The rows kept mounted for the glide to the order now on the page: `settle` it once that glide is over. */
  holding = (): object | null => {
    this.claimed = true;
    return this.held;
  };
  settle = (hold: object | null) => {
    if (!hold || !this.holds.delete(hold)) return;
    if (this.held === hold) this.held = null;
    this.refresh();
  };

  /** A row's height as last measured, or what one never measured takes (undefined: none measured yet). */
  heightOf(key: string): number | undefined {
    return this.heights.get(key) ?? this.typical;
  }

  /** Brings the row into the window and the page to it (`block`: where in the viewport, as scrollIntoView takes it). False: no such row. */
  reveal(key: string, block: ScrollLogicalPosition = "nearest"): boolean {
    if (!this.index.has(key)) return false;
    this.revealing = { key, block };
    this.refresh();
    this.show();
    return true;
  }

  /** The row to reveal, once it's on the page: scrolled to, and the window taken from there. */
  private show() {
    const asked = this.revealing;
    if (!asked) return;
    for (const [row, key] of this.rows) {
      if (key !== asked.key) continue;
      row.scrollIntoView({ block: asked.block, behavior: "instant" });
      this.measure();
      this.revealing = null;
      this.refresh();
      return;
    }
  }

  /** What a row takes while none has been measured: the whole window (the viewport and one above and below it), so only the first row and those kept are mounted, and measured before anything is painted. */
  private unmeasured() {
    return 3 * this.viewHeight;
  }

  private fresh(): Float64Array {
    if (this.stale) {
      this.tops = rowTops(this.keys, this.heights, this.typical ?? this.unmeasured());
      this.stale = false;
    }
    return this.tops;
  }

  /** The list's child `node` is in, by its key. */
  private rowOf(node: EventTarget | null): string | undefined {
    const el = this.el;
    if (!el || !(node instanceof Element)) return undefined;
    let at: Element | null = node;
    while (at && at.parentElement !== el) at = at.parentElement;
    return at ? (this.rows.get(at) ?? at.getAttribute(this.opts.attr) ?? undefined) : undefined;
  }

  private compute(): { parts: readonly Part[] } {
    // From where the viewport is now, not where it last said it was: the page scrolls without a word
    // until its next frame (a row scrolled to, the browser keeping what's in view where it is).
    this.measure();
    const tops = this.fresh();
    const near = nearRows(tops, this.viewTop + this.owed, this.viewHeight);
    const [from, to] = near;
    const keep = new Set<number>(this.ends);
    const add = (key: string | null | undefined) => {
      const i = key == null ? undefined : this.index.get(key);
      if (i !== undefined) keep.add(i);
    };
    for (const key of this.opts.pinned ?? []) add(key);
    add(this.pressed);
    add(this.revealing?.key);
    for (const keys of this.holds.values()) for (const key of keys) add(key);
    // The row the focus is in, and the rows Tab and Shift+Tab go to from it.
    if (this.focused && !this.focused.isConnected) this.focused = null;
    const at = this.focused ? this.index.get(this.rowOf(this.focused) ?? "") : undefined;
    if (at !== undefined) {
      keep.add(at);
      const stop = this.opts.tabStop;
      for (const step of [-1, 1]) {
        let i = at + step;
        while (stop && i >= 0 && i < this.keys.length && !stop(this.keys[i] ?? "")) i += step;
        if (i >= 0 && i < this.keys.length) keep.add(i);
      }
    }
    // What's on the page and can't go: something open in it, or open from it and drawn elsewhere.
    let open: boolean | undefined;
    for (const [row, key] of this.rows) {
      const i = this.index.get(key);
      if (i === undefined || (i >= from && i < to) || keep.has(i)) continue;
      if (this.opts.keep && (row.matches(this.opts.keep) || row.querySelector(this.opts.keep) !== null)) keep.add(i);
      else if ((open ??= this.opts.open ? document.querySelector(this.opts.open) !== null : false)) keep.add(i);
    }
    const parts = windowParts(this.keys, tops, mountedRuns(near, keep));
    return sameParts(parts, this.view.parts) ? this.view : { parts };
  }

  private refresh() {
    const next = this.compute();
    if (next === this.view) return;
    this.view = next;
    for (const l of [...this.listeners]) l();
  }

  /** Once the page has been laid out and painted as it is: a change heard from a resize observer mounts nothing within its own pass. */
  private later() {
    if (this.frame) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = 0;
      this.refresh();
    });
  }

  /** Where the viewport is over the list, from the page. */
  private measure() {
    const el = this.el;
    if (!el?.isConnected) return;
    const top = el.getBoundingClientRect().top + el.clientTop;
    // Nothing measured, a row's height is the window's: it changes with the viewport.
    if (window.innerHeight !== this.viewHeight && this.typical === undefined) this.stale = true;
    this.viewTop = -top;
    this.viewHeight = window.innerHeight;
    this.pageTop = top + window.scrollY;
  }

  private count(height: number, by: 1 | -1) {
    const n = (this.tally.get(height) ?? 0) + by;
    if (n > 0) this.tally.set(height, n);
    else this.tally.delete(height);
  }

  private put(key: string, height: number): boolean {
    const was = this.heights.get(key);
    if (was === height) return false;
    if (was !== undefined) this.count(was, -1);
    this.count(height, 1);
    this.heights.set(key, height);
    this.stale = true;
    return true;
  }

  private forget() {
    this.heights.clear();
    this.tally.clear();
    this.stale = true;
  }

  /**
   * Heights changed (`was`: the tops before they did; `forgotten`: the heights the gaps on the page
   * were drawn from, where the list's width changed and they were let go). The typical height
   * follows, and what's in view is kept where it is. The rows on the page above it have their new
   * heights already: the browser's scroll anchoring has scrolled by what they gained, and without it
   * the page is scrolled by as much here. The gaps above it are drawn again only at the next render:
   * what those gain is `owed` until then.
   */
  private weighed(was: Float64Array, forgotten?: ReadonlyMap<string, number>) {
    const typical = middle(this.tally);
    // The page as it is: its rows as just measured, its gaps as they were drawn.
    const page = rowTops(this.keys, forgotten ? new Map([...forgotten, ...this.heights]) : this.heights, this.typical ?? this.unmeasured());
    const now = !forgotten && typical === this.typical ? page : rowTops(this.keys, this.heights, typical ?? this.unmeasured());
    // The row at the viewport's top edge, as the viewport stood over the list before any of it.
    const at = rowAt(was, this.viewTop);
    this.owed += (now[at] ?? 0) - (page[at] ?? 0);
    this.typical = typical;
    this.tops = now;
    this.stale = false;
    if (!anchors()) this.scrollBy(shiftAbove(was, page, this.viewTop));
  }

  private scrollBy(by: number) {
    if (by === 0) return;
    window.scrollBy({ top: by, behavior: "instant" });
    this.measure();
  }

  /**
   * The rows' elements as the page has them: each new one observed and measured, each gone one let
   * go; all of them measured again when `all` (they changed places); a mounted row that draws
   * nothing takes no room.
   */
  private children(all: boolean) {
    const el = this.el;
    const sizes = this.sizes;
    if (!el || !sizes) return;
    const was = this.fresh();
    const here = new Set<Element>();
    const fresh: Element[] = [];
    for (const child of el.children) {
      const key = child.getAttribute(this.opts.attr);
      if (key === null) continue;
      here.add(child);
      if (!this.rows.has(child)) {
        sizes.observe(child);
        fresh.push(child);
      }
      this.rows.set(child, key);
    }
    for (const child of this.rows.keys()) {
      if (here.has(child)) continue;
      sizes.unobserve(child);
      this.rows.delete(child);
    }
    let changed = false;
    for (const child of all ? here : fresh) {
      const key = this.rows.get(child);
      if (key !== undefined && this.index.has(key) && this.put(key, outerHeight(child))) changed = true;
    }
    const have = new Set(this.rows.values());
    for (const part of this.drawn) if ("row" in part && !have.has(part.key) && this.index.has(part.key) && this.put(part.key, 0)) changed = true;
    if (changed) this.weighed(was);
  }

  /** The parts `parts` are on the page, in `el` (from the list's ref, as React puts them there, before the page is painted). */
  committed(el: HTMLElement, parts: readonly Part[]) {
    if (this.el !== el) this.attach(el);
    this.drawn = parts;
    // The gaps the last change of heights made are on the page now: the browser's scroll anchoring scrolls by what they gained as it lays them out; without it, here.
    const owed = this.owed;
    this.owed = 0;
    if (!anchors()) this.scrollBy(owed);
    const all = this.all;
    this.all = false;
    this.children(all);
    this.show();
    this.refresh();
  }

  /** The list's ref let go of `el`: for good once it's off the page (a ref made anew for each view lets go and takes again). */
  released(el: HTMLElement) {
    queueMicrotask(() => {
      if (this.el === el && !el.isConnected) this.detach();
    });
  }

  private attach(el: HTMLElement) {
    this.detach();
    this.el = el;
    const moved = () => {
      this.measure();
      this.refresh();
    };
    const focusin = (e: FocusEvent) => {
      this.focused = e.target instanceof Element ? e.target : null;
      this.refresh();
    };
    // Where the focus goes, not where it was: a row is never unmounted between losing and taking it.
    const focusout = (e: FocusEvent) => {
      const to = e.relatedTarget;
      this.focused = to instanceof Element && el.contains(to) ? to : null;
      this.refresh();
    };
    const down = (e: PointerEvent) => {
      this.pressed = this.rowOf(e.target) ?? null;
    };
    const up = () => {
      if (this.pressed === null) return;
      this.pressed = null;
      this.refresh();
    };
    this.sizes = new ResizeObserver((entries) => this.resized(entries));
    this.sizes.observe(el);
    // The page above the list changing height moves the list under the viewport, without a scroll.
    this.sizes.observe(document.body);
    // A row's element can come or go with no change to the window (a row that drew nothing, then something).
    const kids = new MutationObserver(() => {
      this.children(false);
      this.refresh();
    });
    kids.observe(el, { childList: true });
    window.addEventListener("scroll", moved, { passive: true });
    window.addEventListener("resize", moved);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
    el.addEventListener("focusin", focusin);
    el.addEventListener("focusout", focusout);
    el.addEventListener("pointerdown", down);
    this.unwire = () => {
      kids.disconnect();
      window.removeEventListener("scroll", moved);
      window.removeEventListener("resize", moved);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", up);
      el.removeEventListener("focusin", focusin);
      el.removeEventListener("focusout", focusout);
      el.removeEventListener("pointerdown", down);
    };
    this.measure();
  }

  private resized(entries: ResizeObserverEntry[]) {
    const el = this.el;
    if (!el) return;
    const was = this.fresh();
    let forgotten: Map<string, number> | undefined;
    let changed = false;
    for (const entry of entries) {
      if (entry.target !== el) continue;
      const width = entry.borderBoxSize[0]?.inlineSize ?? entry.contentRect.width;
      // Measured at another width, the heights aren't these rows': those on the page say theirs again in this same pass.
      if (this.width !== undefined && this.width !== width) {
        forgotten ??= new Map(this.heights);
        this.forget();
      }
      this.width = width;
    }
    for (const entry of entries) {
      const key = this.rows.get(entry.target);
      if (key === undefined || !this.index.has(key)) continue;
      if (this.put(key, outerHeight(entry.target, entry.borderBoxSize[0]?.blockSize))) changed = true;
    }
    if (changed) this.weighed(was, forgotten);
    this.measure();
    this.later();
  }

  private detach() {
    this.unwire?.();
    this.unwire = null;
    this.sizes?.disconnect();
    this.sizes = null;
    if (this.frame) cancelAnimationFrame(this.frame);
    this.frame = 0;
    this.el = null;
    this.rows.clear();
    this.focused = null;
    this.pressed = null;
    this.revealing = null;
    this.holds.clear();
    this.held = null;
    this.drawn = NO_PARTS;
    this.owed = 0;
  }
}

const windows = new Map<string, ListWindow>();
/** The window of the list `id` names (one for each list, wherever it's asked for): what it measured is still there when its page comes back. */
export function listWindow(id: string): ListWindow {
  let win = windows.get(id);
  if (!win) {
    win = new ListWindow();
    windows.set(id, win);
  }
  return win;
}

/**
 * A windowed list of rows of differing heights (W21-R02): `keys` every row in order, `parts` what
 * to draw (each mounted row; for a gap an empty element of the list's own kind, `room` px tall), and
 * `ref` for the element they're drawn in, whose children they are. The window follows the page's
 * scroll and the rows' heights as they're measured, and re-renders only the component that calls this.
 */
export function useWindowed(win: ListWindow, keys: readonly string[], opts: WindowOptions): { ref: (el: HTMLElement | null) => (() => void) | undefined; parts: readonly Part[] } {
  win.sync(keys, opts);
  const view = React.useSyncExternalStore(win.subscribe, win.get, win.get);
  const ref = React.useCallback(
    (el: HTMLElement | null) => {
      if (!el) return;
      win.committed(el, view.parts);
      return () => win.released(el);
    },
    // A new ref for each view: React hands it the list once that view is on the page.
    [win, view],
  );
  return { ref, parts: view.parts };
}
