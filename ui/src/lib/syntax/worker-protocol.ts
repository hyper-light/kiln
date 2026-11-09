/** W26-R15: bounded, request-associated derived syntax work; no server data lives here. */
import type { Tok } from "./engine";

export type HighlightLines = readonly (Tok[][] | null)[];
export type HighlightRequest = { type: "highlight"; id: number; language: string; texts: readonly string[] } | { type: "cancel"; id: number };
export type HighlightResponse = { type: "highlight"; id: number; lines: HighlightLines } | { type: "error"; id: number };

export const MAX_SYNTAX_JOBS = 32;
export const MAX_SYNTAX_TEXTS = 1024;
export const MAX_SYNTAX_JOB_CHARS = 1_000_000;
export const MAX_SYNTAX_PENDING_CHARS = 2_000_000;
/** Bound the main thread's structured clone and token DOM work for one derived result. */
export const MAX_SYNTAX_RESULT_LINES = 100_000;
export const MAX_SYNTAX_RESULT_TOKENS = 100_000;

/** Check before structured cloning, and again at the worker boundary. */
export function textSize(texts: readonly string[]): number | null {
  if (!Array.isArray(texts) || texts.length > MAX_SYNTAX_TEXTS) return null;
  let size = 0;
  for (const text of texts) {
    if (typeof text !== "string") return null;
    size += text.length;
    if (size > MAX_SYNTAX_JOB_CHARS) return null;
  }
  return size;
}
