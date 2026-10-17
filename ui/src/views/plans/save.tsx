/** Keeping a plan an agent made: "Save plan" wherever a plan shows in a conversation. */
import * as React from "react";
import { Link } from "react-router";
import { planHref, plansApi, type PlanRan, type PlanSource } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { SelectField } from "@/components/ui/misc";
import { errorMessage, useResource } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import "./plans.css";

/** The agent a conversation belongs to, so a plan saved from it says where it came from. */
export const PlanSourceContext = React.createContext<PlanSource | null>(null);

type State = { kind: "idle" } | { kind: "saving" } | { kind: "saved"; id: string } | { kind: "error"; msg: string };

export function SavePlan({ body, title, className }: { body: string; title?: string; className?: string }) {
  const source = React.useContext(PlanSourceContext);
  const [state, setState] = React.useState<State>({ kind: "idle" });
  if (!body.trim()) return null;
  const save = async () => {
    setState({ kind: "saving" });
    try {
      const p = await plansApi.create({ title, body, source: source ?? undefined });
      setState({ kind: "saved", id: p.id });
    } catch (e) {
      setState({ kind: "error", msg: errorMessage(e) });
    }
  };
  return (
    <span className={cn("plan-save", className)}>
      {state.kind === "saved" ? (
        <span>
          Saved to plans · <Link to={planHref(state.id)}>open it</Link>
        </span>
      ) : (
        <button type="button" className="plan-save-btn" disabled={state.kind === "saving"} onClick={() => void save()}>
          {state.kind === "saving" ? "Saving…" : "Save plan"}
        </button>
      )}
      {state.kind === "error" && <span className="plan-save-err">{state.msg}</span>}
    </span>
  );
}

/** A todo list as a plan's markdown. */
export function todosAsPlan(todos: readonly { text: string }[]): string {
  return todos.map((t) => `- ${t.text}`).join("\n");
}

/** Run a saved plan on this agent: pick one, as one message or step by step. */
export function RunSavedPlan({ session }: { session: string }) {
  const res = useResource((s) => plansApi.list(s), []);
  const [id, setId] = React.useState("");
  const [mode, setMode] = React.useState<"message" | "steps">("message");
  const [busy, setBusy] = React.useState(false);
  const [ran, setRan] = React.useState<PlanRan | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const plans = res.data ?? [];
  const plan = plans.find((p) => p.id === id) ?? plans[0];
  if (!res.data) return res.error ? <p className="in-none">{errorMessage(res.error)}</p> : null;
  if (!plan)
    return (
      <p className="in-none">
        No saved plans yet. <Link to="/agents/plans/new">Write one</Link>, or save one an agent makes.
      </p>
    );
  const as = plan.steps.length ? mode : "message";
  const run = async () => {
    setBusy(true);
    setError(null);
    setRan(null);
    try {
      setRan(await plansApi.run(plan.id, { session }, as));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="plan-here">
      <SelectField
        label="Plan"
        value={plan.id}
        onChange={(v) => {
          setId(v);
          setRan(null);
        }}
        options={plans.map((p) => ({ value: p.id, label: `${p.title}${p.steps.length ? ` · ${p.steps.length} step${p.steps.length === 1 ? "" : "s"}` : ""}` }))}
      />
      <SelectField
        label="As"
        value={as}
        onChange={(v) => setMode(v as "message" | "steps")}
        options={[{ value: "message", label: "One message" }, ...(plan.steps.length ? [{ value: "steps", label: `Step by step, each after the last turn ends` }] : [])]}
      />
      <div className="plan-actions">
        <Button size="sm" variant="secondary" disabled={busy} onClick={() => void run()}>
          {busy ? "Sending…" : "Run here"}
        </Button>
        <Link to={planHref(plan.id)} className="plan-here-open">
          Open the plan
        </Link>
        {ran && <span className="plan-ok">Sent {ran.sent === 1 ? "the plan" : `${ran.sent} messages`}</span>}
        {error && <span className="plan-error">{error}</span>}
      </div>
    </div>
  );
}
