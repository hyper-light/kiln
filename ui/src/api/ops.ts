/**
 * kiln's operations as `GET /api/ops` lists them (docs/ci.md W20-R01): each one's name, summary,
 * whether it changes anything, and its input's JSON Schema. The command palette's entries for them
 * are made from this list, so the palette and the operations can't drift apart (kiln's principle 4).
 */
import { request } from "./client";

export interface OpInfo {
  name: string;
  summary: string;
  read_only: boolean;
  destructive: boolean;
  input: unknown;
}

export const opsApi = {
  list: (signal?: AbortSignal) => request<{ operations: OpInfo[] }>("GET", "/api/ops", undefined, { signal }).then((r) => r.operations),
};

/** The most an entry says before its first comma ends it. */
const ENTRY_WORDS = 48;

/**
 * An operation in words, for an entry: its summary without asides (parentheses), to its first colon
 * or full stop, and, still long, to its first comma ("Publish a port into a running microVM session").
 */
export function opLabel(summary: string): string {
  let s = summary.replace(/`/g, "");
  for (let was = ""; was !== s;) {
    was = s;
    s = s.replace(/\s*\([^()]*\)/g, "");
  }
  s = (s.split(/:\s|\.\s|\.$/)[0] ?? s).trim();
  return s.length > ENTRY_WORDS ? (s.split(/,\s/)[0] ?? s).trim() : s;
}
