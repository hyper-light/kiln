import * as React from "react";
import { Slot } from "radix-ui";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

// Verbatim from hyperlight-site components/ui/button.tsx, plus `danger` and `ghost-danger`, a
// compact `xs` size for dense table rows (still a pill, still the same voice) and `icon-lg`.
export const buttonVariants = cva(
  "inline-flex shrink-0 items-center justify-center gap-2 whitespace-nowrap rounded-full border font-medium transition-[background-color,border-color,color,box-shadow] duration-200 outline-none focus-visible:ring-2 focus-visible:ring-white/80 focus-visible:ring-offset-4 focus-visible:ring-offset-[#08090a] disabled:pointer-events-none disabled:opacity-40 aria-disabled:pointer-events-none aria-disabled:opacity-40 motion-reduce:transition-none [&_svg]:pointer-events-none [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        primary: "border-[#e4e5e7] bg-[#e4e5e7] text-[#111214] hover:border-white hover:bg-white",
        secondary:
          "border-white/15 bg-white/[0.03] text-[#e4e5e7] hover:border-white/30 hover:bg-white/[0.07]",
        ghost: "border-transparent bg-transparent text-[#b4b5ba] hover:bg-white/[0.06] hover:text-white",
        "ghost-danger": "border-transparent bg-transparent text-[#d1aaad] hover:bg-white/[0.06] hover:text-white",
        danger:
          "border-[#d1aaad]/30 bg-[#d1aaad]/[0.04] text-[#e2c4c6] hover:border-[#d1aaad]/60 hover:bg-[#d1aaad]/[0.09]",
      },
      // Each size is whole (height, padding, text, icon): class lists are joined as they are, so
      // nothing may need overriding.
      size: {
        default: "min-h-11 px-5 py-2.5 text-sm [&_svg]:size-4",
        sm: "min-h-11 px-4 py-2 text-xs [&_svg]:size-4",
        xs: "min-h-8 px-3 py-1 text-xs [&_svg]:size-3.5",
        icon: "size-11 min-h-11 min-w-11 p-0 text-sm [&_svg]:size-4",
        "icon-lg": "size-11 min-h-11 min-w-11 p-0 text-sm [&_svg]:size-5",
      },
    },
    defaultVariants: { variant: "primary", size: "default" },
  },
);

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {
  asChild?: boolean;
}

export const Button = React.forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, asChild = false, type, ...props }, ref) => {
    const Component = asChild ? Slot.Root : "button";
    return (
      <Component
        ref={ref}
        data-slot="button"
        type={asChild ? type : (type ?? "button")}
        className={cn(buttonVariants({ variant, size }), className)}
        {...props}
      />
    );
  },
);
Button.displayName = "Button";
