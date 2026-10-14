import * as React from "react";
import { Tabs as TabsPrimitive } from "radix-ui";
import { cn } from "@/lib/utils";

// Pill segmented control, verbatim styling from hyperlight-site.
export const Tabs = React.forwardRef<
  React.ComponentRef<typeof TabsPrimitive.Root>,
  React.ComponentPropsWithoutRef<typeof TabsPrimitive.Root>
>(({ className, ...props }, ref) => (
  <TabsPrimitive.Root ref={ref} data-slot="tabs" className={cn("w-full", className)} {...props} />
));
Tabs.displayName = "Tabs";

export const TabsList = React.forwardRef<
  React.ComponentRef<typeof TabsPrimitive.List>,
  React.ComponentPropsWithoutRef<typeof TabsPrimitive.List>
>(({ className, ...props }, ref) => (
  <TabsPrimitive.List
    ref={ref}
    data-slot="tabs-list"
    className={cn(
      "inline-flex min-h-11 max-w-full items-center gap-1 overflow-x-auto rounded-full border border-white/10 bg-white/[0.02] p-1",
      className,
    )}
    {...props}
  />
));
TabsList.displayName = "TabsList";

export const tabsTriggerClass =
  "inline-flex min-h-9 shrink-0 items-center justify-center gap-2 rounded-full border border-transparent px-3.5 py-1.5 text-[13px] font-medium whitespace-nowrap text-[#92949b] transition-colors duration-200 outline-none hover:text-[#e4e5e7] focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-white/80 disabled:pointer-events-none disabled:opacity-40 data-[state=active]:border-white/10 data-[state=active]:bg-white/[0.07] data-[state=active]:text-[#f2f2f3] aria-pressed:border-white/10 aria-pressed:bg-white/[0.07] aria-pressed:text-[#f2f2f3] motion-reduce:transition-none";

export const TabsTrigger = React.forwardRef<
  React.ComponentRef<typeof TabsPrimitive.Trigger>,
  React.ComponentPropsWithoutRef<typeof TabsPrimitive.Trigger>
>(({ className, ...props }, ref) => (
  <TabsPrimitive.Trigger ref={ref} data-slot="tabs-trigger" className={cn(tabsTriggerClass, className)} {...props} />
));
TabsTrigger.displayName = "TabsTrigger";

export const TabsContent = React.forwardRef<
  React.ComponentRef<typeof TabsPrimitive.Content>,
  React.ComponentPropsWithoutRef<typeof TabsPrimitive.Content>
>(({ className, ...props }, ref) => (
  <TabsPrimitive.Content
    ref={ref}
    data-slot="tabs-content"
    className={cn(
      "mt-5 rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-white/60 focus-visible:ring-offset-4 focus-visible:ring-offset-[#08090a]",
      className,
    )}
    {...props}
  />
));
TabsContent.displayName = "TabsContent";

/** A pill segmented control not tied to tab panels (filters, ranges). */
export function Segmented<T extends string>({
  value,
  onChange,
  options,
  label,
  className,
}: {
  value: T;
  onChange: (v: T) => void;
  options: { value: T; label: React.ReactNode; count?: number }[];
  label: string;
  className?: string;
}) {
  return (
    <div
      role="group"
      aria-label={label}
      className={cn(
        "inline-flex min-h-11 max-w-full items-center gap-1 overflow-x-auto rounded-full border border-white/10 bg-white/[0.02] p-1",
        className,
      )}
    >
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          aria-pressed={o.value === value}
          onClick={() => onChange(o.value)}
          className={tabsTriggerClass}
        >
          {o.label}
          {o.count !== undefined && <span className="ml-0.5 font-mono text-[9px] text-[#898d99]">{o.count}</span>}
        </button>
      ))}
    </div>
  );
}
