/**
 * How many of Mission control's rows a page can show as it opens (W21-R02): the last words of that many come with
 * its first reads (api/attention.ts `said`), and its first rows wait for theirs, so the list is drawn with its lines.
 */

/** The least a row is tall (px): `.ms-row`'s `min-height` in mission.css (tests/first-rows.test.ts holds this to it). */
export const ROW_LEAST = 84;

/**
 * No more rows than this are on the page before it's scrolled: the list mounts the rows within a viewport below
 * what shows (lib/list-window.ts `nearRows`), so those that begin within two viewports' height of its top, and no row
 * is shorter than `ROW_LEAST`.
 */
export const firstRows = (viewHeight = typeof window === "undefined" ? 0 : window.innerHeight): number => Math.ceil((2 * viewHeight) / ROW_LEAST);
