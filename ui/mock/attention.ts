/** Attention API simulator, plan 01a199e1b177752e84dad5dc990e6a15. */
import type http from "node:http";
import type { AgentInfo } from "../src/api/harness.ts";
import type { AttentionFit, AttentionMark, AttentionSettings, AttentionSnapshot, AttentionUrgency } from "../src/api/attention.ts";
import { requestsForFamily, requestsWakePark } from "../src/lib/attention-requests.ts";
import type { AttentionClose, AttentionFrame, AttentionPair } from "../src/lib/attention-exposure.ts";

/** What the UI posts: an organize command, a visit or a frame, and the snapshot it wants back. */
interface Body {
  action: string;
  agent?: string;
  until?: number;
  revision?: number;
  settings?: Partial<AttentionSettings>;
  order?: string[];
  mode?: "arrival" | "refresh";
  visit?: string;
  source?: "list" | "direct";
  exposure?: string;
  frame_revision?: number;
  chosen_position?: number;
}
type Use = { agent: string; kind: string; at: number };

/** The server's defaults (crates/kiln/src/harness/attention/store.rs), archiving parked chats on by the person's decision. */
const DEFAULTS: AttentionSettings = { sort: "learned", auto_park_days: null, archive_after_days: 7, auto_archive: true, park_later_minutes: 60, park_evening_hour: 18, park_morning_hour: 9 };
const marks: Record<string, AttentionMark> = {};
const archived = new Map<string, AgentInfo>();
const undo = new Map<string, AttentionMark>();
const settings: AttentionSettings = { ...DEFAULTS };
const uses: Use[] = [];
const pairs = new Map<string, AttentionPair>();
const frames = new Map<string, { frame?: AttentionFrame; bound?: number; choice?: string; position?: number }>();
/** Requests set waiting a while (`POST /api/mock/attention/urgency`), most urgent first. */
const urgent = new Map<string, AttentionUrgency>();
let pairCounter = 0;

/** A model that has learned something, for the Settings page to say how it does (names as the server gives them). */
const MODEL: AttentionFit = {
  observations: 42,
  ignored: 3,
  position_corrected: 17,
  improves_baseline: true,
  reason: "Learned decay and use weights beat your most recent use and the order learned without agent turns on your newest picks.",
  parameters: [
    { name: "decay", value: 0.48, standard_error: 0.06 },
    { name: "weight.message", value: 2.1, standard_error: 0.4 },
    { name: "weight.open", value: 1.2, standard_error: 0.3 },
    { name: "weight.subagent_turn", value: 0.2, standard_error: null },
    { name: "range_weight", value: 0.7, standard_error: 0.2 },
    { name: "scale.turn", value: 0.35, standard_error: 0.1 },
  ],
  comparisons: [
    { name: "act_r", choices: 6, log_likelihood: -7.91 },
    { name: "act_r_with_range", choices: 6, log_likelihood: -7.42 },
    { name: "your_recency", choices: 6, log_likelihood: -9.86 },
    { name: "without_agent_turns", choices: 6, log_likelihood: -8.75 },
  ],
  unavailable_comparisons: [{ name: "latest_activity", reason: "Today's order can't be rebuilt for a pick made in the past, so it isn't compared." }],
};

/** Why a request woke a park, as the server says it (harness/attention/requests.rs): the first the park didn't defer. */
function wakeWords(agents: AgentInfo[], id: string, mark: AttentionMark): string {
  const family = agents.filter((a) => a.id === id || a.parent === id);
  const deferred = (kind: string, rid: string, at: number) => mark.deferred_requests?.some((d) => d.kind === kind && d.id === rid && d.at === at) ?? false;
  for (const a of family) {
    for (const ap of a.approvals)
      if (!deferred("approval", ap.id, ap.at)) return ap.tool === "question" ? "It asks you something" : ap.tool === "plan" ? "It proposes a plan" : "It asks for your approval";
    if (a.held && !deferred("held", a.held.why, a.held.since)) return "Its goals wait for you";
    if (a.status === "exited" && a.exit?.error && a.exit.again_at == null) return "It stopped with an error";
    if (a.status === "waiting" && a.approvals.length === 0) return "It waits for you";
  }
  return "It waits for you";
}

