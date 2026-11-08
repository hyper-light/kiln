/**
 * kiln's tree-sitter highlighter in CodeMirror (docs/ci.md W29-R05): a view plugin that colours the
 * document with the engine's tokens as syntax.css's classes, so code reads in the editor as it does
 * in Ship's Files. It highlights again as the text changes, and once a grammar it waits on (its own,
 * or one written inside it: a workflow's `run:`, read as its shell) has loaded.
 */
import { RangeSetBuilder, StateEffect, type Extension } from "@codemirror/state";
import { Decoration, ViewPlugin, type DecorationSet, type EditorView, type ViewUpdate } from "@codemirror/view";
import { highlightNow, onLoad } from "./browser";

/** Said when a grammar arrives, so the plugin highlights again. */
const loaded = StateEffect.define<null>();

/** One mark per class, shared by every editor. */
const marks = new Map<string, Decoration>();
function markOf(cls: string): Decoration {
  let m = marks.get(cls);
  if (!m) {
    m = Decoration.mark({ class: cls });
    marks.set(cls, m);
  }
  return m;
}

/** The document's tokens as marks: none while its grammar loads, or where it reads plain. */
function decorate(view: EditorView, id: string): DecorationSet {
  const doc = view.state.doc;
  const lines = highlightNow(id, doc.toString());
  const b = new RangeSetBuilder<Decoration>();
  if (lines)
    for (let i = 0; i < lines.length && i < doc.lines; i++) {
      const from = doc.line(i + 1).from;
      for (const [s, e, cls] of lines[i] ?? []) if (cls && e > s) b.add(from + s, from + e, markOf(cls));
    }
  return b.finish();
}

/** The document in the colours of the language `id` (lib/syntax/languages.ts). */
export function treeSitter(id: string): Extension {
  return ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      private readonly off: () => void;
      constructor(view: EditorView) {
        this.decorations = decorate(view, id);
        // A grammar arriving is news from outside the editor: it asks for an update, to read it then.
        this.off = onLoad(() => view.dispatch({ effects: loaded.of(null) }));
      }
      update(u: ViewUpdate) {
        if (u.docChanged || u.transactions.some((t) => t.effects.some((e) => e.is(loaded)))) this.decorations = decorate(u.view, id);
      }
      destroy() {
        this.off();
      }
    },
    { decorations: (v) => v.decorations },
  );
}
