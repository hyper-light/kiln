/**
 * Pages agents show (page.tsx), as their `show_page` calls answered: pure, for the timeline's
 * derivation and the frame alike.
 */
import type { ToolCall } from "@/api/harness";

/** A page an agent showed, as its `show_page` call answered (`crate::harness::visuals::ask`). */
export interface ShownPage {
  id: string;
  title: string;
  /** Where it's served: `/v/<id>/` and its path on its machine, on the server or its pages address. */
  path: string;
  /** Its height at each width measured, `[width, height]` in CSS pixels (in Chromium). */
  heights: [number, number][];
  /** The same in other engines (`firefox`), when its machine had them. */
  engines?: Record<string, [number, number][]>;
  files?: number;
}

/** The engine this browser is, as pages are measured in them. */
export function ownEngine(ua: string): "chromium" | "firefox" | "webkit" {
  if (/Firefox\//.test(ua)) return "firefox";
  if (/AppleWebKit/.test(ua) && !/Chrome\/|Chromium\/|Edg\//.test(ua)) return "webkit";
  return "chromium";
}

/** A page's heights as measured in this browser's own engine, else in Chromium. */
export function heightsFor(page: Pick<ShownPage, "heights" | "engines">, ua: string): [number, number][] {
  const own = page.engines?.[ownEngine(ua)];
  return own && own.length ? own : page.heights;
}

/** Whether a tool call is an agent showing a page (Claude Code's and Codex's names for kiln's tool). */
export const isShowPage = (c: Pick<ToolCall, "name">): boolean => /^mcp__kiln(?:[-_]reach)?__show_page$/.test(c.name);

/** The page a `show_page` call showed, from its answer (one line of JSON, `crate::reach::shown`). */
export function pageOf(c: Pick<ToolCall, "output">): ShownPage | null {
  if (!c.output) return null;
  try {
    const p = (JSON.parse(c.output) as { kiln_page?: Partial<ShownPage> }).kiln_page;
    if (!p || typeof p.id !== "string" || !/^[0-9a-f]{32}$/.test(p.id)) return null;
    const pairs = (v: unknown) => (Array.isArray(v) ? v : []).filter((h): h is [number, number] => Array.isArray(h) && h.length === 2 && h.every((n) => typeof n === "number" && n > 0));
    const heights = pairs(p.heights);
    const engines = Object.fromEntries(Object.entries(p.engines && typeof p.engines === "object" ? p.engines : {}).map(([e, hs]) => [e, pairs(hs)]));
    // Where it's served (its own path under `/v/<id>`), never anywhere else.
    const path = typeof p.path === "string" && p.path.startsWith(`/v/${p.id}/`) && !p.path.includes("..") ? p.path : `/v/${p.id}/`;
    return { id: p.id, title: typeof p.title === "string" && p.title ? p.title : "A page", path, heights, engines, files: p.files };
  } catch {
    return null;
  }
}

/** Its height at width `w`: the taller of the two measured widths nearest it (a page grows as it
 *  narrows, mostly). Null when nothing was measured. */
export function heightAt(heights: [number, number][], w: number): number | null {
  if (heights.length === 0) return null;
  const sorted = [...heights].sort((a, b) => a[0] - b[0]);
  let below: [number, number] | undefined;
  let above: [number, number] | undefined;
  for (const h of sorted) {
    if (h[0] <= w) below = h;
    if (h[0] >= w && !above) above = h;
  }
  return Math.max(below?.[1] ?? 0, above?.[1] ?? 0) || null;
}
