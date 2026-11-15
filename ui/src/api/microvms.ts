/** Runners' engines and their microVMs (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2.3, P3.2). */
import type { Res } from "@/lib/data";
import { request } from "./client";
import type { IsolationFacts, LiveAccess, LiveChange, LiveMember, LivePortKey, LiveProtocol } from "./wire";

const enc = encodeURIComponent;

export type Engine = "host" | "microvm";

/**
 * A runner's engine as the server says it (`GET|PUT /api/runners/:name/engine`, crates/kiln/src/harness/engine.rs):
 * the engine it holds (none for a runner that hasn't said what it can do), where that comes from (its owner set it
 * on the server, or what it can do decides) and why it's host where it can't be microvm; what its owner set and how
 * many times; whether it can hold microVMs now and why not; its isolation facts; its sessions on that engine, and
 * those of the other, which finish there (`draining`).
 */
export interface RunnerEngineView {
  runner: string;
  engine: Engine | null;
  from: "server" | "default" | null;
  why: string | null;
  asked: Engine | null;
  version: number;
  microvm: { usable: boolean; why: string | null } | null;
  isolation: IsolationFacts | null;
  virt: { microvm: boolean; kvm: boolean; hvf: boolean } | null;
  sessions: number;
  draining: number;
  /** Each session that drains: it runs on the engine the runner no longer holds, and finishes there (an agent's children first). */
  draining_sessions: DrainingSession[];
  /** The switch they drain for, while any drains: what was set before (null: what it can do), to what, and when. */
  switch: { from: Engine | null; to: Engine | null; at: number } | null;
}

/** A session still on the engine its runner switched away from (`runners.engine`'s `draining_sessions`). */
export interface DrainingSession {
  id: string;
  name: string;
  /** Its harness. */
  kind: string;
  /** How it stands, as agents.list's `stand` says. */
  state: string;
  engine: Engine;
  created_at: number;
}

export interface MicroVm {
  id: string;
  /** The harness session it holds. */
  session: string;
  agent: string;
  image: string;
  start: { kind: "warm" | "template" | "cold"; ms: number };
  up_s: number;
  state: "running" | "idle" | "paused" | "failed" | "draining";
  /** CPU %, the last hour, oldest first. */
  cpu: number[];
  /** Memory % of its own, and network MB/s × 10, the last hour: its telemetry's plots. */
  mem?: number[];
  net?: number[];
  memory: { used: number; total: number };
  disk: { used: number; total: number };
}

export interface WarmPool {
  agent: string;
  image: string;
  cpus: number;
  memory: number;
  warm: number;
  /** The most Shards' rule keeps warm for it here, inside the node's room. */
  bound: number;
  hits: number;
  starts: number;
  idle_s: number;
  /** Released this many seconds from now, its keep window passing with no starts. */
  leaving_s: number | null;
}

export interface RunnerImage {
  name: string;
  from: string;
  digest: string;
  size: number;
  built_s: number;
  state: "built" | "building" | "failed";
  /** Shards' words, when its build failed. */
  why?: string;
}

export interface RunnerMicrovms {
  /** The Hex node's room, from its keeper (a proposal: not one of the server's routes yet). */
  room: { memory_free: number; memory_total: number; vcpus_free: number; vcpus_total: number } | null;
  vms: MicroVm[];
  pools: WarmPool[];
  images: RunnerImage[];
  starts: { warm: number; template: number; cold: number };
  /** Network, MB/s, the last hour. */
  net: number[];
}

export const microvmsApi = {
  runner: (name: string, signal?: AbortSignal) => request<RunnerMicrovms>("GET", `/api/microvm/runners/${enc(name)}`, undefined, { signal }),
};

