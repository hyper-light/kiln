/**
 * An Agentfile's text: in the code editor in its own mode (a Dockerfile's vocabulary and Shards' own
 * directives), checked by Shards as it's typed (`POST /api/agentfiles/check`, 300 ms after typing
 * stops), what it found marked on its lines and said under it; or, read only, as it is (kiln plan
 * 01a1cd8e862e7169a074db3d5fa33f3d, step 31).
 */
import * as React from "react";
import { agentfilesApi } from "@/api/agentfiles";
import { Highlighted } from "@/components/code/highlighted";
import { Hold } from "@/components/ui/hold";
import type { Diagnostic } from "@/components/code/editor";
import { errorMessage } from "@/lib/hooks";
import { checkWords, marks, refused } from "./lint";
import "./agentfiles.css";

const CodeEditor = React.lazy(() => import("@/components/code/editor"));

/** The editor, opened with `value` (a new `opened` opens another text in it), and what Shards said of what's in it. */
export function AgentfileText({ value, opened, onChange, label }: { value: string; opened: string | number; onChange: (text: string) => void; label: string }) {
  const [said, setSaid] = React.useState<{ text: string; error: boolean } | null>(null);
  const lint = async (text: string): Promise<Diagnostic[]> => {
    try {
      const c = await agentfilesApi.check(text);
      setSaid({ text: checkWords(c), error: refused(c) });
      return marks(text, c.diagnostics);
    } catch (e) {
      // What the server says before Shards reads it (an empty text, one past 2 MiB).
      setSaid({ text: errorMessage(e), error: true });
      return [];
    }
  };
  return (
    <div className="af-editor">
      <React.Suspense fallback={<Hold height={320} label="Loading the editor" />}>
        <CodeEditor key={opened} language="agentfile" value={value} onChange={onChange} lint={lint} label={label} />
      </React.Suspense>
      <p className="af-said" data-tone={said?.error ? "error" : undefined}>
        {said?.text ?? "Shards checks it as you type."}
      </p>
    </div>
  );
}

/** A text to read, in its language's colours. */
export function AgentfileRead({ text, label }: { text: string; label: string }) {
  return (
    <pre className="af-read" aria-label={label}>
      <code>
        <Highlighted text={text} lang="agentfile" />
      </code>
    </pre>
  );
}
