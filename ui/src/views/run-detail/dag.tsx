import * as React from "react";
import type { CiPlanJob } from "@/api/ci";
import type { Job } from "@/api/types";
import { formatDuration, spanSeconds } from "@/lib/format";
import { useElementWidth, useNow } from "@/lib/hooks";
import { StatusDot } from "@/components/ui/status";
import { cn } from "@/lib/utils";

const NODE_W = 212;
const NODE_H = 60;
const COL_GAP = 68;
const ROW_GAP = 14;
const PAD = 18;

interface Placed {
  job: Job;
  x: number;
  y: number;
  height: number;
  depth: number;
}

/** Columns by dependency depth (longest path from a root), each node `heightOf` tall. */
export function layoutDag(jobs: Job[], heightOf: (job: Job) => number = () => NODE_H) {
  const byKey = new Map(jobs.map((j) => [j.key, j]));
  const depth = new Map<string, number>();
  const visiting = new Set<string>();
  const depthOf = (key: string): number => {
    const cached = depth.get(key);
    if (cached !== undefined) return cached;
    if (visiting.has(key)) return 0; // cycle guard
    visiting.add(key);
    const j = byKey.get(key);
    const d = j && j.needs.length ? 1 + Math.max(...j.needs.map((n) => (byKey.has(n) ? depthOf(n) : -1))) : 0;
    visiting.delete(key);
    depth.set(key, Math.max(0, d));
    return Math.max(0, d);
  };
  jobs.forEach((j) => depthOf(j.key));

  const cols: Job[][] = [];
  jobs.forEach((j) => {
    const d = depth.get(j.key) ?? 0;
    (cols[d] ??= []).push(j);
  });
  // Order nodes within a column by the mean row of their parents (reduces crossings).
  const rowOf = new Map<string, number>();
  cols.forEach((col, ci) => {
    if (ci > 0) {
      col.sort((a, b) => {
        const m = (j: Job) => {
          const rs = j.needs.map((n) => rowOf.get(n)).filter((x): x is number => x !== undefined);
          return rs.length ? rs.reduce((s, x) => s + x, 0) / rs.length : 0;
        };
        return m(a) - m(b);
      });
    }
    col.forEach((j, i) => rowOf.set(j.key, i));
  });

  const colHeight = (col: Job[]) => col.reduce((sum, j) => sum + heightOf(j), 0) + (col.length - 1) * ROW_GAP;
  const tallest = Math.max(NODE_H, ...cols.map((c) => (c ? colHeight(c) : 0)));
  const height = PAD * 2 + tallest;
  const placed: Placed[] = [];
  cols.forEach((col, ci) => {
    if (!col) return;
    let y = PAD + (tallest - colHeight(col)) / 2;
    for (const job of col) {
      const h = heightOf(job);
      placed.push({ job, depth: ci, x: PAD + ci * (NODE_W + COL_GAP), y, height: h });
      y += h + ROW_GAP;
    }
  });
  const width = PAD * 2 + cols.length * NODE_W + Math.max(0, cols.length - 1) * COL_GAP;
  return { placed, width, height };
}

/** Matrix legs (`id (k=v, …)`) of one job id collapse into a single node. */
function specId(key: string) {
  const i = key.indexOf(" (");
  return i > 0 ? key.slice(0, i) : key;
}

const STATUS_RANK: Record<string, number> = {
  running: 0,
  failed: 1,
  timed_out: 2,
  cancelled: 3,
  queued: 4,
  pending: 5,
  succeeded: 6,
  skipped: 7,
};

export interface DagGroup {
  members: Job[];
  counts: Record<string, number>;
}

