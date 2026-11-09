/**
 * A plan waiting on you, reviewed where its agent is: at the top of its Plan tab, beside its
 * conversation (docs/plans.md, "Reviewing a plan"). Two kinds come here: a plan its goal stack waits
 * on (`plans_ready` with `then: review`), and one it proposed in plan mode, its harness asking. Both
 * read the same way: where it came from, its steps (each opening in place to what to do, when it's
 * done, how it's tested and what it rests on), what its research comes to, and one answer of three:
 * Approve, Reject, or Other… in your own words.
 *
 * However much research a plan has, the review shows a fixed few lines of it up front: its counts,
 * and the few findings its steps rest on that are worth a look before answering (weak, contested,
 * or quoted where kiln couldn't find the words), worst first. Every finding is there on asking, a
 * line each, one opened at a time. Drawn flat: hairlines, a square tint for what's open or picked,
 * and the needs-you edge Mission control's rows have.
 */
import * as React from "react";
import { ChevronRight, MessageSquarePlus } from "lucide-react";
import { clientId, useSession, type Approval, type Command, type SessionStore } from "@/api/harness";
import { plansApi, type Plan, type PlanItem } from "@/api/plans";
import { researchRes, reviewPlanRes } from "@/api/plan-resources";
import { SOURCE_WORDS, type CheckState, type Finding, type Grade, type Research, type ResearchSource } from "@/api/research";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { toast } from "@/components/ui/toast";
import { useAction } from "@/lib/action";
import { harnessSessionsRes, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { Tries } from "@/lib/tries";
import { cn } from "@/lib/utils";
import { stillAsked, useAnswered as useRequestsAnswered } from "@/views/harness/answered";
import { agentLabel } from "@/views/harness/derive";
import { useClock } from "@/views/harness/dom";
import { useGoalStack } from "@/views/harness/goals";
import { Ago } from "@/views/harness/parts";
import { Prose } from "@/views/harness/prose";
import { Optic } from "@/views/harness/signals";
import { TitleText } from "./graph";
import { effectiveNeeds, headline, rounds, type GraphStep } from "./graph-layout";
import { REVIEW_ATTR } from "./reveal";
import { allLooks, asItems, changesSince, changeWords, commentLine, digest, FLAG_WORDS, headingOf, introOf, LONG, restsOn, runsOf, sameWords, stepBasis, stepsWord, type Look } from "./review-model";
import { noteAnswered, RECEIPT_MS, useAnswered, useDraft, type Answer, type Draft } from "./reviewed";
import { criterionSentence, planSteps, stepMarkdown, testSentence } from "./steps";
import "../research/research.css";
import "./review.css";

/** Of a list this long, this many show; the rest on asking. */
const FEW = 3;

/**
 * What waits on your review of agent `agent`'s plans, first in its Plan tab: a plan it proposed (its
 * harness asking), else the plan its goal stack waits on; with neither, what you last answered here,
 * for a while. `send`: its commands (none: a plan proposed is answered in its conversation).
 */
export function ReviewHere({ agent, store, send }: { agent: string; store: SessionStore; send?: (c: Command) => void }) {
  // As the agents list has it (kept up to date by the server's events; its socket says it only as it opens).
  const held = useRes(harnessSessionsRes).data.find((a) => a.id === agent)?.held;
  const asked = useSession(store, (s) => s.state?.approvals);
  const done = useRequestsAnswered();
  const name = useSession(store, (s) => (s.info ? agentLabel(s.info) : "the agent"));
  const harness = useSession(store, (s) => s.info?.kind_name);
  // Its own (a subagent's is answered in its thread).
  const proposed = send && asked ? stillAsked(asked, done).find((a) => a.tool === "plan" && !a.agent && a.plan) : undefined;
  if (proposed && send)
    return (
      <Frame>
        <ProposedReview key={proposed.id} agent={agent} name={name} harness={harness} a={proposed} send={send} />
      </Frame>
    );
  if (held?.plan)
    return (
      <Frame>
        <GoalReview key={`${held.plan}/${held.since}`} agent={agent} name={name} plan={held.plan} since={held.since} />
      </Frame>
    );
  return <Receipt agent={agent} name={name} />;
}

/** What waits on your review of `store`'s agent's plans, if anything (its Plan tab says so): a plan it proposed, or one its goals wait on. */
export function useReviewWaits(store: SessionStore): "proposed" | "goal" | null {
  const held = Boolean(useRes(harnessSessionsRes).data.find((a) => a.id === store.id)?.held?.plan);
  const asked = useSession(store, (s) => s.state?.approvals);
  const done = useRequestsAnswered();
  if (asked && stillAsked(asked, done).some((a) => a.tool === "plan" && !a.agent && Boolean(a.plan))) return "proposed";
  return held ? "goal" : null;
}

function Frame({ children }: { children: React.ReactNode }) {
  return (
    <section className="pr" {...{ [REVIEW_ATTR]: "" }} aria-label="A plan to review">
      <Loadable label="the plan to review" fallback={<div className="pr-hold" aria-busy="true" />}>
        {children}
      </Loadable>
    </section>
  );
}

/* ------------------------------------------------------------------ the two kinds */

/** A plan it proposed in plan mode, its harness asking: answered as its harness takes it (allow; deny, with your words or none). */
function ProposedReview({ agent, name, harness, a, send }: { agent: string; name: string; harness?: string; a: Approval; send: (c: Command) => void }) {
  const words = a.plan ?? "";
  // Kept by kiln within moments of its proposing it (its versions say what changed since you answered).
  const kept = useSuspenseResource(`plans/proposed/${agent}/${words.length}`, (s) => plansApi.proposedBy(agent, s), { pollMs: 5000 }).data[0];
  const fresh = kept && kept.body.trim() === words.trim() ? kept : undefined;
  const items = fresh ? asItems(fresh.items, fresh.steps) : asItems(undefined, planSteps(words));
  const title = headingOf(words) ?? fresh?.title ?? a.title ?? "Its plan";
  const last = useAnswered(agent);
  const since = last?.kind === "proposed" && last.answer === "other" && fresh && last.plan === fresh.id && last.version !== null && last.version < fresh.version ? last : undefined;
  const always = a.always?.[0];
  const draftKey = `${agent}/${a.id}`;
  const answer = (pick: Answer, text: string, from: boolean): Promise<boolean> => {
    const said = text.trim();
    send(
      pick === "approve" && from && always
        ? { type: "approve", id: a.id, decision: "always", always: 0 }
        : { type: "approve", id: a.id, decision: pick === "approve" ? "allow" : "deny", message: pick === "other" ? said : undefined },
    );
    noteAnswered(agent, draftKey, { kind: "proposed", answer: pick, words: said, title, version: fresh?.version ?? null, plan: fresh?.id ?? null, steps: items.length, at: Date.now() });
    return Promise.resolve(true);
  };
  return (
    <ReviewBody
      kind="proposed"
      agent={agent}
      name={name}
      title={title}
      when={
        <>
          {since ? "proposed again" : "plan mode"} · <Ago ts={Math.floor(a.at / 1000)} />
        </>
      }
      from={
        <>
          Proposed by {name}
          {harness ? ` (${harness})` : ""} in plan mode: it waits for your answer
        </>
      }
      intro={introOf(words)}
      delta={since && fresh ? <Changed plan={fresh.id} from={since.version ?? 0} to={fresh.version} items={items} asked={since.words} /> : null}
      items={items}
      stepWords={(n) => stepMarkdown(words, n)}
      research={null}
      draftKey={draftKey}
      always={always ? `${always.label} (${always.scope})` : undefined}
      descs={{
        approve: "It leaves plan mode and starts on it.",
        reject: "It stops there and waits for you. Nothing has changed yet.",
        other: "In your own words: it stays in plan mode, takes them in, and proposes again.",
      }}
      onAnswer={answer}
    />
  );
}

/** The plan agent `agent`'s goal stack waits on (held since `since`): answered by kiln (`plans.review`). */
function GoalReview({ agent, name, plan: id, since }: { agent: string; name: string; plan: string; since: number }) {
  // Read once a review begins (held again once it comes back changed): a plan waiting on you holds still.
  const p = useRes(reviewPlanRes(id)).data;
  if (p === "gone") return <p className="pr-none">The plan its goals wait on is gone: resume them from the goal stack below.</p>;
  if (!p.research) return <GoalReviewOf agent={agent} name={name} p={p} r={null} since={since} />;
  return <WithResearch id={p.research} render={(r) => <GoalReviewOf agent={agent} name={name} p={p} r={r} since={since} />} />;
}

function WithResearch({ id, render }: { id: string; render: (r: Research | null) => React.ReactNode }) {
  const r = useRes(researchRes(id)).data;
  return <>{render(r === "gone" ? null : r)}</>;
}

function GoalReviewOf({ agent, name, p, r, since }: { agent: string; name: string; p: Plan; r: Research | null; since: number }) {
  const stack = useGoalStack(agent);
  const [tries] = React.useState(() => new Tries(clientId));
  const [goalOpen, setGoalOpen] = React.useState(false);
  const items = asItems(p.items, p.steps);
  const goalId = p.source?.goal?.id;
  const tops = stack?.items.filter((i) => !i.part_of) ?? [];
  const at = goalId ? tops.findIndex((i) => i.id === goalId) : -1;
  const goal = at >= 0 ? at + 1 : undefined;
  const goalWord = goal ? `goal ${goal}` : "its goal";
  const back = p.reviewed?.answer === "revise" && p.reviewed.version < p.version ? p.reviewed : undefined;
  const draftKey = `${agent}/${p.id}/v${p.version}`;
  const answer = async (pick: Answer, text: string): Promise<boolean> => {
    const said = text.trim();
    const ask = { answer: pick === "other" ? ("revise" as const) : pick, words: pick === "other" ? said : undefined, version: p.version };
    try {
      await plansApi.review(p.id, { ...ask, key: tries.keyFor(JSON.stringify([p.id, ask])) });
      tries.done();
      noteAnswered(agent, draftKey, { kind: "goal", answer: pick, words: said, title: p.title, version: p.version, plan: p.id, steps: items.length, goal, at: Date.now() });
      invalidate(`plans/${p.id}`);
      invalidate(`harness/goals/${agent}`);
      return true;
    } catch (e) {
      toast.error(`Couldn't answer ${name}'s plan`, errorMessage(e));
      return false;
    }
  };
  return (
    <ReviewBody
      kind="goal"
      agent={agent}
      name={name}
      title={p.title}
      when={
        <>
          v{p.version} · <Ago ts={Math.floor(since / 1000)} />
        </>
      }
      from={
        <>
          Made from {goalWord} of its goal stack, which waits for you
          {p.source?.goal?.text && (
            <>
              {" · "}
              <button type="button" className="pr-link" aria-expanded={goalOpen} onClick={() => setGoalOpen(!goalOpen)}>
                {goalOpen ? "hide the goal" : "the goal"}
              </button>
            </>
          )}
        </>
      }
      goalText={goalOpen ? p.source?.goal?.text : undefined}
      delta={back ? <Changed plan={p.id} from={back.version} to={p.version} items={items} asked={back.words ?? ""} /> : null}
      items={items}
      research={r}
      draftKey={draftKey}
      descs={{
        approve: `Its ${items.length} step${items.length === 1 ? "" : "s"} go to ${name} now, as ${goalWord}'s parts.`,
        reject: `Its steps never run and ${goalWord} is skipped. The rest of the stack waits for you.`,
        other: `In your own words: ${name} changes the plan, and it comes back to you.`,
      }}
      onAnswer={answer}
    />
  );
}

/* ------------------------------------------------------------------ changed since you looked */

/** What changed since you answered Other… on version `from` (now `to`), and what you asked. */
function Changed(props: { plan: string; from: number; to: number; items: PlanItem[]; asked: string }) {
  return (
    <Loadable label="what changed" fallback={null}>
      <ChangedOf {...props} />
    </Loadable>
  );
}

function ChangedOf({ plan, from, to, items, asked }: { plan: string; from: number; to: number; items: PlanItem[]; asked: string }) {
  const [all, setAll] = React.useState(false);
  const versions = useSuspenseResource(`plans/versions/${plan}/${to}`, (s) => plansApi.versions(plan, s)).data;
  const before = versions.find((v) => v.version === from);
  const changes = before ? changesSince(asItems(before.items, planSteps(before.body)), items) : null;
  const lines = changes ? [...changes.list.map(changeWords), sameWords(changes.same)].filter(Boolean) : [];
  const shown = all ? lines : lines.slice(0, 5);
  return (
    <div className="pr-delta">
      <span className="pr-label">Changed since you looked</span>
      {asked && (
        <p className="pr-asked">
          You asked: <q>{asked}</q>
        </p>
      )}
      {shown.length > 0 && (
        <ul className="pr-delta-list">
          {shown.map((l, i) => (
            <li key={i}>{l}</li>
          ))}
        </ul>
      )}
      {lines.length > 5 && (
        <button type="button" className="pr-more" aria-expanded={all} onClick={() => setAll(!all)}>
          {all ? "Fewer" : `${lines.length - 5} more`}
        </button>
      )}
    </div>
  );
}

/* ------------------------------------------------------------------ the review */

interface BodyProps {
  kind: "goal" | "proposed";
  agent: string;
  name: string;
  title: string;
  when: React.ReactNode;
  from: React.ReactNode;
  goalText?: string;
  /** A plan proposed: its words before its steps. */
  intro?: string;
  delta: React.ReactNode;
  items: PlanItem[];
  /** A plan proposed: step `n` as its words have it. */
  stepWords?: (n: number) => string;
  research: Research | null;
  draftKey: string;
  /** What its harness offers to allow from now on with an approval. */
  always?: string;
  descs: Record<Answer, string>;
  /** Send it: resolves true once it went (`from`: and the harness's from-now-on with it). */
  onAnswer: (pick: Answer, words: string, from: boolean) => Promise<boolean>;
}

function ReviewBody(b: BodyProps) {
  const [draft, setDraft] = useDraft(b.draftKey);
  const [open, setOpen] = React.useState<readonly number[]>([]);
  // One finding open at a time, wherever it's listed ("step:2:f5", "rs:f5").
  const [finding, setFinding] = React.useState<string | null>(null);
  const [from, setFrom] = React.useState(false);
  // Once it went, it stands as sent until its review goes; one that didn't (`onAnswer` says why) can be sent again.
  const [went, setWent] = React.useState(false);
  const [answer, answering] = useAction(async (pick: Answer) => setWent(await b.onAnswer(pick, draft.words, from)));
  const busy = answering || went;
  const words = React.useRef<HTMLTextAreaElement>(null);
  const rests = React.useMemo(() => restsOn(b.items), [b.items]);
  const toggle = (n: number) => setOpen(open.includes(n) ? open.filter((x) => x !== n) : [...open, n]);
  const ready = draft.pick !== null && (draft.pick !== "other" || draft.words.trim().length > 0);
  const send = () => {
    if (!busy && ready && draft.pick) answer(draft.pick);
  };
  const pick = (v: Answer, d: Draft = draft) => {
    setDraft({ ...d, pick: v });
    if (v === "other") requestAnimationFrame(() => words.current?.focus());
  };
  const comment = (n: number) => {
    const it = b.items[n - 1];
    setDraft({ pick: "other", words: commentLine(draft.words, n, it?.title ?? "") });
    requestAnimationFrame(() => {
      const t = words.current;
      if (!t) return;
      t.focus({ preventScroll: true });
      t.setSelectionRange(t.value.length, t.value.length);
      t.scrollIntoView({ block: "nearest", behavior: "smooth" });
    });
  };
  const onKeyDown = (e: React.KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault();
      send();
      return;
    }
    const t = e.target as HTMLElement;
    if (e.metaKey || e.ctrlKey || e.altKey || t.closest("textarea, input")) return;
    const v = ({ "1": "approve", "2": "reject", "3": "other" } as const)[e.key as "1" | "2" | "3"];
    if (v) {
      e.preventDefault();
      pick(v);
    }
  };
  const label = !draft.pick ? "Choose an answer" : draft.pick === "approve" ? "Approve plan" : draft.pick === "reject" ? "Reject plan" : `Send to ${b.name}`;
  return (
    <div className="pr-body" onKeyDown={onKeyDown}>
      <div className="pr-top">
        <Optic state="waiting" />
        <span className="pr-eye">Needs you · {b.kind === "goal" ? "a plan to review" : "a proposed plan"}</span>
        <span className="pr-when">{b.when}</span>
      </div>
      <h3 className="pr-title">
        <TitleText parts={headline(b.title, 200)} />
      </h3>
      <p className="pr-from">{b.from}</p>
      {b.goalText && <p className="pr-goal">{b.goalText}</p>}
      {b.intro && <Clamp text={b.intro} className="pr-intro" />}
      {b.delta}
      <Steps b={b} open={open} onToggle={toggle} rests={rests} finding={finding} onFinding={setFinding} onComment={comment} />
      {b.research && <Digest items={b.items} r={b.research} finding={finding} onFinding={setFinding} />}
      <div className="pr-answer">
        <span className="pr-label">Your answer</span>
        <div className="pr-opts" role="radiogroup" aria-label="Your answer">
          {(["approve", "reject", "other"] as const).map((v, i) => {
            const on = draft.pick === v;
            return (
              <div key={v} className="pr-opt" data-v={v} data-on={on || undefined}>
                <button type="button" role="radio" aria-checked={on} className="pr-row" disabled={busy} onClick={() => pick(v)}>
                  <span className="pr-mark" aria-hidden="true" />
                  <span>
                    <span className="pr-opt-label">{v === "approve" ? "Approve" : v === "reject" ? "Reject" : "Other…"}</span>
                    <span className="pr-opt-desc">{v === "other" && on ? b.descs.other.replace(/^In your own words: /, "").replace(/^./, (c) => c.toUpperCase()) : b.descs[v]}</span>
                  </span>
                  <kbd className="pr-key">{i + 1}</kbd>
                </button>
                {on && v === "other" && (
                  <div className="pr-other">
                    <textarea
                      ref={words}
                      className="hx-input pr-words"
                      rows={3}
                      value={draft.words}
                      disabled={busy}
                      aria-label="What should change"
                      placeholder={b.kind === "goal" ? "What should change? “Step 2 first”, “drop step 4”, “use the existing ReviewFiles”…" : "What should change? “Write the failing tests first”…"}
                      onChange={(e) => setDraft({ pick: "other", words: e.target.value })}
                    />
                  </div>
                )}
                {on && v === "approve" && b.always && (
                  <label className="pr-always">
                    <input type="checkbox" className="check" checked={from} disabled={busy} onChange={(e) => setFrom(e.target.checked)} />
                    <span>And from now on: {b.always}</span>
                  </label>
                )}
              </div>
            );
          })}
        </div>
        <div className="pr-foot">
          <p className="pr-keys" aria-hidden="true">
            <span>
              <kbd>1</kbd>
              <kbd>2</kbd>
              <kbd>3</kbd> choose
            </span>
            <span>
              <kbd>⌘</kbd>
              <kbd>↵</kbd> send
            </span>
          </p>
          <Button size="sm" variant={draft.pick === "reject" ? "danger" : "primary"} className="pr-send" disabled={!busy && !ready} pending={busy} onClick={send}>
            {busy ? "Sending…" : label}
          </Button>
        </div>
      </div>
    </div>
  );
}

