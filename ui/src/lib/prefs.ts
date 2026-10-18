/**
 * This browser's preferences (a page's layout, the terminal's settings, the motion toggle, a
 * Grafana link…): one zustand store over localStorage. Every component that reads a preference
 * sees the same value, a change is saved at once, and other tabs follow it (the `storage` event).
 * Each preference keeps its own `kiln.*` key, as it always had.
 */
import { useCallback } from "react";
import { create } from "zustand";

type Values = Record<string, unknown>;

const PREFIX = "kiln.";

function readAll(): Values {
  const out: Values = {};
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (!key?.startsWith(PREFIX)) continue;
      const raw = localStorage.getItem(key);
      if (raw === null) continue;
      try {
        out[key] = JSON.parse(raw);
      } catch {
        /* not one of ours */
      }
    }
  } catch {
    /* storage off (private mode) */
  }
  return out;
}

export const usePrefs = create<{ values: Values }>(() => ({ values: typeof window === "undefined" ? {} : readAll() }));

/** Change a preference: shown everywhere at once, and kept. */
export function setPref(key: string, value: unknown) {
  usePrefs.setState((s) => ({ values: { ...s.values, [key]: value } }));
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    /* storage off: it lasts this visit */
  }
}

/** A preference outside React (event handlers, module code). */
export function getPref<T>(key: string, fallback: T): T {
  const v = usePrefs.getState().values[key];
  return v === undefined ? fallback : (v as T);
}

/** A preference and its setter; `initial` until one is set. */
export function usePref<T>(key: string, initial: T): [T, (v: T) => void] {
  const stored = usePrefs((s) => s.values[key]) as T | undefined;
  const set = useCallback((v: T) => setPref(key, v), [key]);
  return [stored === undefined ? initial : stored, set];
}

// Another tab changed one: follow it.
if (typeof window !== "undefined") {
  window.addEventListener("storage", (e) => {
    const key = e.key;
    if (!key?.startsWith(PREFIX)) return;
    let value: unknown;
    if (e.newValue !== null) {
      try {
        value = JSON.parse(e.newValue);
      } catch {
        return;
      }
    }
    usePrefs.setState((s) => {
      const values = { ...s.values };
      if (value === undefined) delete values[key];
      else values[key] = value;
      return { values };
    });
  });
}
