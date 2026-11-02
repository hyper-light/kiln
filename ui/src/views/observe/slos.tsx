/**
 * SLOs: each one's standing over its timeframe (its SLI, the error budget it has left, how fast it
 * is spending it now), those in trouble first. A new one is good events over all, or time slices
 * that meet a condition (docs/observe.md §9.2).
 */
import * as React from "react";
import { Plus } from "lucide-react";
import { Link } from "@/lib/router";
import type { Slo, SloState } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SearchField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { useRes } from "@/lib/data";
import { ObserveNav, slosRes } from "./parts";

const RANK: Record<string, number> = { BREACHED: 0, WARN: 1, "NO DATA": 2, INVALID: 3, OK: 4 };
const TONE: Record<string, string> = { OK: "succeeded", WARN: "timed_out", BREACHED: "failed", "NO DATA": "skipped", INVALID: "skipped" };
const WORDS: Record<string, string> = { OK: "OK", WARN: "Warn", BREACHED: "Breached", "NO DATA": "No data", INVALID: "Invalid" };

/** An SLO's state, as a monitor's is shown. */
export function SloBadge({ state }: { state: SloState }) {
  return (
    <span className="status">
      <i className="dot" data-status={TONE[state] ?? "skipped"} aria-hidden="true" />
      <span className="status-text" data-status={TONE[state] ?? "skipped"}>
        {WORDS[state] ?? state}
      </span>
    </span>
  );
}

/** An SLI as they are read: to the thousandth near 100, fewer places further off. */
export function sliText(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return "—";
  const a = Math.abs(v);
  const places = a >= 99 && a < 100 ? 3 : a >= 10 ? 1 : 2;
  return `${v.toFixed(places)}%`;
}

/** The error budget left: to a tenth, below zero once it is overspent. */
export function budgetText(v: number | null | undefined): string {
  return v === null || v === undefined || !Number.isFinite(v) ? "—" : `${v.toFixed(1)}%`;
}

/** A target as it was set: 99.9%, not 99.900%. */
export function targetText(v: number): string {
  return `${Number(v.toFixed(4))}%`;
}

/** A burn rate: how many times the pace that spends the budget in the timeframe exactly. */
export function burn(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return "—";
  return `${v >= 10 ? v.toFixed(0) : v.toFixed(v >= 1 ? 1 : 2)}×`;
}

/** What an SLO measures, in a line. */
export function measures(s: Slo): string {
  return s.type === "ratio" ? `${s.good ?? ""} over ${s.total ?? ""}` : `${s.query ?? ""} ${s.condition ?? ""}, ${(s.slice_s ?? 300) / 60} min slices`;
}

/** The error budget left, as a meter: its share of the track, the state's tone. */
export function Budget({ left, state }: { left: number | null | undefined; state: SloState }) {
  const shown = left === null || left === undefined || !Number.isFinite(left) ? null : Math.max(0, Math.min(100, left));
  return (
    <span className="inline-flex items-center gap-2">
      <span className="relative inline-block h-1.5 w-16 overflow-hidden rounded-full bg-white/[0.08]" aria-hidden="true">
        {shown !== null && (
          <span className="absolute inset-y-0 left-0 rounded-full" style={{ width: `${shown}%`, background: state === "OK" ? "#a8c9bc" : state === "WARN" ? "#cbb69a" : "#d1aaad" }} />
        )}
      </span>
      <span className="cell-mono">{budgetText(left)}</span>
    </span>
  );
}

type Show = "all" | SloState;

export default function Slos() {
  return (
    <div className="container-console page">
      <title>SLOs · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="SLOs"
        lead="How much of what each service does is good, against what it promises, over the last 7, 30 or 90 days: its indicator, the error budget left and how fast it is being spent. An SLO's alerts are monitors of its burn rate."
        actions={
          <Button asChild>
            <Link to="/observe/slos/new">
              <Plus /> New SLO
            </Link>
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the SLOs" fallback={<TableHold head={<Head />} rows={4} />}>
        <List />
      </Loadable>
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th>State</th>
      <th>SLO</th>
      <th className="num">Target</th>
      <th className="num">SLI</th>
      <th className="hidden sm:table-cell">Budget left</th>
      <th className="num hidden md:table-cell">Burn, 1 h</th>
    </tr>
  );
}

function List() {
  const all = useRes(slosRes).data;
  const [show, setShow] = React.useState<Show>("all");
  const [filter, setFilter] = React.useState("");
  const f = filter.trim().toLowerCase();
  const shown = all
    .filter((s) => show === "all" || s.standing.state === show)
    .filter((s) => !f || s.name.toLowerCase().includes(f) || measures(s).toLowerCase().includes(f) || (s.tags ?? []).some((t) => t.toLowerCase().includes(f)))
    .sort((a, b) => (RANK[a.standing.state] ?? 5) - (RANK[b.standing.state] ?? 5) || a.name.localeCompare(b.name));
  if (all.length === 0)
    return (
      <p className="empty-line py-16 text-center">
        No SLOs yet. <Link to="/observe/slos/new">Make the first</Link> from a metric of good events and one of all of them.
      </p>
    );
  const count = (s: Show) => all.filter((x) => s === "all" || x.standing.state === s).length;
  return (
    <>
      <div className="mb-4 flex flex-wrap items-center gap-3">
        <Segmented
          label="State"
          value={show}
          onChange={setShow}
          options={(["all", "BREACHED", "WARN", "NO DATA", "OK"] as Show[]).map((s) => ({ value: s, label: s === "all" ? "All" : WORDS[s], count: count(s) }))}
        />
        <SearchField label="Find an SLO" value={filter} onChange={setFilter} placeholder="Name, query or tag" className="w-full sm:ml-auto sm:w-72" />
      </div>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {shown.map((s) => (
              <tr key={s.id}>
                <td className="w-1 whitespace-nowrap">
                  <SloBadge state={s.standing.state} />
                </td>
                <td className="truncate-cell">
                  <Link to={`/observe/slos/${encodeURIComponent(s.id)}`} className="text-[13px] text-[#e4e5e7] hover:text-white">
                    {s.name}
                  </Link>
                  <p className="truncate font-mono text-[10.5px] text-[#8c8f98]">{measures(s)}</p>
                </td>
                <td className="num cell-mono whitespace-nowrap">
                  {targetText(s.target)} <span className="text-[#8c8f98]">· {s.timeframe}</span>
                </td>
                <td className="num cell-mono">{sliText(s.standing.sli)}</td>
                <td className="hidden whitespace-nowrap sm:table-cell">
                  <Budget left={s.standing.budget_left} state={s.standing.state} />
                </td>
                <td className="num cell-mono hidden md:table-cell">{burn(s.standing.burn_rates?.["1h"])}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {shown.length === 0 && <p className="empty-line py-10 text-center">No SLO is in that state{f ? " that matches" : ""}.</p>}
    </>
  );
}
