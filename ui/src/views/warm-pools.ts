/**
 * What a runner's details say of the warm pools kiln names on its microVMs (kiln plan 01a1dd2e…, P3.2;
 * docs/isolation.md, "Warm pools"): each pool's agent, as you may know it, its size and its hits.
 * Pure: words from the server's view, no React.
 */
import type { WarmPool } from "@/api/types";
import { formatBytes } from "@/lib/format";

const MINUTE_MS = 60_000;

/** Whose a pool is: its agent's name (its variant after it, where its sessions ran as one), and nothing of another person's agent. */
export function poolAgent(p: Pick<WarmPool, "agent_from" | "agent_name" | "agent" | "variant">): string {
  const who = p.agent_from === "another" ? "Another person's agent" : (p.agent_name ?? p.agent ?? "An agent");
  return p.variant ? `${who} · ${p.variant.arm} of ${p.variant.experiment}` : who;
}

/** Its microVM's size: its vCPUs and memory as its sessions asked for them, else the least its runtime boots. */
export function poolSize(p: Pick<WarmPool, "vcpus" | "memory_mib">): string {
  const parts = [p.vcpus ? `${p.vcpus} vCPU${p.vcpus === 1 ? "" : "s"}` : null, p.memory_mib ? formatBytes(p.memory_mib * 1024 * 1024) : null].filter((s): s is string => s !== null);
  return parts.length ? parts.join(" · ") : "least size";
}

/** Its hits (starts whose microVM started on a warm host of it) and its starts. */
export function poolHits(p: Pick<WarmPool, "hits" | "starts">): string {
  return `${p.hits} ${p.hits === 1 ? "hit" : "hits"} · ${p.starts} ${p.starts === 1 ? "start" : "starts"}`;
}

/** When it's let go, from `now` (Unix ms), unless another start of it comes first. */
export function poolUntil(p: Pick<WarmPool, "until_ms">, now: number): string {
  const min = Math.ceil((p.until_ms - now) / MINUTE_MS);
  return min <= 1 ? "let go within a minute" : `let go in ${min} min`;
}

/** The pools still kept at `now` (Unix ms): one whose keep window has passed is gone, as the server lets it go. */
export function keptAt<P extends Pick<WarmPool, "until_ms">>(pools: readonly P[], now: number): P[] {
  return pools.filter((p) => now <= p.until_ms);
}

/** What's said where none is kept: how a pool comes, and how long it's kept (its node's keep-alive, `keep_ms`). */
export function noPoolsLine(keepMs: number): string {
  const min = Math.round(keepMs / MINUTE_MS);
  return `None now. An agent started in a microVM here names its pool, kept ${min} ${min === 1 ? "minute" : "minutes"} past its latest start.`;
}
