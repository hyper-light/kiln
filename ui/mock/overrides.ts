/**
 * A session's live overrides in the mock, as the server takes them (crates/kiln/src/harness/overrides.rs;
 * kiln plan 01a1dd2e…, P6b.4): what a person changes of a running session in place, kept on it, and
 * how each place took each change (its microVM, its harness, kiln's scope). Its state carries them
 * (`state.overrides`), as the server's does.
 *
 *   POST /api/harness/sessions/:id/ports               {member?, bind, host_port, guest_port, protocol}  publish (microVM only)
 *   POST /api/harness/sessions/:id/ports/unpublish     {bind, host_port, protocol}
 *   POST /api/harness/sessions/:id/egress              {member?, host, port}                             grant (microVM only)
 *   POST /api/harness/sessions/:id/egress/revoke       {member?, host, port}
 *   POST /api/harness/sessions/:id/networks            {member?, network}                                connect (microVM only)
 *   POST /api/harness/sessions/:id/networks/disconnect {member?, network}
 *   POST /api/harness/sessions/:id/volumes             {path, access, perms?, at?, members?}             add (every engine; perms {read, write, exec}: a proposal)
 *   POST /api/harness/sessions/:id/volumes/remove      {path}
 *
 * Each answers `{taken, agent, engine, change, said}`; a host session's network refused (409) naming
 * its engine; a member is `harness` (the default), `agent:<name>` or `mcp:<name>`.
 */
import type http from "node:http";
import type { LiveChange, LiveHeldVolume, LiveMember, LiveOverrides, LivePort, LiveTook } from "../src/api/wire/index.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
  /** The session's engine and harness, or none for one the mock doesn't know. */
  session: (id: string) => { engine: "microvm" | "host"; harness: string; name: string } | null;
}

const held = new Map<string, LiveOverrides>();
const listeners: ((id: string) => void)[] = [];

/** What session `id` holds of a person's changes (its state's `overrides`). */
export function overridesOf(id: string): LiveOverrides | undefined {
  return held.get(id);
}
/** `fn` hears each session whose overrides changed: its module sends that session's state again. */
export function onOverrides(fn: (id: string) => void) {
  listeners.push(fn);
}

const NAMES: Record<string, string> = { claude: "Claude Code", codex: "Codex", devin: "Devin", gemini: "Gemini" };
const microvmTook: LiveTook = { took: "now", how: "its microVM took it live" };

/** How a harness takes a folder, in the server's words (crates/kiln/src/harness/{claude,kinds}.rs). */
function harnessTook(harness: string, writable: boolean): LiveTook {
  if (harness === "claude")
    return { took: "now", how: "Claude Code took them as its additional directories (permissions.additionalDirectories, a flag setting for this session), and takes them as --add-dir as it starts" };
  if (harness === "codex" && !writable) return { took: "nothing", why: "Codex's sandbox reads outside its folder already: it holds writes alone" };
  if (harness === "codex") return { took: "next_turn", how: "Codex takes the folders it may write with each turn (turn/start's sandboxPolicy.writableRoots)" };
  return { took: "next_start", how: `${NAMES[harness] ?? harness} takes more folders as its session opens: it opens its session again with them, its conversation going on` };
}
const scopeTook = (at: string, access: "ro" | "rw"): LiveTook => ({
  took: "now",
  how: access === "ro" ? `kiln holds its file calls in ${at} to reading: a change there is refused` : `kiln lets its file calls in ${at} read and change it`,
});

/** A member as written (`harness`, `agent:<name>`, `mcp:<name>`), the harness's own by default. */
function memberOf(said: unknown, harness: string): LiveMember | string {
  if (said === undefined || said === null || said === "" || said === "harness") return { kind: "harness", name: harness };
  if (typeof said !== "string") return "a member is `harness`, `agent:<name>` or `mcp:<name>`";
  const m = /^(agent|mcp):(.+)$/.exec(said.trim());
  return m ? ({ kind: m[1] as "agent" | "mcp", name: m[2]! } as LiveMember) : `\`${said}\` isn't a member: \`harness\`, \`agent:<name>\` or \`mcp:<name>\``;
}
const sameMember = (a: LiveMember, b: LiveMember) => a.kind === b.kind && a.name === b.name;

