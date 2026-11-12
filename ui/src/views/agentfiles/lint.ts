/**
 * What Shards says of an Agentfile's text, as the editor shows it (kiln plan
 * 01a1cd8e862e7169a074db3d5fa33f3d, step 31; docs/api.md, "Agentfiles"): each thing it found marked
 * on its lines (Shards' ranges name whole lines), and a line under the editor in words.
 */
import type { Diagnostic } from "@codemirror/lint";
import type { AgentfileCheck, AgentfileDiagnostic, AgentfileSaved } from "@/api/wire";

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;

/** Why Shards didn't check it. */
export function uncheckedWhy(c: AgentfileCheck): string {
  const u = c.unchecked;
  if (u?.reason === "no_shards") return "this server has no Shards to check it with";
  if (u?.reason === "timed_out") return "Shards didn't answer in time";
  if (u?.reason === "failed") return `Shards couldn't check it: ${u.said}`;
  return "Shards didn't say why";
}

/** The line under the editor: what Shards found, or why it didn't look. */
export function checkWords(c: AgentfileCheck): string {
  if (!c.checked) return `Not checked: ${uncheckedWhy(c)}.`;
  const errors = c.diagnostics.filter((d) => d.severity === "error").length;
  const warnings = c.diagnostics.length - errors;
  if (errors + warnings === 0) return "Shards finds no problems.";
  return [errors > 0 && plural(errors, "error"), warnings > 0 && plural(warnings, "warning")].filter(Boolean).join(", ") + ".";
}

/** What a save kept (`was`: the version it was), and what Shards said of its text. */
export function keptWords(saved: AgentfileSaved, was: number): string {
  const v = saved.agentfile.version;
  return v > was ? `Kept as v${v}. ${checkWords(saved.check)}` : "Renamed: a new name alone is no new version.";
}

/** What making `version` current again kept. */
export function restoredWords(saved: AgentfileSaved, version: number): string {
  return `v${version}'s text is current again, as v${saved.agentfile.version}. ${checkWords(saved.check)}`;
}

/** Whether Shards refuses it: an error among what it found. */
export const refused = (c: AgentfileCheck) => c.diagnostics.some((d) => d.severity === "error");

/** Each one Shards found, on its lines of `text` (from 1) as the editor marks them. */
export function marks(text: string, found: readonly AgentfileDiagnostic[]): Diagnostic[] {
  const starts = [0];
  for (let i = 0; i < text.length; i++) if (text[i] === "\n") starts.push(i + 1);
  const from = (line: number) => starts[Math.min(Math.max(line, 1), starts.length) - 1] ?? 0;
  const to = (line: number) => {
    const next = starts[Math.min(Math.max(line, 1), starts.length)];
    return next === undefined ? text.length : next - 1;
  };
  return found.map((d) => {
    const at = from(d.range.start.line);
    return {
      from: at,
      to: Math.max(at, to(Math.max(d.range.end.line, d.range.start.line))),
      severity: d.severity === "error" ? "error" : "warning",
      message: d.code === "shards" ? d.message : `${d.code}: ${d.message}`,
    };
  });
}
