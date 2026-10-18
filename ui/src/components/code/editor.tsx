/**
 * The code editor (docs/ci.md W21-R01): CodeMirror 6 with YAML, `${{ }}` marked inside it,
 * diagnostics from the server 300 ms after typing stops, and completion the page supplies. Lines
 * wrap, so nothing scrolls sideways on a phone. Loaded on demand: import it with React.lazy.
 */
import * as React from "react";
import { autocompletion, closeBrackets, completionKeymap, type CompletionContext, type CompletionResult } from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { yaml } from "@codemirror/lang-yaml";
import { bracketMatching, HighlightStyle, indentOnInput, syntaxHighlighting } from "@codemirror/language";
import { linter, lintGutter, type Diagnostic } from "@codemirror/lint";
import { highlightSelectionMatches, searchKeymap } from "@codemirror/search";
import { EditorState } from "@codemirror/state";
import {
  Decoration,
  drawSelection,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  keymap,
  lineNumbers,
  MatchDecorator,
  ViewPlugin,
  type DecorationSet,
  type ViewUpdate,
} from "@codemirror/view";
import { tags as t } from "@lezer/highlight";

export type { Diagnostic } from "@codemirror/lint";
export type { CompletionContext, CompletionResult } from "@codemirror/autocomplete";

export interface CodeEditorProps {
  /** The text it opens with; give the editor a new `key` to open another. */
  value: string;
  onChange?: (text: string) => void;
  /** The document's problems, asked for 300 ms after typing stops. */
  lint?: (text: string) => Promise<Diagnostic[]>;
  complete?: (ctx: CompletionContext) => CompletionResult | null | Promise<CompletionResult | null>;
  label: string;
}

const highlight = HighlightStyle.define([
  { tag: [t.propertyName, t.definition(t.propertyName)], color: "#a6c4ed" },
  { tag: [t.string, t.special(t.string)], color: "#b8d9d2" },
  { tag: [t.number, t.bool, t.null, t.atom], color: "#d8cbb0" },
  { tag: [t.comment, t.lineComment, t.blockComment], color: "#6d7078", fontStyle: "italic" },
  { tag: [t.keyword, t.operator, t.punctuation, t.separator, t.bracket], color: "#8c8f98" },
  { tag: [t.meta, t.labelName], color: "#b8a6d5" },
]);

const theme = EditorView.theme(
  {
    "&": { color: "#d4dde8", backgroundColor: "transparent", fontSize: "12.5px" },
    ".cm-content": { fontFamily: "var(--font-mono)", caretColor: "#e4e5e7", padding: "10px 0" },
    ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "1.65" },
    ".cm-gutters": { backgroundColor: "transparent", color: "#4f545e", border: "none" },
    ".cm-activeLine": { backgroundColor: "rgb(255 255 255 / 2.5%)" },
    ".cm-activeLineGutter": { backgroundColor: "transparent", color: "#8c8f98" },
    ".cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection": { backgroundColor: "rgb(166 196 237 / 18%)" },
    ".cm-cursor": { borderLeftColor: "#e4e5e7" },
    "&.cm-focused": { outline: "none" },
    ".cm-expr": { color: "#d6c4f0", backgroundColor: "rgb(184 166 213 / 9%)", borderRadius: "3px" },
    ".cm-tooltip": { backgroundColor: "#121419", border: "1px solid #2b2e39", borderRadius: "7px", color: "#d4dde8" },
    ".cm-tooltip-autocomplete > ul > li[aria-selected]": { backgroundColor: "#1d2430", color: "#e4e5e7" },
    ".cm-completionDetail": { color: "#7f8a9a", fontStyle: "normal", marginLeft: "0.6em" },
    ".cm-diagnostic": { fontFamily: "var(--font-mono)", fontSize: "11.5px" },
    ".cm-diagnostic-error": { borderLeftColor: "#d1aaad" },
    ".cm-diagnostic-warning": { borderLeftColor: "#cbb69a" },
    ".cm-lintRange-error": { backgroundImage: "none", textDecoration: "underline wavy #d1aaad", textUnderlineOffset: "3px" },
    ".cm-lintRange-warning": { backgroundImage: "none", textDecoration: "underline wavy #cbb69a", textUnderlineOffset: "3px" },
    ".cm-panels": { backgroundColor: "#0e1014", color: "#d4dde8" },
    ".cm-searchMatch": { backgroundColor: "rgb(216 203 176 / 18%)" },
  },
  { dark: true },
);

/** `${{ … }}` marked, so expressions stand out from the YAML around them. */
const expressions = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    private matcher = new MatchDecorator({ regexp: /\$\{\{[^}]*\}\}/g, decoration: Decoration.mark({ class: "cm-expr" }) });
    constructor(view: EditorView) {
      this.decorations = this.matcher.createDeco(view);
    }
    update(u: ViewUpdate) {
      this.decorations = this.matcher.updateDeco(u, this.decorations);
    }
  },
  { decorations: (v) => v.decorations },
);

export default function CodeEditor(props: CodeEditorProps) {
  // The view reads the latest callbacks through here; the editor itself mounts once.
  const latest = React.useRef(props);
  latest.current = props;
  const mount = React.useCallback((el: HTMLDivElement | null) => {
    if (!el) return;
    const view = new EditorView({
      parent: el,
      state: EditorState.create({
        doc: latest.current.value,
        extensions: [
          lineNumbers(),
          highlightActiveLineGutter(),
          history(),
          drawSelection(),
          indentOnInput(),
          bracketMatching(),
          closeBrackets(),
          highlightActiveLine(),
          highlightSelectionMatches(),
          EditorView.lineWrapping,
          EditorView.contentAttributes.of({ "aria-label": latest.current.label }),
          yaml(),
          syntaxHighlighting(highlight),
          expressions,
          theme,
          lintGutter(),
          linter((v) => latest.current.lint?.(v.state.doc.toString()) ?? [], { delay: 300 }),
          autocompletion({ override: [(ctx) => latest.current.complete?.(ctx) ?? null], icons: false }),
          keymap.of([indentWithTab, ...defaultKeymap, ...historyKeymap, ...searchKeymap, ...completionKeymap]),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) latest.current.onChange?.(u.state.doc.toString());
          }),
        ],
      }),
    });
    return () => view.destroy();
  }, []);
  return <div ref={mount} className="code-editor" />;
}
