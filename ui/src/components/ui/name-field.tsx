/**
 * A name edited where it shows, not in a dialog: Enter, or leaving the field, keeps it; Escape
 * doesn't. Emptied, the thing goes back to the name it had before anyone gave it one. It takes
 * the font of where it sits; `className` sizes it.
 */
import * as React from "react";
import { cn } from "@/lib/utils";

export function NameField({
  value,
  placeholder,
  onSave,
  onDone,
  className,
  label = "Name",
}: {
  value: string;
  placeholder?: string;
  onSave: (name: string) => unknown;
  onDone: () => void;
  className?: string;
  label?: string;
}) {
  const [v, setV] = React.useState(value);
  const done = React.useRef(false);
  // Focused once whatever opened it (a menu closing) is done with focus.
  const focusSoon = React.useCallback((el: HTMLInputElement | null) => {
    if (!el) return;
    const frame = requestAnimationFrame(() => {
      el.focus();
      el.select();
    });
    return () => cancelAnimationFrame(frame);
  }, []);
  const finish = (keep: boolean) => {
    if (done.current) return;
    done.current = true;
    if (keep && v.trim() !== value.trim()) void onSave(v.trim());
    onDone();
  };
  return (
    <input
      ref={focusSoon}
      className={cn("name-field", className)}
      autoFocus
      onFocus={(e) => e.currentTarget.select()}
      value={v}
      placeholder={placeholder}
      maxLength={80}
      enterKeyHint="done"
      spellCheck={false}
      onChange={(e) => setV(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          finish(true);
        } else if (e.key === "Escape") {
          e.preventDefault();
          finish(false);
        }
      }}
      onBlur={() => finish(true)}
      aria-label={label}
    />
  );
}