/** Long words: their first lines, the rest on asking. */
function Clamp({ text, className }: { text: string; className?: string }) {
  const [all, setAll] = React.useState(false);
  const long = text.length > LONG;
  return (
    <div className={className}>
      <div className={cn(long && !all && "pr-clamp")}>
        <Prose text={text} className="pr-prose" />
      </div>
      {long && (
        <button type="button" className="pr-more" aria-expanded={all} onClick={() => setAll(!all)}>
          {all ? "Show less" : "Show all"}
        </button>
      )}
    </div>
  );
}

/* ------------------------------------------------------------------ steps */

function Steps({
  b,
  open,
  onToggle,
  rests,
  finding,
  onFinding,
  onComment,
}: {
  b: BodyProps;
  open: readonly number[];
  onToggle: (n: number) => void;
  rests: Map<string, number[]>;
  finding: string | null;
  onFinding: (k: string | null) => void;
  onComment: (n: number) => void;
}) {
  const needs = effectiveNeeds(b.items.map((it) => it.needs));
  const graph: GraphStep[] = b.items.map((it, i) => ({ n: i + 1, title: it.title || "Untitled step", needs: needs[i] ?? [] }));
  const grouped = rounds(graph);
  const side = grouped.filter((r) => r.length > 1);
  const row = (s: GraphStep, shared: string) => {
    const it = b.items[s.n - 1];
    if (!it) return null;
    const said = s.needs.join(",");
    const after = s.needs.length > 0 && said !== shared && !(s.needs.length === 1 && s.needs[0] === s.n - 1) ? `after ${s.needs.join(", ")}` : "";
    return (
      <Step
        key={s.n}
        n={s.n}
        item={it}
        after={after}
        open={open.includes(s.n)}
        onToggle={() => onToggle(s.n)}
        b={b}
        rests={rests}
        finding={finding}
        onFinding={onFinding}
        onComment={() => onComment(s.n)}
      />
    );
  };
  return (
    <div>
      <span className="pr-label">
        {b.items.length} step{b.items.length === 1 ? "" : "s"}
        {b.items.length > 1 && <span>· {side.length ? `${side.map((r) => runsOf(r.map((s) => s.n))).join("; ")} side by side` : "one after another"}</span>}
      </span>
      <ol className="pr-steps">
        {grouped.map((r) => {
          if (r.length === 1) return r[0] ? row(r[0], "") : null;
          const all = r.map((s) => s.needs.join(","));
          const shared = all.every((x) => x === all[0]) ? (all[0] ?? "") : "";
          return (
            <li key={`r${r[0]?.n}`} className="pr-round">
              <span className="pr-round-label">
                {r.length} at once{shared ? ` · after ${shared.split(",").join(", ")}` : ""}
              </span>
              <ol>{r.map((s) => row(s, shared))}</ol>
            </li>
          );
        })}
      </ol>
    </div>
  );
}

