"use client";
/**
 * Code in its language's colours (lib/syntax): plain until its grammar is here, and on the server
 * and while the page hydrates. A client component, so markdown rendered on the server (a shared
 * plan) is highlighted in the browser.
 */
import * as React from "react";
import { resolveLanguage, useHighlighter, type Tok } from "@/lib/syntax/browser";
import { AboutCard, AboutContext, linesLabel, offsetAt, selecting, wordAt, type Asked } from "./about";
import "./syntax.css";

/** One line in its tokens' colours. */
export function Toks({ text, toks }: { text: string; toks: readonly Tok[] | undefined }) {
  if (!toks?.length) return text;
  const out: React.ReactNode[] = [];
  let at = 0;
  for (const [s, e, cls] of toks) {
    if (s > at) out.push(text.slice(at, s));
    out.push(
      <span key={s} className={cls}>
        {text.slice(s, e)}
      </span>,
    );
    at = e;
  }
  if (at < text.length) out.push(text.slice(at));
  return out;
}

/**
 * `text` in `lang`'s colours (a language's name, or a fence's info string; none: found from `path`,
 * else the text's `#!` line): its lines, for whatever holds them (a `<code>`, a `<pre>`).
 */
export function Highlighted({ text, lang, path }: { text: string; lang?: string | null; path?: string | null }) {
  const hl = useHighlighter(resolveLanguage(lang, path, text));
  const lines = React.useMemo(() => hl?.(text) ?? null, [hl, text]);
  if (!lines) return text;
  return text.split("\n").map((line, i) => (
    <React.Fragment key={i}>
      {i > 0 && "\n"}
      <Toks text={line} toks={lines[i]} />
    </React.Fragment>
  ));
}

/**
 * A fenced block's code in its language's colours. In an agent's conversation a name or a line in
 * it can be asked about (./about.tsx): the block opens at that line for the card.
 */
export function CodeBlock({ text, lang, className }: { text: string; lang?: string | null; className?: string }) {
  const about = React.useContext(AboutContext);
  const [asked, setAsked] = React.useState<{ from: number; to: number; ask: Asked } | null>(null);
  const hl = useHighlighter(resolveLanguage(lang, null, text));
  const toks = React.useMemo(() => hl?.(text) ?? null, [hl, text]);
  const lines = text.split("\n");
  const part = (from: number, to: number) =>
    lines.slice(from, to).map((line, i) => (
      <React.Fragment key={from + i}>
        {i > 0 && "\n"}
        <Toks text={line} toks={toks?.[from + i]} />
      </React.Fragment>
    ));
  const tap = (from: number) =>
    about
      ? (e: React.MouseEvent<HTMLElement>) => {
          if (selecting()) return;
          const off = offsetAt(e.currentTarget, e.clientX, e.clientY);
          if (off === null) return;
          const before = lines.slice(from).join("\n").slice(0, off);
          const n = from + before.split("\n").length - 1;
          const word = wordAt(lines[n] ?? "", off - (before.lastIndexOf("\n") + 1));
          const selection = word ? (lines[n] ?? "").slice(word.start, word.end) : undefined;
          setAsked((prev) => {
            let [lo, hi] = [n, n];
            // Another line, while lines are asked about: the lines between are taken in.
            if (!selection && prev && !prev.ask.selection && (n < prev.from || n > prev.to)) [lo, hi] = [Math.min(prev.from, n), Math.max(prev.to, n)];
            else if (prev && prev.from === n && prev.to === n && prev.ask.selection === selection) return null;
            const label = selection ?? linesLabel(lo + 1, hi + 1);
            const code = lines.slice(Math.max(0, lo - 20), hi + 21).join("\n");
            const ask: Asked = {
              path: lang ? `a ${lang} block in the conversation` : "a code block in the conversation",
              line: lo + 1,
              end: hi > lo ? hi + 1 : undefined,
              selection,
              code,
              label,
              where: `the code block · ${linesLabel(lo + 1, hi + 1)}`,
            };
            return { from: lo, to: hi, ask };
          });
        }
      : undefined;
  const pre = (from: number, to: number, key: string) => (
    <pre key={key} className="hx-pre" data-lang={lang ?? undefined} onClick={tap(from)}>
      <code className={className}>{part(from, to)}</code>
    </pre>
  );
  if (!asked) return pre(0, lines.length, "all");
  return (
    <>
      {pre(0, asked.to + 1, "top")}
      <AboutCard key={`${asked.ask.label}@${asked.from}`} ask={asked.ask} onClose={() => setAsked(null)} />
      {asked.to + 1 < lines.length && pre(asked.to + 1, lines.length, "rest")}
    </>
  );
}
