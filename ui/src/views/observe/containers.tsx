/**
 * Containers (docs/observe.md §10.10), Datadog's: every container the machines run (kiln's runners
 * read each one's cgroup, whatever its engine; Datadog's Agent and OpenTelemetry report theirs), its
 * CPU and memory against its limits, its host and pod, how long it has run; running ones first,
 * stopped ones an hour after; found by a part of a name or by a tag, ordered by a figure.
 */
import * as React from "react";
import { observeApi, type Container, type ContainerQuery } from "@/api/observe";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { useRes, type Res } from "@/lib/data";
import { formatBytes, formatDuration } from "@/lib/format";
import { Link } from "@/lib/router";
import { ObserveNav, compact, useParam, useParams } from "./parts";

type Listed = Awaited<ReturnType<typeof observeApi.containers>>;
const containersRes = (q: ContainerQuery): Res<Listed> => ({
  key: `observe/containers/list?${new URLSearchParams(q as Record<string, string>)}`,
  fetch: (s) => observeApi.containers(q, s),
  opts: { pollMs: 15_000 },
});

const SORTS = [
  { value: "name", label: "By name" },
  { value: "cpu", label: "By CPU" },
  { value: "memory", label: "By memory" },
  { value: "cpu_limit", label: "By CPU of its limit" },
  { value: "memory_limit", label: "By memory of its limit" },
  { value: "throttled", label: "By throttling" },
  { value: "io", label: "By I/O" },
  { value: "uptime", label: "By uptime" },
];

type State = "running" | "stopped" | "all";

/** A container's name as read: its name, else the first 12 of its id (`docker ps`'s). */
export const nameOf = (c: Container) => c.name ?? c.id.slice(0, 12);
/** Its image as read: its name and tag. */
export const imageOf = (c: Container) => [c.image.name ?? c.image.short, c.image.tag].filter(Boolean).join(":");
/** Cores from nanocores. */
export const cores = (n: number | null) => (n === null ? null : n / 1e9);
/** Cores as read: three figures, under a thousandth said so (not 4.34e-4). */
export const coresText = (v: number) => (v === 0 ? "0" : v < 0.001 ? "<0.001" : compact(v));

/** A use against its limit: the use, and its share when there is one (90% or more told apart). */
export function OfLimit({ use, limit, fmt }: { use: number | null; limit: number | null; fmt: (v: number) => string }) {
  if (use === null) return <span className="text-[#8c8f98]">—</span>;
  const share = limit && limit > 0 ? (100 * use) / limit : null;
  return (
    <span className={share !== null && share >= 90 ? "text-[#f27a7a]" : undefined}>
      {fmt(use)}
      {share !== null && <span className="text-[#8c8f98]"> · {share > 0 && share < 0.1 ? "<0.1" : compact(share)}%</span>}
    </span>
  );
}

export default function Containers() {
  const [state] = useParam("state", "running");
  const [q] = useParam("q", "");
  const [sort] = useParam("sort", "name");
  const set = useParams();
  const shown: State = state === "stopped" || state === "all" ? state : "running";
  return (
    <div className="container-console page">
      <title>Containers · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Containers"
        lead="Every container the machines run: kiln's runners read each one's cgroup, whatever runs it (Docker, containerd, CRI-O, Podman, Kubernetes' pods), and Datadog's Agent and OpenTelemetry report theirs. Its CPU and memory against its limits, its host and pod."
      />
      <ObserveNav />
      <div className="mb-4 flex flex-wrap items-end gap-3">
        <Loadable key={q} label="the containers' counts" fallback={<States show={shown} onShow={(v) => set({ state: v === "running" ? null : v })} />}>
          <Counted q={q} show={shown} onShow={(v) => set({ state: v === "running" ? null : v })} />
        </Loadable>
        <SelectField label="Order" hideLabel value={sort} onChange={(v) => set({ sort: v === "name" ? null : v })} options={SORTS} className="min-w-[12rem]" />
        <Search key={q} q={q} onQ={(v) => set({ q: v || null })} />
      </div>
      <Loadable key={`${q}/${sort}/${shown}`} label="the containers" fallback={<TableHold head={<Head />} rows={6} />}>
        <List q={q} sort={sort} state={shown} />
      </Loadable>
    </div>
  );
}

function States({ show, onShow, counts }: { show: State; onShow: (s: State) => void; counts?: { running: number; stopped: number } }) {
  return (
    <Segmented<State>
      label="State"
      value={show}
      onChange={onShow}
      options={[
        { value: "running", label: "Running", count: counts?.running },
        { value: "stopped", label: "Stopped", count: counts?.stopped },
        { value: "all", label: "All", count: counts ? counts.running + counts.stopped : undefined },
      ]}
    />
  );
}