/** Replace each set of matrix legs with one synthetic job keyed by the job id. */
export function groupMatrix(jobs: Job[]): { nodes: Job[]; groups: Map<string, DagGroup> } {
  const bySpec = new Map<string, Job[]>();
  for (const j of jobs) {
    const id = specId(j.key);
    (bySpec.get(id) ?? bySpec.set(id, []).get(id)!).push(j);
  }
  const keyToNode = new Map<string, string>();
  for (const [id, members] of bySpec) for (const m of members) keyToNode.set(m.key, members.length > 1 ? id : m.key);
  const groups = new Map<string, DagGroup>();
  const nodes: Job[] = [];
  for (const [id, members] of bySpec) {
    const needs = [...new Set(members.flatMap((m) => m.needs.map((n) => keyToNode.get(n) ?? n)))];
    if (members.length === 1 && members[0]) {
      nodes.push({ ...members[0], needs });
      continue;
    }
    const counts: Record<string, number> = {};
    for (const m of members) counts[m.status] = (counts[m.status] ?? 0) + 1;
    const first = members[0]!;
    const status = members.map((m) => m.status).sort((a, b) => (STATUS_RANK[a] ?? 9) - (STATUS_RANK[b] ?? 9))[0] ?? first.status;
    const starts = members.map((m) => m.started_at).filter((x): x is number => x != null);
    const ends = members.map((m) => m.finished_at);
    const executors = [...new Set(members.map((m) => m.executor).filter(Boolean))];
    groups.set(id, { members, counts });
    nodes.push({
      ...first,
      id: `group:${id}`,
      key: id,
      display: `${id} × ${members.length}`,
      status,
      needs,
      agent: null,
      attempt: 0,
      executor: executors.length === 1 ? (executors[0] ?? null) : null,
      started_at: starts.length ? Math.min(...starts) : null,
      finished_at: ends.every((e) => e != null) && ends.length ? Math.max(...(ends as number[])) : null,
    });
  }
  return { nodes, groups };
}

/** What a CI run adds to a node (docs/ci.md W21-R04): why a leg waits, and whether a rerun kept it. */
export interface DagNote {
  wait: string | null;
  reused: boolean;
}

/** The most matrix legs a node shows as cells without their values; more are counted instead. */
const MAX_CELLS = 18;

/**
 * A matrix job's legs as a grid inside its node (W21-R04): across, its first axis's values; down,
 * the other axes' combinations; each leg in its cell by its values, a combination left out an
 * empty one. One axis of more than `GRID_COLS` values, or legs that don't fall one to a cell,
 * wrap in order instead.
 */
interface Grid {
  /** What runs across and down, said with the grid. */
  axes: string;
  cols: number;
  cells: (Job | null)[];
  /** A cell's side and the gap between cells, px. */
  size: number;
  gap: number;
  /** One row: its axes said beside it rather than above. */
  inline: boolean;
}

const GRID_COLS = 25;
/** The room a node has for its grid: its width inside its padding and border. */
const GRID_W = NODE_W - 26;

function matrixGrid(members: Job[]): Grid | null {
  const ms = members.map((m) => m.matrix);
  if (!ms.every((m): m is Record<string, unknown> => !!m)) return null;
  // Its axes: the keys every leg has (`include` can add keys to some).
  const axes = Object.keys(ms[0] ?? {}).filter((k) => ms.every((m) => k in m));
  const [across, ...down] = axes;
  if (across === undefined) return null;
  const at = (m: Record<string, unknown>, k: string) => JSON.stringify(m[k]);
  const rowOf = (m: Record<string, unknown>) => down.map((k) => at(m, k)).join("\u0000");
  const colIndex = new Map<string, number>();
  const rowIndex = new Map<string, number>();
  for (const m of ms) {
    if (!colIndex.has(at(m, across))) colIndex.set(at(m, across), colIndex.size);
    if (!rowIndex.has(rowOf(m))) rowIndex.set(rowOf(m), rowIndex.size);
  }
  let cols = colIndex.size;
  let cells: (Job | null)[] = new Array<Job | null>(cols * rowIndex.size).fill(null);
  let placed = cols <= GRID_COLS;
  ms.forEach((m, i) => {
    const cell = (rowIndex.get(rowOf(m)) ?? 0) * cols + (colIndex.get(at(m, across)) ?? 0);
    if (cells[cell]) placed = false;
    else cells[cell] = members[i] ?? null;
  });
  if (!placed) {
    cols = Math.min(GRID_COLS, members.length);
    cells = members;
  }
  const gap = cols > 16 ? 1 : 2;
  const size = Math.max(4, Math.min(9, Math.floor((GRID_W - (cols - 1) * gap) / cols)));
  const label = !placed ? `${axes.join(" × ")}, in order` : down.length ? `${across} → · ${down.join(" × ")} ↓` : `${across} →`;
  return { axes: label, cols, cells, size, gap, inline: cells.length <= cols };
}

