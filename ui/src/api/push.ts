/**
 * Notifications to this device and your others (docs/notifications.md): the server's side
 * (`/api/push`) and this browser's (its service worker, `/sw.js`, and its push subscription).
 */
import type { Res } from "@/lib/data";
import { ApiError, request } from "./client";

/** What a device can hear, named as the events are. */
export type PushKind = "agent.needs_you" | "agent.finished" | "agent.stopped" | "agent.paused" | "machine.battery_low";

export interface PushState {
  enabled: boolean;
  /** Devices can be given notifications here (`why` says why not). */
  available: boolean;
  why: string | null;
  /** The server's key, to subscribe with. */
  public_key: string | null;
  kinds: { id: PushKind; label: string; means: string }[];
  /** The agents you muted. */
  muted: string[];
  /** Whether the server reached the push services when it last looked (`ok: null`: not yet). */
  reached: { ok: boolean | null; error: string | null; checked_at: number } | null;
}

export interface PushDevice {
  id: string;
  endpoint: string;
  device: string;
  hears: PushKind[];
  created_at: number;
  last_ok_at: number | null;
  last_error: string | null;
  last_error_at: number | null;
}

export interface PushTest {
  sent: number;
  results: { id: string; device: string; ok: boolean; error?: string }[];
}

const enc = encodeURIComponent;

/** What a person did with a notice: put it away, acted on it, or (`seen:<agent>`) saw its agent. */
export type MarkHow = "away" | "done" | "seen";

/** One of your marks, kept by the server (`notice_marks`): a request's id, `<kind>:<agent>:<since>`, `seen:<agent>`. */
export interface NoticeMark {
  key: string;
  how: MarkHow;
  /** Unix ms (the server's). */
  at: number;
}

export const pushApi = {
  state: (signal?: AbortSignal) => request<PushState>("GET", "/api/push", undefined, { signal }),
  devices: (signal?: AbortSignal) => request<{ devices: PushDevice[] }>("GET", "/api/push/subscriptions", undefined, { signal }).then((r) => r.devices),
  subscribe: (body: { subscription: PushSubscriptionJSON; device: string; hears?: PushKind[] }) => request<PushDevice>("POST", "/api/push/subscriptions", body),
  remove: (id: string) => request<void>("DELETE", `/api/push/subscriptions/${enc(id)}`),
  test: (id?: string) => request<PushTest>("POST", "/api/push/test", id ? { id } : {}),
  mute: (agent: string, on: boolean) => request<void>(on ? "PUT" : "DELETE", `/api/push/mutes/${enc(agent)}`),
  /** Your marks, and who you are to them (`owner`, `user:<id>`). */
  notices: (signal?: AbortSignal) => request<{ you: string; marks: NoticeMark[] }>("GET", "/api/push/notices", undefined, { signal }),
  /** `tags`: the agents whose shown notifications go too, on every device with kiln open. */
  mark: (keys: string[], how: MarkHow, tags: string[] = []) => request<{ marked: number; at: number }>("POST", "/api/push/notices", { keys, how, tags }),
};

/** A server that can't say (an older one, or you may not run agents): push isn't for here. */
const NONE = (why: string): PushState => ({ enabled: false, available: false, why, public_key: null, kinds: [], muted: [], reached: null });

export const pushStateRes: Res<PushState> = {
  key: "push/state",
  fetch: (s) =>
    pushApi.state(s).catch((e: unknown) => {
      if (e instanceof ApiError && e.status === 401) throw e;
      return NONE(e instanceof ApiError && e.status === 404 ? "This server doesn't send notifications: it's older than this page." : e instanceof Error ? e.message : String(e));
    }),
  opts: { on: ["push"] },
};

export const pushDevicesRes: Res<PushDevice[]> = { key: "push/devices", fetch: (s) => pushApi.devices(s).catch(() => []), opts: { on: ["push"] } };

/* ---------------------------- this browser ---------------------------- */

/** Why this browser can't take notifications: not a secure page; an iPhone's Safari outside the Home Screen; no push at all. */
export type Block = "insecure" | "home-screen" | "unsupported";

export interface ThisDevice {
  block: Block | null;
  /** The browser's permission for this site (`null` while it can't take them). */
  permission: NotificationPermission | null;
  /** Its push subscription's endpoint, when it has one. */
  endpoint: string | null;
}

