/**
 * The conversation stores in words: a store's state, its sizes, counts and ages; the stores the
 * replication figure shows, and a machine's name cut to fit; whether batches moved on a store's
 * cable lately; how long its reads waited.
 */
import type { ConvoStatus, ConvoStore } from "@/api/types";

/** A cable moved data, or a store wrote, this recently (ms): its light runs. */
export const ACTIVE_MS = 6_000;

export function countText(n: number): string {
  if (n >= 1e9) return `${(n / 1e9).toFixed(n >= 1e10 ? 0 : 1)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1)}M`;
  if (n >= 1e4) return `${(n / 1e3).toFixed(0)}k`;
  return n.toLocaleString("en-US");
}

export function sizeText(bytes: number): string {
  if (bytes >= 1 << 30) return `${(bytes / (1 << 30)).toFixed(bytes >= 10 * (1 << 30) ? 1 : 2)} GB`;
  if (bytes >= 1 << 20) return `${(bytes / (1 << 20)).toFixed(0)} MB`;
  if (bytes >= 1 << 10) return `${(bytes / (1 << 10)).toFixed(0)} KB`;
  return `${bytes} B`;
}

export function agoText(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.round(s / 60)}m`;
  if (s < 86_400) return `${Math.round(s / 3600)}h`;
  return `${Math.round(s / 86_400)}d`;
}

/** What a store's state says, in a few words. */
export function stateText(s: ConvoStore, now: number): string {
  const behind = s.feed?.behind ?? s.mirror?.behind ?? null;
  switch (s.state) {
    case "live":
      return "live";
    case "in_sync":
      return "in sync";
    case "catching_up":
      return `${countText(behind ?? 0)} behind · catching up`;
    case "behind":
      return `${countText(behind ?? 0)} behind`;
    case "feeding":
      return "feeding · older kiln";
    case "no_store":
      return "keeps no store";
    case "offline":
      return s.heard ? `offline · ${agoText(now - s.heard)}` : "offline";
  }
}

/** Whether batches moved on a store's cable lately (by the server's clock). */
export function moving(s: ConvoStore, now: number): boolean {
  const at = s.feed?.intake?.last_batch ?? s.mirror?.ship.last_sent ?? 0;
  return at > 0 && now - at < ACTIVE_MS && s.connected;
}

/** What the scene draws: the server's store, each runner's (and the machines that keep none), the mirrors. */
export function storesOf(status: ConvoStatus) {
  const server = status.stores.find((s) => s.role === "server") ?? null;
  const rank = (s: ConvoStore) => (s.id ? (s.connected ? 0 : 1) : 2);
  const runners = status.stores.filter((s) => s.role === "runner").sort((a, b) => rank(a) - rank(b) || a.machine.localeCompare(b.machine) || a.key.localeCompare(b.key));
  const mirrors = status.stores.filter((s) => s.role === "mirror");
  return { server, runners, mirrors };
}

/**
 * A machine's name as its header can hold it: its host without its domain (an address stays whole),
 * cut to fit, keeping a short last part (the -01 of build-farm-01) that tells machines of a kind apart.
 */
export function plateName(machine: string, room: number): string {
  const host = /^[^.]*[a-z][^.]*\./i.test(machine) ? machine.slice(0, machine.indexOf(".")) : machine;
  if (host.length <= room) return host;
  const last = /-[^-]{1,4}$/.exec(host)?.[0] ?? "";
  const head = room - 1 - last.length;
  return head >= 4 ? `${host.slice(0, head)}…${last}` : `${host.slice(0, room - 1)}…`;
}

/** About the `q`th quantile of the waits counted (ms), from their power-of-two buckets. */
export function waitQuantile(waits: number[] | undefined, q: number): number | null {
  const total = (waits ?? []).reduce((a, b) => a + b, 0);
  if (!waits || total === 0) return null;
  const want = Math.max(1, Math.ceil(total * q));
  let seen = 0;
  for (let i = 0; i < waits.length; i++) {
    seen += waits[i]!;
    if (seen >= want) return 2 ** i;
  }
  return 2 ** (waits.length - 1);
}