export async function handleAttention(req: http.IncomingMessage, res: http.ServerResponse, url: URL, all: () => AgentInfo[]): Promise<boolean> {
  const p = url.pathname;
  if (p !== "/api/harness/attention" && p !== "/api/harness/attention/replay" && !p.startsWith("/api/mock/attention/")) return false;
  const send = (code: number, value: unknown) => {
    res.writeHead(code, { "content-type": "application/json" });
    res.end(JSON.stringify(value));
  };
  const read = async <T>(): Promise<T> => {
    const chunks: Buffer[] = [];
    for await (const c of req) chunks.push(c as Buffer);
    return JSON.parse(Buffer.concat(chunks).toString("utf8") || "{}") as T;
  };
  const now = Date.now();
  if (p === "/api/mock/attention/reset" && req.method === "POST") {
    for (const key of Object.keys(marks)) delete marks[key];
    archived.clear();
    undo.clear();
    uses.length = 0;
    pairs.clear();
    frames.clear();
    urgent.clear();
    pairCounter = 0;
    Object.assign(settings, DEFAULTS);
    send(200, {});
    return true;
  }
  // A request set as waiting longer than any answered (`stale`), or not: Needs you's order, most urgent first.
  if (p === "/api/mock/attention/urgency" && req.method === "POST") {
    const { agent, stale = true } = await read<{ agent: string; stale?: boolean }>();
    urgent.set(agent, {
      known: true,
      score: 4,
      stale,
      since: now - 600_000,
      waited: 600_000,
      expected_wait: 150_000,
      observations: 5,
      reason: "Wait divided by your median answered wait for this kind of request.",
    });
    send(200, {});
    return true;
  }
  // kiln parking a quiet chat by itself, as the server does after the days set (attention plan step 8): until the
  // archive with archiving on, else as long as the quiet that parked it (store::automatic_park_ends).
  if (p === "/api/mock/attention/auto-park" && req.method === "POST") {
    const { agent, days = 3 } = await read<{ agent: string; days?: number }>();
    const before = marks[agent] ?? { revision: 0 };
    undo.set(agent, before);
    const rest: AttentionMark = { ...before };
    delete rest.pinned;
    delete rest.deferred_requests;
    delete rest.correction;
    marks[agent] = {
      ...rest,
      parked_at: now,
      parked_until: now + (settings.auto_archive ? settings.archive_after_days : days) * 86_400_000,
      automatic: true,
      reason: `Automatically parked: no interaction or turns for ${days} days`,
      revision: before.revision + 1,
    };
    send(200, {});
    return true;
  }
  // Browser unload can outlive Playwright's page interception. Assert what the simulator retained.
  if (p === "/api/mock/attention/visits" && req.method === "GET") {
    send(200, {
      visits: [...frames].map(([exposure, record]) => ({
        exposure,
        rendered_revision: record.frame?.frame_revision ?? null,
        shown: record.frame?.shown ?? null,
        closed_revision: record.bound ?? null,
        choice: record.choice ?? null,
        position: record.position ?? null,
      })),
    });
    return true;
  }
  if (p.endsWith("/replay")) {
    const order = all()
      .filter((a) => !a.subagent)
      .map((a) => ({ agent: a.id, score: null, reason: uses.some((u) => u.agent === a.id) ? "Your latest interaction" : "No recorded interaction from you" }));
    send(200, { at: Number(url.searchParams.get("at")), order, model: MODEL, choices: [] });
    return true;
  }
  let token: AttentionSnapshot["undo"] = null;
  let mode = url.searchParams.get("mode") ?? "arrival";
  let visit = url.searchParams.get("visit");
  if (req.method === "POST") {
    const command = await read<Body & Partial<AttentionFrame> & Partial<AttentionClose>>();
    if (command.action === "render" && command.exposure && command.frame_revision !== undefined) {
      const record = frames.get(command.exposure) ?? {};
      if (record.bound === undefined || command.frame_revision <= record.bound) {
        if (!record.frame || record.frame.frame_revision < command.frame_revision) record.frame = command as AttentionFrame;
      }
      frames.set(command.exposure, record);
      send(200, { ok: true, accepted: true });
      return true;
    }
    if (command.action === "close" && command.exposure && command.frame_revision !== undefined) {
      const record = frames.get(command.exposure) ?? {};
      if (record.choice === undefined) record.bound = Math.max(record.bound ?? 0, command.frame_revision);
      frames.set(command.exposure, record);
      send(200, { ok: true, accepted: true });
      return true;
    }
    mode = command.mode ?? (command.action === "visit" || command.action === "seen" ? "refresh" : "arrival");
    visit = command.visit ?? null;
    const id = command.agent;
    if (command.action === "settings") Object.assign(settings, command.settings);
    else if (command.action === "reorder")
      (command.order ?? []).forEach((id, i) => {
        marks[id] = { ...marks[id], pinned: i + 1, revision: (marks[id]?.revision ?? 0) + 1 };
      });
    else if (id) {
      const a = all().find((a) => a.id === id) ?? archived.get(id);
      if (!a) {
        send(404, { error: "No such agent" });
        return true;
      }
      const previous = marks[id] ?? { revision: 0 };
      const next: AttentionMark = { ...previous, revision: previous.revision + 1 };
      if (command.action === "undo") {
        if (command.revision !== previous.revision) {
          send(409, { error: "A newer choice replaced this action" });
          return true;
        }
        const old = undo.get(id);
        if (old) marks[id] = { ...old, revision: next.revision };
      } else {
        if (command.action === "pin") {
          next.pinned = now;
          delete next.parked_until;
          delete next.parked_at;
          delete next.deferred_requests;
          delete next.reason;
          delete next.automatic;
          delete next.correction;
        } else if (command.action === "unpin") delete next.pinned;
        else if (command.action === "park") {
          if (!command.until || command.until <= now) {
            send(400, { error: "park time must be in the future" });
            return true;
          }
          next.parked_at = now;
          next.parked_until = command.until;
          const deferred = requestsForFamily(all(), id).requests;
          if (deferred.length > 0) next.deferred_requests = deferred;
          else delete next.deferred_requests;
          next.reason = "You parked it";
          delete next.pinned;
          delete next.automatic;
          delete next.correction;
        } else if (command.action === "unpark") {
          // Bringing back what kiln parked by itself corrects it: said so, for review.
          if (previous.automatic) next.correction = true;
          else delete next.correction;
          next.reason = previous.automatic ? "You brought it back: kiln had parked it automatically" : "You brought it back";
          next.woke_at = now;
          delete next.parked_until;
          delete next.parked_at;
          delete next.deferred_requests;
          delete next.automatic;
        } else if (command.action === "seen" || command.action === "visit") next.updated_seen = now;
        else if (command.action === "archive") {
          archived.set(id, { ...a });
          next.archived = true;
          next.archive_status = "archived";
        } else if (command.action === "restore" || command.action === "restore_copy") {
          archived.delete(id);
          delete next.archived;
          delete next.archive_status;
        }
        if (["pin", "unpin", "park", "unpark"].includes(command.action)) {
          undo.set(id, previous);
          token = { agent: id, revision: next.revision };
        }
        marks[id] = next;
      }
      if (command.action === "visit") {
        uses.push({ agent: id, kind: "open", at: now });
        if (command.source === "list" && command.exposure && command.frame_revision !== undefined && command.chosen_position !== undefined) {
          const record = frames.get(command.exposure) ?? {};
          record.bound = command.frame_revision;
          record.choice = id;
          record.position = command.chosen_position;
          frames.set(command.exposure, record);
        }
      } else if (["pin", "park", "unpark", "archive", "restore", "restore_copy"].includes(command.action)) uses.push({ agent: id, kind: command.action, at: now });
    }
  }
  // A park ends when a request it didn't defer comes, or its time does, saying which (the server's words, requests.rs).
  for (const a of all()) {
    const mark = marks[a.id];
    if (!mark?.parked_until) continue;
    const reason = requestsWakePark(requestsForFamily(all(), a.id), mark.deferred_requests) ? wakeWords(all(), a.id, mark) : mark.parked_until <= now ? "Its park time ended" : undefined;
    if (reason) {
      delete mark.parked_until;
      delete mark.parked_at;
      delete mark.deferred_requests;
      delete mark.automatic;
      mark.reason = reason;
      mark.woke_at = now;
      mark.revision++;
    }
  }
  const visible = all().filter((a) => !archived.has(a.id));
  const last = (id: string) => uses.findLast((u) => u.agent === id)?.at ?? 0;
  const order = [...visible].sort((a, b) => last(b.id) - last(a.id) || a.id.localeCompare(b.id)).map((a) => a.id);
  if (mode === "arrival") {
    const key = visit ?? `mock-visit-${pairCounter + 1}`;
    if (!pairs.has(key)) {
      pairCounter++;
      pairs.set(key, { key: `mock-pair-${pairCounter}`, order_a: order, order_b: [...order], arm: 0, model_order: false });
    }
    visit = key;
  }
  const pair = visit ? pairs.get(visit) : undefined;
  const agents: Record<string, AttentionMark> = {};
  for (const [id, mark] of Object.entries(marks)) agents[id] = { ...mark };
  for (const a of visible) if (!agents[a.id]) agents[a.id] = { revision: 0 };
  for (const id of order) {
    const mark = agents[id];
    if (mark) mark.rank_reason = last(id) > 0 ? "Your latest interaction" : "No recorded interaction from you";
  }
  // Needs you, most urgent first: the requests set waiting longest, then the rest oldest first.
  const waiting = visible.filter((a) => a.approvals.length > 0 || a.status === "waiting" || a.held);
  const since = (a: AgentInfo) => Math.min(...a.approvals.map((x) => x.at), a.held?.since ?? Number.POSITIVE_INFINITY);
  const requestOrder = [...waiting].sort((a, b) => Number(urgent.has(b.id)) - Number(urgent.has(a.id)) || since(a) - since(b) || a.id.localeCompare(b.id)).map((a) => a.id);
  for (const [id, urgency] of urgent) if (agents[id]) agents[id] = { ...agents[id], urgency };
  send(200, {
    you: "owner",
    model: pair ? MODEL : null,
    agents,
    order: pair ? (pair.arm === 0 ? pair.order_a : pair.order_b).filter((id) => !archived.has(id)) : order,
    request_order: requestOrder,
    pair: pair ?? null,
    exposure: pair?.key ?? null,
    settings,
    settings_defaults: { auto_park_days: 3 },
    archived: [...archived.values()],
    undo: token,
  } satisfies AttentionSnapshot);
  return true;
}
