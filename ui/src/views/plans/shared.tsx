/** A shared plan, for anyone with its link: its words only, read-only, no sign-in. */
import * as React from "react";
import { useParams } from "react-router";
import { plansApi } from "@/api/plans";
import { BrandMark } from "@/components/shell/brand";
import { CopyButton } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { errorMessage, useResource } from "@/lib/hooks";
import { Prose } from "@/views/harness/parts";
import { planMarkdown, withoutTitle } from "./steps";
import "./plans.css";

export default function SharedPlanPage() {
  const { token = "" } = useParams();
  const res = useResource((s) => plansApi.shared(token, s), [token]);
  const p = res.data;
  React.useEffect(() => {
    if (p) document.title = `${p.title} · a plan shared from kiln`;
  }, [p]);
  return (
    <main className="plan-shared">
      <header className="plan-shared-head">
        <BrandMark className="h-6 w-5 opacity-80" />
        <span>A plan shared from kiln</span>
      </header>
      {res.error ? (
        <p className="plan-error">{(res.error as { status?: number }).status === 404 ? "This link doesn't open a plan any more." : errorMessage(res.error)}</p>
      ) : !p ? (
        <p className="plan-dim">Loading…</p>
      ) : (
        <article>
          <h1>{p.title}</h1>
          <p className="plan-dim plan-shared-meta">
            {p.steps.length ? `${p.steps.length} step${p.steps.length === 1 ? "" : "s"} · ` : ""}version {p.version} · changed {relativeTime(p.updated_at)}
          </p>
          <Prose text={withoutTitle(p.body, p.title)} />
          <div className="plan-shared-actions">
            <CopyButton text={planMarkdown(p.title, p.body)} label="Copy the plan" />
            <span className="plan-dim">as markdown, to keep in your own kiln: Agents, Plans, New plan</span>
          </div>
        </article>
      )}
    </main>
  );
}