const ios = () => /iP(hone|ad|od)/.test(navigator.userAgent) || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
const standalone = () => window.matchMedia?.("(display-mode: standalone)").matches || (navigator as Navigator & { standalone?: boolean }).standalone === true;

async function registration() {
  return "serviceWorker" in navigator ? navigator.serviceWorker.getRegistration("/") : undefined;
}

export async function readThisDevice(): Promise<ThisDevice> {
  if (!window.isSecureContext) return { block: "insecure", permission: null, endpoint: null };
  if (ios() && !standalone()) return { block: "home-screen", permission: null, endpoint: null };
  if (!("serviceWorker" in navigator) || !("PushManager" in window) || !("Notification" in window)) return { block: "unsupported", permission: null, endpoint: null };
  const sub = await (await registration())?.pushManager.getSubscription();
  return { block: null, permission: Notification.permission, endpoint: sub?.endpoint ?? null };
}

/** Read again whenever the server's side changes (a device on or off, from here or elsewhere). */
export const thisDeviceRes: Res<ThisDevice> = { key: "push/this", fetch: () => readThisDevice(), opts: { on: ["push"] } };

/** What this device is called in your list: "Chrome on Linux", "iPhone". */
export function deviceName(): string {
  const ua = navigator.userAgent;
  if (/iPhone|iPod/.test(ua)) return "iPhone";
  if (/iPad/.test(ua) || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1)) return "iPad";
  const browser = /Edg\//.test(ua) ? "Edge" : /OPR\//.test(ua) ? "Opera" : /Firefox\//.test(ua) ? "Firefox" : /Chrome\//.test(ua) ? "Chrome" : /Safari\//.test(ua) ? "Safari" : "A browser";
  const os = /Android/.test(ua) ? "Android" : /CrOS/.test(ua) ? "ChromeOS" : /Mac OS X/.test(ua) ? "macOS" : /Windows/.test(ua) ? "Windows" : /Linux/.test(ua) ? "Linux" : "";
  return os ? `${browser} on ${os}` : browser;
}

function keyBytes(b64url: string): Uint8Array<ArrayBuffer> {
  const b64 = b64url
    .replace(/-/g, "+")
    .replace(/_/g, "/")
    .padEnd(Math.ceil(b64url.length / 4) * 4, "=");
  const raw = atob(b64);
  const out = new Uint8Array(new ArrayBuffer(raw.length));
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

const same = (a: ArrayBuffer | null | undefined, b: Uint8Array) => !!a && a.byteLength === b.byteLength && new Uint8Array(a).every((x, i) => x === b[i]);

/**
 * Turns notifications on for this device: asks the browser's permission (on this click, never
 * before), starts the service worker, subscribes with the server's key and tells the server.
 * `hears`: what it hears (everything, when not said).
 */
export async function turnOn(publicKey: string, hears?: PushKind[]): Promise<PushDevice> {
  // First, in the click itself: Safari asks only from a tap.
  const permission = await Notification.requestPermission();
  if (permission === "denied") throw new Error("Notifications are blocked for this site. Allow them in the browser's settings for this site, then turn them on here.");
  if (permission !== "granted") throw new Error("Notifications weren't allowed.");
  await navigator.serviceWorker.register("/sw.js", { scope: "/" });
  const reg = await navigator.serviceWorker.ready;
  const key = keyBytes(publicKey);
  let sub = await reg.pushManager.getSubscription();
  // One made with another key (the server's data was reset) can't be used.
  if (sub && !same(sub.options.applicationServerKey, key)) {
    await sub.unsubscribe();
    sub = null;
  }
  sub ??= await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key }).catch((e: unknown) => {
    throw new Error(`This browser couldn't subscribe to its push service (${e instanceof Error ? e.message : String(e)}).`);
  });
  return pushApi.subscribe({ subscription: sub.toJSON(), device: deviceName(), hears });
}

/** What this device hears, changed (its subscription as it is). */
export async function setHears(hears: PushKind[]): Promise<PushDevice> {
  const sub = await (await registration())?.pushManager.getSubscription();
  if (!sub) throw new Error("This device isn't subscribed any more: turn notifications on again.");
  return pushApi.subscribe({ subscription: sub.toJSON(), device: deviceName(), hears });
}

/** Turns notifications off for this device: the server forgets it, and the browser lets go of its subscription. */
export async function turnOff(id: string | undefined): Promise<void> {
  if (id) await pushApi.remove(id);
  const sub = await (await registration())?.pushManager.getSubscription();
  await sub?.unsubscribe();
}
