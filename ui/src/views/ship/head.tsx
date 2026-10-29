/**
 * A section's head in an opened commit, as f-ship draws it (plan 01a1856fe81375b1bd1d1d86f270a45b,
 * step 7): its label on the left above its card, its controls on the right, one height with or
 * without them (ship.css), so nothing under it moves as they come.
 */
import * as React from "react";

export function SectionHead({ label, children }: { label: string; children?: React.ReactNode }) {
  return (
    <header className="ship-sech">
      <h3>{label}</h3>
      {children ? <span className="ship-sech__ctl">{children}</span> : null}
    </header>
  );
}