/** A runner's engine (the server's routes): read, and set (`null`: what it can do decides). */
export const engineApi = {
  get: (name: string, signal?: AbortSignal) => request<RunnerEngineView>("GET", `/api/runners/${enc(name)}/engine`, undefined, { signal }),
  set: (name: string, engine: Engine | null) => request<RunnerEngineView>("PUT", `/api/runners/${enc(name)}/engine`, { engine }),
  /** Ends its draining sessions now (`runners.engine.end_draining`; destructive): the view after, and those ended. */
  endDraining: (name: string) => request<RunnerEngineView & { ended: string[] }>("POST", `/api/runners/${enc(name)}/engine/end`),
  /** Puts back the setting it had before its switch while its sessions drain (`runners.engine.cancel`). */
  cancel: (name: string) => request<RunnerEngineView>("POST", `/api/runners/${enc(name)}/engine/cancel`),
};
/** One runner's engine, kept current while shown (its sessions finish on the engine they started on). */
export const runnerEngineRes = (name: string): Res<RunnerEngineView> => ({ key: `runner-engine/${name}`, fetch: (s) => engineApi.get(name, s), opts: { pollMs: 5000 } });

/** Where a runner's facts say its agents run now (`isolation.engine`), whether it can hold microVMs, and why not; none from a runner before engines. */
export function engineOf(facts: { isolation?: IsolationFacts } | null | undefined): { engine: Engine; can: boolean; why: string | null } | null {
  const i = facts?.isolation;
  if (!i) return null;
  return { engine: i.engine ?? (i.available ? "microvm" : "host"), can: i.available, why: i.why_not };
}
export const runnerVmsRes = (name: string): Res<RunnerMicrovms> => ({ key: `microvm/runner/${name}`, fetch: (s) => microvmsApi.runner(name, s), opts: { pollMs: 5000 } });

/* ------------------------------------------------ a session's sandbox (the agent's Sandbox or Host tab, its rows) */

export type StartKind = MicroVm["start"]["kind"];
/** A session's sandbox in short, as its rows show it: where it runs and how it started. */
export interface SandboxBrief {
  engine: Engine;
  runner: string;
  state: MicroVm["state"] | "ended";
  /** How its microVM started; none on the host. */
  start: { kind: StartKind; ms: number } | null;
  cpu_pct: number;
  memory: { used: number; total: number };
}
export type MemberKind = "harness" | "agent" | "mcp";
export interface SandboxMember {
  name: string;
  kind: MemberKind;
}
export interface Ingress {
  /** The runner's address and port it's published on. */
  host: string;
  guest: number;
  proto: "tcp" | "udp";
  /** The member that listens. */
  to: string;
}
export interface Egress {
  to: string;
  ports: string;
  /** The name resolves for it (Shards grants names, not addresses). */
  dns: boolean;
  /** The MCP server this grant serves, if one. */
  mcp: string | null;
  /** The member it's granted to. */
  who: string;
}
export interface SandboxNetwork {
  name: string;
  members: (SandboxMember & { connected: boolean })[];
}
export interface SandboxVolume {
  path: string;
  mode: "ro" | "rw";
  members: string[];
}
export type ComponentSource = { kind: "git-https" | "git-ssh" | "s3" | "https"; url: string; pin: string };
/** A harness, agent or MCP server at its path in the session; `built`: what it was built with, while another is swapped in. */
export interface SandboxComponent {
  kind: MemberKind;
  name: string;
  path: string;
  from: ComponentSource;
  version: string;
  built: (ComponentSource & { version: string }) | null;
}
export interface SandboxDetail extends SandboxBrief {
  /** On the host: why (its runner's owner, a project that asks for the host there, or it can't run microVMs). */
  why: string | null;
  up_s: number;
  /** On the host: the harness process it runs as. */
  process: { harness: string; pid: number; user: string } | null;
  /** In a microVM: its Hex box, and the Shards microVM it is. */
  hex: { box: string; node: string; state: string } | null;
  vm: { id: string; agent: string; image: string; version: number; digest: string; vcpus: number; memory: number; footprint: { peak: number; steady: number } } | null;
  /** Now; none once ended. */
  now: { cpu_pct: number; memory: { used: number; total: number }; disk: { used: number; total: number } } | null;
  /** The last hour: CPU %, memory % of its own, network MB/s × 10. */
  series: { cpu: number[]; memory: number[]; net: number[] };
  /** In a microVM only: what's published, granted and connected; everything else is denied. */
  network: { ingress: Ingress[]; egress: Egress[]; networks: SandboxNetwork[] } | null;
  members: SandboxMember[];
  volumes: SandboxVolume[];
  /** What each member may touch: its allowed directories. */
  dirs: { member: string; kind: MemberKind; paths: { path: string; mode: "ro" | "rw" }[] }[];
  scope: { project: string; branch: string; paths: string[] } | null;
  components: SandboxComponent[];
  /** In a microVM only: the secret names its image was built with (never values). */
  secrets: { name: string; scope: string }[] | null;
  /** In a microVM that started warm: the pool it came from. */
  pool: { agent: string; image: string; cpus: number; memory: number; warm: number; bound: number } | null;
}

