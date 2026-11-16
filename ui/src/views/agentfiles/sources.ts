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

/* ------------------------------------------------ its build secrets, one of yours each, by name (P4.7) */

/** What a source's fetch is given: a token (a Git remote's, as `x-access-token`; an https URL's, as Bearer), or an S3 key's two halves. */
export type SecretRole = "token" | "key_id" | "secret_key";
/** One of the person's own secrets by name for each role, or none. */
export type SecretChoice = Partial<Record<SecretRole, string | null>>;

/**
 * A build secret a source's fetch reads (Shards build/git.rs `auth`, build/http.rs `Reach::of`,
 * build/s3.rs `KEY_SECRETS`): every id it reads it by, in Shards' order (the first a label names is the
 * one it's given); its own (its host's or bucket's alone), which the picker writes and takes out, the
 * rest serving every source of its kind; and the one it writes.
 */
export interface SecretSlot {
  role: SecretRole;
  label: string;
  ids: string[];
  own: string[];
  id: string;
}

/** A label's build secret id: letters, digits, `_`, `.` and `-` (kiln's `build_id`: Shards' `--secret id=` takes it as one field). */
const ID = /^[A-Za-z0-9_.-]+$/;
const authority = (url: string) => /^[a-z][a-z0-9+.-]*:\/\/(?:[^@/?#]*@)?([^/?#]+)/i.exec(url)?.[1] ?? null;

/** The build secrets a source's fetch reads; none for a path, or for Git over ssh, which an SSH agent signs in and kiln's builds are given secrets alone. */
export function slotsOf(f: Pick<SourceForm, "kind" | "source">): SecretSlot[] {
  const src = f.source.trim();
  if (f.kind === "git") {
    const host = src.startsWith("https://") ? authority(src) : null;
    if (!host) return [];
    // Shards names a remote by its host and port, and a label's id can't hold a `:`: one with a port takes the token every Git source reads.
    if (!ID.test(host)) return [{ role: "token", label: "Token", ids: ["GIT_AUTH_HEADER", "GIT_AUTH_TOKEN"], own: ["GIT_AUTH_TOKEN"], id: "GIT_AUTH_TOKEN" }];
    return [
      {
        role: "token",
        label: "Token",
        ids: [`GIT_AUTH_HEADER.${host}`, `GIT_AUTH_TOKEN.${host}`, "GIT_AUTH_HEADER", "GIT_AUTH_TOKEN"],
        own: [`GIT_AUTH_HEADER.${host}`, `GIT_AUTH_TOKEN.${host}`],
        id: `GIT_AUTH_TOKEN.${host}`,
      },
    ];
  }
  if (f.kind === "https") {
    const host = authority(src)?.replace(/:\d+$/, "");
    if (!host || !ID.test(host)) return [];
    const own = [`HTTP_AUTH_TOKEN_${host}`, `HTTP_AUTH_HEADER_${host}`];
    return [{ role: "token", label: "Token", ids: own, own, id: own[0]! }];
  }
  if (f.kind === "s3") {
    const bucket = /^s3:\/\/([^/\s]+)\//.exec(src)?.[1];
    if (!bucket || !ID.test(bucket)) return [];
    const half = (role: SecretRole, label: string, name: string): SecretSlot => ({ role, label, ids: [`${name}.${bucket}`, name], own: [`${name}.${bucket}`], id: `${name}.${bucket}` });
    return [half("key_id", "Key id", "AWS_ACCESS_KEY_ID"), half("secret_key", "Secret key", "AWS_SECRET_ACCESS_KEY")];
  }
  return [];
}

/** What a label naming a build secret starts with (crates/kiln/src/secrets/mine.rs `LABEL`). */
export const SECRET_KEY = "kiln.secret.";
const LABEL_LINE = /^\s*label\s/i;
/** A label's `key=value` pairs, each written quoted or not. */
const PAIR = /("[^"]*"|'[^']*'|[^\s"'=]+)=("[^"]*"|'[^']*'|\S+)/g;
const unquoted = (s: string) => (/^(["']).*\1$/.test(s) ? s.slice(1, -1) : s);
/** One of a person's own secrets' names (kiln's `check_name`): kept upper-case. */
const NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;
/** A line of labels naming build secrets alone, as the picker writes them after a source's line. */
const SECRET_LINE = /^\s*label\s+["']?kiln\.secret\./i;

/** One `kiln.secret.<ID>=user:<NAME>` pair of a label: its line (from 0), its id and name, and where it is in the line. */
interface Pair {
  line: number;
  id: string;
  name: string;
  from: number;
  to: number;
}

/** Each pair of `text`'s labels naming a build secret one of the person's own, as kiln's server reads them (`references`: either quotes, in any stage). */
function pairsIn(text: string): Pair[] {
  return text.split("\n").flatMap((l, line) =>
    LABEL_LINE.test(l)
      ? [...l.matchAll(PAIR)].flatMap((p) => {
          const key = unquoted(p[1]!);
          const value = unquoted(p[2]!);
          const name = value.startsWith("user:") ? value.slice(5) : "";
          return key.startsWith(SECRET_KEY) && NAME.test(name) ? [{ line, id: key.slice(SECRET_KEY.length), name: name.toUpperCase(), from: p.index, to: p.index + p[0].length }] : [];
        })
      : [],
  );
}

/**
 * The build secrets a source's fetch is given, as `text`'s labels name them (kiln's `references`, the
 * server's reading): for each of its slots, the first of its ids a label names, and that secret's name.
 */
export function secretsOf(text: string, f: Pick<SourceForm, "kind" | "source">): { id: string; name: string }[] {
  const named = new Map<string, string>();
  for (const p of pairsIn(text)) if (!named.has(p.id)) named.set(p.id, p.name);
  return slotsOf(f).flatMap((slot) => {
    const id = slot.ids.find((i) => named.has(i));
    return id ? [{ id, name: named.get(id)! }] : [];
  });
}

/** Whose secret a source's fetch is given, role by role: the first of each slot's ids a label names. */
export function chosenIn(text: string, slots: SecretSlot[]): Partial<Record<SecretRole, string>> {
  const named = new Map<string, string>();
  for (const p of pairsIn(text)) if (!named.has(p.id)) named.set(p.id, p.name);
  const out: Partial<Record<SecretRole, string>> = {};
  for (const s of slots) {
    const id = s.ids.find((i) => named.has(i));
    if (id) out[s.role] = named.get(id)!;
  }
  return out;
}

/**
 * `text` with a source's build secrets as `chosen` names them (a role left out stays as it is): each
 * changed slot's own labels taken out (a line left with no pair, its line too), and its id written
 * `LABEL kiln.secret.<ID>=user:<NAME>` on a line of its own after the source's (`at`, from 1) and the
 * secrets' labels already there.
 */
export function writeSecrets(text: string, at: number, slots: SecretSlot[], chosen: SecretChoice): string {
  const was = chosenIn(text, slots);
  const changed = slots.filter((s) => s.role in chosen && (chosen[s.role] ?? null) !== (was[s.role] ?? null));
  if (changed.length === 0) return text;
  const drop = new Set(changed.flatMap((s) => s.own));
  const lines: (string | null)[] = text.split("\n");
  // Each line's pairs taken out from its end back, so where the others are holds; a pair with the space before it.
  for (const p of pairsIn(text)
    .filter((p) => drop.has(p.id))
    .reverse()) {
    const l = lines[p.line]!;
    const from = l.slice(0, p.from).search(/\s*$/);
    const left = l.slice(0, from) + l.slice(p.to);
    lines[p.line] = /^\s*label\s*$/i.test(left) ? null : left;
  }
  const out: string[] = [];
  let source = -1;
  lines.forEach((l, i) => {
    if (l === null) return;
    out.push(l);
    if (i === at - 1) source = out.length - 1;
  });
  if (source < 0) return text;
  let after = source + 1;
  while (after < out.length && SECRET_LINE.test(out[after]!)) after++;
  const indent = /^\s*/.exec(out[source]!)![0];
  out.splice(after, 0, ...changed.flatMap((s) => (chosen[s.role] ? [`${indent}LABEL ${SECRET_KEY}${s.id}=user:${chosen[s.role]}`] : [])));
  return out.join("\n");
}

/** The one change that makes `a` into `b`: what lies between their common start and end. */
export function oneChange(a: string, b: string): { from: number; to: number; insert: string } {
  const n = Math.min(a.length, b.length);
  let start = 0;
  while (start < n && a[start] === b[start]) start++;
  let end = 0;
  while (end < n - start && a[a.length - 1 - end] === b[b.length - 1 - end]) end++;
  return { from: start, to: a.length - end, insert: b.slice(start, b.length - end) };
}

/** Where a source's line and the secrets' labels written after it are in `text`: from its start to the last's end. */
export function spanOf(text: string, s: Pick<AgentfileSource, "directive" | "name" | "line">): { from: number; to: number } {
  const lines = text.split("\n");
  const at = lineOf(text, s) - 1;
  let last = at;
  while (last + 1 < lines.length && SECRET_LINE.test(lines[last + 1]!)) last++;
  const from = lines.slice(0, at).reduce((n, l) => n + l.length + 1, 0);
  return { from, to: from + lines.slice(at, last + 1).join("\n").length };
}
