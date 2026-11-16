/**
 * A query in kiln's monitoring syntax (Datadog's, as its monitors and metrics take it), edited in place:
 * coloured as it's read (aggregations and functions, windows, metrics, a scope's tags and values,
 * groups, comparisons, numbers) and completed as it's typed, each part in its turn: a time aggregation,
 * its window, a space aggregation, a metric, a scope's tag and its values, `by {…}` and its groups, a
 * comparison. Lines wrap; Enter submits (`onEnter`) unless a completion is open. Uncontrolled once
 * mounted (a new `key` starts it again). Loaded on demand: import it with React.lazy.
 */
import * as React from "react";
import { autocompletion, closeBrackets, completionKeymap, type CompletionContext, type CompletionResult } from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { HighlightStyle, StreamLanguage, syntaxHighlighting } from "@codemirror/language";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap, placeholder } from "@codemirror/view";
import { tags as t } from "@lezer/highlight";
import { completeQuery, SPACE_AGGS, type QueryWords } from "./query-words";

export type { QueryWords };

export interface QueryInputProps {
  value: string;
  onChange: (v: string) => void;
  label: string;
  placeholder?: string;
  words: QueryWords;
  onEnter?: () => void;
}

/** The syntax read as it's typed: whether the cursor is in a scope or a group's braces. */
const language = StreamLanguage.define<{ braces: boolean }>({
  name: "kiln-query",
  startState: () => ({ braces: false }),
  token(stream, st) {
    if (stream.eatSpace()) return null;
    if (st.braces) {
      if (stream.eat("}")) {
        st.braces = false;
        return "bracket";
      }
      if (stream.eat(",") || stream.eat(":")) return "punctuation";
      if (stream.eat("*")) return "atom";
      if (stream.match(/^!?[a-zA-Z_][\w.\-/]*(?=:)/)) return "propertyName";
      if (stream.match(/^[^,}:\s]+/)) return "string";
      stream.next();
      return null;
    }
    if (stream.eat("{")) {
      st.braces = true;
      return "bracket";
    }
    if (stream.match(/^(>=|<=|==|!=|>|<)/)) return "operator";
    if (stream.match(/^-?\d+(?:\.\d+)?/)) return "number";
    if (stream.match(/^last_\d+[smhdw]\b/)) return "atom";
    if (stream.match(/^by\b/)) return "keyword";
    const word = stream.match(/^[a-zA-Z_][\w.]*/) as RegExpMatchArray | null;
    if (word) {
      const next = stream.peek();
      if (next === "(" || (next === ":" && SPACE_AGGS.includes(word[0]))) return "keyword";
      return "variableName";
    }
    stream.next();
    return "punctuation";
  },
});

const highlight = HighlightStyle.define([
  { tag: t.keyword, color: "#b8a6d5" },
  { tag: t.variableName, color: "#a6c4ed" },
  { tag: t.propertyName, color: "#d6c4f0" },
  { tag: t.string, color: "#b8d9d2" },
  { tag: [t.atom, t.number], color: "#d8cbb0" },
  { tag: t.operator, color: "#e2b9bd" },
  { tag: [t.punctuation, t.bracket], color: "#8c8f98" },
]);

const theme = EditorView.theme(
  {
    "&": { color: "#d4dde8", backgroundColor: "transparent", fontSize: "12px" },
    ".cm-content": { fontFamily: "var(--font-mono)", caretColor: "#e4e5e7", padding: "6px 0", minHeight: "54px" },
    ".cm-line": { padding: "0 8px" },
    ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "18px" },
    ".cm-placeholder": { color: "#6d7078" },
    ".cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection": { backgroundColor: "rgb(166 196 237 / 18%)" },
    ".cm-cursor": { borderLeftColor: "#e4e5e7" },
    "&.cm-focused": { outline: "none" },
    ".cm-tooltip": { backgroundColor: "#121419", border: "1px solid #2b2e39", borderRadius: "7px", color: "#d4dde8" },
    ".cm-tooltip-autocomplete > ul": { fontFamily: "var(--font-mono)", fontSize: "12px", maxHeight: "14em" },
    ".cm-tooltip-autocomplete > ul > li[aria-selected]": { backgroundColor: "#1d2430", color: "#e4e5e7" },
    ".cm-completionDetail": { color: "#7f8a9a", fontStyle: "normal", marginLeft: "0.6em" },
  },
  { dark: true },
);

export default function QueryInput(props: QueryInputProps) {
  // The view reads the latest props through here; the editor itself mounts once.
  const latest = React.useRef(props);
  latest.current = props;
  const mount = React.useCallback((el: HTMLDivElement | null) => {
    if (!el) return;
    const p = latest.current;
    const view = new EditorView({
      parent: el,
      state: EditorState.create({
        doc: p.value,
        extensions: [
          history(),
          closeBrackets(),
          EditorView.lineWrapping,
          EditorView.contentAttributes.of({ "aria-label": p.label, role: "textbox", "aria-multiline": "true" }),
          ...(p.placeholder ? [placeholder(p.placeholder)] : []),
          language,
          syntaxHighlighting(highlight),
          theme,
          autocompletion({
            override: [(ctx: CompletionContext): CompletionResult | null => completeQuery(latest.current.words, ctx.state.doc.sliceString(0, ctx.pos), ctx.pos)],
            icons: false,
            activateOnTyping: true,
          }),
          keymap.of([
            ...completionKeymap,
            {
              key: "Enter",
              run: () => {
                if (!latest.current.onEnter) return false;
                latest.current.onEnter();
                return true;
              },
            },
            ...defaultKeymap,
            ...historyKeymap,
          ]),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) latest.current.onChange(u.state.doc.toString());
          }),
        ],
      }),
    });
    return () => view.destroy();
  }, []);
  return <div ref={mount} className="ms-edit-in qi" />;
}
