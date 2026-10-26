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
  /** The same in other engines (`firefox`, `webkit`): its machine's Firefox at each width, and
   *  viewers' own browsers at theirs (`crate::harness::visuals::Visuals::seen`). */
  engines?: Record<string, [number, number][]>;
  files?: number;
}

/** The engine this browser is, as pages are measured in them. */
export function ownEngine(ua: string): "chromium" | "firefox" | "webkit" {
  if (/Firefox\//.test(ua)) return "firefox";
  if (/AppleWebKit/.test(ua) && !/Chrome\/|Chromium\/|Edg\//.test(ua)) return "webkit";
  return "chromium";
}

/** A viewer's own engine's height is taken at its width from one measured this near it, */
const NEAR = 24;
/** or from two around it this near (its machine's are at most 144 apart; viewers' farther). */
const AROUND = 160;

/** A page's heights as measured in this browser's own engine, where they reach its width `w`, else
 *  in Chromium (its machine measured those at every width). */
export function heightsFor(page: Pick<ShownPage, "heights" | "engines">, ua: string, w?: number): [number, number][] {
  const own = page.engines?.[ownEngine(ua)] ?? [];
  if (!own.length) return page.heights;
  if (w === undefined) return own;
  const near = own.filter(([x]) => Math.abs(x - w) <= NEAR);
  if (near.length) return near;
  const around = own.some(([x]) => x <= w && w - x <= AROUND) && own.some(([x]) => x >= w && x - w <= AROUND);
  return around ? own : page.heights;
}

/** `[width, height]` pairs, as kiln answers them (none that aren't). */
export function heightPairs(v: unknown): [number, number][] {
  return (Array.isArray(v) ? v : []).filter((h): h is [number, number] => Array.isArray(h) && h.length === 2 && h.every((n) => typeof n === "number" && n > 0));
}

/** Each engine's heights, as kiln answers them. */
export function enginesOf(v: unknown): Record<string, [number, number][]> {
  return Object.fromEntries(Object.entries(v && typeof v === "object" ? v : {}).map(([e, hs]) => [e, heightPairs(hs)]));
}

/** A tool call, as much of it as says which of kiln's tools it is. */
export type KilnCall = Pick<ToolCall, "name" | "tool" | "title" | "server" | "tool_short" | "output">;

/**
 * Which of kiln's own tools a call is, where its harness names it: Claude Code's and Codex's
 * `mcp__kiln__show_page` (kiln reads its server and the tool's own name from it; `kiln-reach`,
 * `kiln_reach` in sessions from before it was called `kiln`), or an ACP agent's words for it
 * ("Called show_page from kiln", Devin). Null for another server's tool, or a call that doesn't say.
 */
export function kilnTool(c: Pick<ToolCall, "name" | "title" | "server" | "tool_short">): string | null {
  if (c.server) return /^kiln(?:-reach|_reach)?$/.test(c.server) ? (c.tool_short ?? null) : null;
  return /^mcp__kiln(?:-reach|_reach)?__(\w+)$/.exec(c.name)?.[1] ?? /^Called (\w+) from kiln(?:-reach|_reach)?\b/.exec(c.title)?.[1] ?? null;
}

/**
 * Whether a call is of kiln's tool `tool`, whatever its harness calls it: by its name for it
 * (`kilnTool`), or, from a harness that doesn't say which server it called (an ACP agent wording it
 * its own way), by what kiln answered (`answered`).
 */
export function isKilnCall(c: KilnCall, tool: string, answered: (c: KilnCall) => boolean): boolean {
  const named = kilnTool(c);
  if (named) return named === tool;
  return !c.server && !c.name.startsWith("mcp__") && (c.tool === "mcp" || c.tool === "other") && answered(c);
}

/** Whether a tool call is an agent showing a page (kiln's `show_page`). */
export const isShowPage = (c: KilnCall): boolean => isKilnCall(c, "show_page", (x) => pageOf(x) !== null);

const read = (text: string): unknown => {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
};

/** The JSON object that opens at `from` in `s`, to the brace that closes it (its strings skipped). */
function objectAt(s: string, from: number): string {
  let depth = 0;
  let quoted = false;
  for (let i = from; i < s.length; i++) {
    const ch = s[i];
    if (quoted) {
      if (ch === "\\") i++;
      else if (ch === '"') quoted = false;
    } else if (ch === '"') quoted = true;
    else if (ch === "{") depth++;
    else if (ch === "}" && --depth === 0) return s.slice(from, i + 1);
  }
  return s.slice(from);
}

/**
 * What kiln's tool answered under `key` (`kiln_page`, `kiln_stream`: `crate::reach::mcp`), from a
 * call's output as its harness passed it on: kiln's JSON (one line, `crate::reach::shown`), that
 * JSON quoted as text again (an ACP agent's raw output, as Devin's can be), or among other lines
 * and content (its picture said in words, MCP's whole result). Null when it holds no such answer.
 */
export function answerOf(output: string | undefined, key: string): Record<string, unknown> | null {
  if (!output?.includes(key)) return null;
  const text = /^\s*"/.test(output) ? read(output) : undefined;
  if (typeof text === "string") return answerOf(text, key);
  for (const m of output.matchAll(new RegExp(`\\{\\s*"${key}"\\s*:`, "g"))) {
    const answer = (read(objectAt(output, m.index)) as Record<string, unknown> | undefined)?.[key];
    if (answer && typeof answer === "object" && !Array.isArray(answer)) return answer as Record<string, unknown>;
  }
  return null;
}

/** The page a `show_page` call showed, from kiln's answer to it (`answerOf`). */
export function pageOf(c: Pick<ToolCall, "output">): ShownPage | null {
  const p = answerOf(c.output, "kiln_page") as Partial<ShownPage> | null;
  if (!p || typeof p.id !== "string" || !/^[0-9a-f]{32}$/.test(p.id)) return null;
  const heights = heightPairs(p.heights);
  const engines = enginesOf(p.engines);
  // Where it's served (its own path under `/v/<id>`), never anywhere else.
  const path = typeof p.path === "string" && p.path.startsWith(`/v/${p.id}/`) && !p.path.includes("..") ? p.path : `/v/${p.id}/`;
  return { id: p.id, title: typeof p.title === "string" && p.title ? p.title : "A page", path, heights, engines, files: p.files };
}

/** What a page (or live page) goes by while it's being shown: the title its call gave it (its input,
 *  as an object or as text), else its harness's words for the call, else `fallback`. */
export function shownTitle(c: Pick<ToolCall, "title" | "input">, fallback: string): string {
  const input = typeof c.input === "string" ? read(c.input) : c.input;
  const title = input && typeof input === "object" ? (input as { title?: unknown }).title : undefined;
  return typeof title === "string" && title.trim() ? title.trim() : c.title || fallback;
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
