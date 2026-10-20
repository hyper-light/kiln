/**
 * Agents and the agents they started (docs/agents.md, "Agents that run agents"). In Mission
 * control a parent's children sit under it, indented, folding in the page's flow; a child that
 * needs you keeps its own place at the top, naming its parent. A plan run's agents (one per step)
 * sit under the run ("Plan: … · 2 of 3 met"), and the agents an attachment woke under it
 * (docs/ci.md W20-R08). Ending a parent offers to end what it started too.
 */
import * as React from "react";
import "./family.css";
import { ArrowUpRight, ChevronRight } from "lucide-react";
import { Link } from "@/lib/router";
import { attachApi, onWords, type IncidentBrief } from "@/api/attach";
import { harnessApi, type AgentInfo, type SubagentBrief } from "@/api/harness";
import { plansApi, runHref, type PlanRunBrief } from "@/api/plans";
import { useLocalStorage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { agentLabel, taskDot } from "./derive";

/** A row of the list and the rows that go under it; or a plan run, or an attachment, and its agents. */
export interface Family<T> {
  row: T;
  kids: Family<T>[];
  /** A child shown on its own (it needs you, or its parent isn't in the list): its parent's name. */
  of?: string;
  /** A plan run's group (`row` is its first agent's). */
  run?: PlanRunBrief;
  /** An attachment's group: the agents its incidents woke. */
  attached?: { id: string; name: string; on: string };
}

/**
 * The agents an attachment woke under it, where the first of them was; one that needs you keeps
 * its place, naming the attachment.
 */
function byAttachment<T extends { a: AgentInfo; stand: string }>(list: Family<T>[], incidents: IncidentBrief[]): Family<T>[] {
  const out: Family<T>[] = [];
  const groups = new Map<string, Family<T>>();
  for (const f of list) {
    const inc = f.run ? undefined : incidents.find((i) => i.agent === f.row.a.id);
    if (!inc) {
      out.push(f);
      continue;
    }
    if (f.row.stand === "needs") {
      out.push({ ...f, of: f.of ?? `${inc.name}, attached to ${onWords(inc.on)}` });
      continue;
    }
    let g = groups.get(inc.attachment);
    if (!g) {
      g = { row: f.row, kids: [], attached: { id: inc.attachment, name: inc.name, on: inc.on } };
      groups.set(inc.attachment, g);
      out.push(g);
    }
    g.kids.push(f);
  }
  return out;
}

/**
 * Each level's agents of one plan run under that run, where its first agent was; one of them that
 * needs you keeps its place, naming the run.
 */
function byRun<T extends { a: AgentInfo; stand: string }>(list: Family<T>[], runs: PlanRunBrief[]): Family<T>[] {
  const out: Family<T>[] = [];
  const groups = new Map<string, Family<T>>();
  for (const f of list) {
    const kids = byRun(f.kids, runs);
    const node = { ...f, kids };
    const run = runs.find((r) => r.id === f.row.a.plan_run);
    if (!run) {
      out.push(node);
      continue;
    }
    if (f.row.stand === "needs") {
      out.push({ ...node, of: node.of ?? `the plan ${run.title}` });
      continue;
    }
    let g = groups.get(run.id);
    if (!g) {
      g = { row: f.row, kids: [], run };
      groups.set(run.id, g);
      out.push(g);
    }
    g.kids.push(node);
  }
  return out;
}

/**
 * The list as families, in the list's own order: an agent whose parent is in the list goes under
 * it, unless it needs you (then it keeps its place, and says whose it is).
 */
export function families<T extends { a: AgentInfo; stand: string }>(rows: T[], all: AgentInfo[]): Family<T>[] {
  const nodes = new Map<string, Family<T>>(rows.map((r) => [r.a.id, { row: r, kids: [] }]));
  const name = (id: string) => {
    const p = all.find((a) => a.id === id);
    return p ? agentLabel(p) : undefined;
  };
  const top: Family<T>[] = [];
  for (const r of rows) {
    const n = nodes.get(r.a.id);
    if (!n) continue;
    const parent = r.a.parent ? nodes.get(r.a.parent) : undefined;
    if (parent && r.stand !== "needs") parent.kids.push(n);
    else {
      if (r.a.parent) n.of = name(r.a.parent);
      top.push(n);
    }
  }
  return top;
}

const WORDS: Record<string, string> = { needs: "needs you", crashed: "stopped", paused: "paused", working: "working", finished: "finished", ready: "ready", offline: "offline", ended: "ended" };
const TASK_WORDS: Record<string, string> = { running: "working", pending: "queued", completed: "finished", failed: "failed", paused: "paused", waiting: "needs you", stopped: "stopped", killed: "stopped", cancelled: "cancelled" };
const taskWord = (task: SubagentBrief) => TASK_WORDS[task.status] ?? task.status;

/** How a family's children stand, in a few words: "1 working, 1 finished". */
function standing<T extends { stand: string }>(kids: Family<T>[], tasks: SubagentBrief[] = []): string {
  const n = new Map<string, number>();
  for (const word of [...kids.map((k) => WORDS[k.row.stand] ?? k.row.stand), ...tasks.map(taskWord)]) n.set(word, (n.get(word) ?? 0) + 1);
  return [...n].map(([word, count]) => `${count} ${word}`).join(", ");
}

/** Native subagents open their existing thread; they have no independent session controls. */
function NativeChild({ agent, task }: { agent: string; task: SubagentBrief }) {
  return (
    <li className="fam-task">
      <Link to={`/agents/${encodeURIComponent(agent)}?task=${encodeURIComponent(task.id)}`} className="fam-task-link">
        <span className="fam-task-name">{task.description || "Subagent"}</span>
        <span className="fam-task-status">
          <i className="dot" data-status={taskDot(task)} aria-hidden="true" />
          {taskWord(task)}
        </span>
        <ArrowUpRight size={16} aria-hidden="true" />
      </Link>
    </li>
  );
}

/**
 * The families as list items: each row, then the rows under it in a list of their own that folds
 * (open unless you folded it; kept across visits).
 */
export function Families<T extends { a: AgentInfo; stand: string }>({ items, render }: { items: Family<T>[]; render: (row: T, of?: string) => React.ReactNode }) {
  const [folded, setFolded] = useLocalStorage<string[]>("kiln.agents.folded", []);
  const toggle = (id: string) => setFolded(folded.includes(id) ? folded.filter((x) => x !== id) : [...folded, id]);
  const out = (list: Family<T>[]): React.ReactNode =>
    list.map((f) => {
      if (f.run) {
        const run = f.run;
        const open = !folded.includes(run.id);
        return (
          <li key={`run-${run.id}`} className="fam fam-run">
            <div className="fam-run-head">
              <button type="button" className="fam-fold fam-run-fold" aria-expanded={open} aria-label={open ? "Fold the run's agents" : "Show the run's agents"} onClick={() => toggle(run.id)}>
                <ChevronRight size={13} className="fam-chev" aria-hidden="true" />
              </button>
              <Link to={runHref(run.id)} className="fam-run-link">
                Plan: {run.title}
              </Link>
              <span className="fam-stand">
                {run.met} of {run.total} met{run.state !== "running" ? `, ${run.state}` : ""}
              </span>
            </div>
            {open && <ul className="ms-list fam-kids">{out(f.kids)}</ul>}
          </li>
        );
      }
      if (f.attached) {
        const at = f.attached;
        const key = `attached-${at.id}`;
        const open = !folded.includes(key);
        return (
          <li key={key} className="fam fam-run">
            <div className="fam-run-head">
              <button type="button" className="fam-fold fam-run-fold" aria-expanded={open} aria-label={open ? "Fold its agents" : "Show its agents"} onClick={() => toggle(key)}>
                <ChevronRight size={13} className="fam-chev" aria-hidden="true" />
              </button>
              <Link to="/agents/attached" className="fam-run-link">
                {at.name}, attached to {onWords(at.on)}
              </Link>
              <span className="fam-stand">{standing(f.kids)}</span>
            </div>
            {open && <ul className="ms-list fam-kids">{out(f.kids)}</ul>}
          </li>
        );
      }
      const id = f.row.a.id;
      const open = !folded.includes(id);
      const tasks = f.row.a.subagents ?? [];
      const count = f.kids.length + tasks.length;
      return (
        <React.Fragment key={id}>
          {render(f.row, f.of)}
          {count > 0 && (
            <li className="fam">
              <button type="button" className="fam-fold" aria-expanded={open} aria-label={`${open ? "Fold" : "Show"} ${agentLabel(f.row.a)}'s ${count} subagent${count === 1 ? "" : "s"}`} onClick={() => toggle(id)}>
                <ChevronRight size={13} className="fam-chev" aria-hidden="true" />
                <span className="fam-count">{count} agent{count === 1 ? "" : "s"} it started</span>
                <span className="fam-stand">{standing(f.kids, tasks)}</span>
              </button>
              {open && (
                <ul className="ms-list fam-kids">
                  {out(f.kids)}
                  {tasks.map((task) => <NativeChild key={`task-${task.id}`} agent={id} task={task} />)}
                </ul>
              )}
            </li>
          )}
        </React.Fragment>
      );
    });
  return <>{out(items)}</>;
}

/** The incidents whose agents the list may show; none for someone who can't read them. */
function WithAttached<T extends { a: AgentInfo; stand: string }>({ items, render }: { items: Family<T>[]; render: (row: T, of?: string) => React.ReactNode }) {
  const incidents = useSuspenseResource("incidents/agents", (s) => attachApi.incidents(s).catch(() => [] as IncidentBrief[]), { on: ["incident"], pollMs: 30_000 }).data;
  return <Families items={byAttachment(items, incidents)} render={render} />;
}

/** The plan runs whose agents the list shows (a done run's agents may still be listed). */
function WithRuns<T extends { a: AgentInfo; stand: string }>({ rows, all, render }: { rows: T[]; all: AgentInfo[]; render: (row: T, of?: string) => React.ReactNode }) {
  const runs = useSuspenseResource("plans/runs/recent", (s) => plansApi.agentRuns(false, s), { on: ["harness", "harness_closed"], pollMs: 15_000 }).data.runs;
  const items = byRun(families(rows, all), runs);
  return (
    <React.Suspense fallback={<Families items={items} render={render} />}>
      <WithAttached items={items} render={render} />
    </React.Suspense>
  );
}

/** The list as families: a plan run's agents under the run, an attachment's under it. */
export function AgentFamilies<T extends { a: AgentInfo; stand: string }>({ rows, all, render }: { rows: T[]; all: AgentInfo[]; render: (row: T, of?: string) => React.ReactNode }) {
  const items = families(rows, all);
  const plain = <Families items={items} render={render} />;
  return <React.Suspense fallback={plain}>{rows.some((r) => r.a.plan_run) ? <WithRuns rows={rows} all={all} render={render} /> : <WithAttached items={items} render={render} />}</React.Suspense>;
}

/** An agent's live children, as the server knows them (theirs too). */
function useChildren(id: string) {
  return useSuspenseResource(`harness/children/${id}`, () => harnessApi.children(id)).data.children;
}

/** In the End dialog: the agents it started that still run, and whether they end with it. */
function EndsWith({ id, on, onChange }: { id: string; on: boolean; onChange: (on: boolean) => void }) {
  const kids = useChildren(id);
  if (kids.length === 0) return null;
  const names = kids.map((k) => k.name).join(", ");
  return (
    <span className="fam-end">
      <span className="fam-end-what">
        It started {kids.length === 1 ? "an agent that's" : `${kids.length} agents that are`} still running: {names}.
      </span>
      <label className="fam-end-check">
        <input type="checkbox" className="check" checked={on} onChange={(e) => onChange(e.target.checked)} />
        End {kids.length === 1 ? "it" : "them"} too
      </label>
    </span>
  );
}

/** What ending agent `id` ends with it: the line for its End dialog, and the end itself. */
export function useEndWithChildren(id: string) {
  const [withKids, setWithKids] = React.useState(true);
  const note = (
    <React.Suspense fallback={null}>
      <EndsWith id={id} on={withKids} onChange={setWithKids} />
    </React.Suspense>
  );
  return { note, end: () => harnessApi.end(id, withKids) };
}
