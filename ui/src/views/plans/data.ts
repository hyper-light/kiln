/** Kept plans, read through Suspense: the ones kept with an agent session (its Plan tab). */
import { plansApi, type Plan } from "@/api/plans";
import type { Res } from "@/lib/data";

/** The plans kept with agent `session`. */
export const sessionPlansRes = (session: string): Res<Plan[]> => ({ key: `plans/of/${session}`, fetch: (s) => plansApi.of(session, s), opts: { pollMs: 20_000 } });
