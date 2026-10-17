/**
 * Your plans, in an agent's Plan tab: write one right here, step by step (each with its
 * description, examples, acceptance criteria and testing), keep it, change it, and run it on this
 * agent as one message, step by step, or as goals (each step a /goal once the one before is met).
 * Kept plans are the same ones Agents › Plans lists; everything stays in the page's flow.
 */
import * as React from "react";
import { Link } from "react-router";
import { planHref, plansApi, type Plan, type PlanItem, type PlanMode, type PlanRan } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { SelectField } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { errorMessage, useResource } from "@/lib/hooks";
import { PlanItemsEditor } from "./items";
import "./plans.css";

interface Draft {
  /** The plan it changes; none for a new one. */
  id?: string;
  title: string;
  body: string;
  items: PlanItem[];
}

const MODES: { value: PlanMode; label: string }[] = [
  { value: "message", label: "One message: the whole plan" },
  { value: "steps", label: "Step by step: a step a turn" },
  { value: "goals", label: "Goals: each step a /goal, the next once it's met" },
];

export function PlansHere({ session }: { session: string }) {
  const res = useResource((s) => plansApi.list(s), []);
  const [draft, setDraft] = React.useState<Draft | null>(null);
  const [picked, setPicked] = React.useState("");
  const [mode, setMode] = React.useState<PlanMode>("message");
  const [busy, setBusy] = React.useState(false);
  const [note, setNote] = React.useState<string | null>(null);
  const [ran, setRan] = React.useState<PlanRan | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const plans = res.data ?? [];
  const plan = plans.find((p) => p.id === picked) ?? plans[0];

  const edit = (p?: Plan) => {
    setError(null);
    setNote(null);
    setRan(null);
    setDraft(p ? { id: p.id, title: p.title, body: p.body, items: p.items.length ? p.items : [] } : { title: "", body: "", items: [] });
  };
  const save = async () => {
    if (!draft) return;
    setBusy(true);
    setError(null);
    try {
      const body = { title: draft.title.trim() || undefined, body: draft.body, items: draft.items };
      const p = draft.id ? await plansApi.update(draft.id, body) : await plansApi.create({ ...body, source: { session } });
      res.setData((l) => [p, ...(l ?? []).filter((x) => x.id !== p.id)]);
      setPicked(p.id);
      setDraft(null);
      setNote(draft.id ? `Saved as version ${p.version}.` : "Saved.");
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const run = async () => {
    if (!plan) return;
    setBusy(true);
    setError(null);
    setRan(null);
    try {
      setRan(await plansApi.run(plan.id, { session }, steps(plan) ? mode : "message"));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  if (!res.data) return res.error ? <p className="in-none">{errorMessage(res.error)}</p> : null;

  if (draft) {
    return (
      <div className="ph">
        <div>
          <label className="field-label" htmlFor={`ph-title-${session}`}>
            Title
          </label>
          <div className="field">
            <input id={`ph-title-${session}`} value={draft.title} placeholder="From the first step when left empty" onChange={(e) => setDraft({ ...draft, title: e.target.value })} />
          </div>
        </div>
        <div>
          <label className="field-label" htmlFor={`ph-body-${session}`}>
            Overview (optional)
          </label>
          <div className="field">
            <textarea id={`ph-body-${session}`} rows={2} value={draft.body} placeholder="Why, and anything every step should know" onChange={(e) => setDraft({ ...draft, body: e.target.value })} />
          </div>
        </div>
        <div>
          <div className="field-label">Steps</div>
          <PlanItemsEditor items={draft.items} onChange={(items) => setDraft({ ...draft, items })} idPrefix={`ph-${session}`} />
        </div>
        <div className="plan-actions">
          <Button size="sm" disabled={busy || (!draft.items.length && !draft.body.trim())} onClick={() => void save()}>
            {busy ? "Saving…" : draft.id ? "Save changes" : "Save plan"}
          </Button>
          <Button size="sm" variant="ghost" onClick={() => setDraft(null)}>
            Cancel
          </Button>
          {error && <span className="plan-error">{error}</span>}
        </div>
      </div>
    );
  }

  if (!plan)
    return (
      <div className="ph">
        <p className="ph-lead">Write a plan here, a step at a time: each with a description, examples, acceptance criteria and how it&rsquo;s tested. Then run it on this agent.</p>
        <div className="plan-actions">
          <Button size="sm" onClick={() => edit()}>
            New plan
          </Button>
        </div>
      </div>
    );

  const n = steps(plan);
  return (
    <div className="ph">
      <SelectField
        label="Plan"
        value={plan.id}
        onChange={(v) => {
          setPicked(v);
          setRan(null);
          setNote(null);
        }}
        options={plans.map((p) => ({ value: p.id, label: `${p.title}${steps(p) ? ` · ${steps(p)} step${steps(p) === 1 ? "" : "s"}` : ""}` }))}
      />
      <p className="ph-meta">
        v{plan.version} · changed {relativeTime(plan.updated_at)}
        {plan.runs ? ` · run ${plan.runs}×` : ""} · <Link to={planHref(plan.id)}>open in Plans</Link>
      </p>
      {n > 0 && <SelectField label="Run as" value={mode} onChange={(v) => setMode(v as PlanMode)} options={MODES} />}
      <div className="plan-actions">
        <Button size="sm" variant="secondary" disabled={busy} onClick={() => void run()}>
          {busy ? "Sending…" : "Run here"}
        </Button>
        <Button size="sm" variant="ghost" onClick={() => edit(plan)}>
          Edit
        </Button>
        <Button size="sm" variant="ghost" onClick={() => edit()}>
          New plan
        </Button>
      </div>
      {note && <p className="plan-ok">{note}</p>}
      {ran && <p className="plan-ok">{mode === "goals" && n ? `${ran.sent} goals lined up in its goal stack, below.` : `Sent ${ran.sent === 1 ? "the plan" : `${ran.sent} messages`}.`}</p>}
      {error && <p className="plan-error">{error}</p>}
    </div>
  );
}

/** How many steps a plan has: its written steps, else its text's list items. */
function steps(p: Plan) {
  return p.items.length || p.steps.length;
}
