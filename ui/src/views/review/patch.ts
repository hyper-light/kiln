/**
 * A whole `git diff -M` patch, read into files and hunks (the review page's diff, W14-R06), and
 * what the page does to them before they're drawn: whitespace-only changes taken as unchanged when
 * asked, and blocks that moved (deleted in one place, added in another) found.
 *
 * A hunk ends by its line counts, not by what its lines look like, so a deleted line reading
 * `--- …` or `diff --git …` can't end it early.
 */
import type { Hunk } from "@/api/harness";

export interface PatchFile {
  /** Where it is now (where it was, when deleted). */
  path: string;
  /** Where it was, for a rename or a copy. */
  oldPath: string | null;
  status: "added" | "deleted" | "renamed" | "copied" | "modified";
  /** For a rename or a copy: how alike the two are (git's percent). */
  similarity: number | null;
  binary: boolean;
  /** "100644 → 100755" when its mode changed. */
  modeChange: string | null;
  hunks: Hunk[];
}

const HUNK = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/;

/** A path as git writes it: C-quoted ("a/\303\244 b") when it has unusual characters. */
function unquote(p: string): string {
  if (!(p.startsWith('"') && p.endsWith('"') && p.length >= 2)) return p;
  const body = p.slice(1, -1);
  const bytes: number[] = [];
  const named: Record<string, number> = { n: 10, t: 9, r: 13, a: 7, b: 8, f: 12, v: 11, '"': 34, "\\": 92 };
  const enc = new TextEncoder();
  for (let i = 0; i < body.length; i++) {
    const c = body.charAt(i);
    if (c === "\\" && i + 1 < body.length) {
      const oct = body.slice(i + 1, i + 4);
      if (/^[0-7]{3}$/.test(oct)) {
        bytes.push(parseInt(oct, 8));
        i += 3;
        continue;
      }
      const n = body.charAt(i + 1);
      bytes.push(named[n] ?? n.charCodeAt(0));
      i += 1;
      continue;
    }
    for (const b of enc.encode(c)) bytes.push(b);
  }
  return new TextDecoder().decode(new Uint8Array(bytes));
}

const strip = (p: string, prefix: string) => {
  const u = unquote(p.trim());
  return u.startsWith(prefix) ? u.slice(prefix.length) : u;
};

