/**
 * What's typed in the composer, as it will read: the agent's slash commands, wherever they are in
 * the message, and Markdown (code, emphasis, headings, quotes, lists, links). It's drawn under the
 * field, whose own text is clear, so nothing here may move a character: colours, backgrounds,
 * lines and slants only; bold is a stroke, not a heavier weight, and code keeps the field's font.
 */
import * as React from "react";

const INLINE = new RegExp(
  [
    "(?<code>`[^`\\n]+`)",
    "(?<bold>\\*\\*[^*\\s](?:[^*\\n]*[^*\\s])?\\*\\*|__[^_\\s](?:[^_\\n]*[^_\\s])?__)",
    "(?<strike>~~[^~\\s](?:[^~\\n]*[^~\\s])?~~)",
    "(?<em>(?<![\\w*])\\*[^*\\s](?:[^*\\n]*[^*\\s])?\\*(?![\\w*])|(?<![\\w_])_[^_\\s](?:[^_\\n]*[^_\\s])?_(?![\\w_]))",
    "(?<link>\\[[^\\]\\n]+\\]\\([^)\\s]+\\))",
    // A command: "/" at the start or after a space, then its name (trailing punctuation left out).
    "(?<cmd>(?<!\\S)/\\w(?:[\\w:.-]*\\w)?)",
  ].join("|"),
  "g",
);

/** Its marks dimmed around what they mark. */
function wrap(className: string, open: string, inner: string, close: string) {
  return (
    <span className={className}>
      <span className="md-m">{open}</span>
      {inner}
      <span className="md-m">{close}</span>
    </span>
  );
}

/** One line's code, emphasis, links and commands (only the names `names` holds count). */
function inline(line: string, names: ReadonlySet<string>, key: string): React.ReactNode[] {
  const out: React.ReactNode[] = [];
  let at = 0;
  for (const m of line.matchAll(INLINE)) {
    const g = m.groups ?? {};
    const t = m[0];
    let node: React.ReactNode;
    if (g.code) node = wrap("md-code", "`", t.slice(1, -1), "`");
    else if (g.bold) node = wrap("md-b", t.slice(0, 2), t.slice(2, -2), t.slice(-2));
    else if (g.strike) node = wrap("md-s", "~~", t.slice(2, -2), "~~");
    else if (g.em) node = wrap("md-i", t.charAt(0), t.slice(1, -1), t.charAt(0));
    else if (g.link) {
      const mid = t.indexOf("](");
      node = wrap("md-a", "[", t.slice(1, mid), t.slice(mid));
    } else if (g.cmd && names.has(t.slice(1).toLowerCase())) node = <span className="md-cmd">{t}</span>;
    // A "/word" that isn't one of its commands (a path, a fraction) stays as typed.
    else continue;
    if (m.index > at) out.push(line.slice(at, m.index));
    out.push(<React.Fragment key={`${key}.${m.index}`}>{node}</React.Fragment>);
    at = m.index + t.length;
  }
  if (at < line.length) out.push(line.slice(at));
  return out;
}

/** The field's text, marked: line by line, fenced code kept whole. */
function marks(text: string, names: ReadonlySet<string>): React.ReactNode[] {
  const out: React.ReactNode[] = [];
  // The fence an open code block ends with.
  let fence: RegExp | null = null;
  text.split("\n").forEach((line, n) => {
    const key = String(n);
    if (n > 0) out.push("\n");
    if (fence) {
      const closes = fence.test(line);
      if (closes) fence = null;
      out.push(
        <span key={key} className={closes ? "md-m" : "md-pre"}>
          {line}
        </span>,
      );
      return;
    }
    const open = line.match(/^ {0,3}(`{3,}|~{3,})/);
    if (open?.[1]) {
      fence = new RegExp(`^ {0,3}${open[1].charAt(0)}{${open[1].length},}\\s*$`);
      out.push(
        <span key={key} className="md-m">
          {line}
        </span>,
      );
      return;
    }
    const h = line.match(/^( {0,3}#{1,6})(\s.*)?$/);
    if (h?.[1]) {
      out.push(
        <span key={key} className="md-h">
          <span className="md-m">{h[1]}</span>
          {inline(h[2] ?? "", names, key)}
        </span>,
      );
      return;
    }
    if (/^ {0,3}([-*_])(?:\s*\1){2,}\s*$/.test(line)) {
      out.push(
        <span key={key} className="md-m">
          {line}
        </span>,
      );
      return;
    }
    const q = line.match(/^( {0,3}>\s?)(.*)$/);
    if (q?.[1]) {
      out.push(
        <span key={key} className="md-q">
          <span className="md-m">{q[1]}</span>
          {inline(q[2] ?? "", names, key)}
        </span>,
      );
      return;
    }
    const li = line.match(/^(\s*)([-*+]|\d{1,9}[.)])(?=\s)/);
    if (li?.[2]) {
      out.push(
        <React.Fragment key={key}>
          {li[1]}
          <span className="md-li">{li[2]}</span>
          {inline(line.slice(li[0].length), names, key)}
        </React.Fragment>,
      );
      return;
    }
    out.push(<React.Fragment key={key}>{inline(line, names, key)}</React.Fragment>);
  });
  return out;
}

/** The names (and aliases) of these commands, as typed after "/" (any case). */
export function commandNames(commands: readonly { name: string; aliases?: string[] }[]): ReadonlySet<string> {
  return new Set(commands.flatMap((c) => [c.name, ...(c.aliases ?? [])].map((n) => n.toLowerCase())));
}

/**
 * The text drawn under the field, in the field's own size and font. A zero-width space after it
 * holds a last empty line open, as the field does.
 */
export const Marked = React.memo(function Marked({ text, names }: { text: string; names: ReadonlySet<string> }) {
  return (
    <>
      {marks(text, names)}
      {"​"}
    </>
  );
});
