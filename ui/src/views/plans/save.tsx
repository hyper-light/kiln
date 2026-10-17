/** Keeping a plan an agent made: "Save plan" wherever a plan shows in a conversation. */
import * as React from "react";
import { Link } from "react-router";
import { planHref, plansApi, type PlanSource } from "@/api/plans";
import { errorMessage } from "@/lib/hooks";
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