/** The files of a patch, in its order. */
export function parsePatch(text: string): PatchFile[] {
  const out: PatchFile[] = [];
  let cur: PatchFile | null = null;
  let hunk: Hunk | null = null;
  let oldLeft = 0;
  let newLeft = 0;
  let added = false;
  let deleted = false;
  let oldMode: string | null = null;
  const finish = () => {
    if (!cur) return;
    if (!cur.path) cur.path = cur.oldPath ?? "";
    if (added) cur.status = "added";
    else if (deleted) cur.status = "deleted";
    if (cur.status === "deleted") cur.path = cur.oldPath ?? cur.path;
    if (cur.status !== "renamed" && cur.status !== "copied") cur.oldPath = null;
  };
  for (const raw of text.split("\n")) {
    const l = raw.endsWith("\r") ? raw.slice(0, -1) : raw;
    if (hunk && (oldLeft > 0 || newLeft > 0 || l.startsWith("\\"))) {
      const tag = l.charAt(0);
      if (tag === "\\") {
        hunk.lines.push(l);
        continue;
      }
      if (tag === " " || l === "") {
        hunk.lines.push(tag === " " ? l : " ");
        oldLeft--;
        newLeft--;
        continue;
      }
      if (tag === "-") {
        hunk.lines.push(l);
        oldLeft--;
        continue;
      }
      if (tag === "+") {
        hunk.lines.push(l);
        newLeft--;
        continue;
      }
      // Not a hunk line after all: the hunk said more than it had.
      hunk = null;
    }
    if (l.startsWith("diff --git ")) {
      finish();
      cur = { path: "", oldPath: null, status: "modified", similarity: null, binary: false, modeChange: null, hunks: [] };
      out.push(cur);
      hunk = null;
      added = false;
      deleted = false;
      oldMode = null;
      // `a/x b/y`: the paths, when they don't hold " b/" themselves (the ---/+++ lines settle it).
      const rest = l.slice(11);
      const at = rest.lastIndexOf(" b/") >= 0 ? rest.lastIndexOf(" b/") : rest.lastIndexOf(' "b/');
      if (at > 0) {
        cur.oldPath = strip(rest.slice(0, at), "a/");
        cur.path = strip(rest.slice(at + 1), "b/");
      }
      continue;
    }
    if (!cur) continue;
    const m = HUNK.exec(l);
    if (m) {
      const [, os, ol, ns, nl] = m;
      hunk = { old_start: Number(os), old_lines: ol === undefined ? 1 : Number(ol), new_start: Number(ns), new_lines: nl === undefined ? 1 : Number(nl), lines: [] };
      oldLeft = hunk.old_lines;
      newLeft = hunk.new_lines;
      cur.hunks.push(hunk);
      continue;
    }
    if (l.startsWith("new file mode")) added = true;
    else if (l.startsWith("deleted file mode")) deleted = true;
    else if (l.startsWith("old mode ")) oldMode = l.slice(9).trim();
    else if (l.startsWith("new mode ")) cur.modeChange = `${oldMode ?? "?"} → ${l.slice(9).trim()}`;
    else if (l.startsWith("similarity index ")) cur.similarity = parseInt(l.slice(17), 10);
    else if (l.startsWith("rename from ")) {
      cur.status = "renamed";
      cur.oldPath = unquote(l.slice(12));
    } else if (l.startsWith("rename to ")) cur.path = unquote(l.slice(10));
    else if (l.startsWith("copy from ")) {
      cur.status = "copied";
      cur.oldPath = unquote(l.slice(10));
    } else if (l.startsWith("copy to ")) cur.path = unquote(l.slice(8));
    else if (l.startsWith("Binary files ") || l.startsWith("GIT binary patch")) cur.binary = true;
    else if (l.startsWith("--- ")) {
      if (l.slice(4).trim() === "/dev/null") added = true;
      else cur.oldPath = strip(l.slice(4), "a/");
    } else if (l.startsWith("+++ ")) {
      if (l.slice(4).trim() === "/dev/null") deleted = true;
      else cur.path = strip(l.slice(4), "b/");
    }
  }
  finish();
  return out;
}

/* ------------------------------------------------------------------ whitespace */

const squash = (s: string) => s.replace(/\s+/g, "");

/**
 * The hunk with whitespace-only changes taken as unchanged: within a block of changes, a deleted
 * line and an added one equal but for whitespace become one unchanged line (as it reads now).
 * Each side keeps its own order, so line numbers still count true.
 */
export function ignoringWhitespace(h: Hunk): Hunk {
  const lines: string[] = [];
  const ls = h.lines;
  for (let i = 0; i < ls.length;) {
    const tag = (ls[i] ?? "").charAt(0);
    if (tag !== "-" && tag !== "+") {
      lines.push(ls[i] ?? "");
      i++;
      continue;
    }
    const dels: string[] = [];
    const adds: string[] = [];
    for (; i < ls.length; i++) {
      const l = ls[i] ?? "";
      const t = l.charAt(0);
      if (t === "-") dels.push(l.slice(1));
      else if (t === "+") adds.push(l.slice(1));
      else if (t !== "\\") break;
    }
    const want = adds.map(squash);
    let d = 0;
    let a = 0;
    while (d < dels.length || a < adds.length) {
      const del = dels[d];
      const add = adds[a];
      if (del !== undefined && add !== undefined && squash(del) === want[a]) {
        lines.push(` ${add}`);
        d++;
        a++;
      } else if (del !== undefined && (add === undefined || !want.slice(a).includes(squash(del)))) {
        lines.push(`-${del}`);
        d++;
      } else if (add !== undefined) {
        lines.push(`+${add}`);
        a++;
      } else break;
    }
  }
  return { ...h, lines };
}

