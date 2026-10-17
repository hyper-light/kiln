/** hyper-light.dev's round control (the figures' replay and pause buttons), for an agent's own actions. */
import * as React from "react";
import { cn } from "@/lib/utils";
import "./figure.css";

export const RoundButton = React.forwardRef<HTMLButtonElement, React.ButtonHTMLAttributes<HTMLButtonElement> & { label: string; tone?: "danger" | "accent" }>(
  function RoundButton({ label, tone, className, children, ...props }, ref) {
    return (
      <button ref={ref} type="button" className={cn("fig-round", className)} data-tone={tone} aria-label={label} title={label} {...props}>
        {children}
      </button>
    );
  },
);
