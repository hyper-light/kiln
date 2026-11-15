// What the attach form saves (kiln plan 01a1dd2e8618733a87f2793aab1322a8 P5.1b): only what the person
// set. An Agent left as it is stays unset, so the server starts the person's own copy of kiln's agent for
// it (agentdefs.use), else kiln's own; a harness the person picks is theirs, and wins.

/** The Agent choice that leaves it to kiln: kiln's agent for it, or the person's own copy. */
export const KILNS = "";

/** An attachment as kiln keeps it, from what's asked of a person: the rest, kiln's defaults. */
export function defaultsFor({ name, on, harness, wake, playbook, machine, cwd }: { name: string; on: string; harness: string; wake: string[]; playbook: string; machine?: string; cwd?: string }) {
  return {
    name: name.trim(),
    on,
    branches: [],
    harness: { ...(harness !== KILNS && { kind: harness }), ...(machine && { on: machine }), ...(cwd && { cwd }) },
    wake,
    can: ["read"],
    limits: { sessions_per_day: 10, reruns_per_failure: 2, stuck_minutes: 15, slow_percent: 25, tokens_per_day: 5_000_000, runner_minutes_per_day: 300 },
    playbook,
    reviewers: [],
  };
}