/* ------------------------------------------------------------------ moved blocks */

/** A line of a patch's file: `<file index>:o<old line>` for a deleted line, `<file index>:n<new line>` for an added one. */
export type Place = `${number}:${"o" | "n"}${number}`;

export const placeOf = (file: number, side: "o" | "n", line: number): Place => `${file}:${side}${line}`;

/** Where a moved line went, or came from: the file and the line number on that side. */
export interface MovedTo {
  file: number;
  line: number;
}

/** Fewer lines than this in a row aren't a move. */
const MOVE_RUN = 3;

const meaningful = (s: string) => s.trim().length >= 4;

/**
 * Blocks deleted in one place and added in another (the same lines, three or more in a row, at
 * least one with something to it), each side pointing at the other: `del` for deleted lines (where
 * they went: the new side's line), `add` for added ones (where they came from: the old side's).
 * A block changed in place isn't a move: its deletion and addition sit in the same change block.
 */
export function findMoves(files: PatchFile[]): { del: Map<Place, MovedTo>; add: Map<Place, MovedTo> } {
  interface At {
    place: Place;
    file: number;
    line: number;
    block: string;
    text: string;
  }
  const dels: At[][] = [];
  const adds: At[][] = [];
  files.forEach((f, fi) =>
    f.hunks.forEach((h, hi) => {
      let o = h.old_start;
      let n = h.new_start;
      let block = 0;
      let run: At[] = [];
      let kind: "-" | "+" | null = null;
      const flush = () => {
        if (run.length >= MOVE_RUN) (kind === "-" ? dels : adds).push(run);
        run = [];
        kind = null;
      };
      h.lines.forEach((l) => {
        const t = l.charAt(0);
        if (t === "-" || t === "+") {
          if (kind !== t) flush();
          kind = t;
          run.push({ place: placeOf(fi, t === "-" ? "o" : "n", t === "-" ? o : n), file: fi, line: t === "-" ? o : n, block: `${fi}:${hi}:${block}`, text: l.slice(1).trim() });
          if (t === "-") o++;
          else n++;
        } else if (t !== "\\") {
          flush();
          block++;
          o++;
          n++;
        }
      });
      flush();
    }),
  );
  // Each window of MOVE_RUN added lines, by its text.
  const windows = new Map<string, { run: At[]; at: number }[]>();
  for (const run of adds)
    for (let k = 0; k + MOVE_RUN <= run.length; k++) {
      const w = run.slice(k, k + MOVE_RUN);
      if (!w.some((x) => meaningful(x.text))) continue;
      const key = w.map((x) => x.text).join("\n");
      const list = windows.get(key) ?? [];
      list.push({ run, at: k });
      windows.set(key, list);
    }
  const del = new Map<Place, MovedTo>();
  const add = new Map<Place, MovedTo>();
  for (const run of dels)
    for (let k = 0; k + MOVE_RUN <= run.length; k++) {
      const w = run.slice(k, k + MOVE_RUN);
      const hit = windows.get(w.map((x) => x.text).join("\n"))?.find((c) => c.run[c.at]?.block !== w[0]?.block);
      if (!hit) continue;
      for (let j = 0; j < MOVE_RUN; j++) {
        const d = w[j];
        const a = hit.run[hit.at + j];
        if (!d || !a) continue;
        del.set(d.place, { file: a.file, line: a.line });
        add.set(a.place, { file: d.file, line: d.line });
      }
    }
  return { del, add };
}

/** An image git would call binary, that a browser shows. */
export function isImage(path: string): boolean {
  return /\.(png|jpe?g|gif|webp|avif|svg|ico|bmp)$/i.test(path);
}