function Step({
  n,
  item,
  after,
  open,
  onToggle,
  b,
  rests,
  finding,
  onFinding,
  onComment,
}: {
  n: number;
  item: PlanItem;
  after: string;
  open: boolean;
  onToggle: () => void;
  b: BodyProps;
  rests: Map<string, number[]>;
  finding: string | null;
  onFinding: (k: string | null) => void;
  onComment: () => void;
}) {
  const id = React.useId();
  const basis = b.research ? stepBasis(item, b.research, rests) : null;
  const worst = basis?.flagged[0];
  return (
    <li className="pr-step" data-open={open || undefined}>
      <button type="button" className="pr-step-head" aria-expanded={open} aria-controls={id} onClick={onToggle}>
        <span className="pr-n">{n}</span>
        <span className="pr-step-title">
          <TitleText parts={headline(item.title || "Untitled step")} />
          {after && <span className="pr-after">{after}</span>}
        </span>
        <span className="pr-step-mark">{worst && <GradeChip f={worst.f} label={`rests on ${worst.f.id}, ${worst.flags.map((f) => FLAG_WORDS[f]).join(" and ")}`} />}</span>
        <ChevronRight size={13} className="pr-chev" aria-hidden="true" />
      </button>
      <div id={id} hidden={!open}>
        {open && <StepDetail n={n} item={item} b={b} basis={basis} finding={finding} onFinding={onFinding} onComment={onComment} />}
      </div>
    </li>
  );
}

