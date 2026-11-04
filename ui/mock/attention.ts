/** Attention API simulator, plan 01a199e1b177752e84dad5dc990e6a15. */
import type http from "node:http";
import type { AgentInfo } from "../src/api/harness.ts";
import type { AttentionCommand, AttentionMark, AttentionSnapshot } from "../src/api/attention.ts";
import { requestsForFamily, requestsWakePark } from "../src/lib/attention-requests.ts";
import type { AttentionClose, AttentionFrame, AttentionPair } from "../src/lib/attention-exposure.ts";
const marks: Record<string, AttentionMark> = {};
const archived = new Map<string, AgentInfo>();
const undo = new Map<string, AttentionMark>();
const settings: AttentionSnapshot["settings"] = {
  sort: "learned",
  auto_park_days: null,
  archive_after_days: 7,
  auto_archive: false,
  park_later_minutes: 60,
  park_evening_hour: 18,
  park_morning_hour: 9,
};
const uses: NonNullable<AttentionSnapshot["uses"]> = [];
const pairs = new Map<string, AttentionPair>();
const frames = new Map<string, { frame?: AttentionFrame; bound?: number; choice?: string }>();
let pairCounter = 0;

export async function handleAttention(req: http.IncomingMessage, res: http.ServerResponse, url: URL, all: () => AgentInfo[]): Promise<boolean> {
  if (url.pathname !== "/api/harness/attention" && url.pathname !== "/api/harness/attention/replay" && url.pathname !== "/api/mock/attention/reset" && url.pathname !== "/api/mock/attention/visits")
    return false;
  const send = (code: number, value: unknown) => {
    res.writeHead(code, { "content-type": "application/json" });
    res.end(JSON.stringify(value));
  };
  if (url.pathname === "/api/mock/attention/reset" && req.method === "POST") {
    for (const key of Object.keys(marks)) delete marks[key];
    archived.clear();
    undo.clear();
    uses.length = 0;
    pairs.clear();
    frames.clear();
    pairCounter = 0;
    Object.assign(settings, { sort: "learned", auto_park_days: null, archive_after_days: 7, auto_archive: false, park_later_minutes: 60, park_evening_hour: 18, park_morning_hour: 9 });
    send(200, {});
    return true;
  }
  // Browser unload can outlive Playwright's page interception. Assert what the simulator retained.
  if (url.pathname === "/api/mock/attention/visits" && req.method === "GET") {
    send(200, {
      visits: [...frames].map(([exposure, record]) => ({ exposure, rendered_revision: record.frame?.frame_revision ?? null, closed_revision: record.bound ?? null, choice: record.choice ?? null })),
    });
    return true;
  }
  const now = Date.now();
  const model = { observations: 0, ignored: 0, position_corrected: 0, improves_baseline: false, reason: "Using your recent interactions while the model learns.", parameters: [], comparisons: [] };
  if (url.pathname.endsWith("/replay")) {
    send(200, { at: Number(url.searchParams.get("at")), order: all().map((a) => ({ agent: a.id, score: 0, reason: "Recent interactions" })), model, choices: [] });
    return true;
  }
  let token: AttentionSnapshot["undo"];
  let mode = url.searchParams.get("mode") ?? "arrival";
  let visit = url.searchParams.get("visit") ?? "legacy";
  if (req.method === "POST") {
    const chunks: Buffer[] = [];
    for await (const c of req) chunks.push(c as Buffer);
    const command = JSON.parse(Buffer.concat(chunks).toString("utf8")) as AttentionCommand | (AttentionFrame & { action: "render" }) | (AttentionClose & { action: "close" });
    if (command.action === "render") {
      const record = frames.get(command.exposure) ?? {};
      if (record.bound === undefined || command.frame_revision <= record.bound) {
        if (!record.frame || record.frame.frame_revision < command.frame_revision) record.frame = command;
      }
      frames.set(command.exposure, record);
      send(200, { ok: true, accepted: true });
      return true;
    }
    if (command.action === "close") {
      const record = frames.get(command.exposure) ?? {};
      record.bound = Math.max(record.bound ?? 0, command.frame_revision);
      frames.set(command.exposure, record);
      send(200, { ok: true, accepted: true });
      return true;
    }
    mode = command.mode ?? "arrival";
    visit = command.visit ?? "legacy";
    const id = command.agent;
    if (command.action === "settings") Object.assign(settings, command.settings);
    else if (command.action === "reorder")
      (command.order ?? []).forEach((id, i) => {
        marks[id] = { ...marks[id], pinned: i, revision: (marks[id]?.revision ?? 0) + 1 };
      });
    else if (id) {
      const a = all().find((a) => a.id === id) ?? archived.get(id);
      if (!a) {
        send(404, { error: "No such agent" });
        return true;
      }
      const previous = marks[id] ?? { revision: 0 };
      const next = { ...previous, revision: previous.revision + 1 };
      if (command.action === "undo") {
        if (command.revision !== previous.revision) {
          send(409, { error: "This chat changed since that action" });
          return true;
        }
        const old = undo.get(id);
        if (old) marks[id] = { ...old, revision: next.revision };
      } else {
        if (command.action === "pin") {
          next.pinned = now;
          delete next.parked_until;
          delete next.deferred_requests;
        } else if (command.action === "unpin") delete next.pinned;
        else if (command.action === "park") {
          next.parked_at = now;
          next.parked_until = command.until;
          next.deferred_requests = requestsForFamily(all(), id).requests;
          delete next.pinned;
        } else if (command.action === "unpark") {
          delete next.parked_until;
          delete next.deferred_requests;
          next.wake_reason = "You unparked this chat";
        } else if (command.action === "seen" || command.action === "visit") next.updated_seen = now;
        else if (command.action === "archive") {
          archived.set(id, { ...a });
          next.archived = true;
        } else if (command.action === "restore" || command.action === "restore_copy") {
          archived.delete(id);
          delete next.archived;
        }
        if (["pin", "unpin", "park", "unpark"].includes(command.action)) {
          undo.set(id, previous);
          token = { agent: id, revision: next.revision };
        }
        marks[id] = next;
      }
      if (command.action === "visit") {
        uses.push({ agent: id, kind: "open", at: now });
        if (command.exposure && command.frame_revision !== undefined) {
          const record = frames.get(command.exposure) ?? {};
          record.bound = command.frame_revision;
          record.choice = id;
          frames.set(command.exposure, record);
        }
      }
    }
  }
  for (const a of all()) {
    const mark = marks[a.id];
    if (!mark?.parked_until) continue;
    const reason = requestsWakePark(requestsForFamily(all(), a.id), mark.deferred_requests) ? "Needs you" : mark.parked_until <= now ? "Park time reached" : undefined;
    if (reason) {
      delete mark.parked_until;
      delete mark.deferred_requests;
      mark.wake_reason = reason;
      mark.revision++;
    }
  }
  const order = [...all()].sort((a, b) => (uses.findLast((u) => u.agent === b.id)?.at ?? 0) - (uses.findLast((u) => u.agent === a.id)?.at ?? 0) || a.id.localeCompare(b.id)).map((a) => a.id);
  if (mode === "arrival" && !pairs.has(visit)) {
    pairCounter++;
    pairs.set(visit, { key: `mock-pair-${pairCounter}`, order_a: order, order_b: [...order], arm: 0, model_order: false });
  }
  const pair = pairs.get(visit);
  send(200, {
    you: "owner",
    model,
    agents: marks,
    order: pair ? (pair.arm === 0 ? pair.order_a : pair.order_b) : [],
    pair,
    exposure: pair?.key,
    settings,
    archived: [...archived.values()],
    uses,
    examination: { curves: [], unavailable: [], exposures: 0, censored: [...frames.values()].filter((f) => f.bound === undefined).length, ignored: 0, opportunities: 0 },
    ...(token ? { undo: token } : {}),
  } satisfies AttentionSnapshot);
  return true;
}
