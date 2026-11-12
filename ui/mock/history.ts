/** W21-R02: the demo uses the opt-in bounded protocol too. Providers reference the fixtures'
 * existing canonical arrays; this module never stores another event list. */
import type { IncomingMessage, ServerResponse } from "node:http";
import type { HEvent, State } from "../src/api/harness.ts";

const incarnation = Date.now();
const sources = new Map<string, { cursor: string; events: () => readonly HEvent[] }>();
const bytes = (e: HEvent) => Buffer.byteLength(JSON.stringify(e));
const mainUser = (e: HEvent) => e.kind === "user" && !e.parent;
const amount = (value: string | null, fallback: number, low: number, high: number) => Math.min(high, Math.max(low, Number(value) || fallback));

export function mockHistory(id: string, source: () => readonly HEvent[], state: State, params?: URLSearchParams) {
  const all = source();
  if (!params?.has("recent_turns") || process.env.MOCK_NO_SYNCED === "1") return { events: all, frame: null };
  const cursor = `${incarnation}.0.${id}`;
  sources.set(id, { cursor, events: source });
  const turns = amount(params.get("recent_turns"), 8, 1, 30);
  const budget = amount(params.get("event_bytes"), 262144, 16384, 1048576);
  const required = new Set<string>();
  const user = all.findLast(mainUser);
  const answer = all.findLast((e) => e.kind === "assistant" && !e.parent);
  if (user) required.add(user.id);
  if (answer) required.add(answer.id);
  const tools = new Set([...state.approvals.map((a) => a.tool_id), ...state.tasks.filter((t) => ["running", "pending", "paused"].includes(t.status)).map((t) => t.tool_id)]);
  for (const e of all) {
    if (((e.kind === "assistant" || e.kind === "thinking") && e.streaming) || (e.kind === "tool" && (e.status === "running" || tools.has(e.tool_id)))) required.add(e.id);
  }
  for (let had = -1; had !== required.size;) {
    had = required.size;
    for (const e of all)
      if (required.has(e.id)) {
        if (e.parent) required.add(e.parent);
        if (e.kind === "user" && e.reply_to) required.add(e.reply_to);
      }
  }
  let start = all.length,
    size = 0,
    seen = 0;
  for (let i = all.length - 1; i >= 0; i--) {
    const e = all[i];
    if (!e || size + bytes(e) > budget) break;
    size += bytes(e);
    start = i;
    if (mainUser(e) && ++seen >= turns) break;
  }
  if (start === all.length && all.length) start = all.length - 1;
  const selected = all.filter((e, i) => i >= start || required.has(e.id));
  return {
    events: selected,
    frame: { t: "history" as const, cursor, before: all[start]?.seq ?? (all.at(-1)?.seq ?? -1) + 1, has_more: start > 0, oversized: selected.reduce((sum, e) => sum + bytes(e), 0) > budget },
  };
}

export function handleMockHistory(req: IncomingMessage, res: ServerResponse, url: URL): boolean {
  const match = /^\/api\/harness\/sessions\/([^/]+)\/events$/.exec(url.pathname);
  if (req.method !== "GET" || !match || !url.searchParams.has("cursor")) return false;
  const source = sources.get(decodeURIComponent(match[1] ?? ""));
  const cursor = url.searchParams.get("cursor");
  res.setHeader("content-type", "application/json");
  res.setHeader("cache-control", "private, no-store");
  if (!source || cursor !== source.cursor) {
    res.writeHead(409);
    res.end(JSON.stringify({ error: "The demo conversation restarted; reconnect and retry." }));
    return true;
  }
  const before = Number(url.searchParams.get("before"));
  if (!Number.isSafeInteger(before) || before < 0) {
    res.writeHead(400);
    res.end(JSON.stringify({ error: "Invalid history boundary" }));
    return true;
  }
  const all = source
    .events()
    .filter((e) => e.seq < before)
    .toSorted((a, b) => a.seq - b.seq);
  const limit = amount(url.searchParams.get("limit"), 100, 1, 1000);
  const budget = amount(url.searchParams.get("bytes"), 262144, 16384, 1048576);
  let start = all.length,
    size = 0,
    count = 0;
  for (let i = all.length - 1; i >= 0; i--) {
    const e = all[i];
    if (!e || (count > 0 && (count >= limit || size + bytes(e) > budget))) break;
    size += bytes(e);
    start = i;
    count++;
  }
  const events = all.slice(start);
  res.end(JSON.stringify({ events, history: { cursor, before: events[0]?.seq ?? null, has_more: start > 0, oversized: size > budget } }));
  return true;
}
