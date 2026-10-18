import * as React from "react";
import { Link } from "@/lib/router";
import { AlertTriangle, ChevronDown, RotateCcw, Search, X } from "lucide-react";
import { cn } from "@/lib/utils";
import { Button } from "./button";
import { pad2 } from "@/lib/format";

/* ---------------------------- page scaffolding ---------------------------- */

export function PageIntro({
  index,
  label,
  title,
  lead,
  actions,
}: {
  index: number;
  label: string;
  title: React.ReactNode;
  lead?: React.ReactNode;
  actions?: React.ReactNode;
}) {
  return (
    <header className="page-intro">
      <div className="min-w-0">
        <div className="eyebrow">
          <span className="spectrum-rule" />
          {pad2(index)} / {label}
        </div>
        <h1>{title}</h1>
        {lead && <p className="lead">{lead}</p>}
      </div>
      {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
    </header>
  );
}

/**
 * Where a page sits, above its title, the way every page's eyebrow reads: `NN / Section / …`, each
 * part before the last a link back up.
 */
export function Crumbs({ index, items }: { index: number; items: { label: React.ReactNode; to?: string }[] }) {
  return (
    <nav className="eyebrow crumbs" aria-label="Breadcrumb">
      <span className="spectrum-rule" />
      {items.map((it, i) => (
        <React.Fragment key={i}>
          {i > 0 && <span aria-hidden="true">/</span>}
          {it.to ? (
            <Link to={it.to}>
              {i === 0 ? `${pad2(index)} / ` : ""}
              {it.label}
            </Link>
          ) : (
            <span>
              {i === 0 ? `${pad2(index)} / ` : ""}
              {it.label}
            </span>
          )}
        </React.Fragment>
      ))}
    </nav>
  );
}

export function SectionHeading({
  index,
  label,
  title,
  aside,
  className,
}: {
  index?: string;
  label: string;
  title?: React.ReactNode;
  aside?: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("section-heading", className)}>
      <div className="min-w-0">
        <div className="section-number eyebrow">
          {index ? `${index} / ` : ""}
          {label}
        </div>
        {title && <h2>{title}</h2>}
      </div>
      {aside && <div className="flex shrink-0 items-center gap-2">{aside}</div>}
    </div>
  );
}

export function Panel({
  eyebrow,
  title,
  description,
  actions,
  children,
  caption,
  className,
  bodyClassName,
}: {
  eyebrow?: React.ReactNode;
  title?: React.ReactNode;
  description?: React.ReactNode;
  actions?: React.ReactNode;
  children?: React.ReactNode;
  caption?: React.ReactNode;
  className?: string;
  bodyClassName?: string;
}) {
  return (
    <section className={cn("panel", className)}>
      {(eyebrow || title || actions) && (
        <div className="panel-header">
          <div className="min-w-0">
            {eyebrow && <span className="panel-eyebrow">{eyebrow}</span>}
            {title && <h3 className="panel-title">{title}</h3>}
            {description && <p className="panel-desc">{description}</p>}
          </div>
          {actions && <div className="flex shrink-0 items-center gap-1.5">{actions}</div>}
        </div>
      )}
      <div className={cn("panel-body", bodyClassName)}>{children}</div>
      {caption && <div className="panel-caption">{caption}</div>}
    </section>
  );
}

export function Facts({
  items,
  className,
}: {
  items: { label: React.ReactNode; value: React.ReactNode; tone?: "pass" | "fail" | "error" | "pending" }[];
  className?: string;
}) {
  return (
    <dl className={cn("facts", className)}>
      {items.map((it, i) => (
        <div key={i} data-tone={it.tone}>
          <dt>{it.label}</dt>
          <dd>{it.value}</dd>
        </div>
      ))}
    </dl>
  );
}

/* ---------------------------- inputs ---------------------------- */

export const SearchField = React.forwardRef<
  HTMLInputElement,
  { value: string; onChange: (v: string) => void; placeholder?: string; label: string; className?: string }
