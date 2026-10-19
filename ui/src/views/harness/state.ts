/**
 * What the agent console remembers for you, in this browser:
 * - how a message goes while an agent works (queue, steer, now), until you change it;
 * - where you were in each conversation, so coming back (or reloading) puts you there;
 * - what you were writing to each agent, until it's sent;
 * - what you sent while it couldn't take it (its machine away), until it can.
 */
import { create } from "zustand";
import { createJSONStorage, persist, type PersistStorage, type StorageValue } from "zustand/middleware";
import type { Attachment, SendMode } from "@/api/harness";

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
export type Place = { end: true } | { id: string; offset: number };

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

/* ------------------------------------------------------------------ messages kept here */

/**
 * A message written while its agent couldn't take one (its machine away, the connection down): it
 * waits in this browser and goes, in order, once the agent can take it.
 */
export interface Held {
  id: string;
  text: string;
  at: number;
  attachments?: Attachment[];
  overrides?: { model?: string; effort?: string };
  /** How it was to go: "now" and "steer" keep that when it goes (older ones: queued). */
  mode?: SendMode;
}

interface HeldState {
  held: Record<string, Held[]>;
  hold: (agent: string, m: Held) => void;
  edit: (agent: string, id: string, text: string) => void;
  drop: (agent: string, ...ids: string[]) => void;
}

/** Inline files (no upload to keep them by) stay in memory only: the browser's storage is small. */
const keepable = (m: Held): Held => ({ ...m, attachments: m.attachments?.filter((a) => !a.data_base64) });

/** The messages kept for each agent until it can take them: in this browser, across reloads. */
export const useHeld = create<HeldState>()(
  persist(
    (set) => ({
      held: {},
      hold: (agent, m) => set((s) => ({ held: { ...s.held, [agent]: [...(s.held[agent] ?? []), m] } })),
      edit: (agent, id, text) => set((s) => ({ held: { ...s.held, [agent]: (s.held[agent] ?? []).map((m) => (m.id === id ? { ...m, text } : m)) } })),
      drop: (agent, ...ids) =>
        set((s) => {
          const left = (s.held[agent] ?? []).filter((m) => !ids.includes(m.id));
          const held = { ...s.held };
          if (left.length) held[agent] = left;
          else delete held[agent];
          return { held };
        }),
    }),
    {
      name: "kiln.agents.held",
      storage: createJSONStorage(() => localStorage),
      partialize: (s) => ({ held: Object.fromEntries(Object.entries(s.held).map(([k, v]) => [k, v.map(keepable)])) }),
    },
  ),
);