function gridHeight(g: Grid) {
  const rows = Math.ceil(g.cells.length / g.cols);
  return rows * g.size + (rows - 1) * g.gap;
}

/** A node's height: a grid adds its own and a line for its axes (beside a one-row grid). */
const heightWith = (g: Grid | undefined) => (g ? NODE_H + (g.inline ? 16 : 20 + gridHeight(g)) : NODE_H);

const STEP_DONE = new Set(["succeeded", "failed", "skipped", "cancelled", "timed_out"]);

/** Steps done of all, over a node's legs: a leg whose steps aren't written yet counts its plan's. */
function stepsOf(legs: Job[], planned: number): { done: number; total: number } | null {
  let done = 0;
  let total = 0;
  for (const l of legs) {
    total += l.steps.length || planned;
    for (const st of l.steps) if (STEP_DONE.has(st.status)) done++;
  }
  return total ? { done, total } : null;
}

/** What a job whose legs aren't made yet waits for (W21-R04), from the run's plan. */
function waitingFor(p: CiPlanJob, finished: boolean): string {
  if (finished) return "didn't run";
  const names = p.needs.length > 2 ? `${p.needs.slice(0, 2).join(", ")} +${p.needs.length - 2}` : p.needs.join(" and ");
  if (p.matrix_waits) return `its matrix waits on the outputs of ${names}`;
  return names ? `waits for ${names}` : "waits to start";
}

/**
 * The jobs and what each needs, read left to right. Where the graph doesn't fit the width it has (a
 * phone, a long pipeline), its columns stack instead, one under the other: nothing scrolls sideways.
 * A CI run also draws its critical path heavier, shows a matrix job's legs as a grid of cells by
 * state, each node's steps done and its time against how long it typically takes, why a queued leg
 * waits, and (from its plan) the jobs whose legs aren't made yet, with what they wait for.
 */