>(({ value, onChange, placeholder, label, className }, ref) => (
  <div className={cn("field", className)}>
    <Search size={15} aria-hidden="true" />
    <input
      ref={ref}
      type="search"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      placeholder={placeholder}
      aria-label={label}
    />
    {value && (
      <button
        type="button"
        onClick={() => onChange("")}
        aria-label={`Clear ${label.toLowerCase()}`}
        className="grid size-10 shrink-0 place-items-center text-[#9499a5] hover:text-white"
      >
        <X size={14} />
      </button>
    )}
  </div>
));
SearchField.displayName = "SearchField";

export function TextField({
  label,
  hint,
  className,
  ...props
}: React.InputHTMLAttributes<HTMLInputElement> & { label: string; hint?: React.ReactNode }) {
  const id = React.useId();
  return (
    <div className={className}>
      <label htmlFor={id} className="field-label">
        {label}
      </label>
      <div className="field">
        <input id={id} {...props} />
      </div>
      {hint && <div className="field-hint">{hint}</div>}
    </div>
  );
}

export function SelectField({
  label,
  value,
  onChange,
  options,
  className,
  hideLabel,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string }[];
  className?: string;
  hideLabel?: boolean;
}) {
  const id = React.useId();
  return (
    <div className={className}>
      <label htmlFor={id} className={hideLabel ? "sr-only" : "field-label"}>
        {label}
      </label>
      <div className="select">
        <select id={id} value={value} onChange={(e) => onChange(e.target.value)}>
          {options.map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </select>
        <ChevronDown size={14} aria-hidden="true" />
      </div>
    </div>
  );
}

/* ---------------------------- states ---------------------------- */

export function EmptyState({
  icon,
  title,
  children,
  action,
  className,
}: {
  icon?: React.ReactNode;
  title: string;
  children?: React.ReactNode;
  action?: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("empty-state", className)}>
      {icon}
      <h2>{title}</h2>
      {children && <p>{children}</p>}
      {action}
    </div>
  );
}

export function ErrorState({ error, onRetry, compact }: { error: Error | string; onRetry?: () => void; compact?: boolean }) {
  const msg = typeof error === "string" ? error : error.message;
  return (
    <div
      role="alert"
      className={cn(
        "flex flex-wrap items-center gap-3 rounded-md border border-[#cbb69a]/25 bg-[#cbb69a]/[0.03] text-[12.5px] text-[#d8c8b0]",
        compact ? "px-3 py-2" : "px-4 py-3",
      )}
    >
      <AlertTriangle size={15} className="text-[#cbb69a]" aria-hidden="true" />
      <span className="min-w-0 flex-1 break-words">{msg}</span>
      {onRetry && (
        <Button variant="ghost" size="xs" onClick={onRetry}>
          <RotateCcw /> Retry
        </Button>
      )}
    </div>
  );
}

/**
 * A question asked in place, under what it acts on: what will happen, then the act and a way out.
 * Nothing opens over the page.
 */
export function Confirm({ label, tone, children, actions, className }: { label: string; tone?: "danger"; children: React.ReactNode; actions: React.ReactNode; className?: string }) {
  return (
    <div className={cn("confirm", className)} role="group" aria-label={label} data-tone={tone}>
      {children}
      <div className="confirm-actions">{actions}</div>
    </div>
  );
}

export function Skeleton({ className, style }: { className?: string; style?: React.CSSProperties }) {
  return <div className={cn("skeleton", className)} style={style} aria-hidden="true" />;
}

export function SkeletonRows({ rows = 8, cols = 6 }: { rows?: number; cols?: number }) {
  return (
    <div className="row-list" aria-busy="true" aria-label="Loading">
      {Array.from({ length: rows }, (_, r) => (
        <div key={r} className="flex items-center gap-4 px-3 py-3">
          <Skeleton className="size-2 rounded-full" />
          {Array.from({ length: cols }, (_, c) => (
            <Skeleton key={c} className="h-3" style={{ width: `${[18, 8, 14, 22, 10, 9][c % 6]}%`, opacity: 1 - r * 0.07 }} />
          ))}
        </div>
      ))}
    </div>
  );
}

