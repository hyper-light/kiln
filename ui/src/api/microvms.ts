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
