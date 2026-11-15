/** Runners' engines and their microVMs (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2.3, P3.2). */
import type { Res } from "@/lib/data";
import { request } from "./client";

const enc = encodeURIComponent;

export type Engine = "host" | "microvm";

export interface EngineView {
  engine: Engine;
  /** Why it runs this engine: the default for a runner that can, its owner's choice, or it can't. */
  origin: "default" | "owner" | "cannot";
  can: boolean;
  /** `KVM`, `Hypervisor.framework`, `WHP`, where known. */
  hypervisor: string | null;
  /** Shards' own words for why it can't (host_check), none when it can. */
  why: string | null;
  /** When Shards' host check last ran, unix seconds. */
  checked_at: number;
  /** The Hex node's classes: `shards-image` when it can boot images. */
  hex_class: string[];
  /** The Hex node's room, from its keeper. */
  room: { memory_free: number; memory_total: number; vcpus_free: number; vcpus_total: number } | null;
  /** A switch under way: the other engine's microVMs drain first. */
  switching: { to: Engine; draining: string[]; since: number } | null;
  /** The workspace's default engine (Settings → Runners), and the projects that ask for another here. */
  default: Engine;
  overrides: { project: string; engine: Engine }[];
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
  vms: MicroVm[];
  pools: WarmPool[];
  images: RunnerImage[];
  starts: { warm: number; template: number; cold: number };
  /** Network, MB/s, the last hour. */
  net: number[];
}

export const microvmsApi = {
  engines: (signal?: AbortSignal) => request<Record<string, EngineView>>("GET", "/api/microvm/engines", undefined, { signal }),
  runner: (name: string, signal?: AbortSignal) => request<RunnerMicrovms>("GET", `/api/microvm/runners/${enc(name)}`, undefined, { signal }),
  setEngine: (name: string, body: { engine?: Engine; end?: boolean; cancel?: boolean }) => request<EngineView>("PUT", `/api/microvm/runners/${enc(name)}/engine`, body),
};

/** Every runner's engine, kept current while shown (a drain moves on its own). */
export const enginesRes: Res<Record<string, EngineView>> = { key: "microvm/engines", fetch: (s) => microvmsApi.engines(s), opts: { pollMs: 5000 } };
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
  publish: (id: string, i: Ingress, remove = false) => request<SandboxDetail>(remove ? "DELETE" : "POST", ses(id, "/ingress"), i),
  grant: (id: string, e: Egress, remove = false) => request<SandboxDetail>(remove ? "DELETE" : "POST", ses(id, "/egress"), e),
  connect: (id: string, network: string, member: string, connected: boolean) => request<SandboxDetail>("PUT", ses(id, `/networks/${enc(network)}`), { member, connected }),
  volume: (id: string, v: SandboxVolume, remove = false) => request<SandboxDetail>(remove ? "DELETE" : "POST", ses(id, "/volumes"), v),
  swap: (id: string, name: string, source: string, pin: string) => request<SandboxDetail>("POST", ses(id, `/components/${enc(name)}`), { source, pin }),
  restore: (id: string, name: string) => request<SandboxDetail>("DELETE", ses(id, `/components/${enc(name)}`)),
};

/** Every session's sandbox in short, for rows (polled: starts and stops move it). */
export const sandboxesRes: Res<Record<string, SandboxBrief>> = { key: "microvm/sessions", fetch: (s) => sandboxApi.all(s), opts: { pollMs: 5000 } };
/** One session's sandbox in full, live while shown. */
export const sandboxRes = (id: string): Res<SandboxDetail> => ({ key: `microvm/session/${id}`, fetch: (s) => sandboxApi.get(id, s), opts: { pollMs: 2000 } });
