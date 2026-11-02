/**
 * A workflow's stages and uses as typed (docs/ci.md W29-R05, as the approved still pl-3 draws a draft):
 * read from the text as it's written, each line the server's where it had one; a job or a use the text
 * adds is a line of its own, marked new (`state: "added"`), and one it takes out is gone. The other
 * groups stay as the server gave them.
 */
import { parseDocument } from "yaml";
import type { ShipPipelineGroup, ShipPipelineLine } from "@/api/ship";

type Js = Record<string, unknown>;
const obj = (v: unknown): Js => (v && typeof v === "object" && !Array.isArray(v) ? (v as Js) : {});
const strs = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : v === undefined || v === null ? [] : [String(v)]);

/** Jobs by stage: those that need nothing first, then each job after the last of what it needs (as the server's stages). */
export function stagesOf(jobs: Js): string[][] {
  const depth = new Map<string, number>();
  const needsOf = (id: string) => strs(obj(jobs[id]).needs).filter((n) => n in jobs);
  for (let pass = 0; pass < 64; pass++)
    for (const id of Object.keys(jobs)) {
      if (depth.has(id)) continue;
      const n = needsOf(id);
      if (n.every((x) => depth.has(x))) depth.set(id, n.length ? Math.max(...n.map((x) => depth.get(x) ?? 0)) + 1 : 0);
    }
  // A cycle's jobs: in the first stage, where the YAML's problems say why.
  for (const id of Object.keys(jobs)) if (!depth.has(id)) depth.set(id, 0);
  const out: string[][] = [];
  for (const id of Object.keys(jobs)) (out[depth.get(id) ?? 0] ??= []).push(id);
  return out.filter(Boolean);
}

/** A job the text adds, worded as the server words a job: its labels, where it runs, what it needs. */
function newJob(id: string, j: Js): ShipPipelineLine {
  const labels = strs(j["runs-on"]).join(", ");
  let value = j.uses ? `calls ${String(j.uses)}` : [labels, j.container ? "in a container" : "bare metal"].filter(Boolean).join(" · ");
  if (strs(j.needs).length) value += ` · needs ${strs(j.needs).join(", ")}`;
  return { key: `jobs.${id}`, kind: "job", name: String(j.name ?? id), value, state: "added" };
}

/** A use the text adds: new, and asked to be pinned unless it names a whole commit. */
function newUse(u: string): ShipPipelineLine {
  const ref = u.includes("@") ? u.slice(u.lastIndexOf("@") + 1) : "";
  return { key: `uses.${u}`, kind: "use", name: u, value: `typed · new${/^[0-9a-f]{40}$/.test(ref) || u.startsWith("./") ? "" : " · pin it?"}`, state: "added" };
}

/** Each job's `uses:` (a reusable workflow's call) and its steps', once each, in the file's order. */
function usesOf(jobs: Js): string[] {
  const out: string[] = [];
  for (const j of Object.values(jobs).map(obj)) {
    if (j.uses) out.push(String(j.uses));
    for (const s of Array.isArray(j.steps) ? j.steps.map(obj) : []) if (s.uses) out.push(String(s.uses));
  }
  return [...new Set(out)];
}

/** `groups` with their stages and uses as `text` has them; null where the text isn't YAML yet, or has no jobs to read. */
export function typedGroups(text: string, groups: ShipPipelineGroup[]): ShipPipelineGroup[] | null {
  const d = parseDocument(text);
  if (d.errors.length) return null;
  const jobs = obj(obj(d.toJS()).jobs);
  if (!Object.keys(jobs).length) return null;
  const had = new Map(groups.flatMap((g) => g.lines.map((l) => [l.key, l] as const)));
  const stages: ShipPipelineGroup[] = stagesOf(jobs).map((ids, i) => ({ name: `Stage ${i + 1}`, lines: ids.map((id) => had.get(`jobs.${id}`) ?? newJob(id, obj(jobs[id]))) }));
  const uses = usesOf(jobs).map((u) => had.get(`uses.${u}`) ?? newUse(u));
  const out: ShipPipelineGroup[] = [];
  let placed = false;
  for (const g of groups) {
    if (g.name.startsWith("Stage ")) {
      if (!placed) out.push(...stages);
      placed = true;
    } else out.push(g.name === "Uses" ? { ...g, lines: uses } : g);
  }
  // No stages before (a file that had no jobs): after its triggers.
  if (!placed) out.splice(out.findIndex((g) => g.name === "Triggers") + 1, 0, ...stages);
  return out;
}

/** What the text adds, as the strip says it: `helm-lint`, `azure/setup-helm@v4`. */
export const addedOf = (groups: ShipPipelineGroup[]): ShipPipelineLine[] => groups.flatMap((g) => g.lines).filter((l) => l.state === "added");
