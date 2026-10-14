import * as React from "react";
import type { Job } from "@/api/types";
import { formatDuration, spanSeconds } from "@/lib/format";
import { useNow } from "@/lib/hooks";
import { StatusDot } from "@/components/ui/status";

const NODE_W = 212;
const NODE_H = 60;
const COL_GAP = 68;
const ROW_GAP = 14;
const PAD = 18;

interface Placed {
  job: Job;
  x: number;
  y: number;
  depth: number;
}

/** Columns by dependency depth (longest path from a root). */
export function layoutDag(jobs: Job[]) {
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

  const maxRows = Math.max(1, ...cols.map((c) => c?.length ?? 0));
  const height = PAD * 2 + maxRows * NODE_H + (maxRows - 1) * ROW_GAP;
  const placed: Placed[] = [];
  cols.forEach((col, ci) => {
    if (!col) return;
    const colH = col.length * NODE_H + (col.length - 1) * ROW_GAP;
    const offset = (height - PAD * 2 - colH) / 2;
    col.forEach((job, ri) => {
      placed.push({ job, depth: ci, x: PAD + ci * (NODE_W + COL_GAP), y: PAD + offset + ri * (NODE_H + ROW_GAP) });
    });
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
  running: 0, failed: 1, timed_out: 2, cancelled: 3, queued: 4, pending: 5, succeeded: 6, skipped: 7,
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

export function JobDag({ jobs, selected, onSelect }: { jobs: Job[]; selected: string | null; onSelect: (id: string) => void }) {
  const now = useNow();
  const { nodes, groups } = React.useMemo(() => groupMatrix(jobs), [jobs]);
  const { placed, width, height } = React.useMemo(() => layoutDag(nodes), [nodes]);
  const pos = new Map(placed.map((p) => [p.job.key, p]));
  const gradId = React.useId().replace(/:/g, "");

  const edges: { d: string; active: boolean; key: string }[] = [];
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
      });
    }
  }

  return (
    <div className="overflow-x-auto" role="group" aria-label="Job dependency graph">
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
            <path key={e.key} d={e.d} className="dag-edge" data-active={e.active} style={e.active ? { stroke: `url(#${gradId})` } : undefined} />
          ))}
          {edges.map((e) => {
            const m = /([\d.]+),([\d.]+)$/.exec(e.d);
            return m ? <circle key={`${e.key}-end`} cx={Number(m[1]) - 0.5} cy={Number(m[2])} r={2} fill={e.active ? "#bab0e3" : "#3b424e"} /> : null;
          })}
        </svg>
        {placed.map(({ job, x, y }) => {
          const dur = spanSeconds(job.started_at, job.finished_at, now);
          const group = groups.get(job.key);
          const isSelected = group ? group.members.some((m) => m.id === selected) : selected === job.id;
          // A group selects its most interesting leg: failing or running first.
          const target = group
            ? ([...group.members].sort((a, b) => (STATUS_RANK[a.status] ?? 9) - (STATUS_RANK[b.status] ?? 9))[0]?.id ?? job.id)
            : job.id;
          const summary = group
            ? Object.entries(group.counts)
                .sort((a, b) => (STATUS_RANK[a[0]] ?? 9) - (STATUS_RANK[b[0]] ?? 9))
                .map(([st, n]) => `${n} ${st.replace("_", " ")}`)
                .join(" · ")
            : null;
          return (
            <button
              key={job.id}
              type="button"
              className="dag-node"
              data-status={job.status}
              data-selected={isSelected}
              data-group={group ? "true" : undefined}
              style={{ left: x, top: y, width: NODE_W, height: NODE_H }}
              onClick={() => onSelect(target)}
              aria-pressed={isSelected}
              aria-label={`${job.display}: ${job.status}`}
            >
              <span className="flex min-w-0 items-center gap-2">
                <StatusDot status={job.status} />
                <span className="min-w-0 flex-1 truncate text-[12.5px] text-[#e4e5e7]">{job.display}</span>
                {job.attempt > 1 && <span className="font-mono text-[9px] text-[#cbb69a]">×{job.attempt}</span>}
              </span>
              <span className="flex min-w-0 items-center gap-2 font-mono text-[10px] text-[#8c8f98]">
                <span className={job.status === "running" ? "text-[#d8d0f2] tabular-nums" : "tabular-nums"}>{dur === null ? job.status.replace("_", " ") : formatDuration(dur)}</span>
                {summary ? <span className="min-w-0 truncate">· {summary}</span> : job.agent && <span className="min-w-0 truncate">· {job.agent}</span>}
                {job.executor && (
                  <span className="ml-auto shrink-0 rounded-full border border-white/10 px-1.5 text-[8.5px] tracking-[0.06em] text-[#a9acb5] uppercase">
                    {job.executor === "baremetal" ? "metal" : job.executor}
                  </span>
                )}
              </span>
            </button>
          );
        })}
      </div>
    </div>
  );
}
