/**
 * Branching a subagent, under its thread's crumbs (no popup): what changes, then a Variant (a
 * subagent beside it, under the same agent, from its conversation so far) or a New agent (one of
 * its own, the same way). `Command::Branch`: to a subagent kiln started, its session; to one of its
 * harness's own, by its task, its agent's session.
 */
import * as React from "react";
import { ArrowUpRight, GitFork } from "lucide-react";
import { clientId, harnessApi } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import type { SubagentNode } from "./subagent-tree";

export function BranchPanel({ n, can, onDone }: { n: Pick<SubagentNode, "kind" | "session" | "task" | "name">; can: boolean; onDone: () => void }) {
  const [words, setWords] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const go = (top: boolean) => {
    const text = words.trim();
    if (!text) return;
    setBusy(true);
    harnessApi
      .command(n.session, { type: "branch", words: text, top, ...(n.kind === "harness" && n.task ? { task: n.task } : {}) })
      .then(() => {
        setWords("");
        onDone();
      })
      .catch((e: unknown) => toast.error(`Couldn't branch ${n.name}`, errorMessage(e)))
      .finally(() => setBusy(false));
  };
  const off = !can || busy || words.trim() === "";
  return (
    <div className="hx-branch" role="group" aria-label={`Branch ${n.name}`}>
      <input
        value={words}
        placeholder="What changes"
        aria-label="What changes"
        disabled={!can || busy}
        onChange={(e) => setWords(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") go(false);
        }}
      />
      <Button size="xs" variant="secondary" disabled={off} onClick={() => go(false)}>
        <GitFork size={13} aria-hidden="true" />
        Variant
      </Button>
      <Button size="xs" variant="secondary" disabled={off} onClick={() => go(true)}>
        <ArrowUpRight size={13} aria-hidden="true" />
        New agent
      </Button>
    </div>
  );
}

/** Keep, on a finished variant: its parent is told to use its result (subagent-tree.ts `keepOf`); then it reads Kept. */
export function KeepButton({ keep }: { keep: { to: string; text: string } }) {
  const [state, setState] = React.useState<"keep" | "sending" | "kept">("keep");
  const go = () => {
    setState("sending");
    harnessApi.command(keep.to, { type: "send", id: clientId(), text: keep.text, mode: "queue" }).then(
      () => setState("kept"),
      (e: unknown) => {
        setState("keep");
        toast.error("Couldn't tell its parent", errorMessage(e));
      },
    );
  };
  return (
    <Button size="xs" variant="secondary" className="ml-auto" disabled={state !== "keep"} onClick={go}>
      {state === "kept" ? "Kept" : "Keep"}
    </Button>
  );
}
