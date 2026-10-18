/** Kept plans, read through Suspense under one key (the list, a plan's page, an agent's Plan tab). */
import { plansApi, type Plan } from "@/api/plans";
import type { Res } from "@/lib/data";

/** Every kept plan. */
export const plansRes: Res<Plan[]> = { key: "plans", fetch: (s) => plansApi.list(s), opts: { pollMs: 20_000 } };
