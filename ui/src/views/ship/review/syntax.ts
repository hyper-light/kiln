/** W26-R07, R15: the unchanged hunk sides sent to the syntax worker, and their original line numbers. */
import type { Hunk } from "@/api/harness";
import type { Tok } from "@/lib/syntax/engine";

export interface FileToks {
  old: Map<number, Tok[]>;
  neu: Map<number, Tok[]>;
}

export const NO_TOKS: FileToks = { old: new Map(), neu: new Map() };

export interface DiffSyntax {
  texts: string[];
  places: { side: "old" | "neu"; numbers: number[] }[];
}

/** Keep each hunk side intact: multiline strings/comments need its context, including folded lines. */
export function syntaxOf(hunks: readonly Hunk[]): DiffSyntax {
  const texts: string[] = [];
  const places: DiffSyntax["places"] = [];
  for (const h of hunks) {
    const oldLines: string[] = [];
    const oldNums: number[] = [];
    const newLines: string[] = [];
    const newNums: number[] = [];
    let o = h.old_start;
    let n = h.new_start;
    for (const l of h.lines) {
      const t = l.charAt(0);
      if (t === "\\") continue;
      const text = l.slice(1);
      if (t !== "+") {
        oldLines.push(text);
        oldNums.push(o++);
      }
      if (t !== "-") {
        newLines.push(text);
        newNums.push(n++);
      }
    }
    if (oldLines.length) {
      texts.push(oldLines.join("\n"));
      places.push({ side: "old", numbers: oldNums });
    }
    if (newLines.length) {
      texts.push(newLines.join("\n"));
      places.push({ side: "neu", numbers: newNums });
    }
  }
  return { texts, places };
}

/** A worker response belongs to exactly these texts; signs, gaps and line numbers never enter the parser. */
export function tokensOf(syntax: DiffSyntax, lines: readonly (Tok[][] | null)[] | null): FileToks {
  if (!lines) return NO_TOKS;
  const out: FileToks = { old: new Map(), neu: new Map() };
  syntax.places.forEach((place, i) => {
    lines[i]?.forEach((toks, row) => {
      const number = place.numbers[row];
      if (number != null) out[place.side].set(number, toks);
    });
  });
  return out;
}
