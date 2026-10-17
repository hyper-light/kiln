/**
 * What the agent console remembers for you, in this browser:
 * - how a message goes while an agent works (queue, steer, now), until you change it;
 * - where you were in each conversation, so coming back (or reloading) puts you there;
 * - what you were writing to each agent, until it's sent.
 */
import { create } from "zustand";
import { createJSONStorage, persist } from "zustand/middleware";
import type { SendMode } from "@/api/harness";

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
      storage: createJSONStorage(() => sessionStorage),
      partialize: (s) => ({ drafts: s.drafts }),
    },
  ),
);
