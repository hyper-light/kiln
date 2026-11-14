/**
 * Markdown as it streams in, cut where what came before can't change any more: a top-level block
 * ends at a blank line, unless what follows could still belong to it (a list's next item, an
 * indented line, the rest of a fenced code block) or the text defines links or footnotes that
 * anything before may use. A reply re-renders with each word: cut so, only its last block is
 * parsed again, not the whole reply. The pieces joined are the text.
 *
 * A text that grows is cut by a `BlockCutter`, which reads only what came since it last cut: cut
 * from its first character at every word, a long reply's cutting alone grew with the square of its
 * length (bench/reasoning.bench.ts).
 */

const FENCE = /^ {0,3}(`{3,}|~{3,})/;
/** A list item's marker: a bullet, or a number and its dot or parenthesis (alone at the end too). */
const ITEM = /^ {0,3}(?:[-+*]|\d{1,9}[.)])(?:[ \t]|$)/;
/**
 * A line that makes the text one block: a link reference or a footnote defined anywhere is used
 * from anywhere (a link's at the start of its line, a footnote's wherever its container puts it),
 * and raw HTML of the kinds that go on past a blank line (comments, `<pre>`, `<script>` …), which
 * begin at the start of a line (CommonMark's HTML blocks 1 to 5), holds what follows. Read line by
 * line outside fenced code. Code that only shows such a line (an HTML file, a regular expression's
 * `[^a-z]`, a footnote's mark with nothing defining it) defines nothing and holds nothing, and a
 * reply with a code block like that was parsed whole again with every word that came.
 */
const HOLDS = /^ {0,3}(?:\[[^\]\n]+\]:|<(?:[!?]|(?:pre|script|style|textarea)\b))|\[\^[^\]\n]+\]:/i;

interface Fence {
  mark: string;
  size: number;
}

/** A text as far as it was cut, and how its last line (which more text may still change) was reached. */
interface Cut {
  text: string;
  blocks: string[];
  /** The blocks that ended before the last line: more text can't change them. */
  settled: number;
  /** Where the last line begins, and as it was reached: where the block being read begins, the fence it's in, whether a blank line came before it. */
  at: number;
  start: number;
  fence: Fence | null;
  blank: boolean;
  /** A line made the text one block. */
  whole: boolean;
}

/** `text` cut, on from `from` where `text` continues the text that was cut to. */
function cut(text: string, from?: Cut): Cut {
  if (from?.whole) return { ...from, text, blocks: [text] };
  const out = from ? from.blocks.slice(0, from.settled) : [];
  let start = from?.start ?? 0;
  let fence = from?.fence ?? null;
  let blank = from?.blank ?? false;
  let at = from?.at ?? 0;
  // As the last line was reached: a text that ends inside a line is read again from there.
  let last = { at, start, fence, blank, settled: out.length };
  while (at < text.length) {
    const nl = text.indexOf("\n", at);
    const end = nl < 0 ? text.length : nl + 1;
    const line = text.slice(at, nl < 0 ? text.length : nl);
    last = { at, start, fence, blank, settled: out.length };
    if (fence) {
      const close = FENCE.exec(line);
      if (close && close[1]![0] === fence.mark && close[1]!.length >= fence.size && line.slice(close[0].length).trim() === "") fence = null;
      blank = false;
    } else if (line === "" || !/\S/.test(line)) {
      blank = true;
    } else {
      // One block for good once the line that says so is whole; a line still coming may yet read
      // otherwise (`<pre` grown to `<prefix`), so the text is cut from its start again after it.
      // (Most lines hold neither character either rule begins with, and are asked no more.)
      if ((line.includes("[") || line.includes("<")) && HOLDS.test(line)) return { text, blocks: [text], settled: 0, at: 0, start: 0, fence: null, blank: false, whole: nl >= 0 };
      // A block starts here when a blank line came before it and nothing above can take it in. A
      // line still coming that's too short to tell waits for more.
      const partial = nl < 0;
      if (blank && at > start && !/^\s/.test(line) && !ITEM.test(line) && !(partial && line.length < 3)) {
        out.push(text.slice(start, at));
        start = at;
      }
      const open = line.includes("```") || line.includes("~~~") ? FENCE.exec(line) : null;
      if (open) fence = { mark: open[1]![0]!, size: open[1]!.length };
      blank = false;
    }
    at = end;
  }
  // A text that ends with its line's end has no line to read again: the next cut is on from here.
  if (text.endsWith("\n")) last = { at, start, fence, blank, settled: out.length };
  out.push(text.slice(start));
  return { text, blocks: out, whole: false, ...last };
}

export function streamBlocks(text: string): string[] {
  return cut(text).blocks;
}

/**
 * Cuts a text as it grows, as `streamBlocks` cuts each: what it was given before isn't read again,
 * and the blocks that can't change any more are the very strings it gave before. A text that isn't
 * a continuation of the last is cut from its start.
 */
export class BlockCutter {
  private last: Cut | null = null;

  cut(text: string): string[] {
    const last = this.last;
    if (last && last.text === text) return last.blocks;
    this.last = cut(text, last && text.startsWith(last.text) ? last : undefined);
    return this.last.blocks;
  }
}