export async function handleOverrides(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const m = url.pathname.match(/^\/api\/harness\/sessions\/([^/]+)\/(ports|egress|networks|volumes)(?:\/(unpublish|revoke|disconnect|remove))?$/);
  if (!m || (req.method ?? "GET") !== "POST") return false;
  const id = decodeURIComponent(m[1]!);
  const [what, undo] = [m[2]!, m[3]];
  const fail = (status: number, error: string) => (ctx.json(res, status, { error }), true);
  const s = ctx.session(id);
  if (!s) return fail(404, `no agent ${id}`);
  const valid: Record<string, string | undefined> = { ports: "unpublish", egress: "revoke", networks: "disconnect", volumes: "remove" };
  if (undo && valid[what] !== undo) return fail(404, "not found");
  const microvm = s.engine === "microvm";
  if (what !== "volumes" && !microvm) return fail(409, `${s.name} runs on its machine's host (engine host): ports, egress and networks are a microVM session's alone`);
  const b = await ctx.body<Record<string, unknown>>(req);
  const o = held.get(id) ?? { revision: 0, ports: [], egress: [], networks: [], volumes: [] };
  let change: LiveChange;
  if (what === "ports") {
    const bind = String(b.bind ?? "").trim();
    const [hostPort, protocol] = [Number(b.host_port), b.protocol];
    if (!bind || !(hostPort > 0 && hostPort < 65536) || (protocol !== "tcp" && protocol !== "udp")) return fail(400, "a port is published at `bind`:`host_port`/`protocol` (tcp or udp)");
    if (undo) {
      change = { op: "unpublish", of: { bind, host_port: hostPort, protocol } };
      o.ports = (o.ports ?? []).filter((p) => !(p.port.bind === bind && p.port.host_port === hostPort && p.port.protocol === protocol));
    } else {
      const member = memberOf(b.member, s.harness);
      const guestPort = Number(b.guest_port);
      if (typeof member === "string") return fail(400, member);
      if (!(guestPort > 0 && guestPort < 65536)) return fail(400, "`guest_port` is the member's port in the microVM");
      if ((o.ports ?? []).some((p) => p.port.bind === bind && p.port.host_port === hostPort && p.port.protocol === protocol)) return fail(409, `${bind}:${hostPort}/${protocol} is published already`);
      const port: LivePort = { member, bind, host_port: hostPort, guest_port: guestPort, protocol };
      change = { op: "publish", of: port };
      o.ports = [...(o.ports ?? []), { port, microvm: microvmTook }];
    }
  } else if (what === "egress") {
    const member = memberOf(b.member, s.harness);
    const host = String(b.host ?? "")
      .trim()
      .toLowerCase();
    const port = Number(b.port);
    if (typeof member === "string") return fail(400, member);
    if (!/^[a-z0-9.:-]+$/.test(host) || !(port > 0 && port < 65536)) return fail(400, "egress is to a `host` (a DNS name or an address) on a `port`");
    const egress = { member, host, port };
    if (undo) {
      change = { op: "revoke", of: egress };
      o.egress = (o.egress ?? []).filter((e) => !(sameMember(e.egress.member, member) && e.egress.host === host && e.egress.port === port));
    } else {
      change = { op: "grant", of: egress };
      o.egress = [...(o.egress ?? []).filter((e) => !(sameMember(e.egress.member, member) && e.egress.host === host && e.egress.port === port)), { egress, microvm: microvmTook }];
    }
  } else if (what === "networks") {
    const member = memberOf(b.member, s.harness);
    const name = String(b.network ?? "").trim();
    if (typeof member === "string") return fail(400, member);
    if (!name) return fail(400, "`network` is the network's name");
    const network = { member, name };
    if (undo) {
      change = { op: "disconnect", of: network };
      o.networks = (o.networks ?? []).filter((n) => !(sameMember(n.network.member, member) && n.network.name === name));
    } else {
      change = { op: "connect", of: network };
      o.networks = [...(o.networks ?? []).filter((n) => !(sameMember(n.network.member, member) && n.network.name === name)), { network, microvm: microvmTook }];
    }
  } else {
    const path = String(b.path ?? "").trim();
    if (!path.startsWith("/")) return fail(400, "`path` is an absolute folder on the session's machine");
    if (undo) {
      change = { op: "remove", of: path };
      o.volumes = (o.volumes ?? []).filter((v) => v.volume.path !== path);
    } else {
      // Its permissions, each its own (a proposal beside `access`, which says ro or rw alone): any of read, write, execute.
      const p = b.perms as { read?: unknown; write?: unknown; exec?: unknown } | undefined;
      const perms = p && typeof p === "object" ? { read: p.read === true, write: p.write === true, exec: p.exec === true } : undefined;
      if (perms && !perms.read && !perms.write && !perms.exec) return fail(400, "`perms` grants at least one of read, write, execute");
      const access: "ro" | "rw" | null = perms ? (perms.write ? "rw" : "ro") : b.access === "ro" || b.access === "rw" ? b.access : null;
      if (!access) return fail(400, "`access` is ro or rw");
      const at = typeof b.at === "string" && b.at.trim() ? b.at.trim() : path;
      if (!microvm && at !== path) return fail(400, `a session on its machine's host sees ${path} at its own path, not ${at}`);
      const said = Array.isArray(b.members) ? b.members : [undefined];
      const members: LiveMember[] = [];
      for (const x of said) {
        const mm = memberOf(x, s.harness);
        if (typeof mm === "string") return fail(400, mm);
        members.push(mm);
      }
      const volume = { path, at, access, members, ...(perms ? { perms } : {}) };
      change = { op: "add", of: volume };
      const v: LiveHeldVolume = { volume, harness: harnessTook(s.harness, access === "rw"), scope: scopeTook(at, access), ...(microvm ? { microvm: microvmTook } : {}) };
      o.volumes = [...(o.volumes ?? []).filter((x) => x.volume.path !== path), v];
    }
  }
  o.revision = (o.revision ?? 0) + 1;
  held.set(id, o);
  for (const l of listeners) l(id);
  ctx.json(res, 200, { taken: true, agent: id, engine: s.engine, change, said: "its machine applies it in place: agents_get shows each override and how each place took it (state.overrides)" });
  return true;
}