function StepDetail({
  n,
  item,
  b,
  basis,
  finding,
  onFinding,
  onComment,
}: {
  n: number;
  item: PlanItem;
  b: BodyProps;
  basis: ReturnType<typeof stepBasis> | null;
  finding: string | null;
  onFinding: (k: string | null) => void;
  onComment: () => void;
}) {
  const words = b.stepWords?.(n);
  return (
    <div className="pr-detail">
      {words !== undefined ? (
        words.trim() && <Clamp text={words} className="pr-v" />
      ) : (
        <>
          {item.description.trim() && (
            <div>
              <span className="pr-k">What to do</span>
              <Clamp text={item.description} className="pr-v" />
            </div>
          )}
          {item.examples.trim() && (
            <div>
              <span className="pr-k">Examples</span>
              <Clamp text={item.examples} className="pr-v" />
            </div>
          )}
          {item.acceptance.length > 0 && <Few label="Done when" lines={item.acceptance.map(criterionSentence)} />}
          {item.testing.length > 0 && <Few label="Tested by" lines={item.testing.map(testSentence)} />}
        </>
      )}
      {b.research && basis && <Basis n={n} item={item} basis={basis} sources={b.research.sources} finding={finding} onFinding={onFinding} />}
      <div>
        <Button size="xs" variant="secondary" onClick={onComment}>
          <MessageSquarePlus aria-hidden="true" /> Comment on step {n}
        </Button>
      </div>
    </div>
  );
}

