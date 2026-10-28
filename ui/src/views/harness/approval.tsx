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
import { experimentsApi, experimentWhereRes } from "@/api/experiments";
import { useRes } from "@/lib/data";
import { answersToClaude, Choices, type ChoiceAnswer, type ChoiceQuestion, type ChoicesProps, type ExperimentSpec } from "./choices";
import { Prose, ToolIcon } from "./parts";
import { SavePlan } from "@/views/plans/save";
import { Highlighted } from "@/components/code/highlighted";

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
function Questions({ a, onDecide, agentName, kind, agentId }: { a: Approval; onDecide: Decide; agentName?: string; kind?: string; agentId?: string }) {
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
  const answer = (answers: ChoiceAnswer[]) => {
    setBusy(true);
    // Structured for current machines (commas and notes intact), the joined form for older ones.
    onDecide({ type: "approve", id: a.id, decision: "allow", choices: answers, answers: answersToClaude(answers) });
  };
  const choices = (more: Partial<ChoicesProps>) => (
    <Choices
      layout="flow"
      questions={questions}
      agentName={agentName}
      kind={kind}
      busy={busy}
      canDecline
      onDecline={() => onDecide({ type: "approve", id: a.id, decision: "deny", message: "The user chose not to answer." })}
      onSubmit={answer}
      {...more}
    />
  );
  if (!agentId) return choices({});
  // Its question's options can be an experiment instead (docs/ci.md W20-R11), once kiln knows the
  // repository the agent works in.
  return (
    <React.Suspense fallback={choices({})}>
      <WithExperiment agentId={agentId} questions={questions} answer={answer} render={choices} />
    </React.Suspense>
  );
}

/** A question's option as an experiment's option's name: letters, digits, `-` and `_`. */
function optionName(label: string, taken: string[]): string {
  const base = (
    label
      .toLowerCase()
      .replace(/[^a-z0-9_-]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 36) || "option"
  ).replace(/^baseline$/, "baseline-option");
  let name = base;
  for (let n = 2; taken.includes(name); n++) name = `${base}-${n}`;
  return name;
}

/** The dialog's criteria, as an experiment takes them. */
function criteriaOf(spec: ExperimentSpec) {
  return spec.criteria.map((c, i) => {
    if (c.kind === "pipeline") return { name: `pipeline-${i + 1}`, kind: "pipeline", workflow: c.pipeline ?? "", required: c.required ?? true };
    if (c.kind === "command") return { name: `command-${i + 1}`, kind: "command", command: c.command ?? "", required: c.required ?? true };
    const better = c.better ?? "lower";
    const threshold = !c.threshold
      ? undefined
      : c.threshold.type === "relative"
        ? { better: c.threshold.value / 100 }
        : better === "lower"
          ? { at_most: c.threshold.value }
          : { at_least: c.threshold.value };
    return { name: c.metric ?? `metric-${i + 1}`, kind: "metric", metric: c.metric ?? "", better, required: c.required ?? false, ...(threshold && { threshold }) };
  });
}

function WithExperiment({
  agentId,
  questions,
  answer,
  render,
}: {
  agentId: string;
  questions: ChoiceQuestion[];
  answer: (answers: ChoiceAnswer[]) => void;
  render: (more: Partial<ChoicesProps>) => React.ReactElement;
}) {
  const where = useRes(experimentWhereRes(agentId)).data;
  const [error, setError] = React.useState<string | null>(null);
  if (!where?.may) return render({});
  const start = async (spec: ExperimentSpec, others: ChoiceAnswer[]) => {
    setError(null);
    const q = questions.find((x) => x.question === spec.question);
    const names: string[] = [];
    const options = spec.options.map((label) => {
      const name = optionName(label, names);
      names.push(name);
      const description = q?.options.find((o) => o.label === label)?.description;
      return { name, implement: description ? `${label}: ${description}` : label };
    });
    const byLabel = (label: string) => names[spec.options.indexOf(label)] ?? label;
    try {
      const e = await experimentsApi.start({
        repo: where.repo,
        agent: agentId,
        question: spec.question,
        options,
        strategy: spec.strategy,
        ...(spec.strategy === "custom" && { variants: spec.variants.map((v) => v.map(byLabel)) }),
        baseline: spec.baseline,
        criteria: criteriaOf(spec),
        repetitions: spec.repetitions,
        max_parallel: spec.maxParallel,
        ...(spec.budgetMinutes && { budget_minutes: spec.budgetMinutes }),
        decide: spec.decide,
      });
      answer([
        ...others,
        {
          question: spec.question,
          id: spec.questionId,
          selected: [],
          other: `Running experiment ${e.id} instead: kiln builds each option with an agent of its own and measures them. Wait for its results, which come as your next message.`,
        },
      ]);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };
  return (
    <>
      {render({ onExperiment: (spec, others) => void start(spec, others), pipelines: where.workflows })}
      {error && <p className="text-[12.5px] text-[#d1aaad]">{error}</p>}
    </>
  );
}

/** Asked to say why not: the note is ready to type in when it shows. */
const focusIt = (el: HTMLTextAreaElement | null) => el?.focus();

/** The full card, where the decision is made. */
/** Reasons that only restate that it's asking. */
const GENERIC = /^(this |the )?(command|tool|action|operation|edit)s? (requires?|needs?) (your )?(approval|permission)\.?$/i;

export function ApprovalCard({
  a,
  cwd,
  onDecide,
  className,
  agentName,
  kind,
  agentId,
}: {
  a: Approval;
  cwd: string;
  onDecide: Decide;
  className?: string;
  agentName?: string;
  kind?: string;
  agentId?: string;
}) {
  const [denying, setDenying] = React.useState(false);
  const [note, setNote] = React.useState("");
  const isQuestion = (a.questions?.length ?? 0) > 0;
  const isPlan = Boolean(a.plan);
  const stats = diffStats(a.diffs);
  if (isQuestion)
    return (
      <div className={cn("hx-approval-q", className)}>
        <Questions a={a} onDecide={onDecide} agentName={agentName} kind={kind} agentId={agentId} />
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
          <code>
            <Highlighted text={a.command} lang="bash" />
          </code>
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
      {isPlan && <SavePlan body={a.plan ?? ""} />}

      {!isQuestion && a.cwd && a.cwd !== cwd && <div className="hx-approval-where">in {tildify(a.cwd)}</div>}

      {isQuestion ? (
        <Questions a={a} onDecide={onDecide} />
      ) : denying ? (
        <div className="hx-stack">
          <textarea
            ref={focusIt}
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
