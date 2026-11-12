/** W21-R02: canonical plan/research owners; review transport envelopes are discarded on receipt. */
import { ApiError } from "./client";
import { plansApi, type Plan, type PlanReviewContext } from "./plans";
import { researchApi, type Research } from "./research";
import type { Res } from "@/lib/data";
import { resourceCohort, resourceRecord, type ResourceOptions } from "@/lib/resource";

export const unlessGone = (error: unknown): "gone" => {
  if (error instanceof ApiError && (error.status === 404 || error.status === 400)) return "gone";
  throw error;
};

/** A canonical Plan remains fresh when the same record is proposed again, even while another view retains it. */
const planFreshness = (id: string): ResourceOptions<Plan | "gone"> => ({
  on: ["harness"],
  relevant: (event) => event.type === "harness" && event.info.held?.plan === id,
});

/** Ordinary and review readers publish the same revision-consistent canonical records. */
export const planRes = (id: string): Res<Plan | "gone"> => ({
  key: `plans/${id}`,
  opts: planFreshness(id),
  fetch: async (signal) => {
    try {
      return reviewCohort(id, await plansApi.reviewContext(id, signal));
    } catch (error) {
      return unlessGone(error);
    }
  },
});
export const researchRes = (id: string): Res<Research | "gone"> => ({ key: `research/${id}`, fetch: (signal) => researchApi.get(id, signal).catch(unlessGone), opts: { pollMs: 4000 } });

/** Reject a malformed/crossed transport revision before either canonical owner can see it. */
export function reviewCohort(id: string, context: PlanReviewContext) {
  const { plan, research, cohort } = context;
  if (
    plan.id !== id ||
    cohort.plan_version !== plan.version ||
    cohort.research_id !== plan.research ||
    (research !== null && research.id !== plan.research) ||
    (research !== null && research.plan !== plan.id) ||
    (research?.updated_at ?? null) !== cohort.research_updated_at
  )
    throw new ApiError(409, "The plan and its research changed while loading. Try again.");
  return resourceCohort<Plan | "gone">(plan, plan.research ? [resourceRecord(researchRes(plan.research), research ?? "gone")] : []);
}

export const reviewPlanRes = planRes;