/** A list, its first few lines; the rest on asking. */
function Few({ label, lines }: { label: string; lines: string[] }) {
  const [all, setAll] = React.useState(false);
  const shown = all ? lines : lines.slice(0, FEW + 1);
  return (
    <div>
      <span className="pr-k">{label}</span>
      <ul className="pr-list">
        {shown.map((l, i) => (
          <li key={i}>{l}</li>
        ))}
      </ul>
      {lines.length > FEW + 1 && (
        <button type="button" className="pr-more" aria-expanded={all} onClick={() => setAll(!all)}>
          {all ? "Fewer" : `${lines.length - shown.length} more`}
        </button>
      )}
    </div>
  );
}

/** What a step rests on: how many and how they're graded, those worth a look; every one on asking. */
function Basis({
  n,
  item,
  basis,
  sources,
  finding,
  onFinding,
}: {
  n: number;
  item: PlanItem;
  basis: ReturnType<typeof stepBasis>;
  sources: ResearchSource[];
  finding: string | null;
  onFinding: (k: string | null) => void;
}) {
  const [all, setAll] = React.useState(false);
  if (!(item.basis ?? []).length) return <p className="pr-warn">It rests on no finding of its research.</p>;
  const many = basis.all.length > FEW;
  const shown = !many || all ? basis.all : basis.flagged.slice(0, FEW);
  return (
    <div>
      <span className="pr-k">
        Rests on {basis.all.length} finding{basis.all.length === 1 ? "" : "s"} <Grades counts={basis.grades} />
      </span>
      {many && !all && basis.flagged.length === 0 && <p className="pr-dim">None of them weak, contested, or quoted where kiln couldn't find the words.</p>}
      {shown.length > 0 && <Findings looks={shown} sources={sources} at={`step:${n}`} finding={finding} onFinding={onFinding} />}
      {many && (
        <button type="button" className="pr-more" aria-expanded={all} onClick={() => setAll(!all)}>
          {all ? "Fewer" : `All ${basis.all.length}`}
        </button>
      )}
      {basis.missing.length > 0 && (
        <p className="pr-warn">
          {basis.missing.join(", ")} {basis.missing.length === 1 ? "isn't a finding" : "aren't findings"} of its research.
        </p>
      )}
    </div>
  );
}

/* ------------------------------------------------------------------ research */

/** Grades as kiln draws them (research.css), with how many of each. */
function Grades({ counts }: { counts: [Grade, number][] }) {
  return (
    <span className="pr-grades">
      {counts.map(([g, k]) => (
        <span key={g} className="rs-grade rs-grade-sm">
          <b data-g={g}>{g}</b>
          {k}
        </span>
      ))}
    </span>
  );
}

function GradeChip({ f, label }: { f: Finding; label?: string }) {
  return (
    <span className="rs-grade rs-grade-sm" title={f.confidence.why} aria-label={label ?? `graded ${f.confidence.grade}, ${f.confidence.score} of 100`}>
      <b data-g={f.confidence.grade}>{f.confidence.grade}</b>
    </span>
  );
}

/** What a plan's research comes to, in a few lines: its counts, the findings worth a look first; every finding on asking. */
function Digest({ items, r, finding, onFinding }: { items: PlanItem[]; r: Research; finding: string | null; onFinding: (k: string | null) => void }) {
  const [more, setMore] = React.useState(false);
  const [all, setAll] = React.useState(false);
  const d = digest(items, r);
  const look = more ? d.look : d.look.slice(0, FEW);
  return (
    <div className="pr-rs">
      <span className="pr-label">
        Research
        <span>
          · {d.findings} finding{d.findings === 1 ? "" : "s"} · {d.sources} source{d.sources === 1 ? "" : "s"}, {d.inside} inside, {d.outside} outside
        </span>
      </span>
      <Grades counts={d.grades} />
      {d.look.length > 0 ? (
        <>
          <p className="pr-sub">
            To look at before answering <span>· {d.look.length}</span>
          </p>
          <Findings looks={look} sources={r.sources} at="rs" finding={finding} onFinding={onFinding} />
          {d.look.length > FEW && (
            <button type="button" className="pr-more" aria-expanded={more} onClick={() => setMore(!more)}>
              {more ? "Fewer" : `${d.look.length - FEW} more to look at`}
            </button>
          )}
        </>
      ) : (
        <p className="pr-dim">
          Nothing to look at: no finding its steps rest on is weak, contested, or quoted where kiln couldn't find the words.
          {d.least && (
            <>
              {" "}
              Least sure: <GradeChip f={d.least.f} /> {d.least.f.id} · {d.least.f.confidence.score}, which {stepsWord(d.least.steps)} {d.least.steps.length === 1 ? "rests" : "rest"} on.
            </>
          )}
        </p>
      )}
      {d.bare.length > 0 && (
        <p className="pr-warn">
          {stepsWord(d.bare).replace(/^s/, "S")} {d.bare.length === 1 ? "rests" : "rest"} on no finding.
        </p>
      )}
      {d.checking > 0 && (
        <p className="pr-dim">
          kiln is still looking for {d.checking} quote{d.checking === 1 ? "" : "s"} where {d.checking === 1 ? "it's" : "they're"} cited: grades may change.
        </p>
      )}
      {/* The rest, a line each: those worth a look are above already. */}
      <button type="button" className="pr-more" aria-expanded={all} onClick={() => setAll(!all)}>
        {all ? "Hide them" : d.look.length ? `The other ${d.findings - d.look.length} findings` : `All ${d.findings} findings`}
      </button>
      {all && <AllFindings items={items} r={r} skip={d.look} finding={finding} onFinding={onFinding} />}
    </div>
  );
}

/** Every finding but those listed already (`skip`), a line each: those its steps rest on first (worst first), those none does on asking. */
function AllFindings({ items, r, skip, finding, onFinding }: { items: PlanItem[]; r: Research; skip: Look[]; finding: string | null; onFinding: (k: string | null) => void }) {
  const [rest, setRest] = React.useState(false);
  const shown = new Set(skip.map((l) => l.f.id));
  const all = allLooks(items, r);
  const used = all.used.filter((l) => !shown.has(l.f.id));
  const unused = all.unused.filter((l) => !shown.has(l.f.id));
  return (
    <div className="pr-all">
      {used.length > 0 && <Findings looks={used} sources={r.sources} at="all" finding={finding} onFinding={onFinding} />}
      {unused.length > 0 && (
        <>
          <button type="button" className="pr-more" aria-expanded={rest} onClick={() => setRest(!rest)}>
            {rest ? `Hide the ${unused.length} no step rests on` : `${unused.length} no step rests on`}
          </button>
          {rest && <Findings looks={unused} sources={r.sources} at="unused" finding={finding} onFinding={onFinding} />}
        </>
      )}
    </div>
  );
}

const CHECK_WORDS: Record<CheckState, string> = {
  found: "quote found",
  not_found: "quote not found there",
  gone: "not there",
  exists: "there",
  pending: "checking",
  unchecked: "not checked",
};

/** Findings, a line each: its grade, id and claim (two lines at most), why it's worth a look and who rests on it; one opened at a time. */
function Findings({ looks, sources, at, finding, onFinding }: { looks: Look[]; sources: ResearchSource[]; at: string; finding: string | null; onFinding: (k: string | null) => void }) {
  return (
    <ul className="pr-fs">
      {looks.map((l) => {
        const key = `${at}:${l.f.id}`;
        const open = finding === key;
        return (
          <li key={l.f.id} className="pr-f" data-open={open || undefined}>
            <button type="button" className="pr-f-head" aria-expanded={open} onClick={() => onFinding(open ? null : key)}>
              <GradeChip f={l.f} />
              <span className="pr-f-claim">
                <span className="pr-f-id">{l.f.id}</span>
                {l.f.claim}
              </span>
              <span className="pr-f-meta">
                {l.flags.length > 0 && <span data-flag="">{l.flags.map((f) => FLAG_WORDS[f]).join(", ")}</span>}
                {l.flags.length > 0 && " · "}
                {l.steps.length ? stepsWord(l.steps) : "no step"} · {l.f.confidence.score}
              </span>
            </button>
            {open && <FindingOpen f={l.f} sources={sources} />}
          </li>
        );
      })}
    </ul>
  );
}

/** A finding opened: why kiln graded it so, what's against it, and its quotes with where they're from. */
function FindingOpen({ f, sources }: { f: Finding; sources: ResearchSource[] }) {
  const number = (id: string) => sources.findIndex((s) => s.id === id) + 1;
  return (
    <div className="pr-f-open">
      <p className="pr-why">
        {f.confidence.grade} · {f.confidence.score} of 100 · {f.confidence.why}
        {f.inferred && " · inferred"}
      </p>
      {f.against.map((k, i) => (
        <p key={i} className="pr-against">
          Against: {k.claim} <span className="pr-dim">· weighs {Math.round((f.against_weights[i] ?? 0) * 100)}%</span>
        </p>
      ))}
      {f.cites.length > 0 && (
        <ol className="pr-cites">
          {f.cites.map((c, i) => {
            const s = sources.find((x) => x.id === c.source);
            const state = c.check?.state;
            return (
              <li key={i}>
                <span className="pr-dim">[{number(c.source)}]</span> {c.quote ? <q>{c.quote}</q> : <span>{s?.title ?? c.source}</span>}
                <span className="pr-dim">
                  {" "}
                  · {s ? `${SOURCE_WORDS[s.type]}: ${s.title}` : c.source}
                  {c.at ? `, ${c.at}` : ""}
                </span>
                {state && (
                  <span className="pr-check" data-state={state} title={c.check?.said}>
                    {" "}
                    · {CHECK_WORDS[state]}
                  </span>
                )}
              </li>
            );
          })}
        </ol>
      )}
      {f.note && <p className="pr-dim">{f.note}</p>}
    </div>
  );
}

/* ------------------------------------------------------------------ answered */

/** What you answered here, for a while once its review has gone: what it did, and what comes next. */
function Receipt({ agent, name }: { agent: string; name: string }) {
  const a = useAnswered(agent);
  const now = useClock(30_000);
  if (!a || now - a.at > RECEIPT_MS) return null;
  const goal = a.goal ? `goal ${a.goal}` : "its goal";
  const head = a.answer === "approve" ? "Approved by you" : a.answer === "reject" ? "Rejected by you" : `Sent to ${name}`;
  const text =
    a.kind === "goal"
      ? a.answer === "approve"
        ? `Its ${a.steps} step${a.steps === 1 ? "" : "s"} went to ${name} as ${goal}'s parts: the stack goes on.`
        : a.answer === "reject"
          ? `${goal.replace(/^g/, "G")} is skipped and its steps won't run; ${name} was told. The stack waits for you: resume it from the goal stack below.`
          : `It's changing the plan as you asked; once it's ready again it comes back here, with what changed.`
      : a.answer === "approve"
        ? "It left plan mode and started on it."
        : a.answer === "reject"
          ? "It stopped there and waits for you."
          : "It's still planning, with your words; its next proposal comes back here, with what changed.";
  return (
    <section className="pr-receipt" data-v={a.answer} aria-live="polite">
      <p className="pr-receipt-top">
        {head} · <Ago ts={Math.floor(a.at / 1000)} />
      </p>
      <p className="pr-receipt-title">{a.title}</p>
      {a.answer === "other" && a.words && (
        <p className="pr-asked">
          You said: <q>{a.words}</q>
        </p>
      )}
      <p className="pr-receipt-text">{text}</p>
    </section>
  );
}