export function JobDag({
  jobs,
  selected,
  onSelect,
  critical,
  notes,
  plan,
  finished,
}: {
  jobs: Job[];
  selected: string | null;
  onSelect: (id: string) => void;
  critical?: Set<string>;
  notes?: Map<string, DagNote>;
  plan?: CiPlanJob[];
  finished?: boolean;
}) {
  const now = useNow();
  const [ref, avail] = useElementWidth<HTMLDivElement>();
  const { nodes, groups, grids, waiting, planned } = React.useMemo(() => {
    const { nodes, groups } = groupMatrix(jobs);
    const grids = new Map<string, Grid>();
    if (notes) {
      for (const [key, g] of groups) {
        const grid = matrixGrid(g.members);
        if (grid) grids.set(key, grid);
      }
    }
    // The plan's jobs with no legs yet: a node each, drawn from what it needs. A job calling a
    // workflow has that workflow's jobs as its legs (`call/test`).
    const keys = nodes.map((n) => n.key);
    const made = (job: string) => keys.filter((k) => specId(k) === job || k.startsWith(`${job}/`));
    const planned = new Map((plan ?? []).map((p) => [p.job, p]));
    const waiting = new Map((plan ?? []).filter((p) => made(p.job).length === 0).map((p) => [p.job, p]));
    const stubs: Job[] = [...waiting.values()].map((p) => ({
      id: `plan:${p.job}`,
      run_id: "",
      key: p.job,
      display: p.name ?? p.job,
      status: "pending",
      labels: [],
      needs: p.needs.flatMap((n) => (waiting.has(n) ? [n] : made(n))),
      agent: null,
      executor: null,
      attempt: 0,
      queued_at: null,
      started_at: null,
      finished_at: null,
      exit_code: null,
      error: null,
      peak_rss: null,
      cpu_seconds: null,
      steps: [],
    }));
    return { nodes: [...nodes, ...stubs], groups, grids, waiting, planned };
  }, [jobs, notes, plan]);
  const { placed, width, height } = React.useMemo(() => layoutDag(nodes, (j) => heightWith(grids.get(j.key))), [nodes, grids]);
  const pos = new Map(placed.map((p) => [p.job.key, p]));
  const gradId = React.useId().replace(/:/g, "");
  const fits = avail === 0 || width <= avail;

  const node = (job: Job, style?: React.CSSProperties) => {
    const waits = job.id.startsWith("plan:") ? waiting.get(job.key) : undefined;
    if (waits) return stub(job, waits, style);
    const dur = spanSeconds(job.started_at, job.finished_at, now);
    const group = groups.get(job.key);
    const grid = grids.get(job.key);
    const p = notes ? planned.get(specId(job.key)) : undefined;
    const steps = notes && dur !== null ? stepsOf(group ? group.members : [job], p?.steps ?? 0) : null;
    const typical = p?.typical ?? null;
    const isSelected = group ? group.members.some((m) => m.id === selected) : selected === job.id;
    // A group selects its most interesting leg: failing or running first.
    const target = group ? ([...group.members].sort((a, b) => (STATUS_RANK[a.status] ?? 9) - (STATUS_RANK[b.status] ?? 9))[0]?.id ?? job.id) : job.id;
    const summary = group
      ? Object.entries(group.counts)
          .sort((a, b) => (STATUS_RANK[a[0]] ?? 9) - (STATUS_RANK[b[0]] ?? 9))
          .map(([st, n]) => `${n} ${st.replace("_", " ")}`)
          .join(" · ")
      : null;
    const note = group ? undefined : notes?.get(job.id);
    const cells = group && notes && !grid && group.members.length <= MAX_CELLS ? group.members : null;
    const kept = group ? group.members.every((m) => notes?.get(m.id)?.reused) : note?.reused;
    return (
      <button
        key={job.id}
        type="button"
        className="dag-node"
        data-flow={style ? undefined : ""}
        data-status={job.status}
        data-selected={isSelected}
        data-group={group ? "true" : undefined}
        style={style}
        onClick={() => onSelect(target)}
        aria-pressed={isSelected}
        aria-label={`${job.display}: ${job.status}`}
      >
        <span className="flex min-w-0 items-center gap-2">
          <StatusDot status={job.status} />
          <span className="min-w-0 flex-1 truncate text-[12.5px] text-[#e4e5e7]">{job.display}</span>
          {job.attempt > 1 && <span className="font-mono text-[9px] text-[#cbb69a]">×{job.attempt}</span>}
          {kept && <span className="font-mono text-[9px] text-[#93a1b4]">kept</span>}
        </span>
        <span className="flex min-w-0 items-center gap-2 font-mono text-[10px] text-[#8c8f98]">
          <span className={cn("shrink-0 tabular-nums", job.status === "running" && "text-[#d8d0f2]")} title={typical !== null ? `typically ${formatDuration(typical)}` : undefined}>
            {dur === null ? job.status.replace("_", " ") : job.status === "running" && typical !== null ? `${formatDuration(dur)} of ~${formatDuration(typical)}` : formatDuration(dur)}
          </span>
          {steps && (
            <span className="shrink-0 tabular-nums" title={`${steps.done} of ${steps.total} steps done`}>
              {steps.done}/{steps.total}
            </span>
          )}
          {cells ? (
            <span className="dag-cells" aria-label={summary ?? undefined}>
              {cells.map((m) => (
                <i key={m.id} className="dag-cell" data-status={m.status} title={`${m.display}: ${m.status.replace("_", " ")}`} />
              ))}
            </span>
          ) : summary ? (
            <span className="min-w-0 truncate">· {summary}</span>
          ) : note?.wait ? (
            <span className="min-w-0 truncate" title={note.wait}>
              · {note.wait}
            </span>
          ) : (
            job.agent && <span className="min-w-0 truncate">· {job.agent}</span>
          )}
          {job.executor && (
            <span className="ml-auto shrink-0 rounded-full border border-white/10 px-1.5 text-[8.5px] tracking-[0.06em] text-[#a9acb5] uppercase">
              {job.executor === "baremetal" ? "metal" : job.executor}
            </span>
          )}
        </span>
        {grid && (
          <span className={grid.inline ? "flex min-w-0 items-center gap-2" : "grid gap-1"}>
            <span className="dag-axes">{grid.axes}</span>
            <span
              className="dag-grid"
              role="img"
              aria-label={summary ?? undefined}
              style={{ "--cell": `${grid.size}px`, gap: grid.gap, gridTemplateColumns: `repeat(${grid.cols}, var(--cell))` } as React.CSSProperties}
            >
              {grid.cells.map((m, i) =>
                m ? <i key={m.id} className="dag-cell" data-status={m.status} title={`${m.display}: ${m.status.replace("_", " ")}`} /> : <i key={`-${i}`} className="dag-cell" data-empty="" />,
              )}
            </span>
          </span>
        )}
      </button>
    );
  };

  // A job whose legs aren't made yet: not one to open, and said as what it waits for.
  const stub = (job: Job, p: CiPlanJob, style?: React.CSSProperties) => {
    const why = waitingFor(p, !!finished);
    return (
      <div key={job.id} className="dag-node" data-waiting="" data-flow={style ? undefined : ""} data-status="pending" style={style} role="group" aria-label={`${job.display}: ${why}`}>
        <span className="flex min-w-0 items-center gap-2">
          <StatusDot status="pending" />
          <span className="min-w-0 flex-1 truncate text-[12.5px] text-[#c8cad0]">{job.display}</span>
        </span>
        <span className="flex min-w-0 items-center gap-2 font-mono text-[10px] text-[#8c8f98]">
          <span className="min-w-0 truncate" title={why}>
            {why}
          </span>
          {p.typical !== null && (
            <span className="ml-auto shrink-0 tabular-nums" title="how long it typically takes">
              ~{formatDuration(p.typical)}
            </span>
          )}
        </span>
      </div>
    );
  };

  if (!fits) {
    const stages: Job[][] = [];
    for (const p of placed) (stages[p.depth] ??= []).push(p.job);
    return (
      <div ref={ref} role="group" aria-label="Job dependency graph">
        <ol className="dag-stages">
          {stages.map((stage, i) => (
            <li key={i} className="dag-stage">
              {stage.map((j) => node(j))}
            </li>
          ))}
        </ol>
      </div>
    );
  }

  const edges: { d: string; active: boolean; critical: boolean; key: string }[] = [];
  for (const p of placed) {
    for (const n of p.job.needs) {
      const from = pos.get(n);
      if (!from) continue;
      const x1 = from.x + NODE_W;
      const y1 = from.y + NODE_H / 2;
      const x2 = p.x;
      const y2 = p.y + NODE_H / 2;
      const mx = (x1 + x2) / 2;
      edges.push({
        key: `${n}->${p.job.key}`,
        d: `M${x1},${y1} C${mx},${y1} ${mx},${y2} ${x2},${y2}`,
        active: p.job.status === "running" || (p.job.status === "queued" && from.job.status === "succeeded"),
        critical: !!critical?.has(n) && critical.has(p.job.key),
      });
    }
  }

  return (
    <div ref={ref} role="group" aria-label="Job dependency graph">
      <div className="relative" style={{ width, height, minWidth: "100%" }}>
        <svg className="pointer-events-none absolute inset-0" width={width} height={height} aria-hidden="true">
          <defs>
            <linearGradient id={gradId} x1="0" x2={width} y1="0" y2="0" gradientUnits="userSpaceOnUse">
              <stop offset="0" stopColor="#b8d9d2" />
              <stop offset="0.28" stopColor="#a6c4ed" />
              <stop offset="0.53" stopColor="#b8a6d5" />
              <stop offset="0.75" stopColor="#d6a2aa" />
              <stop offset="1" stopColor="#d8cbb0" />
            </linearGradient>
          </defs>
          {edges.map((e) => (
            <path key={e.key} d={e.d} className="dag-edge" data-active={e.active} data-critical={e.critical || undefined} style={e.active ? { stroke: `url(#${gradId})` } : undefined} />
          ))}
          {edges.map((e) => {
            const m = /([\d.]+),([\d.]+)$/.exec(e.d);
            return m ? <circle key={`${e.key}-end`} cx={Number(m[1]) - 0.5} cy={Number(m[2])} r={2} fill={e.active ? "#bab0e3" : "#3b424e"} /> : null;
          })}
        </svg>
        {placed.map(({ job, x, y, height: h }) => node(job, { left: x, top: y, width: NODE_W, height: h }))}
      </div>
    </div>
  );
}
