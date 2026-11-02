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

/**
 * A job's line as the server words it (`kiln_ci::pipelines::job_line`, the approved stills'): its id;
 * its legs where a matrix makes more than one (`3 platforms`), where it runs (bare metal unless a
 * container is asked for) and its labels beyond that, or the workflow it calls; its time in the
 * last finished run; and what it needs.
 */
export function jobLine(id: string, j: Js, secs?: number): ShipPipelineLine {
  const parts: string[] = [];
  if (j.uses) parts.push(`calls ${String(j.uses)}`);
  else {
    const m = obj(obj(j.strategy).matrix);
    const count = (a: unknown) => (Array.isArray(a) ? Math.max(1, a.length) : 1);
    // kiln's `platforms`, or an axis named for them (`platform`, `os`): pl-2's `3 platforms`.
    const named = Object.entries(m).find(([k, a]) => ["platform", "platforms", "os"].includes(k) && count(a) > 1);
    if (named) parts.push(`${count(named[1])} platforms`);
    else {
      const legs = Object.entries(m)
        .filter(([k, a]) => Array.isArray(a) && !["include", "exclude", "platforms"].includes(k))
        .reduce((n, [, a]) => n * count(a), 1);
      if (legs > 1) parts.push(`${legs} legs`);
    }
    parts.push(j.container || j.image ? "in a container" : "bare metal");
    const on = j["runs-on"];
    if (on && typeof on === "object" && !Array.isArray(on)) {
      const g = obj(on);
      if (g.group) parts.push(`group ${String(g.group)}`);
      parts.push(...strs(g.labels).filter((t) => !["baremetal", "bare-metal"].includes(t)));
    } else parts.push(...strs(on).filter((t) => !["baremetal", "bare-metal"].includes(t)));
  }
  // A job only a branch runs: pl-2's `on main`.
  const on = onBranch(j.if === undefined ? "" : String(j.if));
  if (on) parts.push(`on ${on}`);
  // Its time in the last finished run (pl-2's `18m`).
  if (secs !== undefined) parts.push(spanWords(secs));
  if (strs(j.needs).length) parts.push(`needs ${strs(j.needs).join(", ")}`);
  return { key: `jobs.${id}`, kind: "job", name: id, value: parts.join(" · ") };
}

/** The branch a job's `if:` keeps it to, where that's all it says (`kiln_ci::pipelines::on_branch`). */
function onBranch(cond: string): string | null {
  const c = cond
    .trim()
    .replace(/^\$\{\{/, "")
    .replace(/\}\}$/, "")
    .trim();
  const at = c.indexOf("==");
  if (at < 0) return null;
  const v = c
    .slice(at + 2)
    .trim()
    .replace(/^['"]+|['"]+$/g, "");
  const l = c.slice(0, at).trim();
  const b = l === "github.ref" ? (v.startsWith("refs/heads/") ? v.slice("refs/heads/".length) : null) : l === "github.ref_name" ? v : null;
  return b && !b.includes(" ") ? b : null;
}

/** A span in its largest unit, as the server says it (`kiln_ci::pipelines::span_words`): `40s`, `18m`, `2h`, `3d`, `3w`. */
export function spanWords(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  return s < 60 ? `${s}s` : s < 3600 ? `${Math.floor(s / 60)}m` : s < 86_400 ? `${Math.floor(s / 3600)}h` : s < 7 * 86_400 ? `${Math.floor(s / 86_400)}d` : `${Math.floor(s / (7 * 86_400))}w`;
}

/** A job the text adds: its line, marked new. */
const newJob = (id: string, j: Js): ShipPipelineLine => ({ ...jobLine(id, j), state: "added" });

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