const ses = (id: string, rest = "") => `/api/microvm/sessions/${enc(id)}${rest}`;
export const sandboxApi = {
  all: (signal?: AbortSignal) => request<Record<string, SandboxBrief>>("GET", "/api/microvm/sessions", undefined, { signal }),
  get: (id: string, signal?: AbortSignal) => request<SandboxDetail>("GET", ses(id), undefined, { signal }),
  act: (id: string, act: "pause" | "resume" | "restart" | "stop", body: { version?: number } = {}) => request<SandboxDetail>("POST", ses(id, `/${act}`), body),
  swap: (id: string, name: string, source: string, pin: string) => request<SandboxDetail>("POST", ses(id, `/components/${enc(name)}`), { source, pin }),
  restore: (id: string, name: string) => request<SandboxDetail>("DELETE", ses(id, `/components/${enc(name)}`)),
};

/** Every session's sandbox in short, for rows (polled: starts and stops move it). */
export const sandboxesRes: Res<Record<string, SandboxBrief>> = { key: "microvm/sessions", fetch: (s) => sandboxApi.all(s), opts: { pollMs: 5000 } };
/** One session's sandbox in full, live while shown. */
export const sandboxRes = (id: string): Res<SandboxDetail> => ({ key: `microvm/session/${id}`, fetch: (s) => sandboxApi.get(id, s), opts: { pollMs: 2000 } });

/* ------------------------------------------------ a session's live overrides (the server's routes) */

/**
 * What a person changes of a running session in place (`POST /api/harness/sessions/:id/…`;
 * crates/kiln/src/harness/overrides.rs, kiln plan 01a1dd2e…, P6b.4): ports published and egress granted
 * into a microVM, a member connected to a network (a microVM session's alone), a folder given (every
 * engine). Each answers once its machine has it; what the session holds, and how each place took it,
 * comes in its state (`state.overrides`).
 */
export interface OverrideAnswer {
  taken: boolean;
  agent: string;
  engine: Engine;
  change: LiveChange;
  said: string;
}
const live = (id: string, rest: string) => `/api/harness/sessions/${enc(id)}/${rest}`;
export const overridesApi = {
  publish: (id: string, p: { member?: string; bind: string; host_port: number; guest_port: number; protocol: LiveProtocol }) => request<OverrideAnswer>("POST", live(id, "ports"), p),
  unpublish: (id: string, k: LivePortKey) => request<OverrideAnswer>("POST", live(id, "ports/unpublish"), k),
  grant: (id: string, e: { member?: string; host: string; port: number }) => request<OverrideAnswer>("POST", live(id, "egress"), e),
  revoke: (id: string, e: { member?: string; host: string; port: number }) => request<OverrideAnswer>("POST", live(id, "egress/revoke"), e),
  connect: (id: string, n: { member?: string; network: string }) => request<OverrideAnswer>("POST", live(id, "networks"), n),
  disconnect: (id: string, n: { member?: string; network: string }) => request<OverrideAnswer>("POST", live(id, "networks/disconnect"), n),
  addVolume: (id: string, v: { path: string; access: LiveAccess; at?: string; members?: string[] }) => request<OverrideAnswer>("POST", live(id, "volumes"), v),
  removeVolume: (id: string, path: string) => request<OverrideAnswer>("POST", live(id, "volumes/remove"), { path }),
};

/** A member as the routes take it: `harness`, `agent:<name>`, `mcp:<name>`. */
export const memberWord = (m: LiveMember | SandboxMember): string => (m.kind === "harness" ? "harness" : `${m.kind}:${m.name}`);
