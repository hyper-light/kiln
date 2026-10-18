/** A shared plan, for anyone with its link: its words only, read-only, no sign-in. */
import { useParams } from "react-router";
import { plansApi } from "@/api/plans";
import { BrandMark } from "@/components/shell/brand";
import { Loadable } from "@/components/ui/loading";
import { CopyButton } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import { Prose } from "@/views/harness/parts";
import { itemsMarkdown, planMarkdown, withoutTitle } from "./steps";
import "./plans.css";

export default function SharedPlanPage() {
  const { token = "" } = useParams();
  return (
    <main className="plan-shared">
      <header className="plan-shared-head">
        <BrandMark className="h-6 w-5 opacity-80" />
        <span>A plan shared from kiln</span>
      </header>
      <Loadable label="the plan" className="plan-shared-loading">
        <SharedPlan token={token} />
      </Loadable>
    </main>
  );
}

/** The plan, or why there is none: the link was stopped (404), or something else failed. */
async function readShared(token: string, signal: AbortSignal) {
  try {
    return await plansApi.shared(token, signal);
  } catch (e) {
    if ((e as { status?: number }).status === 404) return null;
    throw e;
  }
}

function SharedPlan({ token }: { token: string }) {
  const p = useSuspenseResource(`shared/plans/${token}`, (s) => readShared(token, s)).data;
  if (!p) return <p className="plan-error">This link doesn't open a plan any more.</p>;
  return (
    <article>
      <title>{`${p.title} · a plan shared from kiln`}</title>
      <h1>{p.title}</h1>
      <p className="plan-dim plan-shared-meta">
        {p.steps.length ? `${p.steps.length} step${p.steps.length === 1 ? "" : "s"} · ` : ""}version {p.version} · changed {relativeTime(p.updated_at)}
      </p>
      <Prose text={[withoutTitle(p.body, p.title), itemsMarkdown(p.items ?? [])].filter(Boolean).join("\n\n")} />
      <div className="plan-shared-actions">
        <CopyButton text={planMarkdown(p.title, p.body, p.items ?? [])} label="Copy the plan" />
        <span className="plan-dim">as markdown, to keep in your own kiln: Agents, Plans, New plan</span>
      </div>
    </article>
  );
}
