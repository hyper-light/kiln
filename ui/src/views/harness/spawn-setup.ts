/**
 * Starting an agent, decided: the harnesses your machines have, where the one picked runs, its
 * folder, its permissions, model, effort and speed, and the spec that starts it. Pure, so Mission
 * control's composer (new-agent.tsx) and its tests share one reading.
 */
import type { ControlOption, KindInfo, ModeOption, Spec } from "@/api/harness";
import type { Agent, ReachRunner } from "@/api/types";

/** Each harness's own permission modes, as its agents' pages name them (claude.rs and codex.rs `modes`), for runners that don't report them (what the machine reports wins). */
const MODES: Record<string, ModeOption[]> = {
  claude: [
    { id: "default", label: "Ask", description: "Ask before edits and commands" },
    { id: "acceptEdits", label: "Accept edits", description: "Edit files without asking; ask before commands" },
    { id: "plan", label: "Plan", description: "Read and plan only; change nothing" },
    { id: "auto", label: "Auto", description: "A classifier approves safe actions; risky ones still ask" },
    { id: "dontAsk", label: "Allowed only", description: "Never asks: refuses anything not already allowed" },
    { id: "bypassPermissions", label: "Bypass", description: "Skips every permission check", dangerous: true },
  ],
  codex: [
    { id: "auto", label: "Auto", description: "Works in its folder; asks before going beyond it" },
    { id: "read-only", label: "Read only", description: "Reads and plans; asks before any change" },
    { id: "untrusted", label: "Ask", description: "Asks before running anything it does not trust" },
    { id: "full-access", label: "Full access", description: "No sandbox, never asks", dangerous: true },
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

/** The machines that run any harness, in the order they're listed. */
export const machinesWithAny = (byTarget: Record<string, KindInfo[]>): string[] =>
  Object.entries(byTarget)
    .filter(([, ks]) => ks.some((x) => x.available))
    .map(([t]) => t);

/**
 * The harness on a machine, with its models: the machine's own list, else (no session of it has
 * listed them there yet) the list another machine has for it, so its models can be chosen before.
 */
export function onMachine(byTarget: Record<string, KindInfo[]>, target: string, kind: string): KindInfo | undefined {
  const k = byTarget[target]?.find((x) => x.id === kind);
  if (!k || (k.models?.length ?? 0) > 0) return k;
  const elsewhere = Object.values(byTarget)
    .map((ks) => ks.find((x) => x.id === kind)?.models ?? [])
    .find((ms) => ms.length > 0);
  return elsewhere ? { ...k, models: elsewhere } : k;
}

/** The machines that have harness `kind`. */
export const machinesWith = (byTarget: Record<string, KindInfo[]>, kind: string): string[] =>
  Object.entries(byTarget)
    .filter(([, ks]) => ks.some((x) => x.id === kind && x.available))
    .map(([t]) => t);

/** The first of `wanted` (picked, handed over, used last) that has the harness, else the first that does. */
export const targetOf = (where: string[], wanted: (string | null | undefined)[]): string => wanted.find((t): t is string => Boolean(t) && where.includes(t ?? "")) ?? where[0] ?? "";

/** A machine as people know it: the kiln server, or its name (with its hostname when that differs). */
/** A folder, short: its last folder under "…" when it's deeper than two ("~/…/kiln"). */
export function shortPath(cwd: string): string {
  const t = cwd.trim() || "~";
  const parts = t.split("/").filter(Boolean);
  if (parts.length <= 2) return t;
  const root = t.startsWith("~") ? "~" : "";
  return `${root}/…/${parts[parts.length - 1]}`;
}

/** A machine as the composer's slot says it, short: its name without its domain, and its host's when that's another ("kiln-a094a12d (fedora-2)"). */
export function shortMachineLabel(target: string, machines: Agent[]): string {
  if (target === "local") return "kiln server";
  const n = target.replace(/^agent:/, "");
  const first = (s: string) => s.split(".")[0] || s;
  const host = machines.find((x) => x.name === n)?.hostname;
  return host && first(host).toLowerCase() !== first(n).toLowerCase() ? `${first(n)} (${first(host)})` : first(n);
}

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

/** Claude Code's fast mode (claude.rs `fast_speed`). */
const CLAUDE_FAST: ControlOption = { id: "fast", label: "Fast", description: "Up to 2.5x faster, at a higher price per token" };

/**
 * The speeds past standard the model runs at; with none chosen, its default model's (as the line
 * shows it). A machine whose runner lists no model's speeds (before they came with its models) is
 * taken to give Claude Code's Opus models fast mode, as their agents' pages show it.
 */
export function speedsFor(k: KindInfo | undefined, model: string): ControlOption[] {
  const models = k?.models ?? [];
  const m = model ? models.find((x) => x.id === model) : (models.find((x) => x.default) ?? models[0]);
  if (m?.speeds?.length) return m.speeds;
  const said = models.some((x) => x.speeds !== undefined);
  const opus = Boolean(m) && [m?.id, m?.label, m?.id === "default" ? m.description : ""].some((t) => /(^|[-\s])opus\b/i.test(t ?? ""));
  return !said && k?.id === "claude" && opus ? [CLAUDE_FAST] : [];
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
