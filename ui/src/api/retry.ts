/** Retrying a finished job, here or elsewhere (docs/ci.md W05-R28): what the run page's retry
 * control offers and says, apart from the requests (`ci.legRunners`, `ci.retryLeg`) so it's
 * tested as it is. */
import type { Placement } from "./wire";

/** A runner a finished job could run on again, able to or not and why. */
export interface RetryRunner {
  name: string;
  connected: boolean;
  draining: boolean;
  busy: number;
  slots: number;
  able: boolean;
  why: string[];
}

/** Where a finished job could run again (`GET /api/ci/legs/:id/runners`). */
export interface RetryOptions {
  job: string;
  run: string;
  key: string;
  /** `ci` (a leg of the engine's), `pipeline` (a `.kiln/` pipeline's job), or the provider that ran it. */
  kind: string;
  /** The runner its last attempt ran on. */
  ran_on: string | null;
  /** Whether that runner can be avoided (not a Kubernetes pod made for it, nor another provider's). */
  elsewhere: boolean;
  /** Whether it can run again now, and why not. */
  retry: { can: boolean; why: string | null };
  runners: RetryRunner[];
}

/** What a retry answered (`POST /api/ci/legs/:id/retry`): a leg's new attempt of its run, or a pipeline job's next one in place. */
export interface Retried {
  kind: string;
  run: string;
  attempt?: number;
  /** The leg's key (the engine's): the new attempt's leg of it. */
  key?: string;
  placement?: Placement | null;
}

/** The choices of where to run it again: any runner that can, any but the one it ran on, or each
 * runner able to, those with something to say about them saying it. */
export function retryChoices(o: RetryOptions): { value: string; label: string }[] {
  const out = [{ value: "", label: "Any runner that can run it" }];
  if (o.elsewhere && o.ran_on) out.push({ value: "elsewhere", label: `Any runner but ${o.ran_on}` });
  for (const r of o.runners.filter((x) => x.able)) {
    const state = !r.connected ? "offline" : r.draining ? "draining" : r.busy >= r.slots ? "busy" : "";
    const notes = [r.name === o.ran_on ? "ran here" : "", state].filter(Boolean);
    out.push({ value: `runner:${r.name}`, label: `On ${r.name}${notes.length ? ` (${notes.join(", ")})` : ""}` });
  }
  return out;
}

/** The retry's request for a choice of `retryChoices`. */
export function retryBody(choice: string): { runner?: string; elsewhere?: boolean } {
  if (choice === "elsewhere") return { elsewhere: true };
  if (choice.startsWith("runner:")) return { runner: choice.slice("runner:".length) };
  return {};
}

/** Where a placement asks a job to run: `on X`, `anywhere but X`; null for anywhere. */
export function placementWords(p: Placement | null | undefined): string | null {
  if (p?.runner) return `on ${p.runner}`;
  if (p?.avoid) return `anywhere but ${p.avoid}`;
  return null;
}

/** What a retry did, said once it's asked. */
export function retriedWords(r: Retried): string {
  const where = placementWords(r.placement);
  const what = r.kind === "ci" ? `Attempt ${r.attempt ?? "?"} of the run started` : "Retrying";
  return where ? `${what}: ${r.kind === "ci" ? "its leg runs" : "it runs"} ${where}` : what;
}

/** Why a leg waits for the runner its retry asked for (wait reason `pinned`). */
export function pinnedSentence(runner: string, state: string | undefined): string {
  switch (state) {
    case "offline":
      return `Waiting for ${runner}, where its retry asked it to run, to connect.`;
    case "draining":
      return `Waiting for ${runner}, where its retry asked it to run: it's draining.`;
    case "unable":
      return `${runner}, where its retry asked it to run, can't run it now: retry it elsewhere.`;
    default:
      return `Waiting for room on ${runner}, where its retry asked it to run.`;
  }
}
