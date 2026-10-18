/**
 * Monitors: each one's state (its worst group's), what it watches, whether it is muted; those in
 * trouble first. A new one starts from a query, in Datadog's syntax.
 */
import * as React from "react";
import { Plus } from "lucide-react";
import { Link } from "@/lib/router";
import type { Monitor, MonitorState } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SearchField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { useRes } from "@/lib/data";
import { ObserveNav, StateBadge, monitorsRes } from "./parts";

type Show = "all" | MonitorState | "muted";
const RANK: Record<string, number> = { ALERT: 0, WARN: 1, "NO DATA": 2, OK: 3 };
const muted = (m: Monitor, now: number) => m.muted_until !== null && m.muted_until > now;

export default function Monitors() {
  return (
    <div className="container-console page">
      <title>Monitors · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Monitors"
        lead="Each one a query kiln evaluates as data arrives: a metric, how it changed, or a count of logs, against thresholds. A change of state is an event, and a message to the integrations it mentions."
        actions={
          <Button asChild>
            <Link to="/observe/monitors/new">
              <Plus /> New monitor
            </Link>
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the monitors" fallback={<TableHold head={<Head />} rows={6} />}>
        <List />
      </Loadable>
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th>State</th>
      <th>Monitor</th>
      <th className="num">Groups</th>
      <th>Tags</th>
    </tr>
  );
}

function List() {
  const all = useRes(monitorsRes).data;
  const [show, setShow] = React.useState<Show>("all");
  const [filter, setFilter] = React.useState("");
  const now = Date.now();
  const count = (s: Show) => all.filter((m) => (s === "all" ? true : s === "muted" ? muted(m, now) : m.state === s)).length;
  const f = filter.trim().toLowerCase();
  const shown = all
    .filter((m) => (show === "all" ? true : show === "muted" ? muted(m, now) : m.state === show))
    .filter((m) => !f || m.name.toLowerCase().includes(f) || m.query.toLowerCase().includes(f) || m.tags.some((t) => t.toLowerCase().includes(f)))
    .sort((a, b) => (RANK[a.state] ?? 4) - (RANK[b.state] ?? 4) || a.name.localeCompare(b.name));
  if (all.length === 0)
    return (
      <p className="empty-line py-16 text-center">
        No monitors yet. <Link to="/observe/monitors/new">Make the first</Link>, or start from a query in the{" "}
        <Link to="/observe/metrics" className="text-link">
          metrics explorer
        </Link>
        .
      </p>
    );
  return (
    <>
      <div className="mb-4 flex flex-wrap items-center gap-3">
        <Segmented
          label="State"
          value={show}
          onChange={setShow}
          options={(["all", "ALERT", "WARN", "NO DATA", "OK", "muted"] as Show[]).map((s) => ({
            value: s,
            label: s === "all" ? "All" : s === "muted" ? "Muted" : s === "NO DATA" ? "No data" : s[0] + s.slice(1).toLowerCase(),
            count: count(s),
          }))}
        />
        <SearchField label="Find a monitor" value={filter} onChange={setFilter} placeholder="Name, query or tag" className="w-full sm:ml-auto sm:w-72" />
      </div>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {shown.map((m) => {
              const trouble = m.groups.filter((g) => g.state !== "OK").length;
              return (
                <tr key={m.id}>
                  <td className="w-1 whitespace-nowrap">
                    <StateBadge state={m.state} muted={muted(m, now)} />
                  </td>
                  <td className="truncate-cell">
                    <Link to={`/observe/monitors/${encodeURIComponent(m.id)}`} className="text-[13px] text-[#e4e5e7] hover:text-white">
                      {m.name}
                    </Link>
                    <p className="truncate font-mono text-[10.5px] text-[#8c8f98]">{m.query}</p>
                  </td>
                  <td className="num cell-mono">{m.groups.length ? `${trouble ? `${trouble} of ` : ""}${m.groups.length}` : "—"}</td>
                  <td>
                    <span className="inline-flex flex-wrap gap-1">
                      {m.tags.map((t) => (
                        <span key={t} className="chip">
                          {t}
                        </span>
                      ))}
                    </span>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {shown.length === 0 && (
        <p className="empty-line py-10 text-center">
          No monitor is {show === "muted" ? "muted" : "in that state"}
          {f ? " that matches" : ""}.
        </p>
      )}
    </>
  );
}
