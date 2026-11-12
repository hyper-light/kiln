/**
 * What the agent console remembers for you, in this browser:
 * - how a message goes while an agent works (queue, steer, now), until you change it;
 * - where you were in each conversation, so coming back (or reloading) puts you there;
 * - what you were writing to each agent (and in each review comment), until it's sent;
 * - what you sent while it couldn't take it (its machine away), until it can.
 */
import * as React from "react";
import { create } from "zustand";
import { createJSONStorage, persist, type PersistStorage, type StorageValue } from "zustand/middleware";
import type { SendMode } from "@/api/harness";
import { useHeld, type Held } from "@/api/held";
import { mailScope } from "@/api/client";
export { useHeld, type Held } from "@/api/held";
import type { Taken } from "./controls";

/* ------------------------------------------------------------------ send mode */

interface SendModeState {
  mode: SendMode;
  setMode: (mode: SendMode) => void;
}

/** How a message goes while the agent works: kept across messages, agents and reloads until changed. */
export const useSendMode = create<SendModeState>()(
  persist((set) => ({ mode: "queue", setMode: (mode) => set({ mode }) }), {
    name: "kiln.agents.sendMode",
    storage: createJSONStorage(() => localStorage),
    partialize: (s) => ({ mode: s.mode }),
  }),
);

/* ------------------------------------------------------------------ where you were */

/** A place in a conversation: its end (following what comes), or an event shown `offset` px below the top of the view. */
export type Place = { end: true } | { id: string; offset: number; row?: true; seq?: number; neighbors?: readonly { id: string; row?: true }[] };

interface PlacesState {
  places: Record<string, Place & { at: number }>;
  setPlace: (key: string, place: Place) => void;
}

/** At most this many conversations' places are kept (the latest). */
const PLACES = 100;

/** Where you were in each conversation (and each subagent's thread), for this tab: survives reloads. */
export const usePlaces = create<PlacesState>()(
  persist(
    (set) => ({
      places: {},
      setPlace: (key, place) =>
        set((s) => {
          const places = { ...s.places, [key]: { ...place, at: Date.now() } };
          const keys = Object.keys(places);
          if (keys.length > PLACES) for (const k of keys.sort((a, b) => (places[a]?.at ?? 0) - (places[b]?.at ?? 0)).slice(0, keys.length - PLACES)) delete places[k];
          return { places };
        }),
    }),
    {
      name: "kiln.agents.places",
      storage: createJSONStorage(() => sessionStorage),
      partialize: (s) => ({ places: s.places }),
    },
  ),
);

/**
 * A browser storage encoded and written at most this often (and as the page goes away): what
 * changes with every key typed doesn't serialize all drafts on every key.
 */
function lazily<T>(store: () => Storage, ms = 600): PersistStorage<T> {
  const pending = new Map<string, StorageValue<T>>();
  let timer = 0;
  const flush = () => {
    window.clearTimeout(timer);
    timer = 0;
    for (const [k, v] of pending) {
      try {
        store().setItem(k, JSON.stringify(v));
      } catch {
        /* storage off (private mode): this visit only */
      }
    }
    pending.clear();
  };
  if (typeof window !== "undefined") window.addEventListener("pagehide", flush);
  return {
    getItem: (k) => pending.get(k) ?? (JSON.parse(store().getItem(k) ?? "null") as StorageValue<T> | null),
    setItem: (k, v) => {
      pending.set(k, v);
      if (!timer) timer = window.setTimeout(flush, ms);
    },
    removeItem: (k) => {
      pending.delete(k);
      store().removeItem(k);
    },
  };
}

/* ------------------------------------------------------------------ drafts */

interface DraftsState {
  drafts: Record<string, string>;
  setDraft: (agent: string, text: string) => void;
}

/** What you're writing to each agent, until it's sent (for this tab: survives leaving and reloads). */
export const useDrafts = create<DraftsState>()(
  persist(
    (set) => ({
      drafts: {},
      setDraft: (agent, text) =>
        set((s) => {
          const drafts = { ...s.drafts };
          if (text) drafts[agent] = text;
          else delete drafts[agent];
          return { drafts };
        }),
    }),
    {
      name: "kiln.agents.drafts",
      storage: lazily(() => sessionStorage),
      partialize: (s) => ({ drafts: s.drafts }),
    },
  ),
);

/**
 * One draft by its key (an agent's id, or where a review comment is written: ship/review/draft.ts):
 * what's written there and how to change it. Only what reads that key renders as it changes, so
 * typing renders its composer and nothing else. Drafts are this browser's, and the pages that show
 * them are drawn here: what a server render would read is what's kept here.
 */
export function useDraft(key: string): [string, (text: string | ((was: string) => string)) => void] {
  const read = () => useDrafts.getState().drafts[key] ?? "";
  const text = React.useSyncExternalStore(useDrafts.subscribe, read, read);
  const set = React.useCallback((t: string | ((was: string) => string)) => useDrafts.getState().setDraft(key, typeof t === "function" ? t(useDrafts.getState().drafts[key] ?? "") : t), [key]);
  return [text, set];
}

/* ------------------------------------------------------------------ messages kept here */

/**
 * A message written while its agent couldn't take one (its machine away, the connection down): it
 * waits in this browser and goes, in order, once the agent can take it.
 */
/** The kept messages whose files this page is uploading (one kept from before a reload is not). */
export const uploadingHere = new Set<string>();

/**
 * Keeps message `m` for `agent` with the files `taken` from its box (W21-R15): those still on their
 * way it waits for, and goes once they're up, in its turn, with each that made it.
 */
export async function holdWith(agent: string, m: Held, taken: Taken) {
  return holdManyWith([{ agent, message: m }], taken);
}

/** A send to several recipients transfers all browser originals in the same transaction. */
export async function holdManyWith(messages: { agent: string; message: Held }[], taken: Taken) {
  const waits = taken.uploading.length > 0;
  const owner = mailScope();
  if (
    !(await useHeld.getState().holdMany(
      messages.map(({ agent, message }) => ({
        agent,
        message: { ...message, attachments: taken.files.length ? taken.files : undefined, originals: taken.originals, uploading: waits ? taken.uploading : undefined },
      })),
    ))
  ) {
    taken.back();
    return false;
  }
  taken.commit?.();
  if (waits) for (const { message } of messages) uploadingHere.add(message.id);
  if (!waits) return true;
  void taken.up.then(async (files) => {
    for (const { agent, message } of messages) {
      uploadingHere.delete(message.id);
      if (!(await useHeld.getState().uploaded(agent, message.id, files, owner ?? undefined, taken.originals))) taken.back();
    }
  });
  return true;
}
