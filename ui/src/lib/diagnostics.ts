/**
 * The server's diagnostics (docs/ci.md W02-R08), placed in the text the code editor has: each by its
 * line and column (from 1), which every one has, as CodeMirror's lint takes them.
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
    return {
      from,
      to: Math.min(Math.max(at(d.range.end), from + 1), end),
      severity: d.severity === "error" ? "error" : d.severity === "warning" ? "warning" : "info",
      message: `${d.code}: ${d.message}`,
    };
  });
}
