/**
 * A plan's steps, written one by one: each a piece of the work with its description, examples,
 * acceptance criteria (each in words or a measure; as goals, what its /goal checks) and tests (each
 * a scenario and what it passes by). One step open at a time; the others read as a line each, with which of
 * their parts are filled. In the page's flow; the console's own fields.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Plus } from "lucide-react";
import { emptyItem, type PlanItem } from "@/api/plans";
import { CriteriaEditor, TestsEditor } from "./criteria";
import "./plans.css";

const PARTS: { key: "description" | "examples"; label: string; hint: string; rows: number }[] = [
  { key: "description", label: "Description", hint: "What to do. Markdown.", rows: 3 },
  { key: "examples", label: "Examples", hint: "What's meant, shown: inputs and outputs, before and after.", rows: 2 },
];

/** Which of a step's parts are filled, as its line says them. */
function filledParts(it: PlanItem): string[] {
  const out = PARTS.filter((p) => it[p.key].trim()).map((p) => p.label.toLowerCase());
  if (it.acceptance.length) out.push(`${it.acceptance.length} criteri${it.acceptance.length === 1 ? "on" : "a"}`);
  if (it.testing.length) out.push(`${it.testing.length} test${it.testing.length === 1 ? "" : "s"}`);
  return out;
}

export function PlanItemsEditor({ items, onChange, idPrefix }: { items: PlanItem[]; onChange: (items: PlanItem[]) => void; idPrefix: string }) {
  const [open, setOpen] = React.useState<number | null>(null);
  const set = (i: number, patch: Partial<PlanItem>) => onChange(items.map((it, j) => (j === i ? { ...it, ...patch } : it)));
  const move = (i: number, by: -1 | 1) => {
    const j = i + by;
    if (j < 0 || j >= items.length) return;
    const next = [...items];
    const [it] = next.splice(i, 1);
    if (it) next.splice(j, 0, it);
    onChange(next);
    setOpen((o) => (o === i ? j : o === j ? i : o));
  };
  const remove = (i: number) => {
    onChange(items.filter((_, j) => j !== i));
    setOpen(null);
  };
  const add = () => {
    onChange([...items, emptyItem()]);
    setOpen(items.length);
  };

  return (
    <div className="pi">
      {items.length > 0 && (
        <ol className="pi-list">
          {items.map((it, i) => {
            const filled = filledParts(it);
            const isOpen = open === i;
            return (
              <li key={i} className="pi-item" data-open={isOpen ? "" : undefined}>
                <div className="pi-head">
                  <span className="pi-n">{i + 1}</span>
                  <button type="button" className="pi-title" aria-expanded={isOpen} onClick={() => setOpen(isOpen ? null : i)}>
                    <span className="pi-name">{it.title.trim() || "Untitled step"}</span>
                    <span className="pi-parts">{filled.length ? filled.join(" · ") : "no details yet"}</span>
                  </button>
                  <span className="pi-tools">
                    <button type="button" className="pi-icon" aria-label={`Move step ${i + 1} up`} disabled={i === 0} onClick={() => move(i, -1)}>
                      <ArrowUp size={13} />
                    </button>
                    <button type="button" className="pi-icon" aria-label={`Move step ${i + 1} down`} disabled={i === items.length - 1} onClick={() => move(i, 1)}>
                      <ArrowDown size={13} />
                    </button>
                  </span>
                </div>
                {isOpen && (
                  <div className="pi-body">
                    <div>
                      <label className="field-label" htmlFor={`${idPrefix}-${i}-title`}>
                        Title
                      </label>
                      <div className="field">
                        <input id={`${idPrefix}-${i}-title`} value={it.title} placeholder="What this step is, short" onChange={(e) => set(i, { title: e.target.value })} />
                      </div>
                    </div>
                    {PARTS.map((p) => (
                      <div key={p.key}>
                        <label className="field-label" htmlFor={`${idPrefix}-${i}-${p.key}`}>
                          {p.label}
                        </label>
                        <div className="field">
                          <textarea id={`${idPrefix}-${i}-${p.key}`} rows={p.rows} value={it[p.key]} onChange={(e) => set(i, { [p.key]: e.target.value })} />
                        </div>
                        <div className="field-hint">{p.hint}</div>
                      </div>
                    ))}
                    <div>
                      <div className="field-label">Acceptance criteria</div>
                      <CriteriaEditor criteria={it.acceptance} onChange={(acceptance) => set(i, { acceptance })} idPrefix={`${idPrefix}-${i}-ac`} />
                      <div className="field-hint">When it&rsquo;s done: each in words, or a measure against a threshold. Run as goals, this is what each /goal checks.</div>
                    </div>
                    <div>
                      <div className="field-label">Tests</div>
                      <TestsEditor tests={it.testing} onChange={(testing) => set(i, { testing })} idPrefix={`${idPrefix}-${i}-ts`} />
                      <div className="field-hint">Each a scenario, and what it passes by.</div>
                    </div>
                    <div className="pi-actions">
                      <button type="button" className="pi-text" onClick={() => setOpen(null)}>
                        Done
                      </button>
                      <button type="button" className="pi-text pi-remove" onClick={() => remove(i)}>
                        Remove this step
                      </button>
                    </div>
                  </div>
                )}
              </li>
            );
          })}
        </ol>
      )}
      <button type="button" className="pi-add" onClick={add}>
        <Plus size={13} aria-hidden="true" /> Add a step
      </button>
    </div>
  );
}
