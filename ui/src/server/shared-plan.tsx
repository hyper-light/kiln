/**
 * A shared plan, rendered on the server (a server component): its words arrive as HTML, markdown
 * included, so its page sends none of the markdown code. Read from kiln's API, which serves shared
 * plans to anyone with the link.
 */
import "@tanstack/react-start/server-only";
import type { SharedPlan } from "@/api/plans";
import { relativeTime } from "@/lib/format";
import { MarkdownBody } from "@/lib/markdown";
import { itemsMarkdown, withoutTitle } from "@/views/plans/steps";
import "@/views/harness/harness.css";
import "@/views/plans/plans.css";

/** Where the UI server reaches kiln's API (kiln sets it when it starts the UI server). */
const API = (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env.KILN_API_URL ?? "http://127.0.0.1:7070";

/** The plan its link shares; null when the link no longer shares one. */
export async function readSharedPlan(token: string): Promise<SharedPlan | null> {
  const res = await fetch(`${API}/api/shared/plans/${encodeURIComponent(token)}`);
  if (res.status === 404) return null;
  if (!res.ok) throw new Error(`kiln answered ${res.status}`);
  return (await res.json()) as SharedPlan;
}

export function SharedPlanView({ plan }: { plan: SharedPlan }) {
  const steps = plan.steps.length;
  return (
    <>
      <h1>{plan.title}</h1>
      <p className="plan-dim plan-shared-meta">
        {steps ? `${steps} step${steps === 1 ? "" : "s"} · ` : ""}version {plan.version} · changed {relativeTime(plan.updated_at)}
      </p>
      <div className="hx-prose">
        <MarkdownBody text={[withoutTitle(plan.body, plan.title), itemsMarkdown(plan.items ?? [])].filter(Boolean).join("\n\n")} />
      </div>
    </>
  );
}
