/**
 * A conversation's session socket, as its page asks for it (harness.ts's SessionStore), and the one
 * opening the page opens before the page's code is in (W21-R02, goal 8): the conversation route's
 * beforeLoad runs in the shell, which doesn't carry the session store, so the socket opens there and
 * the store takes it, with what it said meanwhile, when it asks for the same.
 */
import { wsUrl } from "./client";

/**
 * The browser unpacks gzip itself (every current one does): a session's snapshot comes packed, a
 * long conversation's history in a tenth of the bytes.
 */
const GUNZIP = typeof DecompressionStream !== "undefined";

/** A conversation's window (W21-R02; docs/wire.md, "Windows"): its newest turns and about this many bytes of them at the most. */
const WINDOW = { recent_turns: "8", event_bytes: "262144" };
/**
 * The window's first part: the fewest bytes a window is bounded to (history.rs `FLOOR`), its newest events, which are
 * the ones a conversation opens on. The socket's snapshot is that part alone, so a slow link carries a screen's worth
 * before the conversation shows, not the window (kiln-lab bar, 41,000 events: 88 KB packed for the window). The rest
 * of the window follows it on the socket as a message of its own (`earlier`), and joins above what's shown.
 */
const FIRST_BYTES = 16 * 1024;
/** The socket's query: a snapshot packed when it can be unpacked here, a state and a changeset as what changed in them, and its events a window. */
const TAKES = `${GUNZIP ? "?z=gzip&" : "?"}patch=state&patch=changes&window=1&lean=1&${new URLSearchParams(WINDOW)}`;
/** And the window's first part ahead, asked where nothing of the conversation is shown yet: what's shown already is added to or replaced whole. */
const TAKES_FIRST = `&first_bytes=${FIRST_BYTES}`;

/** A session's socket: for a page with nothing of its conversation shown (`first`), the window's first part ahead; `after`, what came after what's shown. */
export function sessionSocketUrl(id: string, first: boolean, after = ""): string {
  return wsUrl(`/api/harness/sessions/${encodeURIComponent(id)}/ws${TAKES}${first ? TAKES_FIRST : ""}${after}`);
}

/** A socket opened ahead, and what it said before its store took it. */
export interface EarlySocket {
  ws: WebSocket;
  url: string;
  heard: MessageEvent[];
}
const early = new Map<string, EarlySocket>();
/** The session store, once harness.ts is here: then its page's navigation readies the store itself, at once. */
let ready: ((id: string, signal: AbortSignal) => Promise<void>) | null = null;
export function storesHere(readySession: (id: string, signal: AbortSignal) => Promise<void>) {
  ready = readySession;
}

/**
 * Opening session `id`'s page: its store readied, when it's here; else the socket it will ask for
 * opened, until its store takes it or the opening is left (`signal`).
 */
export function startSession(id: string, signal: AbortSignal) {
  if (ready) {
    void ready(id, signal);
    return;
  }
  if (signal.aborted || early.has(id)) return;
  const url = sessionSocketUrl(id, true);
  let ws: WebSocket;
  try {
    ws = new WebSocket(url);
  } catch {
    // The store opens its own.
    return;
  }
  ws.binaryType = "arraybuffer";
  const ahead: EarlySocket = { ws, url, heard: [] };
  ws.onmessage = (message) => ahead.heard.push(message);
  ws.onclose = () => {
    if (early.get(id) === ahead) early.delete(id);
  };
  early.set(id, ahead);
  signal.addEventListener(
    "abort",
    () => {
      if (early.get(id) !== ahead) return;
      early.delete(id);
      ws.close();
    },
    { once: true },
  );
}

/** The socket opened ahead for session `id`, still open or opening, for its store to take when it asks for `url`; closed when it asks for another. */
export function takeSessionSocket(id: string, url: string): EarlySocket | undefined {
  const ahead = early.get(id);
  if (!ahead) return undefined;
  early.delete(id);
  if (ahead.url === url && ahead.ws.readyState <= WebSocket.OPEN) return ahead;
  ahead.ws.close();
  return undefined;
}
