/**
 * Agents and the agents they started (docs/agents.md, "Agents that run agents"). In Mission
 * control a parent's children sit under it, indented, folding in the page's flow; a child that
 * needs you keeps its own place at the top, naming its parent. Ending a parent offers to end
 * what it started too.
 */
import * as React from "react";
import "./family.css";
import { ChevronRight } from "lucide-react";
import { harnessApi, type AgentInfo } from "@/api/harness";
import { useLocalStorage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { agentLabel } from "./derive";

/** A row of the list and the rows that go under it. */
export interface Family<T> {
  row: T;
  kids: Family<T>[];
  /** A child shown on its own (it needs you, or its parent isn't in the list): its parent's name. */
  of?: string;
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

/** How a family's children stand, in a few words: "1 working, 1 finished". */
function standing<T extends { stand: string }>(kids: Family<T>[]): string {
  const n = new Map<string, number>();
  for (const k of kids) n.set(k.row.stand, (n.get(k.row.stand) ?? 0) + 1);
  return [...n].map(([s, c]) => `${c} ${WORDS[s] ?? s}`).join(", ");
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
      const id = f.row.a.id;
      const open = !folded.includes(id);
      return (
        <React.Fragment key={id}>
          {render(f.row, f.of)}
          {f.kids.length > 0 && (
            <li className="fam">
              <button type="button" className="fam-fold" aria-expanded={open} onClick={() => toggle(id)}>
                <ChevronRight size={13} className="fam-chev" aria-hidden="true" />
                {f.kids.length} agent{f.kids.length === 1 ? "" : "s"} it started
                <span className="fam-stand">{standing(f.kids)}</span>
              </button>
              {open && <ul className="ms-list fam-kids">{out(f.kids)}</ul>}
            </li>
          )}
        </React.Fragment>
      );
    });
  return <>{out(items)}</>;
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
