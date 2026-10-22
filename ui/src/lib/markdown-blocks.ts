/**
 * Markdown as it streams in, cut where what came before can't change any more: a top-level block
 * ends at a blank line, unless what follows could still belong to it (a list's next item, an
 * indented line, the rest of a fenced code block) or the text defines links or footnotes that
 * anything before may use. A reply re-renders with each word: cut so, only its last block is
 * parsed again, not the whole reply. The pieces joined are the text.
 */

const FENCE = /^ {0,3}(`{3,}|~{3,})/;
/** A list item's marker: a bullet, or a number and its dot or parenthesis (alone at the end too). */
const ITEM = /^ {0,3}(?:[-+*]|\d{1,9}[.)])(?:[ \t]|$)/;
/** A link reference or footnote defined anywhere is used from anywhere, and raw HTML of the kinds
 *  that go on past a blank line (comments, `<pre>`, `<script>` …) holds what follows: such text
 *  isn't cut. */
const WHOLE = /^ {0,3}\[[^\]\n]+\]:|\[\^|<(?:[!?]|(?:pre|script|style|textarea)\b)/im;

export function streamBlocks(text: string): string[] {
  if (WHOLE.test(text)) return [text];
  const out: string[] = [];
  let start = 0;
  let fence: { mark: string; size: number } | null = null;
  let blank = false;
  let at = 0;
  while (at < text.length) {
    const nl = text.indexOf("\n", at);
    const end = nl < 0 ? text.length : nl + 1;
    const line = text.slice(at, nl < 0 ? text.length : nl);
    if (fence) {
      const close = FENCE.exec(line);
      if (close && close[1]![0] === fence.mark && close[1]!.length >= fence.size && line.slice(close[0].length).trim() === "") fence = null;
      blank = false;
    } else if (line.trim() === "") {
      blank = true;
    } else {
      // A block starts here when a blank line came before it and nothing above can take it in. A
      // line still coming that's too short to tell waits for more.
      const partial = nl < 0;
      if (blank && at > start && !/^\s/.test(line) && !ITEM.test(line) && !(partial && line.length < 3)) {
        out.push(text.slice(start, at));
        start = at;
      }
      const open = FENCE.exec(line);
      if (open) fence = { mark: open[1]![0]!, size: open[1]!.length };
      blank = false;
    }
    at = end;
  }
  out.push(text.slice(start));
  return out;
}
