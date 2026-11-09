/**
 * Whether this browser is signed in: checked once when the app starts, and again whenever a
 * request finds the session gone or a sign-in succeeds (api/client.ts). The live event stream runs
 * only while signed in.
 */
import { create } from "zustand";
import { ApiError, isAuthed, onAuthChange, setAuthed } from "@/api/client";
import { events } from "@/api/events";
import type { Me } from "@/api/types";
import { bootstrapForPage, type PrivateBootstrap } from "./private-bootstrap";

export type AuthState = { kind: "loading" } | { kind: "anon"; error?: string } | { kind: "authed"; me: Me | null };

export const useAuth = create<AuthState>(() => ({ kind: "loading" }));

function set(next: AuthState) {
  useAuth.setState(next, true);
  if (next.kind === "authed") events.start();
  else events.stop();
}

let checking: Promise<void> | null = null;
let epoch = 0;
let started = false;
let authRequest: AbortController | null = null;

/**
 * Warm the console's two typefaces independently. The document also preloads their Latin files;
 * a slow font must not delay authenticating or starting the page's live session/data requests.
 */
export const fontsReady: Promise<unknown> =
  typeof document === "undefined" || !document.fonts
    ? Promise.resolve()
    : Promise.race([
        Promise.all([document.fonts.load('450 15px "Geist Variable"'), document.fonts.load('400 10px "Geist Mono Variable"')]).catch(() => undefined),
        new Promise((r) => setTimeout(r, 500)),
      ]);

function accept(me: Me, at: number) {
  if (at !== epoch) return;
  set(me.authenticated ? { kind: "authed", me } : { kind: "anon" });
  setAuthed(me.authenticated);
}

/** Called once in SignedIn, after the root loader is available and before its auth hook reads. */
export function startAuth(bootstrap: PrivateBootstrap | null, pathname: string, seed?: () => void) {
  if (typeof window === "undefined" || started) return;
  started = true;
  if (isAuthed() === null && bootstrapForPage(bootstrap, pathname)) {
    if (bootstrap.me.authenticated) seed?.();
    accept(bootstrap.me, epoch);
  } else void checkAuth();
}

/** Ask the server who this is (at most one question at a time). */
export function checkAuth(): Promise<void> {
  if (checking) return checking;
  const at = epoch;
  const controller = new AbortController();
  authRequest = controller;
  // Both success and 401 belong to this check's epoch. The shared request helper announces a
  // 401 immediately, before this guard could reject an obsolete response from a previous login.
  const pending = fetch("/api/me", {
    credentials: "same-origin",
    cache: "no-store",
    signal: controller.signal,
  })
    .then(async (response) => {
      if (!response.ok) throw new ApiError(response.status, response.status === 401 ? "Not signed in" : `HTTP ${response.status}`);
      return (await response.json()) as Me;
    })
    .then(
      (me) => accept(me, at),
      (e: unknown) => {
        if (at !== epoch) return;
        const status = (e as { status?: number }).status;
        set(
          status === 401
            ? { kind: "anon" }
            : {
                kind: "anon",
                error: e instanceof Error ? e.message : String(e),
              },
        );
        if (status === 401) setAuthed(false);
      },
    )
    .finally(() => {
      if (checking === pending) checking = null;
      if (authRequest === controller) authRequest = null;
    });
  checking = pending;
  return pending;
}

onAuthChange((authed) => {
  if (!authed) {
    epoch++;
    authRequest?.abort();
    authRequest = null;
    checking = null;
    set({ kind: "anon" });
  } else if (useAuth.getState().kind !== "authed") void checkAuth();
});
