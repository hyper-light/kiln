import * as React from "react";
import { Link } from "@/lib/router";
import { AlertTriangle, ChevronDown, RotateCcw, Search, X } from "lucide-react";
import { cn } from "@/lib/utils";
import { Button } from "./button";
import { pad2 } from "@/lib/format";
import { useLocalStorage } from "@/lib/hooks";
import { setPref } from "@/lib/prefs";

/* ---------------------------- page scaffolding ---------------------------- */

export function PageIntro({
  index,
  label,
  title,
  lead,
  actions,
  compact,
}: {
  index: number;
  label: string;
  title: React.ReactNode;
  lead?: React.ReactNode;
  actions?: React.ReactNode;
  /** On a phone, one line: the title, then the lead, then the actions. */
  compact?: boolean;
}) {
  return (
    <header className="page-intro" data-compact={compact || undefined}>
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
  fold,
}: {
  index?: string;
  label: string;
  title?: React.ReactNode;
  aside?: React.ReactNode;
  className?: string;
  /** The heading folds its section: the whole of it is the toggle. */
  fold?: { open: boolean; onToggle: () => void; controls: string };
}) {
  return (
    <div className={cn("section-heading", fold && "section-fold", className)} data-open={fold?.open || undefined}>
      <div className="min-w-0">
        <div className="section-number eyebrow">
          {index ? `${index} / ` : ""}
          {label}
        </div>
        {title &&
          (fold ? (
            <h2>
              <button type="button" className="section-toggle" aria-expanded={fold.open} aria-controls={fold.controls} onClick={fold.onToggle}>
                {title}
                <ChevronDown size={16} className="section-chev" aria-hidden="true" />
              </button>
            </h2>
          ) : (
            <h2>{title}</h2>
          ))}
      </div>
      {aside && <div className="section-aside flex shrink-0 items-center gap-2">{aside}</div>}
    </div>
  );
}

/**
 * Open the folded section `#hash` names (`/system#system.updates`) and bring it into view as the
 * page draws: again as what is above it loads (its place moves), until you scroll yourself. For a
 * page's root: called as it mounts, and when its hash changes.
 */
export function openFold(hash: string) {
  let id: string;
  try {
    id = decodeURIComponent(hash.replace(/^#/, ""));
  } catch {
    return;
  }
  if (!/^[\w-]+\.[\w.-]+$/.test(id) || typeof window === "undefined") return;
  setPref(`kiln.fold.${id}`, true);
  let seeking = true;
  const seek = () => {
    if (seeking) document.getElementById(id)?.scrollIntoView({ block: "start" });
  };
  const moved = new ResizeObserver(() => requestAnimationFrame(seek));
  const stop = () => {
    seeking = false;
    moved.disconnect();
    for (const ev of ["wheel", "touchstart", "keydown"] as const) window.removeEventListener(ev, stop);
  };
  for (const ev of ["wheel", "touchstart", "keydown"] as const) window.addEventListener(ev, stop, { passive: true });
  window.setTimeout(stop, 6000);
  moved.observe(document.body);
  requestAnimationFrame(seek);
}

/** A page section its heading folds away: open at first, then as you left it (kept in this browser). A link to `#<id>` opens it ([`openFold`]). */
export function FoldSection({
  id,
  index,
  label,
  title,
  aside,
  className,
  children,
}: {
  /** Where its state is kept: `kiln.fold.<id>`. */
  id: string;
  index?: string;
  label: string;
  title: React.ReactNode;
  aside?: React.ReactNode;
  className?: string;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useLocalStorage(`kiln.fold.${id}`, true);
  const body = React.useId();
  return (
    <section id={id} className={cn("mt-12", className)}>
      <SectionHeading index={index} label={label} title={title} aside={open ? aside : undefined} fold={{ open, onToggle: () => setOpen(!open), controls: body }} />
      {/* Folded, it isn't there at all: nothing in it polls or moves. */}
      <div id={body}>{open && children}</div>
    </section>
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
  <div className={cn("field search-field", className)}>
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
      <button type="button" onClick={() => onChange("")} aria-label={`Clear ${label.toLowerCase()}`} className="field-clear">
        <X size={14} />
      </button>
    )}
  </div>
));
SearchField.displayName = "SearchField";

/**
 * A filter or search field and what stands beside it, all one height (--field-h): the field takes
 * the room left, a select beside it --companion-w (200 px; 150 px in a bar under 520 px). `aside` (a
 * count, a download) ends the bar, or goes under it once the bar is narrow. On a phone an open filter
 * field spans the bar.
 */
export function FilterBar({ children, aside, className }: { children: React.ReactNode; aside?: React.ReactNode; className?: string }) {
  return (
    <div className={cn("filter-bar", className)}>
      <div className="filter-bar-row">{children}</div>
      {aside && <div className="filter-bar-aside">{aside}</div>}
    </div>
  );
}

export function TextField({
  label,
  hint,
  className,
  ...props
}: React.InputHTMLAttributes<HTMLInputElement> & { label: string; hint?: React.ReactNode }) {
  const auto = React.useId();
  const id = props.id ?? auto;
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
  options: { value: string; label: string; disabled?: boolean }[];
  className?: string;
  hideLabel?: boolean;
}) {
  const id = React.useId();
  return (
    <div className={cn("select-field", className)}>
      <label htmlFor={id} className={hideLabel ? "sr-only" : "field-label"}>
        {label}
      </label>
      <div className="select">
        <select id={id} value={value} onChange={(e) => onChange(e.target.value)}>
          {options.map((o) => (
            <option key={o.value} value={o.value} disabled={o.disabled}>
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
