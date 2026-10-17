/** A shared plan, for anyone with its link: its words only, read-only, no sign-in. */
import { useParams } from "react-router";
import { plansApi } from "@/api/plans";
import { BrandMark } from "@/components/shell/brand";
import { Loadable } from "@/components/ui/loading";
import { CopyButton } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { useDocumentTitle } from "@/lib/hooks";
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
      <Loadable label="Loading the plan">
        <SharedPlan key={token} token={token} />
      </Loadable>
    </main>
  );
}

function SharedPlan({ token }: { token: string }) {
  // A link that's no longer shared (or never was) reads as none.
  const res = useSuspenseResource(`shared-plan/${token}`, (s) =>
    plansApi.shared(token, s).catch((e: unknown) => {
      if ((e as { status?: number }).status === 404) return null;
      throw e;
    }),
  );
  const p = res.data;
  useDocumentTitle(p ? `${p.title} · a shared plan` : "A shared plan");
  if (!p) return <p className="plan-error">This link doesn&rsquo;t open a plan any more.</p>;
  return (
    <article>
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
