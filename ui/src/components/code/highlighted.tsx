"use client";
/**
 * Code in its language's colours (lib/syntax): plain until its grammar is here, and on the server
 * and while the page hydrates. A client component, so markdown rendered on the server (a shared
 * plan) is highlighted in the browser.
 */
import * as React from "react";
import { resolveLanguage, useHighlighter, type Tok } from "@/lib/syntax/browser";
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
