/**
 * What System → Updates and Builds share, outside any one of their parts: the server upgrade this
 * tab started and waits for across the restart, the machine or build picked in the figure (its row
 * or tile lights below), the upload under way, and a clock that ticks once a minute.
 */
import * as React from "react";
import { create } from "zustand";
import { api } from "@/api/client";
import { events } from "@/api/events";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";

/* ---------------------------- the server's upgrade ---------------------------- */

/** A server upgrade in flight: the page reconnects to the new server and says what changed. */
const PENDING_KEY = "kiln.serverUpgrade";
/** How long a server upgrade may take before the page says it didn't change the build. */
export const PATIENCE_MS = 4 * 60_000;

type Pending = { from: string; at: number } | null;

function readPending(): Pending {
  try {
    const v = sessionStorage.getItem(PENDING_KEY);
    return v ? (JSON.parse(v) as { from: string; at: number }) : null;
  } catch {
    return null;
  }
}

/** The server upgrade this tab started and waits for (kept for the tab, across the restart). */
export const usePending = create<{ pending: Pending }>(() => ({ pending: typeof sessionStorage === "undefined" ? null : readPending() }));

export function setPending(v: Pending) {
  try {
    if (v) sessionStorage.setItem(PENDING_KEY, JSON.stringify(v));
    else sessionStorage.removeItem(PENDING_KEY);
  } catch {
    /* private mode */
  }
  usePending.setState({ pending: v });
}

/** Back from the restart (or out of patience): say what the server runs now. */
export async function settlePending() {
  const p = usePending.getState().pending;
  if (!p) return;
  try {
    const d = await api.updates();
    if (d.server.build !== p.from) {
      toast.success("Server upgraded", `build ${p.from} → ${d.server.build}`);
      setPending(null);
    } else if (Date.now() - p.at > PATIENCE_MS) {
      toast({ title: "The server is still on the same build", description: `build ${d.server.build}`, tone: "info" });
      setPending(null);
    }
  } catch {
    /* still restarting: the next reconnection asks again */
  }
}
events.onReopen(() => void settlePending());

/** Runner events come in bursts: Updates is read again once they settle. */
let soon: number | null = null;
export function readAgainSoon() {
  if (soon !== null) window.clearTimeout(soon);
  soon = window.setTimeout(() => {
    soon = null;
    invalidate("updates");
  }, 350);
}

/* ---------------------------- picked in the figure ---------------------------- */

/** The machine or build picked in the Rollout figure: `server`, `runner:<name>` or `build:<key>`. */
export const usePicked = create<{ picked: string | null }>(() => ({ picked: null }));

export function pick(key: string | null) {
  usePicked.setState((s) => ({ picked: key !== null && s.picked === key ? null : key }));
}

/* ---------------------------- uploads ---------------------------- */

/** The platform a binary is being uploaded for. */
export const useUploading = create<{ target: string | null }>(() => ({ target: null }));

async function upload(target: string, file: File) {
  useUploading.setState({ target });
  try {
    const b = await api.uploadBinary(target, file);
    toast.success("Binary uploaded", `build ${b.build} for ${target}`);
    invalidate("updates");
  } catch (e) {
    toast.error("Upload failed", errorMessage(e));
  } finally {
    useUploading.setState({ target: null });
  }
}

/** A hidden file input, and choosing a binary with it for a platform: what it uploads. */
export function useUploadPicker(): { input: React.ReactElement; choose: (target: string) => void } {
  const ref = React.useRef<HTMLInputElement>(null);
  const target = React.useRef("");
  const input = React.createElement("input", {
    ref,
    type: "file",
    className: "sr-only",
    tabIndex: -1,
    "aria-hidden": true,
    onChange: (e: React.ChangeEvent<HTMLInputElement>) => {
      const file = e.target.files?.[0];
      e.target.value = "";
      if (file) void upload(target.current, file);
    },
  });
  return {
    input,
    choose: (t: string) => {
      target.current = t;
      ref.current?.click();
    },
  };
}

/* ---------------------------- a minute's clock ---------------------------- */

const thisMinute = () => Math.floor(Date.now() / 60_000) * 60_000;
const minuteSubs = new Set<() => void>();
let minuteTimer: number | null = null;
let minuteSeen = thisMinute();

function subscribeMinute(fn: () => void) {
  minuteSubs.add(fn);
  if (minuteTimer === null) {
    minuteTimer = window.setInterval(() => {
      const m = thisMinute();
      if (m === minuteSeen) return;
      minuteSeen = m;
      minuteSubs.forEach((l) => l());
    }, 5_000);
  }
  return () => {
    minuteSubs.delete(fn);
    if (minuteSubs.size === 0 && minuteTimer !== null) {
      window.clearInterval(minuteTimer);
      minuteTimer = null;
    }
  };
}

/** Now, to the minute: "4m ago" stays true, and the page isn't drawn again every second for it. */
export function useMinute(): number {
  return React.useSyncExternalStore(subscribeMinute, thisMinute, thisMinute);
}