/* ---------------------------- data bits ---------------------------- */

export function Meter({ value, max = 100, tone, label }: { value: number; max?: number; tone?: "warn" | "fail"; label?: string }) {
  const pct = max > 0 ? Math.min(100, Math.max(0, (value / max) * 100)) : 0;
  const auto = tone ?? (pct > 92 ? "fail" : pct > 80 ? "warn" : undefined);
  return (
    <div className="meter" data-tone={auto} role="meter" aria-valuenow={Math.round(pct)} aria-valuemin={0} aria-valuemax={100} aria-label={label}>
      <span style={{ width: `${pct}%` }} />
    </div>
  );
}

/** Tiny inline SVG sparkline (no chart lib: used dozens of times per page). */
export function Sparkline({
  values,
  max,
  width = 120,
  height = 28,
  className,
  stroke = "#b8a6d5",
}: {
  values: number[];
  max?: number;
  width?: number;
  height?: number;
  className?: string;
  stroke?: string;
}) {
  const id = React.useId();
  if (values.length < 2) {
    return <svg width={width} height={height} className={className} aria-hidden="true" />;
  }
  const hi = max ?? Math.max(1, ...values);
  const step = width / (values.length - 1);
  const pts = values.map((v, i) => [i * step, height - 1 - (Math.min(v, hi) / hi) * (height - 3)] as const);
  const line = pts.map(([x, y], i) => `${i ? "L" : "M"}${x.toFixed(1)},${y.toFixed(1)}`).join("");
  const area = `${line}L${width},${height}L0,${height}Z`;
  return (
    <svg width={width} height={height} viewBox={`0 0 ${width} ${height}`} className={cn("spark overflow-visible", className)} aria-hidden="true" preserveAspectRatio="none">
      <defs>
        <linearGradient id={`${id}-f`} x1="0" x2="0" y1="0" y2="1">
          <stop offset="0" stopColor={stroke} stopOpacity="0.18" />
          <stop offset="1" stopColor={stroke} stopOpacity="0" />
        </linearGradient>
      </defs>
      <path d={area} fill={`url(#${id}-f)`} />
      <path d={line} fill="none" stroke={stroke} strokeWidth="1" strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

export function Kbd({ children }: { children: React.ReactNode }) {
  return <kbd className="kbd">{children}</kbd>;
}

/** Concentric optical rings (hyperlight OG motif). `size` is pixels or any CSS length. */
export function Optic({ size = 220, className }: { size?: number | string; className?: string }) {
  const colors = ["#939da7", "#aeb1bb", "#b0a4c4", "#c1a8b5", "#a6b5ca", "#bbc7bf"];
  const scale = (f: number) => (typeof size === "number" ? size * f : `calc(${size} * ${f.toFixed(3)})`);
  return (
    <div className={cn("optic", className)} style={{ width: size, height: size }} aria-hidden="true">
      {Array.from({ length: 12 }, (_, i) => {
        const w = scale(0.42 + i * 0.05);
        const h = scale(0.24 + i * 0.035);
        return (
          <span
            key={i}
            style={
              {
                width: w,
                height: h,
                borderColor: colors[i % colors.length],
                opacity: 0.12 + i * 0.025,
                transform: `rotate(${i * 6}deg)`,
                "--i": i,
                "--r": `${i * 6}deg`,
              } as React.CSSProperties
            }
          />
        );
      })}
    </div>
  );
}

export function CopyButton({ text, label = "Copy", className }: { text: string; label?: string; className?: string }) {
  const [copied, setCopied] = React.useState(false);
  return (
    <Button
      variant="secondary"
      size="sm"
      className={className}
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
        } catch {
          const ta = document.createElement("textarea");
          ta.value = text;
          document.body.appendChild(ta);
          ta.select();
          document.execCommand("copy");
          ta.remove();
        }
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1600);
      }}
    >
      {copied ? "Copied" : label}
    </Button>
  );
}