/** [`States`] counted: what `q` matches, running and stopped. */
function Counted({ q, show, onShow }: { q: string; show: State; onShow: (s: State) => void }) {
  const running = useRes(containersRes({ state: "running", filter: q || undefined, count: 1 })).data.total_matching;
  const stopped = useRes(containersRes({ state: "stopped", filter: q || undefined, count: 1 })).data.total_matching;
  return <States show={show} onShow={onShow} counts={{ running, stopped }} />;
}

function Search({ q, onQ }: { q: string; onQ: (q: string) => void }) {
  const [text, setText] = React.useState(q);
  return (
    <form
      className="field w-full sm:ml-auto sm:w-80"
      onSubmit={(e) => {
        e.preventDefault();
        onQ(text.trim());
      }}
    >
      <input aria-label="Find containers" type="search" value={text} onChange={(e) => setText(e.target.value)} placeholder="A name, an image, or kube_namespace:shop" spellCheck={false} />
    </form>
  );
}

function Head() {
  return (
    <tr>
      <th>Container</th>
      <th className="hidden md:table-cell">Host</th>
      <th className="hidden lg:table-cell">Pod</th>
      <th className="hidden text-right sm:table-cell">CPU</th>
      <th className="hidden text-right sm:table-cell">Memory</th>
      <th className="hidden text-right lg:table-cell">Up</th>
    </tr>
  );
}

function List({ q, sort, state }: { q: string; sort: string; state: State }) {
  const { containers, total_matching } = useRes(containersRes({ state, filter: q || undefined, sort_field: sort as ContainerQuery["sort_field"], count: 500 })).data;
  if (containers.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        {q
          ? "No container matches."
          : state === "stopped"
            ? "No container stopped in the last hour."
            : "No container reports. A runner on Linux reports every container its machine runs; Datadog's Agent's container check and OpenTelemetry's receivers report theirs."}
      </p>
    );
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {containers.map((c) => (
              <Row key={c.id} c={c} />
            ))}
          </tbody>
        </table>
      </div>
      <p className="mt-3 text-[12.5px] text-[#8c8f98]">
        {total_matching > containers.length ? `${containers.length} of ${total_matching} shown. ` : ""}
        CPU in cores and memory in use (its working set, what an out-of-memory kill reads), each its newest of the last ten minutes, against its limit.
      </p>
    </>
  );
}

function Row({ c }: { c: Container }) {
  const f = c.figures;
  const pod = c.kube.pod ? `${c.kube.namespace ? `${c.kube.namespace}/` : ""}${c.kube.pod}` : null;
  const memory = f.working_set_bytes ?? f.memory_bytes;
  return (
    <tr>
      <td className="truncate-cell">
        <Link to={`/observe/containers/${c.id}`} className="font-mono text-[13px] text-[#e4e5e7] hover:text-white">
          {nameOf(c)}
        </Link>
        <p className="truncate text-[11px] text-[#8c8f98]">
          {c.state === "stopped" && <span className="text-[#d1aaad]">stopped · </span>}
          {imageOf(c) || c.id.slice(0, 12)}
          {c.runtime ? ` · ${c.runtime}` : ""}
        </p>
        <p className="truncate text-[11px] text-[#8c8f98] sm:hidden">
          {f.cpu_nanocores !== null ? `CPU ${coresText(f.cpu_nanocores / 1e9)} cores` : ""}
          {memory !== null ? ` · memory ${formatBytes(memory)}` : ""}
          {c.host ? ` · ${c.host}` : ""}
        </p>
      </td>
      <td className="hidden max-w-[12rem] truncate whitespace-nowrap text-[12px] md:table-cell">
        {c.host ? (
          <Link to={`/observe/hosts/${encodeURIComponent(c.host)}`} className="font-mono hover:text-white">
            {c.host}
          </Link>
        ) : (
          <span className="text-[#8c8f98]">—</span>
        )}
      </td>
      <td className="hidden max-w-[16rem] truncate text-[12px] lg:table-cell">{pod ?? <span className="text-[#8c8f98]">—</span>}</td>
      <td className="hidden whitespace-nowrap text-right font-mono text-[12px] sm:table-cell">
        <OfLimit use={cores(f.cpu_nanocores)} limit={cores(f.cpu_limit_nanocores)} fmt={coresText} />
      </td>
      <td className="hidden whitespace-nowrap text-right font-mono text-[12px] sm:table-cell">
        <OfLimit use={memory} limit={f.memory_limit_bytes} fmt={(v) => formatBytes(v)} />
      </td>
      <td className="hidden whitespace-nowrap text-right font-mono text-[12px] lg:table-cell">
        {f.uptime_seconds !== null ? formatDuration(f.uptime_seconds) : <span className="text-[#8c8f98]">—</span>}
      </td>
    </tr>
  );
}
