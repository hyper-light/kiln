/**
 * The frame hyper-light.dev's figures share, without the animation: a mono eyebrow, a title and a
 * line of description, round controls, a rail of tabs with a prism underline, a grid of facts and a
 * caption under a hairline.
 */
import * as React from "react";
import { cn } from "@/lib/utils";
import "./figure.css";

export type Tone = "neutral" | "pass" | "fail" | "pending" | "error";

export function Figure({
  eyebrow,
  title,
  description,
  actions,
  children,
  rail,
  panel,
  caption,
  className,
  titleAs: H = "h2",
  ...rest
}: {
  eyebrow?: React.ReactNode;
  title?: React.ReactNode;
  description?: React.ReactNode;
  actions?: React.ReactNode;
  children?: React.ReactNode;
  rail?: React.ReactNode;
  panel?: React.ReactNode;
  caption?: React.ReactNode;
  className?: string;
  titleAs?: "h1" | "h2" | "h3";
} & Omit<React.HTMLAttributes<HTMLElement>, "title">) {
  return (
    <figure className={cn("fig", className)} {...rest}>
      {(eyebrow || title || description || actions) && (
        <header className="fig-head">
          <div className="fig-head-text">
            {eyebrow && <p className="fig-eyebrow">{eyebrow}</p>}
            {title && <H className="fig-title">{title}</H>}
            {description && <div className="fig-desc">{description}</div>}
          </div>
          {actions && <div className="fig-actions">{actions}</div>}
        </header>
      )}
      {children}
      {rail}
      {panel && <div className="fig-panel">{panel}</div>}
      {caption && <figcaption className="fig-caption">{caption}</figcaption>}
    </figure>
  );
}

/** The site's round control (replay, pause), for a figure's own actions. */
export const RoundButton = React.forwardRef<HTMLButtonElement, React.ButtonHTMLAttributes<HTMLButtonElement> & { label: string; tone?: "danger" | "accent" }>(
  function RoundButton({ label, tone, className, children, ...props }, ref) {
    return (
      <button ref={ref} type="button" className={cn("fig-round", className)} data-tone={tone} aria-label={label} title={label} {...props}>
        {children}
      </button>
    );
  },
);

export interface RailItem<T extends string> {
  id: T;
  label: React.ReactNode;
  /** Shown instead of the label in a narrow figure. */
  short?: React.ReactNode;
  count?: number;
  tone?: Tone;
  disabled?: boolean;
}

/** The step rail: equal tabs across the figure, the chosen one lit with a moving prism underline. */
export function StepRail<T extends string>({ items, value, onChange, label, className }: { items: RailItem<T>[]; value: T; onChange: (v: T) => void; label: string; className?: string }) {
  const ref = React.useRef<HTMLDivElement>(null);
  // Keep the chosen tab in view when the rail scrolls (narrow figures), without moving the page.
  React.useEffect(() => {
    const rail = ref.current;
    if (!rail || getComputedStyle(rail).overflowX !== "auto") return;
    const active = rail.querySelector<HTMLElement>('[aria-selected="true"]');
    if (!active) return;
    const b = rail.getBoundingClientRect();
    const a = active.getBoundingClientRect();
    const delta = a.left < b.left + 12 ? a.left - b.left - 12 : a.right > b.right - 12 ? a.right - b.right + 12 : 0;
    if (delta) rail.scrollBy({ left: delta });
  }, [value]);
  const keyDown = (e: React.KeyboardEvent<HTMLButtonElement>, i: number) => {
    const n = items.length;
    const next = e.key === "ArrowRight" ? (i + 1) % n : e.key === "ArrowLeft" ? (i + n - 1) % n : e.key === "Home" ? 0 : e.key === "End" ? n - 1 : -1;
    if (next < 0) return;
    e.preventDefault();
    const item = items[next];
    if (!item) return;
    onChange(item.id);
    ref.current?.querySelectorAll<HTMLButtonElement>("button")[next]?.focus();
  };
  return (
    <div ref={ref} className={cn("fig-rail", className)} role="tablist" aria-label={label}>
      {items.map((it, i) => (
        <button
          key={it.id}
          type="button"
          role="tab"
          aria-selected={value === it.id}
          tabIndex={value === it.id ? 0 : -1}
          disabled={it.disabled}
          onClick={() => onChange(it.id)}
          onKeyDown={(e) => keyDown(e, i)}
        >
          <span className={it.short ? "fig-long" : undefined}>{it.label}</span>
          {it.short && (
            <span className="fig-short" aria-hidden="true">
              {it.short}
            </span>
          )}
          {it.count !== undefined && (
            <span className="fig-count" data-tone={it.tone}>
              {it.count}
            </span>
          )}
        </button>
      ))}
    </div>
  );
}

export interface Fact {
  label: React.ReactNode;
  value: React.ReactNode;
  tone?: Tone;
  title?: string;
}

/** Label over a mono value, each under its own hairline. */
export function Facts({ items, className }: { items: Fact[]; className?: string }) {
  return (
    <dl className={cn("fig-facts", className)}>
      {items.map((f, i) => (
        <div key={i} data-tone={f.tone} title={f.title}>
          <dt>{f.label}</dt>
          <dd>{f.value}</dd>
        </div>
      ))}
    </dl>
  );
}
