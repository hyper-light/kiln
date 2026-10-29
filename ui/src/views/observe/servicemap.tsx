/**
 * The service map (docs/observe.md §8.7): who calls whom over the range, from the spans: each
 * service a box (its requests and errors), each call an arrow from the caller to what it calls,
 * callers to the left of what they call (each service as far right as its longest chain of callers
 * puts it). A peer only named by its callers' spans (a database, a queue, a host) is drawn dashed.
 */
import * as React from "react";
import { observeApi, type ServiceMap } from "@/api/observe";
import { SERIES } from "@/components/charts/types";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro } from "@/components/ui/misc";
import { useRes, type Res } from "@/lib/data";
import { useNavigate } from "@/lib/router";
import { ApmTabs, latency } from "./apm";
import { ObserveNav, RANGES, RangePicker, compact, pollFor, useRange, type Range } from "./parts";

const mapRes = (range: Range): Res<ServiceMap> => ({
  key: `observe/apm/map/${range}`,
  fetch: (s) => {
    const to = Date.now();
    return observeApi.serviceMap(to - RANGES[range], to, undefined, s);
  },
  opts: { pollMs: pollFor(range) },
});

export default function ServiceMapPage() {
  const [range, setRange] = useRange("15m");
  return (
    <div className="container-console page">
      <title>Service map · APM · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Service map"
        lead="Who calls whom, from the spans: callers to the left of what they call. A peer only named by its callers (a database, a queue, a host) is drawn dashed."
      />
      <ObserveNav />
      <ApmTabs />
      <div className="mb-5">
        <RangePicker value={range} onChange={setRange} />
      </div>
      <Loadable key={range} label="the map" fallback={<Hold height={420} label="Loading the map" />}>
        <MapView range={range} />
      </Loadable>
    </div>
  );
}

const BOX_W = 176;
const BOX_H = 52;
const COL = 260;
const ROW = 76;

/** Each node's column: as far right as its longest chain of callers puts it (a cycle's calls back
 * are left out of the reckoning), and its row within its column, in the order of its callers'. */
function layout(m: ServiceMap): { at: Map<string, { x: number; y: number }>; width: number; height: number } {
  const names = m.nodes.map((n) => n.service);
  const out = new Map<string, string[]>(names.map((n) => [n, []]));
  for (const e of m.edges) if (e.from !== e.to) out.get(e.from)?.push(e.to);
  // Depth first from every node, marking the edges back to an ancestor (a cycle's): the rest is a
  // DAG, ranked by its longest paths.
  const rank = new Map<string, number>(names.map((n) => [n, 0]));
  const state = new Map<string, number>();
  const back = new Set<string>();
  for (const root of names) {
    if (state.has(root)) continue;
    const stack: [string, number][] = [[root, 0]];
    state.set(root, 1);
    while (stack.length > 0) {
      const top = stack[stack.length - 1];
      if (!top) break;
      const [n, i] = top;
      const next = out.get(n)?.[i];
      if (next === undefined) {
        state.set(n, 2);
        stack.pop();
        continue;
      }
      top[1] = i + 1;
      const s = state.get(next);
      if (s === 1) back.add(`${n}\u0000${next}`);
      else if (s === undefined) {
        state.set(next, 1);
        stack.push([next, 0]);
      }
    }
  }
  for (let round = 0; round < names.length; round++) {
    let moved = false;
    for (const e of m.edges) {
      if (e.from === e.to || back.has(`${e.from}\u0000${e.to}`)) continue;
      const want = (rank.get(e.from) ?? 0) + 1;
      if ((rank.get(e.to) ?? 0) < want) {
        rank.set(e.to, want);
        moved = true;
      }
    }
    if (!moved) break;
  }
  const columns: string[][] = [];
  for (const n of [...names].sort()) {
    const r = rank.get(n) ?? 0;
    (columns[r] ??= []).push(n);
  }
  // Each column after the first in the order of its nodes' callers' rows (the barycenter
  // heuristic's one sweep: fewer crossings).
  const row = new Map<string, number>();
  columns.forEach((col, c) => {
    if (c > 0) {
      const mean = (n: string) => {
        const ys = m.edges.filter((e) => e.to === n && row.has(e.from)).map((e) => row.get(e.from) ?? 0);
        return ys.length ? ys.reduce((a, b) => a + b, 0) / ys.length : Number.POSITIVE_INFINITY;
      };
      col.sort((a, b) => mean(a) - mean(b) || a.localeCompare(b));
    }
    col.forEach((n, i) => row.set(n, i));
  });
  const tallest = Math.max(1, ...columns.map((c) => c?.length ?? 0));
  const at = new Map<string, { x: number; y: number }>();
  columns.forEach((col, c) => col?.forEach((n, i) => at.set(n, { x: 16 + c * COL, y: 16 + (i + (tallest - col.length) / 2) * ROW })));
  return { at, width: 32 + Math.max(0, columns.length - 1) * COL + BOX_W, height: 32 + (tallest - 1) * ROW + BOX_H };
}

