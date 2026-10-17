/**
 * Whether this browser is signed in: checked once when the app starts, and again whenever a
 * request finds the session gone or a sign-in succeeds (api/client.ts). The live event stream runs
 * only while signed in.
 */
import { create } from "zustand";
import { api, onAuthChange } from "@/api/client";
import { events } from "@/api/events";
import type { Me } from "@/api/types";

export type AuthState = { kind: "loading" } | { kind: "anon"; error?: string } | { kind: "authed"; me: Me | null };

export const useAuth = create<AuthState>(() => ({ kind: "loading" }));

function set(next: AuthState) {
  useAuth.setState(next, true);
  if (next.kind === "authed") events.start();
  else events.stop();
}

let checking: Promise<void> | null = null;

/** Ask the server who this is (at most one question at a time). */
export function checkAuth(): Promise<void> {
  checking ??= api
    .me()
    .then(
      (me) => set(me.authenticated ? { kind: "authed", me } : { kind: "anon" }),
      (e: unknown) => {
        const status = (e as { status?: number }).status;
        set(status === 401 ? { kind: "anon" } : { kind: "anon", error: e instanceof Error ? e.message : String(e) });
      },
    )
    .finally(() => {
      checking = null;
    });
  return checking;
}

onAuthChange((authed) => {
  if (!authed) set({ kind: "anon" });
  else if (useAuth.getState().kind !== "authed") void checkAuth();
});
