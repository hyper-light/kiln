/**
 * The server's diagnostics (docs/ci.md W02-R08), placed in the text the code editor has: each by its
 * line and column (from 1), which every one has, as CodeMirror's lint takes them; one that says
 * what would fix it offers that as its quick fix (W21-R01).
 */
import type { CiDiagnostic } from "@/api/ci";
import type { Diagnostic } from "@/components/code/editor";

export function placed(content: string, diagnostics: CiDiagnostic[]): Diagnostic[] {
  const end = content.length;
  const starts = [0];
  for (let i = 0; i < content.length; i++) if (content[i] === "\n") starts.push(i + 1);
  const at = (p: { line: number; col: number }) => {
    const line = starts[Math.max(0, p.line - 1)] ?? end;
    const next = starts[p.line] ?? end + 1;
    return Math.min(line + Math.max(0, p.col - 1), next - 1, end);
  };
  return diagnostics.map((d) => {
    const from = at(d.range.start);
    const out: Diagnostic = {
      from,
      to: Math.min(Math.max(at(d.range.end), from + 1), end),
      severity: d.severity === "error" ? "error" : d.severity === "warning" ? "warning" : "info",
      message: `${d.code}: ${d.message}`,
    };
    const edits = d.fix?.edits ?? [];
    if (d.fix && edits.length) {
      // The edits are against the text as it was checked: once it has changed, the next check's
      // fix is the one to take, and this one does nothing.
      const changes = edits.map((e) => ({ from: at(e.range.start), to: Math.max(at(e.range.start), at(e.range.end)), insert: e.text })).sort((a, b) => a.from - b.from);
      out.actions = [
        {
          name: d.fix.title,
          apply: (view) => {
            if (view.state.doc.toString() === content) view.dispatch({ changes });
          },
        },
      ];
    }
    return out;
  });
}
