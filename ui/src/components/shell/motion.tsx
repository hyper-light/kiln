/**
 * The global motion toggle: paused here (kept per browser) or by the device's reduced-motion
 * setting, which always wins. Applied to the page as `:root[data-motion]`, where every animation
 * in the stylesheet looks.
 */
import { useSyncExternalStore } from "react";
import { Pause, Play } from "lucide-react";
import { getPref, setPref, usePref, usePrefs } from "@/lib/prefs";

const KEY = "kiln.motionPaused";
const media = typeof window === "undefined" ? null : window.matchMedia("(prefers-reduced-motion: reduce)");

function apply() {
  document.documentElement.dataset.motion = getPref(KEY, false) || media?.matches ? "paused" : "playing";
}
if (media) {
  apply();
  usePrefs.subscribe(apply);
  media.addEventListener("change", apply);
}

function subscribeToReduced(callback: () => void) {
  media?.addEventListener("change", callback);
  return () => media?.removeEventListener("change", callback);
}
const getReduced = () => media?.matches ?? false;

export function useMotionPreference() {
  const [pausedHere] = usePref(KEY, false);
  const reduced = useSyncExternalStore(subscribeToReduced, getReduced, () => false);
  return { paused: pausedHere || reduced, reduced, toggle: () => setPref(KEY, !pausedHere) };
}

export function MotionToggle() {
  const { paused, reduced, toggle } = useMotionPreference();
  return (
    <button
      className="motion-toggle"
      onClick={toggle}
      aria-pressed={paused}
      disabled={reduced}
      title={reduced ? "Following your device’s reduced-motion setting" : undefined}
    >
      {paused ? <Play size={11} aria-hidden="true" /> : <Pause size={11} aria-hidden="true" />}
      {reduced ? "Reduced motion" : paused ? "Resume motion" : "Pause motion"}
    </button>
  );
}
