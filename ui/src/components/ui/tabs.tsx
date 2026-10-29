import * as React from "react";
import { cn } from "@/lib/utils";

const trigger =
  "inline-flex shrink-0 items-center justify-center gap-2 rounded-full border border-transparent text-[13px] font-medium whitespace-nowrap text-[#92949b] transition-colors duration-200 outline-none hover:text-[#e4e5e7] focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-white/80 disabled:pointer-events-none disabled:opacity-40 data-[state=active]:border-white/10 data-[state=active]:bg-white/[0.07] data-[state=active]:text-[#f2f2f3] aria-pressed:border-white/10 aria-pressed:bg-white/[0.07] aria-pressed:text-[#f2f2f3] motion-reduce:transition-none";

// Pill segmented control, verbatim styling from hyperlight-site.
export const tabsTriggerClass = `${trigger} min-h-9 px-3.5 py-1.5`;

/** Each size whole: the control's box and its pills (class lists are joined as they are, never merged). */
const SIZES = {
  default: { box: "min-h-11 gap-1 p-1", pill: tabsTriggerClass },
  // A filter bar's height (--field-h), beside its field; where it can't fit beside it (a phone), it wraps.
  bar: { box: "min-h-[var(--field-h)] gap-0.5 p-[3px]", pill: `${trigger} h-8 px-3` },
};

/** A pill segmented control not tied to tab panels (filters, ranges). */
export function Segmented<T extends string>({
  value,
  onChange,
  options,
  label,
  className,
  size = "default",
}: {
  value: T;
  onChange: (v: T) => void;
  options: { value: T; label: React.ReactNode; count?: number }[];
  label: string;
  className?: string;
  size?: keyof typeof SIZES;
}) {
  const s = SIZES[size];
  return (
    <div role="group" aria-label={label} className={cn("inline-flex max-w-full flex-wrap items-center rounded-[22px] border border-white/10 bg-white/[0.02]", s.box, className)}>
      {options.map((o) => (
        <button key={o.value} type="button" aria-pressed={o.value === value} onClick={() => onChange(o.value)} className={s.pill}>
          {o.label}
          {o.count !== undefined && <span className="ml-0.5 font-mono text-[9px] text-[#898d99] tabular-nums">{o.count}</span>}
        </button>
      ))}
    </div>
  );
}
