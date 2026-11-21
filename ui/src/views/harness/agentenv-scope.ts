/**
 * Environments' scopes, decided (docs/agents.md, "Scopes, stacks, and applied on its own"): a scope
 * matched to a session, a session to be as scopes see it, and a stack's row moved. What Mission
 * control's composer needs of them, apart from agentenv-logic.ts (the Engine tab's), so its page
 * carries no more.
 */
import type { AgentenvChip, AgentenvFacts } from "@/api/agentenvs";

/**
 * Scope `scope` matches a session that is `facts` (crates/kiln/src/agentenvs, held to the same
 * testdata/scope-cases.json): chips of one key are any of them, chips of different keys all of them;
 * a key the session has no value for (its agent, its effort) matches none of its chips. No chips:
 * every session.
 */
export function scopeMatches(scope: readonly AgentenvChip[], facts: AgentenvFacts): boolean {
  return [...new Set(scope.map((c) => c.key))].every((k) => {
    const v = facts[k];
    return v !== undefined && scope.some((c) => c.key === k && c.value === v);
  });
}

/** A machine's architecture as OCI names it (runners say `x86_64`, `aarch64`). */
export const ociArch = (arch: string) => ({ x86_64: "amd64", aarch64: "arm64" })[arch] ?? arch;

/** What a session to be started is, as scopes see it: the composer's machine (`local`: the server), harness, model, permissions and effort. */
export function factsOf(at: { target: string; machines: readonly { name: string; os: string; arch: string }[]; harness: string; model: string; mode: string; effort: string }): AgentenvFacts {
  const machine = at.target === "local" ? "local" : at.target.replace(/^agent:/, "");
  const m = at.machines.find((x) => x.name === machine);
  return {
    machine,
    os: m?.os ?? "",
    arch: m ? ociArch(m.arch) : "",
    harness: at.harness,
    model: at.model,
    permission: `${at.harness}:${at.mode}`,
    ...(at.effort ? { effort: `${at.harness}:${at.effort}` } : {}),
  };
}

/** Item `i` moved by `by` (−1 up, +1 down), the others as they were. */
export function moved<T>(list: readonly T[], i: number, by: -1 | 1): T[] {
  const j = i + by;
  if (j < 0 || j >= list.length) return [...list];
  const next = [...list];
  [next[i], next[j]] = [next[j]!, next[i]!];
  return next;
}
