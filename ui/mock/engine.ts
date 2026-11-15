/**
 * A runner's engine in the mock, as the server has it (crates/kiln/src/harness/engine.rs; kiln plan
 * 01a1dd2e…, P2.3): how it runs new agent sessions, `host` or `microvm`.
 *
 *   GET /api/runners/:name/engine   the engine it holds, where that comes from and why, what its owner
 *                                   set, whether it can hold microVMs now and why not, its isolation
 *                                   facts, and its sessions on each engine (`draining`: the other's)
 *   PUT /api/runners/:name/engine   {engine: "host" | "microvm" | null}: set it, or take what was set
 *                                   away (null: what it can do); microvm refused (409) where it can't
 *   POST /api/runners/:name/engine/end     its draining sessions ended now (`ended`, their ids)
 *   POST /api/runners/:name/engine/cancel  the setting before its switch put back while any drains
 *
 * Its engine is what its owner set, else `microvm` where it can hold microVMs, else `host`; one set to
 * microvm where it can't any more is held as host, saying why. Each runner's facts carry it
 * (`isolation.engine`, and `virt.microvm` only on a microvm one), as the server writes them over what
 * each runner says.
 */
import type http from "node:http";
import type { Agent } from "../src/api/types.ts";

type Engine = "host" | "microvm";
interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
  runner: (name: string) => Agent | undefined;
  runners: () => Agent[];
  /** Its sessions now (each on it, in a microVM or not, running or not). */
  sessionsOn: (name: string) => { id: string; kind: string; microvm: boolean; running: boolean }[];
}

const NO_MICROVM = "this machine can't hold agents in microVMs";
const NOT_SAID = "its runner doesn't say whether it starts microVMs: its kiln is from before them";

/** What each runner's owner set, and how many times (metal-ryzen-02's owner runs its agents on its host). */
const asked = new Map<string, { engine: Engine | null; version: number }>([["metal-ryzen-02", { engine: "host", version: 1 }]]);
const askedOf = (name: string) => asked.get(name) ?? { engine: null, version: 0 };
/** Each runner's last switch, for its drain's cancel (metal-ryzen-01's owner took its host setting away 42 minutes ago: one session it started on its host drains). */
const switched = new Map<string, { from: Engine | null; to: Engine | null; at: number }>([["metal-ryzen-01", { from: "host", to: null, at: Date.now() - 42 * 60_000 }]]);
/** The sessions an end of a drain ended, which the engine's view leaves out. */
const ended = new Set<string>();

function capable(a: Agent | undefined): { usable: boolean; why: string | null } | null {
  const facts = a?.facts;
  if (!facts) return null;
  const i = facts.isolation;
  if (!i) return { usable: false, why: NOT_SAID };
  return i.available ? { usable: true, why: null } : { usable: false, why: i.why_not ?? NO_MICROVM };
}

function held(name: string, can: { usable: boolean; why: string | null }): { engine: Engine; from: "server" | "default"; why: string | null } {
  const set = askedOf(name).engine;
  if (set === "microvm" && !can.usable) return { engine: "host", from: "default", why: `its owner set microvm, but it can't hold microVMs now: ${can.why ?? NO_MICROVM}` };
  if (set) return { engine: set, from: "server", why: null };
  return can.usable ? { engine: "microvm", from: "default", why: null } : { engine: "host", from: "default", why: can.why };
}

/** Its facts as the server holds them: its engine in `isolation.engine`, `virt.microvm` only on a microvm one. */
function overlay(a: Agent) {
  const can = capable(a);
  if (!can || !a.facts) return;
  const h = held(a.name, can);
  if (a.facts.isolation) a.facts.isolation.engine = h.engine;
  a.facts.virt.microvm = h.engine === "microvm";
}

