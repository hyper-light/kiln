/**
 * An Agentfile's install sources in words and back (P4.6): a pin in a few characters, where a
 * source's line is now, and a `HARNESS` or `AGENT` line written again from the source picker in
 * Shards' own syntax (crates/agentfile `source_of` and its fetch flags: `--checksum` pins an http(s)
 * or s3:// source, `--version-id` an s3:// one, `#REF` a Git one; a path takes none).
 */
import type { AgentfileSource } from "@/api/agentfiles";

/** A pin in a few characters: a digest's first 8, an object version's, a ref and the commit it named. */
export function pinWords(pin: AgentfileSource["pin"]): string {
  if (!pin) return "unpinned";
  if (pin.by === "sha256") return `sha256 ${pin.value.replace(/^sha256:/, "").slice(0, 8)}`;
  if (pin.by === "version-id") return `ver ${pin.value.slice(0, 8)}`;
  if (pin.by === "ref") return pin.resolved ? `${pin.value} → ${pin.resolved.slice(0, 8)}` : pin.value;
  return "kiln's pin";
}

const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const LINE = /^(\s*)(HARNESS|AGENT)\s+(\S+)((?:\s+--\S+)*)\s+FROM\s+(\S+)(.*)$/;

/** Where its line is in `text` now (from 1): the line that names it, else where it was when kept. */
export function lineOf(text: string, s: Pick<AgentfileSource, "directive" | "name" | "line">): number {
  const re = new RegExp(`^\\s*${s.directive}\\s+${escape(s.name)}(\\s|$)`);
  const at = text.split("\n").findIndex((l) => re.test(l));
  return at >= 0 ? at + 1 : s.line;
}

/** What the picker edits: the kind, the source, its pin (a ref, with `:dir` if it names one; a SHA-256; an S3 object's version), and an S3 region. */
export interface SourceForm {
  kind: "git" | "s3" | "https" | "path";
  source: string;
  pin: string;
  region: string;
}

const flagOf = (flags: string, name: string) => new RegExp(`--${name}=(\\S+)`).exec(flags)?.[1] ?? "";

/** The picker opened on a line as written: its source, and its pin as the line pins it. */
export function formOf(s: AgentfileSource, line: string): SourceForm {
  const m = LINE.exec(line);
  const flags = m?.[4] ?? "";
  const written = m?.[5] ?? s.source;
  const hash = written.indexOf("#");
  const source = hash >= 0 ? written.slice(0, hash) : written;
  const kind = s.kind === "git-https" || s.kind === "git-ssh" ? "git" : s.kind === "s3" ? "s3" : s.kind === "path" ? "path" : "https";
  const pin = kind === "git" ? (hash >= 0 ? written.slice(hash + 1) : "") : flagOf(flags, "version-id") || flagOf(flags, "checksum").replace(/^sha256:/, "");
  return { kind, source, pin, region: flagOf(flags, "region") };
}

const HEX = /^(sha256:)?[0-9a-f]{64}$/;

/** What's wrong with it before it's written, in a few words; null when it can be. */
export function problemOf(f: SourceForm): string | null {
  const src = f.source.trim();
  const pin = f.pin.trim();
  if (f.kind === "git") {
    if (!/^(https:\/\/\S+\.git|git@[^\s:]+:\S+\.git|ssh:\/\/\S+\.git)$/.test(src)) return "a .git URL";
    return pin && !/\s/.test(pin) ? null : "a ref";
  }
  if (f.kind === "s3") {
    if (!/^s3:\/\/[^/\s]+\/\S+$/.test(src)) return "s3://bucket/key";
    if (f.region.trim() && !/^[a-z0-9-]+$/.test(f.region.trim())) return "a region";
    return pin && !/\s/.test(pin) ? null : "a version or SHA-256";
  }
  if (f.kind === "https") {
    if (!/^https:\/\/\S+$/.test(src) || /\.git$/.test(src)) return "an https URL";
    return HEX.test(pin) ? null : "64 hex digits";
  }
  return /^(\.{1,2}\/|\/)\S*$/.test(src) ? null : "./ or /";
}

/** Flags the picker writes: any other a line has (`--redirect`, `--endpoint`, …) stays as written. */
const OWN = /^--(checksum|version-id|region)=/;

/** The line written again from the picker, in Shards' syntax; null when it isn't a source line or the form has a problem. */
export function writeSource(line: string, f: SourceForm): string | null {
  const m = LINE.exec(line);
  if (!m || problemOf(f)) return null;
  const [, indent = "", directive, name, flags = "", , rest = ""] = m;
  const kept = flags.split(/\s+/).filter((x) => x && !OWN.test(x));
  const pin = f.pin.trim();
  const sha = pin.replace(/^sha256:/, "");
  const own =
    f.kind === "https"
      ? [`--checksum=sha256:${sha}`]
      : f.kind === "s3"
        ? [HEX.test(pin) ? `--checksum=sha256:${sha}` : `--version-id=${pin}`, ...(f.region.trim() ? [`--region=${f.region.trim()}`] : [])]
        : [];
  const source = f.kind === "git" ? `${f.source.trim()}#${pin}` : f.source.trim();
  return `${indent}${directive} ${name}${[...own, ...kept].map((x) => ` ${x}`).join("")} FROM ${source}${rest}`;
}
