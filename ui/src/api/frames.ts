/**
 * What comes down the UI's sockets, read before it's applied (docs/wire.md, "What can't be read").
 * A frame is applied only when it reads as its socket's type: JSON, an object, its tag one of the
 * generated type's tags. Each socket's client keeps those tags in a table written
 * `satisfies Record<Tag, true>`, so the table can't drift from the type. A frame that doesn't read
 * isn't applied: the page says so up the same socket (`{t: "unread", of, why}`), and the server,
 * which sent it, notes it.
 */
import type { StreamDown, UnreadFrame } from "./wire";

/** A frame that came down, read: the frame, or what the page tells the server of it. */
export type Read<T> = { frame: T } | { unread: UnreadFrame };

/** What the page tells the server of a frame it couldn't read: its tag (`of`; "" when it couldn't read that far) and why. */
export function unread(of: string, why: string): { unread: UnreadFrame } {
  return { unread: { t: "unread", of, why } };
}

/** `data` read as a frame whose tag (`tag`) is one of `tags`. */
export function readFrame<T>(data: string, tag: "t" | "type", tags: Record<string, true>): Read<T> {
  let v: unknown;
  try {
    v = JSON.parse(data);
  } catch (e) {
    return unread("", `not JSON: ${e instanceof Error ? e.message : String(e)}`);
  }
  if (typeof v !== "object" || v === null || Array.isArray(v)) return unread("", "not a JSON object");
  const t = (v as Record<string, unknown>)[tag];
  if (typeof t !== "string") return unread("", `no ${tag}`);
  if (!Object.hasOwn(tags, t)) return unread(t, "unknown frame");
  return { frame: v as T };
}

/** The keys of a union of objects, each member's. */
type KeysOf<T> = T extends unknown ? keyof T : never;

/**
 * What goes down a live page's socket as text (`StreamDown`), by the one key that names each frame:
 * the server's `webrtc` and `refused`, and each kind of what the stream's runner says of it
 * (`StreamSaid`). Written `satisfies Record<..., true>`, so it can't drift from the type.
 */
const STREAM_FRAMES = {
  webrtc: true,
  refused: true,
  opened: true,
  error: true,
  agent: true,
  navigated: true,
  loaded: true,
  focus: true,
  fields: true,
  selected: true,
  hidden: true,
  tabs: true,
  dialog: true,
  stats: true,
  recording: true,
  recorded: true,
  copied: true,
  closed: true,
} satisfies Record<KeysOf<StreamDown>, true>;

/** `data`, from a live page's socket, read as a frame whose one key is one of `STREAM_FRAMES`. */
export function readStreamFrame(data: string): Read<StreamDown> {
  let v: unknown;
  try {
    v = JSON.parse(data);
  } catch (e) {
    return unread("", `not JSON: ${e instanceof Error ? e.message : String(e)}`);
  }
  if (typeof v !== "object" || v === null || Array.isArray(v)) return unread("", "not a JSON object");
  const keys = Object.keys(v);
  const [key] = keys;
  if (keys.length !== 1 || key === undefined) return unread("", `${keys.length} keys, not one naming what it says`);
  if (!Object.hasOwn(STREAM_FRAMES, key)) return unread(key, "unknown frame");
  return { frame: v as StreamDown };
}