function view(name: string, ctx: Ctx) {
  const a = ctx.runner(name);
  const can = capable(a);
  const h = can ? held(name, can) : null;
  const sessions = ctx.sessionsOn(name).filter((s) => s.running && !ended.has(s.id));
  const on = h ? sessions.filter((s) => s.microvm === (h.engine === "microvm")).length : 0;
  const draining = h ? sessions.filter((s) => s.microvm !== (h.engine === "microvm")) : [];
  const sw = switched.get(name);
  const facts = a?.facts;
  return {
    runner: name,
    engine: h?.engine ?? null,
    from: h?.from ?? null,
    why: h?.why ?? null,
    asked: askedOf(name).engine,
    version: askedOf(name).version,
    microvm: can ? { usable: can.usable, why: can.why } : null,
    isolation: facts?.isolation ?? null,
    virt: facts ? { microvm: facts.virt.microvm, kvm: facts.virt.kvm, hvf: facts.virt.hvf } : null,
    sessions: on,
    draining: draining.length,
    draining_sessions: draining.map((s) => ({ id: s.id, name: s.id, kind: s.kind, state: "working", engine: s.microvm ? "microvm" : "host", created_at: 0 })),
    switch: draining.length > 0 && sw && sw.to === askedOf(name).engine ? sw : null,
  };
}

let overlaid = false;

export async function handleEngine(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  // The runners' facts carry their engines from the first request on, as the server's do.
  if (!overlaid) {
    overlaid = true;
    for (const a of ctx.runners()) overlay(a);
  }
  const act = url.pathname.match(/^\/api\/runners\/([^/]+)\/engine\/(end|cancel)$/);
  if (act && req.method === "POST") {
    const name = decodeURIComponent(act[1]!);
    if (!ctx.runner(name)) return (ctx.json(res, 404, { error: `no runner named ${name}` }), true);
    const v = view(name, ctx);
    if (act[2] === "end") {
      const ids = v.draining_sessions.map((d) => d.id);
      ids.forEach((id) => ended.add(id));
      return (ctx.json(res, 200, { ...view(name, ctx), ended: ids }), true);
    }
    const sw = switched.get(name);
    if (!sw || sw.to !== askedOf(name).engine) return (ctx.json(res, 409, { error: `${name}'s engine wasn't switched: set it instead` }), true);
    if (v.draining === 0) return (ctx.json(res, 409, { error: `nothing of ${name} drains: its switch is done, so set its engine instead` }), true);
    asked.set(name, { engine: sw.from, version: askedOf(name).version + 1 });
    switched.set(name, { from: sw.to, to: sw.from, at: Date.now() });
    const a = ctx.runner(name);
    if (a) overlay(a);
    return (ctx.json(res, 200, view(name, ctx)), true);
  }
  const m = url.pathname.match(/^\/api\/runners\/([^/]+)\/engine$/);
  if (!m) return false;
  const name = decodeURIComponent(m[1]!);
  const a = ctx.runner(name);
  const method = req.method ?? "GET";
  if (!a) return (ctx.json(res, 404, { error: `no runner named ${name}` }), true);
  if (method === "GET") return (ctx.json(res, 200, view(name, ctx)), true);
  if (method !== "PUT") return false;
  const shape = 'expected {"engine": "host"} or {"engine": "microvm"}, or {"engine": null} for what the runner can do';
  const b = await ctx.body<Record<string, unknown>>(req);
  const extra = Object.keys(b).find((k) => k !== "engine");
  if (extra) return (ctx.json(res, 400, { error: `${JSON.stringify(extra)} isn't one: ${shape}` }), true);
  if (!("engine" in b)) return (ctx.json(res, 400, { error: shape }), true);
  const e = b.engine;
  if (e !== null && e !== "host" && e !== "microvm") return (ctx.json(res, 400, { error: `a runner's engine is "host" or "microvm", not ${JSON.stringify(e)}` }), true);
  if (e === "microvm") {
    const can = capable(a);
    if (!can) return (ctx.json(res, 409, { error: `${a.label ?? name} hasn't said yet whether it can run microVMs: set its engine once it has` }), true);
    if (!can.usable) return (ctx.json(res, 409, { error: `${a.label ?? name} can't run its sessions in microVMs: ${can.why ?? NO_MICROVM}` }), true);
  }
  if (e !== askedOf(name).engine) switched.set(name, { from: askedOf(name).engine, to: e, at: Date.now() });
  asked.set(name, { engine: e, version: askedOf(name).version + 1 });
  overlay(a);
  return (ctx.json(res, 200, view(name, ctx)), true);
}
