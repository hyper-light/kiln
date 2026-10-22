/** Programs, read through Suspense under one key (lib/resource.ts): no effects. */
import { programsApi, type Program } from "@/api/programs";
import type { Res } from "@/lib/data";

/** Every program kiln runs, the critical path first: new data when settings, assignments or change
 * requests change, and every 10 s (not on each agent's event: a stream of those would load it
 * again and again, and how a worker stands can wait 10 s). */
export const programsRes: Res<Program[]> = { key: "programs", fetch: (s) => programsApi.list(s), opts: { pollMs: 10_000, on: ["program", "agent_change"] } };
