/** W21-R02: actual destination loading shares the view's canonical owners; no speculative data. */
import { isAuthed } from "@/api/client";
import { sessionStore } from "@/api/harness";
import { goalsRes } from "@/api/goal-resources";
import { reviewPlanRes } from "@/api/plan-resources";
import { harnessSessionsRes } from "./data";
import { acquireResource, resourceData } from "./resource";
import { loadInspectorTab, type InspectorTab } from "@/views/harness/inspector-code";

interface Destination {
  id: string;
  tab: InspectorTab;
  signal?: AbortSignal;
}
interface NavigationReads {
  code(tab: InspectorTab): Promise<unknown>;
  session(id: string, signal?: AbortSignal): Promise<unknown>;
  review(id: string, signal?: AbortSignal): Promise<unknown>;
  goals(id: string, signal?: AbortSignal): Promise<unknown>;
  held(id: string): string | undefined;
  agents(signal?: AbortSignal): Promise<readonly { id: string; held?: { plan?: string | null } | null }[]>;
}

function prepareSession(id: string, signal?: AbortSignal): Promise<void> {
  if (signal?.aborted) return Promise.resolve();
  const store = sessionStore(id);
  return new Promise<void>((resolve) => {
    const release = store.subscribe(() => {});
    let released = false;
    const stop = () => {
      if (released) return;
      released = true;
      release();
      signal?.removeEventListener("abort", stop);
      resolve();
    };
    signal?.addEventListener("abort", stop, { once: true });
    void store.ready().then(stop, stop);
  });
}

const reads: NavigationReads = {
  code: loadInspectorTab,
  session: prepareSession,
  goals: (id, signal) => acquireResource(goalsRes(id), signal),
  review: (id, signal) => acquireResource(reviewPlanRes(id), signal),
  held: (id) => resourceData(harnessSessionsRes)?.find((agent) => agent.id === id)?.held?.plan ?? undefined,
  agents: (signal) => acquireResource(harnessSessionsRes, signal),
};

/** Exported orchestration is testable without a browser or hidden mounted view. */
export async function loadAgentDestination({ id, tab, signal }: Destination, source: NavigationReads = reads): Promise<void> {
  if (signal?.aborted) return;
  const pending = [source.code(tab), source.session(id, signal)];
  if (tab === "plan") {
    pending.push(source.goals(id, signal));
    let plan = source.held(id);
    if (plan) pending.push(source.review(plan, signal));
    pending.push(
      source.agents(signal).then(async (agents) => {
        if (signal?.aborted) return;
        const current = agents.find((agent) => agent.id === id)?.held?.plan;
        if (current && current !== plan) {
          plan = current;
          await source.review(current, signal);
        }
      }),
    );
  }
  await Promise.allSettled(pending);
}

export function prepareAgentNavigation(destination: Destination): Promise<void> {
  if (typeof window === "undefined" || isAuthed() === false) return Promise.resolve();
  return loadAgentDestination(destination);
}
