/**
 * Live pages agents show (live.tsx), as their `show_live` calls answered: pure, for the timeline's
 * derivation and the view alike.
 */
import type { ToolCall } from "@/api/harness";

/** A page streamed from the agent's machine, as its `show_live` call answered
 *  (`crate::harness::streams`). */
export interface LiveStream {
  id: string;
  session: string;
  url: string;
  title: string;
  /** The browser profile it runs in, when not the conversation's (it starts again in the same). */
  profile?: "clean" | "personal";
}

/** Whether a tool call is an agent showing a live page (Claude Code's and Codex's names for kiln's tool). */
export const isShowLive = (c: Pick<ToolCall, "name">): boolean => /^mcp__kiln(?:[-_]reach)?__show_live$/.test(c.name);

/** The stream a `show_live` call opened, from its answer (one line of JSON, `crate::reach::shown`). */
export function liveOf(c: Pick<ToolCall, "output">): LiveStream | null {
  if (!c.output) return null;
  try {
    const s = (JSON.parse(c.output) as { kiln_stream?: Partial<LiveStream> }).kiln_stream;
    if (!s || typeof s.id !== "string" || !/^[0-9a-f]{32}$/.test(s.id) || typeof s.session !== "string") return null;
    return { id: s.id, session: s.session, url: typeof s.url === "string" ? s.url : "", title: typeof s.title === "string" && s.title ? s.title : (s.url ?? "A live page") };
  } catch {
    return null;
  }
}

/** A chunk as the server relays it (`crate::browser::stream::frame`): kind, flags, timestamp (µs),
 *  part and count, then the bytes. */
export interface Chunk {
  kind: "v" | "a" | "c";
  flags: string;
  ts: number;
  part: number;
  parts: number;
  data: Uint8Array;
}

export function chunkOf(buf: ArrayBuffer): Chunk | null {
  if (buf.byteLength < 12) return null;
  const b = new Uint8Array(buf);
  const kind = String.fromCharCode(b[0] ?? 0);
  if (kind !== "v" && kind !== "a" && kind !== "c") return null;
  return { kind, flags: String.fromCharCode(b[1] ?? 0), ts: Number(new DataView(buf).getBigInt64(2)), part: b[10] ?? 0, parts: b[11] ?? 1, data: b.subarray(12) };
}

/** Keys that are the viewer's shortcuts on a Mac and the page's with Control elsewhere. */
const SHORTCUTS = new Set(["a", "c", "v", "x", "z", "y", "f", "s", "l", "r", "t", "b", "i", "u", "k"]);

/** Modifiers as the DevTools protocol has them (alt 1, ctrl 2, meta 4, shift 8): a Mac's Command
 *  is Control on a page on another system, for its shortcuts. */
export function modifiers(e: { altKey: boolean; ctrlKey: boolean; metaKey: boolean; shiftKey: boolean; key?: string }, macViewer: boolean, macPage: boolean): number {
  let ctrl = e.ctrlKey;
  let meta = e.metaKey;
  if (macViewer && !macPage && meta && (!e.key || SHORTCUTS.has(e.key.toLowerCase()))) {
    ctrl = true;
    meta = false;
  }
  return (e.altKey ? 1 : 0) | (ctrl ? 2 : 0) | (meta ? 4 : 0) | (e.shiftKey ? 8 : 0);
}
