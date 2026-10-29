/**
 * The infrastructure list (docs/observe.md §10.9), Datadog's: every host that reports (kiln's
 * runners, Datadog's Agent, OpenTelemetry's resources), whether it is reporting, its CPU, I/O wait,
 * load, memory and disk, what reports it and its apps; found by a part of its name or by a tag,
 * sorted by any of its figures.
 */
import * as React from "react";
import { observeApi, type Host, type HostQuery } from "@/api/observe";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField } from "@/components/ui/misc";
import { useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { Link } from "@/lib/router";
import { ObserveNav, compact, useParam } from "./parts";

type HostList = { host_list: Host[]; total_matching: number; total_returned: number };

const hostsRes = (q: HostQuery): Res<HostList> => ({ key: `observe/hosts?${new URLSearchParams(q as Record<string, string>)}`, fetch: (s) => observeApi.hosts(q, s), opts: { pollMs: 30_000 } });
const totalsRes: Res<{ total_up: number; total_active: number }> = { key: "observe/hosts/totals", fetch: (s) => observeApi.hostTotals(s), opts: { pollMs: 30_000 } };

const SORTS = [
  { value: "name", label: "By name" },
  { value: "status", label: "By status" },
  { value: "cpu", label: "By CPU" },
  { value: "iowait", label: "By I/O wait" },
  { value: "load", label: "By load" },
  { value: "memory", label: "By memory" },
  { value: "disk", label: "By disk" },
];

/** What a host's status reads as, and its dot's tone. */
export const STATUS: Record<Host["kiln"]["status"], { word: string; tone: string }> = {
  reporting: { word: "Reporting", tone: "succeeded" },
  late: { word: "Late", tone: "queued" },
  gone: { word: "Gone", tone: "cancelled" },
};

export function Status({ h }: { h: Host }) {
  const s = STATUS[h.kiln.status];
  return (
    <span className="status whitespace-nowrap">
      <i className="dot" data-status={s.tone} aria-hidden="true" />
      <span className="status-text" data-status={s.tone}>
        {s.word}
        {h.is_muted ? " · muted" : ""}
      </span>
    </span>
  );
}

/** A percentage, a high one told apart. */
export function Percent({ v }: { v: number | null }) {
  if (v === null) return <span className="text-[#8c8f98]">—</span>;
  return <span className={v >= 90 ? "text-[#f27a7a]" : undefined}>{compact(v)}%</span>;
}

export default function Hosts() {
  const [q, setQ] = useParam("q", "");
  const [sort, setSort] = useParam("sort", "name");
  return (
    <div className="container-console page">
      <title>Hosts · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Hosts"
        lead="Every host that reports, from kiln's runners, Datadog's Agent and OpenTelemetry's resources: whether it is reporting, its CPU, I/O wait, load, memory and disk, and what reports it."
      />
      <ObserveNav />
      <Search key={q} q={q} onQ={setQ} sort={sort} onSort={setSort} />
      <Loadable key={`${q}/${sort}`} label="the hosts" fallback={<TableHold head={<Head />} rows={6} />}>
        <List q={q} sort={sort} />
      </Loadable>
    </div>
  );
}

function Search({ q, onQ, sort, onSort }: { q: string; onQ: (q: string) => void; sort: string; onSort: (s: string) => void }) {
  const [text, setText] = React.useState(q);
  return (
    <form
      className="mb-4 flex flex-wrap items-end gap-3"
      onSubmit={(e) => {
        e.preventDefault();
        onQ(text.trim());
      }}
    >
      <div className="field min-w-[16rem] flex-1 sm:max-w-md">
        <input aria-label="Find hosts" value={text} onChange={(e) => setText(e.target.value)} placeholder="Find by name, or a tag (env:prod)" spellCheck={false} />
      </div>
      <SelectField label="Order" hideLabel value={sort} onChange={onSort} options={SORTS} className="min-w-[10rem]" />
    </form>
  );
}

function Head() {
  return (
    <tr>
      <th>Host</th>
      <th className="hidden sm:table-cell">Status</th>
      <th className="hidden text-right sm:table-cell">CPU</th>
      <th className="hidden text-right lg:table-cell">I/O wait</th>
      <th className="hidden text-right md:table-cell">Load</th>
      <th className="hidden text-right md:table-cell">Memory</th>
      <th className="hidden text-right lg:table-cell">Disk</th>
    </tr>
  );
}

function List({ q, sort }: { q: string; sort: string }) {
  const query: HostQuery = { filter: q || undefined, sort_field: sort, sort_dir: sort === "name" ? "asc" : "desc", count: 1000 };
  const { host_list: hosts, total_matching } = useRes(hostsRes(query)).data;
  const totals = useRes(totalsRes).data;
  if (hosts.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        {q ? "No host matches." : "No host has reported. A host is listed once it reports: a kiln runner's own figures, Datadog's Agent, or a series tagged host:<name>."}
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
            {hosts.map((h) => (
              <Row key={h.name} h={h} />
            ))}
          </tbody>
        </table>
      </div>
      <p className="mt-3 text-[12.5px] text-[#8c8f98]">
        {total_matching > hosts.length ? `${hosts.length} of ${total_matching} hosts shown. ` : ""}
        {totals.total_up} up (a sample within two hours), {totals.total_active} active (within one). Figures are each host's newest of the last ten minutes; load over 15 minutes.
      </p>
    </>
  );
}

function Row({ h }: { h: Host }) {
  const f = h.kiln.figures;
  const about = [h.sources.join(", ") || "its series", h.apps.length > 0 ? h.apps.slice(0, 4).join(" · ") + (h.apps.length > 4 ? ` +${h.apps.length - 4}` : "") : ""].filter(Boolean).join(" — ");
  return (
    <tr>
      <td className="truncate-cell">
        <Link to={`/observe/hosts/${encodeURIComponent(h.name)}`} className="font-mono text-[13px] text-[#e4e5e7] hover:text-white">
          {h.name}
        </Link>
        <p className="truncate text-[11px] text-[#8c8f98]">{about}</p>
        <p className="truncate text-[11px] text-[#8c8f98] sm:hidden">
          <Status h={h} />
          {f.cpu !== null ? ` · CPU ${compact(f.cpu)}%` : ""}
          {f.memory !== null ? ` · memory ${compact(f.memory)}%` : ""}
          {f.load !== null ? ` · load ${compact(f.load)}` : ""}
        </p>
      </td>
      <td className="hidden text-[12px] sm:table-cell">
        <Status h={h} />
        {h.last_reported_time !== null && <div className="text-[11px] text-[#8c8f98]">{relativeTime(h.last_reported_time)}</div>}
      </td>
      <td className="hidden text-right font-mono text-[12px] sm:table-cell">
        <Percent v={f.cpu} />
      </td>
      <td className="hidden text-right font-mono text-[12px] lg:table-cell">
        <Percent v={f.iowait} />
      </td>
      <td className="hidden text-right font-mono text-[12px] md:table-cell">{f.load !== null ? compact(f.load) : <span className="text-[#8c8f98]">—</span>}</td>
      <td className="hidden text-right font-mono text-[12px] md:table-cell">
        <Percent v={f.memory} />
      </td>
      <td className="hidden text-right font-mono text-[12px] lg:table-cell">
        <Percent v={f.disk} />
      </td>
    </tr>
  );
}
