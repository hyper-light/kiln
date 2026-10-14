import type { Status } from "@/api/types";
import { statusLabel } from "@/lib/format";
import { cn } from "@/lib/utils";

export function StatusDot({ status, className, title }: { status: Status | string; className?: string; title?: string }) {
  return (
    <i
      className={cn("dot", className)}
      data-status={status}
      role="img"
      aria-label={title ?? statusLabel(status)}
      title={title ?? statusLabel(status)}
    />
  );
}

export function StatusBadge({ status, className }: { status: Status | string; className?: string }) {
  return (
    <span className={cn("status", className)}>
      <i className="dot" data-status={status} aria-hidden="true" />
      <span className="status-text" data-status={status}>
        {statusLabel(status)}
      </span>
    </span>
  );
}

export function LabelChip({ label }: { label: string }) {
  const tone = ["k8s", "baremetal", "bench", "gpu"].includes(label) ? label : undefined;
  return (
    <span className="chip" data-tone={tone}>
      {label}
    </span>
  );
}

export function Chips({ labels, max = 6 }: { labels: string[]; max?: number }) {
  const shown = labels.slice(0, max);
  return (
    <span className="inline-flex flex-wrap gap-1">
      {shown.map((l) => (
        <LabelChip key={l} label={l} />
      ))}
      {labels.length > max && <span className="chip">+{labels.length - max}</span>}
    </span>
  );
}
