/**
 * What starts an agent from elsewhere in kiln (Duplicate, a message's "Start new agent on this",
 * Ask an agent, a shell's "Start agents in this shell", the command palette, an ended agent's
 * "Start a new one"): Mission control's composer, filled in. There is no dialog: each way in hands
 * the composer what it knows and opens Mission control; the composer takes it once and says where
 * it came from, with Clear for your own defaults.
 */
import { create } from "zustand";
import { useDrafts } from "./state";

/** Mission control's composer keeps what's written in it under this key of the drafts store. */
export const MISSION_DRAFT = "kiln:mission";

export interface Prefill {
  /** Where it came from, as the composer says it ("Like Update the docs"). */
  from: string;
  kind?: string;
  target?: string;
  cwd?: string;
  shell?: string;
  mode?: string;
  model?: string;
  effort?: string;
  /** When it was handed over: the composer takes each once. */
  at: number;
}

interface PrefillState {
  prefill: Prefill | null;
  hand: (p: Omit<Prefill, "at">) => void;
  clear: () => void;
}

export const usePrefill = create<PrefillState>()((set) => ({
  prefill: null,
  hand: (p) => set({ prefill: { ...p, at: Date.now() } }),
  clear: () => set({ prefill: null }),
}));

/** Open Mission control's composer with what's known filled in (`prompt`: its first message). */
export function startFrom(navigate: (to: string) => void, p: Omit<Prefill, "at"> & { prompt?: string }) {
  const { prompt, ...rest } = p;
  usePrefill.getState().hand(rest);
  if (prompt) useDrafts.getState().setDraft(MISSION_DRAFT, prompt);
  navigate("/agents?new=1");
}
