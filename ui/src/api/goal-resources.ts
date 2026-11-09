/** W21-R02: a selected Plan destination reads the same goal stack as its panel. */
import { onAuthChange } from "./client";
import { harnessApi, type GoalStack } from "./harness";
import { stackReads } from "./goal-reads";
import type { Res } from "@/lib/data";

export const goalReads = stackReads(() => performance.now());
let epoch = 0;
onAuthChange((authed) => {
  if (!authed) {
    epoch++;
    goalReads.clear();
  }
});

export const goalsRes = (agent: string): Res<{ stack: GoalStack | null; error: unknown }> => ({
  key: `harness/goals/${agent}`,
  opts: { pollMs: 2500 },
  fetch: (signal) => {
    const at = epoch;
    const took = goalReads.ask(agent);
    const current = () => {
      if (signal.aborted || at !== epoch) throw new DOMException("Navigation canceled", "AbortError");
    };
    return harnessApi.goals(agent, signal).then(
      (stack) => {
        current();
        return { stack: took(stack), error: null };
      },
      (error: unknown) => {
        current();
        return { stack: goalReads.last(agent), error };
      },
    );
  },
});
