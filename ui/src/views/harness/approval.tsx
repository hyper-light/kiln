/**
 * What an agent is waiting on you for: a command to run, a file change, questions, a plan.
 * Shows exactly what would happen and answers in the harness's own terms (once, always, or no
 * with a note), whatever harness asked.
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { Check, ChevronDown, ShieldAlert, X } from "lucide-react";
import type { Approval, Command } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { diffStats, relPath, tildify } from "./derive";
import { DiffStat, FileDiffView } from "./diff";
import { answersToClaude, Choices, type ChoiceQuestion } from "./choices";
import { Prose, ToolIcon } from "./parts";

export type Decide = (c: Extract<Command, { type: "approve" }>) => void;

const VERB: Partial<Record<Approval["tool"], string>> = {
  shell: "run a command",
  edit: "change files",
  web: "go online",
  mcp: "use a connected tool",
  agent: "start a subagent",
  question: "ask you",
  plan: "start on its plan",
};

/** A question, answered where it was asked: in the conversation, drawn like the agent's own words. */
function Questions({ a, onDecide, agentName, kind }: { a: Approval; onDecide: Decide; agentName?: string; kind?: string }) {
  const [busy, setBusy] = React.useState(false);
  const questions: ChoiceQuestion[] = (a.questions ?? []).map((q) => ({
    id: q.id,
    question: q.question,
    header: q.header,
    multi: q.multi,
    options: q.options.map((o) => ({ label: o.label, description: o.description, preview: o.preview })),
    other: q.other ?? true,
    secret: q.secret,
  }));
  return (
    <Choices
      layout="flow"
      questions={questions}
      agentName={agentName}
      kind={kind}
      busy={busy}
      canDecline
      onDecline={() => onDecide({ type: "approve", id: a.id, decision: "deny", message: "The user chose not to answer." })}
      onSubmit={(answers) => {
        setBusy(true);
        // Structured for current machines (commas and notes intact), the joined form for older ones.
        onDecide({ type: "approve", id: a.id, decision: "allow", choices: answers, answers: answersToClaude(answers) });
      }}
    />
  );
}

/** The full card, where the decision is made. */
/** Reasons that only restate that it's asking. */
const GENERIC = /^(this |the )?(command|tool|action|operation|edit)s? (requires?|needs?) (your )?(approval|permission)\.?$/i;

export function ApprovalCard({ a, cwd, onDecide, className, agentName, kind }: { a: Approval; cwd: string; onDecide: Decide; className?: string; agentName?: string; kind?: string }) {
  const [denying, setDenying] = React.useState(false);
  const [note, setNote] = React.useState("");
  const noteRef = React.useRef<HTMLTextAreaElement>(null);
  React.useEffect(() => {
    if (denying) noteRef.current?.focus();
  }, [denying]);
  const isQuestion = (a.questions?.length ?? 0) > 0;
  const isPlan = Boolean(a.plan);
  const stats = diffStats(a.diffs);
  if (isQuestion)
    return (
      <div className={cn("hx-approval-q", className)}>
        <Questions a={a} onDecide={onDecide} agentName={agentName} kind={kind} />
      </div>
    );

  return (
    <div className={cn("hx-approval", a.default_to_no && "hx-approval-danger", className)} role="group" aria-label={`Approval: ${a.title}`}>
      <div className="hx-approval-head">
        <span className="hx-approval-icon">{a.default_to_no ? <ShieldAlert size={14} /> : <ToolIcon kind={a.tool} size={14} />}</span>
        <span className="hx-approval-ask">
          {a.agent ? <span className="hx-dim">{a.agent} wants to </span> : <span className="hx-dim">Wants to </span>}
          {VERB[a.tool] ?? `use ${a.tool_name}`}
        </span>
        {a.diffs && a.diffs.length > 0 && <DiffStat add={stats.add} del={stats.del} className="ml-auto" />}
      </div>

      {a.command ? (
        <pre className="hx-approval-cmd">
          <code>{a.command}</code>
        </pre>
      ) : !isQuestion && !isPlan && (!a.diffs || a.diffs.length === 0) ? (
        <div className="hx-approval-title">{a.title}</div>
      ) : null}
      {a.detail && <div className="hx-dim text-[12px]">{a.detail}</div>}
      {a.reason && !GENERIC.test(a.reason.trim()) && <div className="hx-approval-reason">{a.reason}</div>}

      {a.diffs && a.diffs.length > 0 && (
        <div className="hx-stack">
          {a.diffs.map((d, i) => (
            <FileDiffView key={i} diff={d} label={relPath(d.path, cwd)} defaultOpen={(a.diffs?.length ?? 0) <= 2} />
          ))}
        </div>
      )}
      {isPlan && <Prose text={a.plan ?? ""} className="hx-plan-text" />}

      {!isQuestion && a.cwd && a.cwd !== cwd && <div className="hx-approval-where">in {tildify(a.cwd)}</div>}

      {isQuestion ? (
        <Questions a={a} onDecide={onDecide} />
      ) : denying ? (
        <div className="hx-stack">
          <textarea
            ref={noteRef}
            className="hx-input"
            rows={2}
            value={note}
            placeholder="Tell it what to do instead (optional)"
            onChange={(e) => setNote(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) onDecide({ type: "approve", id: a.id, decision: "deny", message: note.trim() || undefined });
              if (e.key === "Escape") setDenying(false);
            }}
          />
          <div className="hx-approve-actions">
            <Button size="sm" variant="danger" onClick={() => onDecide({ type: "approve", id: a.id, decision: "deny", message: note.trim() || undefined })}>
              <X /> {note.trim() ? "Deny with note" : "Deny"}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setDenying(false)}>
              Back
            </Button>
          </div>
        </div>
      ) : (
        <div className="hx-approve-actions">
          <Button size="sm" variant={a.default_to_no ? "secondary" : "primary"} onClick={() => onDecide({ type: "approve", id: a.id, decision: "allow" })}>
            <Check /> {isPlan ? "Approve plan" : "Allow"}
          </Button>
          {(a.always?.length ?? 0) > 0 && (
            <DropdownMenu.Root>
              <DropdownMenu.Trigger asChild>
                <Button size="sm" variant="secondary">
                  Always <ChevronDown />
                </Button>
              </DropdownMenu.Trigger>
              <DropdownMenu.Portal>
                <DropdownMenu.Content className="popover w-80" sideOffset={6} align="start">
                  {(a.always ?? []).map((o, i) => (
                    <DropdownMenu.Item key={i} className="menu-item" onSelect={() => onDecide({ type: "approve", id: a.id, decision: "always", always: i })}>
                      <span className="flex-1">{o.label}</span>
                      <span className="font-mono text-[10px] text-[var(--color-faint)]">{o.scope}</span>
                    </DropdownMenu.Item>
                  ))}
                </DropdownMenu.Content>
              </DropdownMenu.Portal>
            </DropdownMenu.Root>
          )}
          <Button size="sm" variant="ghost" onClick={() => setDenying(true)}>
            <X /> {isPlan ? "Keep planning" : "Deny"}
          </Button>
        </div>
      )}
    </div>
  );
}
