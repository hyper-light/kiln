import type { Status } from "@/api/types";

/** "3s", "4m 12s", "2h 05m", "3d 4h" */
export function formatDuration(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined || !Number.isFinite(seconds)) return "—";
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, "0")}s`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${String(m % 60).padStart(2, "0")}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}

/** Duration between two unix-second stamps; open-ended uses `now` (ms). */
export function spanSeconds(start: number | null, end: number | null, nowMs: number): number | null {
  if (start === null) return null;
  const e = end ?? nowMs / 1000;
  return Math.max(0, e - start);
}

const rtf = typeof Intl !== "undefined" ? new Intl.RelativeTimeFormat("en", { numeric: "auto", style: "narrow" }) : null;

/** Compact relative time: "now", "12s ago", "4m ago", "3h ago", "2d ago". */
export function relativeTime(unixSeconds: number | null | undefined, nowMs = Date.now()): string {
  if (!unixSeconds) return "—";
  const diff = Math.round(nowMs / 1000 - unixSeconds);
  const abs = Math.abs(diff);
  const suffix = diff >= 0 ? " ago" : "";
  const prefix = diff < 0 ? "in " : "";
  if (abs < 5) return "now";
  if (abs < 60) return `${prefix}${abs}s${suffix}`;
  if (abs < 3600) return `${prefix}${Math.floor(abs / 60)}m${suffix}`;
  if (abs < 86400) return `${prefix}${Math.floor(abs / 3600)}h${suffix}`;
  if (abs < 86400 * 30) return `${prefix}${Math.floor(abs / 86400)}d${suffix}`;
  if (rtf) return absoluteDate(unixSeconds);
  return absoluteDate(unixSeconds);
}

export function absoluteTime(unixSeconds: number | null | undefined): string {
  if (!unixSeconds) return "—";
  return new Date(unixSeconds * 1000).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function absoluteDate(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
}

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
export function formatBytes(bytes: number | null | undefined, digits = 1): string {
  if (bytes === null || bytes === undefined || !Number.isFinite(bytes)) return "—";
  let v = Math.abs(bytes);
  let i = 0;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i++;
  }
  const sign = bytes < 0 ? "-" : "";
  return `${sign}${i === 0 ? v.toFixed(0) : v.toFixed(v >= 100 ? 0 : digits)} ${UNITS[i]}`;
}

export function formatRate(bytesPerSec: number | null | undefined): string {
  if (bytesPerSec === null || bytesPerSec === undefined) return "—";
  return `${formatBytes(bytesPerSec)}/s`;
}

export function formatPct(v: number | null | undefined, digits = 0): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return "—";
  return `${v.toFixed(digits)}%`;
}

export function formatNumber(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return "—";
  return new Intl.NumberFormat().format(v);
}

/** Zero-padded index, the site's `01 / Label` style. */
export function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

export function firstLine(s: string): string {
  const i = s.indexOf("\n");
  return i === -1 ? s : s.slice(0, i);
}

export function statusLabel(s: Status | string): string {
  switch (s) {
    case "timed_out":
      return "timed out";
    default:
      return s;
  }
}

export function isTerminal(s: Status): boolean {
  return s === "succeeded" || s === "failed" || s === "cancelled" || s === "skipped" || s === "timed_out";
}

export function isActive(s: Status): boolean {
  return s === "running" || s === "queued" || s === "pending";
}

export function formatUptime(s: number): string {
  return formatDuration(s);
}
