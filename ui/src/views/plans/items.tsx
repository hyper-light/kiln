/**
 * A plan's steps, written one by one: each a piece of the work with its description, examples,
 * acceptance criteria (when it's done; as goals, what its /goal checks) and testing (examples,
 * scenarios, criteria). One step open at a time; the others read as a line each, with which of
 * their parts are filled. In the page's flow; the console's own fields.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Plus } from "lucide-react";
import { emptyItem, type PlanItem } from "@/api/plans";
import "./plans.css";

const PARTS: { key: Exclude<keyof PlanItem, "title">; label: string; hint: string; rows: number }[] = [
  { key: "description", label: "Description", hint: "What to do. Markdown.", rows: 3 },
  { key: "examples", label: "Examples", hint: "What's meant, shown: inputs and outputs, before and after.", rows: 2 },
  { key: "acceptance", label: "Acceptance criteria", hint: "When it's done. Run as goals, this is what each /goal checks.", rows: 2 },
  { key: "testing", label: "Testing", hint: "How it's tested: examples, scenarios, criteria.", rows: 2 },
];

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
            const filled = PARTS.filter((p) => it[p.key].trim()).map((p) => p.label.toLowerCase());
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
