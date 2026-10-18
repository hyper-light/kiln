/**
 * A shared plan: for anyone with its link, signed in or not. Rendered on the server: the plan's
 * words come as a server component (server/shared-plan.tsx), so the page sends no markdown code.
 * Only Copy runs in the browser.
 */
import { createFileRoute } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { renderServerComponent } from "@tanstack/react-start/rsc";
import { BrandMark } from "@/components/shell/brand";
import { CopyButton } from "@/components/ui/misc";
import { readSharedPlan, SharedPlanView } from "@/server/shared-plan";
import { planMarkdown } from "@/views/plans/steps";
import "@/views/plans/plans.css";

const getSharedPlan = createServerFn({ method: "GET" })
  .validator((token: string) => token)
  .handler(async ({ data: token }) => {
    const plan = await readSharedPlan(token);
    if (!plan) return null;
    return {
      title: plan.title,
      markdown: planMarkdown(plan.title, plan.body, plan.items ?? []),
      View: await renderServerComponent(<SharedPlanView plan={plan} />),
    };
  });

export const Route = createFileRoute("/shared/plans/$token")({
  loader: ({ params }) => getSharedPlan({ data: params.token }),
  head: ({ loaderData }) => ({ meta: [{ title: loaderData ? `${loaderData.title} · a plan shared from kiln` : "A plan shared from kiln" }] }),
  component: SharedPlanPage,
  errorComponent: () => (
    <Frame>
      <p className="plan-error">This plan couldn&apos;t be read just now. Try again in a moment.</p>
    </Frame>
  ),
});

function Frame({ children }: { children: React.ReactNode }) {
  return (
    <main className="plan-shared">
      <header className="plan-shared-head">
        <BrandMark className="h-6 w-5 opacity-80" />
        <span>A plan shared from kiln</span>
      </header>
      {children}
    </main>
  );
}

function SharedPlanPage() {
  const plan = Route.useLoaderData();
  return (
    <Frame>
      {plan ? (
        <article>
          {plan.View}
          <div className="plan-shared-actions">
            <CopyButton text={plan.markdown} label="Copy the plan" />
            <span className="plan-dim">as markdown, to keep in your own kiln: Agents, Plans, New plan</span>
          </div>
        </article>
      ) : (
        <p className="plan-error">This link doesn&apos;t open a plan any more.</p>
      )}
    </Frame>
  );
}
