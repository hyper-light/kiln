import type { KindInfo } from "@/api/harness";

/** Each harness's effort levels, lowest first, for runners that don't report them yet. */
const EFFORTS: Record<string, string[]> = { claude: ["low", "medium", "high", "xhigh", "max"], codex: ["low", "medium", "high", "xhigh", "max", "ultra"] };
const capital = (v: string) => v.charAt(0).toUpperCase() + v.slice(1);
/** An effort level as people read it: "xhigh" is "Extra high". */
const effortLabel = (id: string) => (id === "xhigh" ? "Extra high" : capital(id));
/** The chosen model's levels, or the native default's before one is chosen; older runners' own
 * effort choices remain the fallback. */
export const effortsFor = (k: KindInfo | undefined, model = ""): { id: string; label: string; hint?: string }[] => {
  const chosen = model.trim() ? k?.models?.find((m) => m.id === model.trim()) : k?.models?.find((m) => m.default);
  if (chosen?.efforts) {
    const named = new Map((k?.efforts ?? []).map((e) => [e.id, e]));
    return chosen.efforts.map((id) => ({ id, label: named.get(id)?.label || effortLabel(id), hint: named.get(id)?.description || undefined }));
  }
  return k?.efforts?.map((e) => ({ id: e.id, label: e.label || effortLabel(e.id), hint: e.description || undefined })) ?? (EFFORTS[k?.id ?? ""] ?? []).map((id) => ({ id, label: effortLabel(id) }));
};

/** A resumed conversation keeps its model, which may not take the new agent's chosen effort. */
export const resumeEffort = (k: KindInfo | undefined, model: string | undefined, effort: string): string | undefined => {
  if (!effort) return undefined;
  const levels = (model ? k?.models?.find((m) => m.id === model)?.efforts : undefined) ?? k?.efforts?.map((e) => e.id);
  return !levels || levels.includes(effort) ? effort : undefined;
};
