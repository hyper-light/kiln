/**
 * What the Releases pages edit as YAML (a template, an app, a platform's definition): an editor
 * that marks where the YAML doesn't read, and what it reads as.
 */
import * as React from "react";
import { parseDocument, stringify } from "yaml";

import type { Diagnostic } from "@/components/code/editor";

const CodeEditor = React.lazy(() => import("@/components/code/editor"));

/** Where `text` doesn't read as YAML, and why. */
export async function yamlLint(text: string): Promise<Diagnostic[]> {
  const end = Math.max(text.length, 1);
  return parseDocument(text).errors.map((e) => {
    const from = Math.min(e.pos[0], end - 1);
    return { from, to: Math.min(Math.max(e.pos[1], from + 1), end), severity: "error" as const, message: e.message };
  });
}

/** `text` read as a YAML map: what it says, or why it isn't one. */
export function yamlMap(text: string): { value?: Record<string, unknown>; error?: string } {
  const doc = parseDocument(text);
  const first = doc.errors[0];
  if (first) return { error: first.message };
  const v: unknown = doc.toJS();
  if (v === null || v === undefined) return { value: {} };
  if (typeof v !== "object" || Array.isArray(v)) return { error: "It's a map: each setting a name, a colon and its value." };
  return { value: v as Record<string, unknown> };
}

/** A value as YAML, as people write it. */
export const toYaml = (v: unknown) => (v === undefined || v === null || (typeof v === "object" && Object.keys(v).length === 0) ? "" : stringify(v, { lineWidth: 0 }));

export function YamlEditor({ value, onChange, label }: { value: string; onChange: (text: string) => void; label: string }) {
  return (
    <div className="rel-yaml">
      <React.Suspense fallback={<pre className="rel-pre">{value}</pre>}>
        <CodeEditor value={value} onChange={onChange} lint={yamlLint} label={label} />
      </React.Suspense>
    </div>
  );
}
