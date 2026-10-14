import { createContext, useContext, useEffect, useState, useSyncExternalStore } from "react";
import { Pause, Play } from "lucide-react";
import { readLocal } from "@/lib/hooks";

const MotionContext = createContext({ paused: false, reduced: false, toggle: () => {} });

function subscribeToMotion(callback: () => void) {
  const media = window.matchMedia("(prefers-reduced-motion: reduce)");
  media.addEventListener("change", callback);
  return () => media.removeEventListener("change", callback);
}
const getReducedMotion = () => window.matchMedia("(prefers-reduced-motion: reduce)").matches;

export function MotionProvider({ children }: { children: React.ReactNode }) {
  const [paused, setPaused] = useState<boolean>(() => readLocal("kiln.motionPaused", false));
  const reduced = useSyncExternalStore(subscribeToMotion, getReducedMotion, () => false);

  useEffect(() => {
    document.documentElement.dataset.motion = paused || reduced ? "paused" : "playing";
  }, [paused, reduced]);

  return (
    <MotionContext.Provider
      value={{
        paused: paused || reduced,
        reduced,
        toggle: () =>
          setPaused((v) => {
            try {
              localStorage.setItem("kiln.motionPaused", JSON.stringify(!v));
            } catch {
              /* ignore */
            }
            return !v;
          }),
      }}
    >
      {children}
    </MotionContext.Provider>
  );
}

export function useMotionPreference() {
  return useContext(MotionContext);
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