function MapView({ range }: { range: Range }) {
  const m = useRes(mapRes(range)).data;
  const navigate = useNavigate();
  const { at, width, height } = React.useMemo(() => layout(m), [m]);
  if (m.nodes.length === 0) return <p className="empty-line py-16 text-center">No spans in this range.</p>;
  const most = Math.max(1, ...m.edges.map((e) => e.calls));
  const nodes = new Map(m.nodes.map((n) => [n.service, n]));
  return (
    <section aria-label="Service map">
      <div className="overflow-x-auto rounded-[12px] border border-[#1d1f23] bg-[#0b0c0e]">
        <svg role="img" aria-label={`${m.nodes.length} services, ${m.edges.length} calls`} width={width} height={height} viewBox={`0 0 ${width} ${height}`} className="block">
          <defs>
            <marker id="map-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
              <path d="M0,0 L8,4 L0,8 z" fill="#5b5e66" />
            </marker>
          </defs>
          {m.edges.map((e) => {
            const a = at.get(e.from);
            const b = at.get(e.to);
            if (!a || !b) return null;
            const forward = b.x > a.x;
            const [x1, y1] = forward ? [a.x + BOX_W, a.y + BOX_H / 2] : [a.x + BOX_W / 2, a.y + BOX_H];
            const [x2, y2] = forward ? [b.x, b.y + BOX_H / 2] : [b.x + BOX_W / 2, b.y + BOX_H];
            const d = forward ? `M${x1},${y1} C${x1 + 60},${y1} ${x2 - 60},${y2} ${x2},${y2}` : `M${x1},${y1} C${x1},${y1 + 60} ${x2},${y2 + 60} ${x2},${y2}`;
            const failing = e.errors > 0;
            return (
              <path
                key={`${e.from}>${e.to}`}
                d={d}
                fill="none"
                stroke={failing ? SERIES.fail : "#5b5e66"}
                strokeOpacity={failing ? 0.8 : 1}
                strokeWidth={1 + 2.5 * Math.sqrt(e.calls / most)}
                strokeDasharray={e.inferred ? "5 4" : undefined}
                markerEnd="url(#map-arrow)"
              >
                <title>{`${e.from} → ${e.to}: ${compact(e.calls)} calls, ${compact(e.errors)} errors, ${latency(e.avg_s)} on average`}</title>
              </path>
            );
          })}
          {[...at.entries()].map(([name, p]) => {
            const n = nodes.get(name);
            const rate = n && n.hits > 0 ? n.errors / n.hits : 0;
            const label = name.length > 22 ? `${name.slice(0, 21)}…` : name;
            return (
              <g
                key={name}
                transform={`translate(${p.x},${p.y})`}
                className={n?.inferred ? undefined : "cursor-pointer"}
                onClick={n?.inferred ? undefined : () => navigate(`/observe/apm/services/${encodeURIComponent(name)}`)}
              >
                <title>{n?.inferred ? `${name}: named by its callers` : `${name}: ${compact(n?.hits ?? 0)} requests, ${compact(n?.errors ?? 0)} errors`}</title>
                <rect width={BOX_W} height={BOX_H} rx={8} fill="#121317" stroke={rate > 0 ? SERIES.fail : "#2a2c31"} strokeDasharray={n?.inferred ? "5 4" : undefined} />
                <text x={12} y={21} fill="#e4e5e7" fontSize={12} fontFamily="var(--font-mono, monospace)">
                  {label}
                </text>
                <text x={12} y={39} fill="#8c8f98" fontSize={10.5}>
                  {n?.inferred ? "named by its callers" : `${compact(n?.hits ?? 0)} requests${rate > 0 ? ` · ${compact(rate * 100)}% errors` : ""}`}
                </text>
              </g>
            );
          })}
        </svg>
      </div>
      <p className="mt-3 font-mono text-[10px] text-[#7b7e88]">
        {m.spans_read.toLocaleString()} spans read{m.sampled_one_in > 1 ? `, one trace in ${m.sampled_one_in.toLocaleString()}` : ""} · {m.took_ms.toFixed(1)} ms
      </p>
    </section>
  );
}
