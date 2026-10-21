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
  /** Its engine, when not Chromium, and the device it's laid out as (it starts again in them). */
  engine?: string;
  device?: string;
}

/** Where a point of the frame is in the page: the page drawn whole within the frame (a device's
 *  window letterboxed), `page` its size in CSS pixels, `picture` the picture's in its pixels. */
export function pagePoint(at: { x: number; y: number }, frame: { width: number; height: number }, picture: { width: number; height: number }, page: { width: number; height: number }): { x: number; y: number } {
  if (!picture.width || !picture.height || !frame.width || !frame.height) return { x: Math.round(at.x), y: Math.round(at.y) };
  const k = Math.min(frame.width / picture.width, frame.height / picture.height);
  const [w, h] = [picture.width * k, picture.height * k];
  const [ox, oy] = [(frame.width - w) / 2, (frame.height - h) / 2];
  const clamp = (v: number, max: number) => Math.round(Math.min(max, Math.max(0, v)));
  return { x: clamp(((at.x - ox) * page.width) / w, page.width), y: clamp(((at.y - oy) * page.height) / h, page.height) };
}

/** What's typed in an address field, as an address: one with its scheme as typed (http, https,
 *  file, about); a host and its path over https, or over http when it's this machine's, its LAN's
 *  or has a port (a dev server's); anything else, words, searched for. */
export function typedUrl(typed: string): string {
  const t = typed.trim();
  if (!t) return "";
  if (/^(https?|file):\/\//i.test(t) || /^about:/i.test(t)) return t;
  const host = /^([^\s/?#]+)(?:[/?#]\S*)?$/.exec(t)?.[1];
  if (host) {
    const port = /:\d+$/.test(host);
    const name = host.replace(/:\d+$/, "").replace(/^\[|\]$/g, "").toLowerCase();
    const local = name === "localhost" || name.endsWith(".localhost") || name.endsWith(".local") || name === "::1" || /^(127|10)\.\d|^192\.168\.|^172\.(1[6-9]|2\d|3[01])\.|^169\.254\./.test(name);
    if (name.includes(".") || name.includes(":") || name === "localhost" || port) return `${local || port ? "http" : "https"}://${t}`;
  }
  return `https://duckduckgo.com/?q=${encodeURIComponent(t)}`;
}

/** Whether a tool call is an agent showing a live page (Claude Code's and Codex's names for kiln's tool). */
export const isShowLive = (c: Pick<ToolCall, "name">): boolean => /^mcp__kiln(?:[-_]reach)?__show_live$/.test(c.name);

/** The stream a `show_live` call opened, from its answer (one line of JSON, `crate::reach::shown`). */
export function liveOf(c: Pick<ToolCall, "output">): LiveStream | null {
  if (!c.output) return null;
  try {
    const s = (JSON.parse(c.output) as { kiln_stream?: Partial<LiveStream> }).kiln_stream;
    if (!s || typeof s.id !== "string" || !/^[0-9a-f]{32}$/.test(s.id) || typeof s.session !== "string") return null;
    const str = (v: unknown) => (typeof v === "string" && v ? v : undefined);
    return { id: s.id, session: s.session, url: typeof s.url === "string" ? s.url : "", title: typeof s.title === "string" && s.title ? s.title : (s.url ?? "A live page"), engine: str(s.engine), device: str(s.device) };
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

/** The page's field that has the focus, as the page says it (`crate::browser::stream`'s FOCUS):
 *  its type, input mode, autocomplete, Enter key's hint, label, and whether it takes lines. */
export interface PageField {
  editable: boolean;
  type?: string;
  mode?: string;
  ac?: string;
  hint?: string;
  label?: string;
  multi?: boolean;
}

/** A field's place in the page: x, y, width and height (CSS pixels), its type, input mode and
 *  autocomplete. */
export type FieldPlace = [number, number, number, number, string, string, string];

/** The page's field at a point, if one is there. */
export function fieldAt(places: FieldPlace[], x: number, y: number): PageField | null {
  const p = places.find(([fx, fy, w, h]) => x >= fx && x <= fx + w && y >= fy && y <= fy + h);
  return p ? { editable: true, type: p[4], mode: p[5], ac: p[6] } : null;
}

/** Whether a field takes what a password manager fills: a password, a username, a one-time code. */
export const secret = (f: Pick<PageField, "type" | "ac">): boolean => f.type === "password" || /\b(username|current-password|new-password|one-time-code|webauthn)\b/.test(f.ac ?? "");

/** The viewer's own field, made like the page's: a password's hides what's typed, the keyboard is
 *  the one its input mode wants, and autofill offers what its autocomplete asks for. */
export function shapeOf(f: Pick<PageField, "type" | "mode" | "ac" | "hint">): { type: "text" | "password"; inputMode: string; autocomplete: string; enterKeyHint: string } {
  const password = f.type === "password";
  const modes: Record<string, string> = { email: "email", url: "url", tel: "tel", number: "decimal", search: "search" };
  return {
    type: password ? "password" : "text",
    inputMode: f.mode || modes[f.type ?? ""] || "text",
    autocomplete: f.ac || (password ? "current-password" : f.type === "email" ? "email" : "off"),
    enterKeyHint: f.hint ?? "",
  };
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
