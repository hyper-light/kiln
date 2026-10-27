/**
 * Starting an agent, decided: the harnesses your machines have, where the one picked runs, its
 * folder, its permissions, model, effort and speed, and the spec that starts it. Pure, so Mission
 * control's composer (new-agent.tsx) and its tests share one reading.
 */
import type { ControlOption, KindInfo, ModeOption, Spec } from "@/api/harness";
import type { Agent, ReachRunner } from "@/api/types";

/** Each harness's own permission modes, for runners that don't report them (what the machine reports wins). */
const MODES: Record<string, ModeOption[]> = {
  claude: [
    { id: "default", label: "Ask first", description: "Asks before commands and edits" },
    { id: "acceptEdits", label: "Edits OK", description: "Edits files freely, asks before commands" },
    { id: "plan", label: "Plan first", description: "Researches and proposes a plan; changes nothing until you approve" },
  ],
  codex: [
    { id: "untrusted", label: "Ask first", description: "Asks before anything beyond reading" },
    { id: "auto", label: "Auto", description: "Works in its folder; asks to go outside it or online" },
    { id: "read-only", label: "Read only", description: "Reads and answers; changes nothing" },
  ],
};
export const modesFor = (k: KindInfo | undefined): ModeOption[] => k?.modes ?? MODES[k?.id ?? ""] ?? [];

/** Every harness any machine has, once each: those installed somewhere first. */
export function harnessesOf(byTarget: Record<string, KindInfo[]>): KindInfo[] {
  const m = new Map<string, KindInfo>();
  for (const ks of Object.values(byTarget))
    for (const k of ks) {
      const cur = m.get(k.id);
      m.set(k.id, cur ? { ...cur, available: cur.available || k.available, verified: cur.verified || k.verified } : k);
    }
  return [...m.values()].sort((a, b) => Number(b.available) - Number(a.available));
}

/** The harness wanted, unless no machine has it: then the first one that some machine has. */
export function kindOf(all: KindInfo[], wanted: string): string {
  const installed = all.filter((k) => k.available);
  return installed.length > 0 && !installed.some((k) => k.id === wanted) ? (installed[0]?.id ?? wanted) : wanted;
}

/** The machines that have harness `kind`. */
export const machinesWith = (byTarget: Record<string, KindInfo[]>, kind: string): string[] =>
  Object.entries(byTarget)
    .filter(([, ks]) => ks.some((x) => x.id === kind && x.available))
    .map(([t]) => t);

/** The first of `wanted` (picked, handed over, used last) that has the harness, else the first that does. */
export const targetOf = (where: string[], wanted: (string | null | undefined)[]): string => wanted.find((t): t is string => Boolean(t) && where.includes(t ?? "")) ?? where[0] ?? "";

/** A machine as people know it: the kiln server, or its name (with its hostname when that differs). */
export function machineLabel(target: string, machines: Agent[]): string {
  if (target === "local") return "kiln server";
  const n = target.replace(/^agent:/, "");
  const m = machines.find((x) => x.name === n);
  return m?.hostname && m.hostname !== n ? `${n} (${m.hostname})` : n;
}

/**
 * The permissions a new agent starts in: a mode that turns checks off only as confirmed for this
 * start (`unchecked`, never remembered); a remembered or handed-over one if it's safe; else the
 * harness's default; else its first safe one.
 */
export function modeOf(modes: ModeOption[], { unchecked, remembered, fallback }: { unchecked: string; remembered?: string; fallback?: string }): string {
  if (unchecked && modes.some((m) => m.id === unchecked)) return unchecked;
  const safe = (id: string | undefined): id is string => Boolean(id) && modes.some((m) => m.id === id && !m.dangerous);
  if (safe(remembered)) return remembered;
  if (safe(fallback)) return fallback;
  return modes.find((m) => !m.dangerous)?.id ?? "";
}

/** The model chosen, if the harness on that machine offers it (else none: the harness's own), and the one the line shows. */
export function modelOf(k: KindInfo | undefined, chosen: string): { chosen: string; shown: string } {
  const models = k?.models ?? [];
  const ok = models.some((m) => m.id === chosen) ? chosen : "";
  return { chosen: ok, shown: ok || models.find((m) => m.default)?.id || models[0]?.id || "" };
}

/** The speeds past standard the model runs at; with none chosen, its default model's. */
export function speedsFor(k: KindInfo | undefined, model: string): ControlOption[] {
  const models = k?.models ?? [];
  const m = model ? models.find((x) => x.id === model) : models.find((x) => x.default);
  return m?.speeds ?? [];
}

/** A remembered choice, if it's still one of these; else none. */
export const oneOf = (options: { id: string }[], v: string | undefined): string => (v && options.some((o) => o.id === v) ? v : "");

export interface Choices {
  kind: string;
  cwd: string;
  shell: string;
  mode: string;
  model: string;
  effort: string;
  speed: string;
  name: string;
  prompt: string;
  reach: string;
  volume: boolean;
}

/** The spec that starts it. In a shell it starts where the shell is; "~" (or nothing) is the machine's home. */
export function specOf(c: Choices): Spec {
  const cwd = c.cwd.trim();
  return {
    kind: c.kind,
    cwd: c.shell || !cwd || cwd === "~" ? undefined : cwd,
    mode: c.mode || undefined,
    model: c.model.trim() || undefined,
    effort: c.effort || undefined,
    speed: c.speed || undefined,
    name: c.name.trim() || undefined,
    prompt: c.prompt.trim() || undefined,
    shell: c.shell || undefined,
    reach: c.reach || undefined,
    workspace: c.volume && !c.shell ? { kind: "volume" } : undefined,
  };
}

/** The folders used on each machine, most recent first (eight at most), with `cwd` now the latest on `target`. */
export function withDir(dirs: Record<string, string[]>, target: string, cwd: string): Record<string, string[]> {
  const d = cwd.trim();
  if (!d || d === "~") return dirs;
  return { ...dirs, [target]: [d, ...(dirs[target] ?? []).filter((x) => x !== d)].slice(0, 8) };
}

/** Where an agent's commands can run, besides its own machine: each other runner that takes them, and each platform they make up. */
export function reachChoices(runners: ReachRunner[], target: string): { value: string; label: string }[] {
  const others = runners.filter((r) => `agent:${r.name}` !== target);
  const platforms = [...new Set(others.map((r) => `${r.os}/${r.arch}`))].sort();
  return [...platforms.map((p) => ({ value: p, label: `Prefer any ${p}` })), ...others.map((r) => ({ value: r.name, label: `Prefer ${r.name} (${r.os}/${r.arch})` }))];
}
