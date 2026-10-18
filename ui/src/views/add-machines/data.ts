/** Data for Runners → Add machines (and the notice on Runners): join requests, what discovery hears, and what each machine is to this server. */
import { api } from "@/api/client";
import type { Agent, DiscoveredInstance, DiscoveryInfo, DiscoveryVia, InstallOs, JoinInfo, JoinRequest } from "@/api/types";
import type { Res } from "@/lib/data";

/**
 * Machines asking to join (`kiln join`) and the kiln instances discovery knows. Read again on the
 * `join_requests` and `discovery` events (scan progress arrives as the latter, up to twice a
 * second; one read at a time, lib/resource.ts) and when a runner connects or leaves (discovery then
 * counts it as one of ours); slow polling covers a dropped event stream.
 */
export const joinRequestsRes: Res<JoinRequest[]> = { key: "join-requests", fetch: (s) => api.joinRequests(s), opts: { pollMs: 15_000, on: ["join_requests"] } };
export const discoveryRes: Res<DiscoveryInfo> = { key: "discovery", fetch: (s) => api.discovery(s), opts: { pollMs: 30_000, on: ["discovery", "agent"] } };
/** The install commands, with the join token they carry. */
export const joinRes: Res<JoinInfo> = { key: "join", fetch: (s) => api.join(s) };

/** A runner that connected while Add machines was open. */
export interface Arrival {
  agent: Agent;
  /** When the page saw it connect (unix seconds). */
  at: number;
}

/** `123456` → `123 456`. */
export function formatCode(code: string) {
  return code.length === 6 ? `${code.slice(0, 3)} ${code.slice(3)}` : code;
}

/** The system this browser runs on: the likeliest one for the next machine, and the one fixes are shown for first. */
export function detectOs(): InstallOs {
  const ua = typeof navigator === "undefined" ? "" : navigator.userAgent;
  if (/Windows/i.test(ua)) return "windows";
  if (/Macintosh|Mac OS X|iPhone|iPad/i.test(ua)) return "macos";
  return "linux";
}

export const OS_NAME: Record<InstallOs, string> = { macos: "macOS", windows: "Windows", linux: "Linux" };

/** The one-liner for `os`: `install` from discovery-v2 servers, else the older per-platform fields. */
export function installCommand(info: JoinInfo, os: InstallOs): string {
  const v2 = info.install?.[os];
  if (v2) return v2;
  return os === "windows" ? (info.windows ?? info.command) : os === "macos" ? (info.macos ?? info.command) : info.command;
}

/** What a discovered machine is to this server, in the order the page lists them. */
export type Stand = "asking" | "fresh" | "invited" | "elsewhere" | "server" | "here" | "self";

export const STAND_RANK: Record<Stand, number> = { asking: 0, fresh: 1, invited: 2, elsewhere: 3, server: 4, here: 5, self: 6 };

export const STAND_WORD: Record<Stand, string> = {
  asking: "asking to join",
  fresh: "not set up yet",
  invited: "not set up yet",
  elsewhere: "runner of another server",
  server: "another server",
  here: "a runner here",
  self: "this server",
};

export function standOf(i: DiscoveredInstance, selfId: string | undefined): Stand {
  if (i.id === selfId) return "self";
  if (i.joined) return "here";
  if (i.request) return "asking";
  // A server with our CA is this server under another id or address.
  if (i.role === "server") return i.same_ca ? "self" : "server";
  // An agent enrolled with our CA counts as joined, so `enrolled` here is another server's.
  if (i.enrolled) return "elsewhere";
  return i.invited ? "invited" : "fresh";
}

/** The machines not set up as runners here, the ones that need a decision first; then the rest. */
export function sortInstances(list: DiscoveredInstance[], selfId: string | undefined) {
  return list
    .map((i) => ({ i, stand: standOf(i, selfId) }))
    .sort((a, b) => STAND_RANK[a.stand] - STAND_RANK[b.stand] || displayName(a.i).localeCompare(displayName(b.i)));
}

export function displayName(i: DiscoveredInstance) {
  return i.name || i.instance;
}

/** IPv4 first: the address people type. */
export function addressOf(i: DiscoveredInstance): string | undefined {
  return i.addresses.find((a) => !a.includes(":")) ?? i.addresses[0];
}

/** Discovery's "agent" is a runner machine (not an AI agent session). */
export function roleWord(role: string) {
  return role === "agent" ? "runner" : role;
}

/** The tag for a discovery source; "native" is the server OS's own DNS-SD, named after it. */
export function viaWord(v: DiscoveryVia, nativeLabel?: string): string {
  switch (v) {
    case "native":
      return /bonjour/i.test(nativeLabel ?? "") ? "BONJOUR" : /windows/i.test(nativeLabel ?? "") ? "WINDOWS" : "NATIVE";
    case "seed":
      return "SEED";
    default:
      return v.toUpperCase();
  }
}

export const VIA_TITLE: Record<DiscoveryVia, string> = {
  mdns: "Heard over multicast DNS",
  native: "Heard through the system's own DNS-SD",
  "dns-sd": "Found in a DNS-SD domain",
  scan: "Found by a network scan",
  probe: "Found by asking its address",
  tailscale: "Found on the tailnet",
  seed: "A seed address in the config",
};

/** Why a scan can't take `cidr`, or null. The server checks too; this answers before asking it. */
export function cidrProblem(cidr: string): string | null {
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\/(\d{1,2})$/.exec(cidr);
  if (!m || m.slice(1, 5).some((o) => Number(o) > 255) || Number(m[5]) > 32) return `${cidr} isn't a subnet like 192.168.1.0/24`;
  if (Number(m[5]) < 22) return `${cidr} is too big: scan at most a /22 at a time`;
  return null;
}

/** "a", "a and b", "a, b and c". */
export function listWords(words: string[]) {
  if (words.length < 2) return words.join("");
  return `${words.slice(0, -1).join(", ")} and ${words[words.length - 1]}`;
}

/** Scroll a row (or a section, to its start) into view, focus it and mark it for a moment: the page's jump links. */
export function jumpTo(id: string, block: ScrollLogicalPosition = "center") {
  const el = document.getElementById(id);
  if (!el) return;
  const still = window.matchMedia("(prefers-reduced-motion: reduce)").matches || document.documentElement.dataset.motion === "paused";
  el.scrollIntoView({ block, behavior: still ? "auto" : "smooth" });
  el.focus({ preventScroll: true });
  el.dataset.flash = "";
  window.setTimeout(() => delete el.dataset.flash, 2400);
}
