/**
 * The System page's conversation stores (docs/conversations.md, "Telemetry"): replication live, the
 * stores as databases with batches moving between them; the replication log, live, searchable and
 * downloadable; every store in a table; and the dashboard of the one picked, its series over an hour
 * or a day and what happened to it lately.
 */
import * as React from "react";
import { api } from "@/api/client";
import type { ConvoNote, ConvoSeries, ConvoStatus, ConvoStore } from "@/api/types";
import { Chart } from "@/components/charts/chart";
import { SERIES } from "@/components/charts/types";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, Facts, FoldSection, Panel, SectionHeading, Sparkline } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { useRes, type Res } from "@/lib/data";
import { formatBytes } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import { RelTime } from "../shared";
import { ReplicationFigure, ReplicationLog } from "./replication";
import { agoText, countText, sizeText, stateText, waitQuantile } from "./scene";
import "./stores.css";

export const convoStatusRes: Res<ConvoStatus> = { key: "conversations/status", fetch: (s) => api.convoStatus(s), opts: { pollMs: 2_000 } };

const DOT: Record<ConvoStore["state"], string> = {
  live: "succeeded",
  in_sync: "succeeded",
  catching_up: "running",
  feeding: "running",
  behind: "timed_out",
  offline: "cancelled",
  no_store: "skipped",
};
const ROLE: Record<ConvoStore["role"], string> = { server: "server", runner: "runner", mirror: "mirror" };

const perSec = (v: number | null | undefined) => (v == null ? "—" : `${v < 10 ? v.toFixed(1) : Math.round(v).toLocaleString("en-US")}/s`);
/** An axis's tick: whole when it is (the axis steps by 0.5 at small rates: 0, 0.5, 1, never 0, 1, 1). */
const tick = (v: number) => (Number.isInteger(v) ? v.toLocaleString("en-US") : v < 10 ? v.toFixed(1) : Math.round(v).toLocaleString("en-US"));

/** A store's name: its machine, its role, and its id where it has one. */
function storeName(s: ConvoStore) {
  if (s.role === "server") return "Server";
  if (s.role === "mirror") return `${s.machine} · copy`;
  return s.machine;
}

export function StoresSection({ index }: { index: string }) {
  return (
    <FoldSection id="system.stores" index={index} label="Conversations" title="Conversation stores">
      <Loadable label="the conversation stores" fallback={<StoresHold />}>
        <StoresBody />
      </Loadable>
    </FoldSection>
  );
}

function StoresBody() {
  const status = useRes(convoStatusRes).data;
  const [picked, setPicked] = React.useState<string | null>(null);
  if (!status.on) {
    return (
      <EmptyState title="This server keeps no conversation store">
        <p>Conversations are kept only by each machine&apos;s harness: KILN_CONVERSATIONS is off here.</p>
      </EmptyState>
    );
  }
  const shown = status.stores.find((s) => s.key === picked) ?? status.stores[0] ?? null;
  return (
    <>
      <ReplicationFigure status={status} selected={picked} onSelect={setPicked} />
      <Loadable label="the replication log" fallback={<LogHold />}>
        <ReplicationLog />
      </Loadable>
      <StoresTable status={status} selected={shown?.key ?? null} onSelect={setPicked} />
      {shown && <StoreDashboard key={shown.key} status={status} store={shown} />}
    </>
  );
}

function StoresHead() {
  return (
    <thead>
      <tr>
        <th className="w-6" aria-label="State" />
        <th>Store</th>
        <th>State</th>
        <th className="num">File</th>
        <th className="num hidden sm:table-cell">Free</th>
        <th className="hidden md:table-cell">Writes</th>
        <th className="num hidden lg:table-cell">Rows to come</th>
        <th className="hidden md:table-cell">Heard</th>
        <th className="hidden xl:table-cell">Kiln</th>
      </tr>
    </thead>
  );
}

function StoresTable({ status, selected, onSelect }: { status: ConvoStatus; selected: string | null; onSelect: (key: string) => void }) {
  return (
    <div className="table-wrap mt-6">
      <table className="data-table cv-table">
        <StoresHead />
        <tbody>
          {status.stores.map((s) => {
            const f = s.report?.file;
            const behind = s.feed?.behind ?? s.mirror?.behind ?? null;
            const spark = s.spark.map((v) => v ?? 0);
            return (
              <tr key={s.key} data-selected={s.key === selected} onClick={() => onSelect(s.key)} className="cursor-pointer">
                <td>
                  <StatusDot status={DOT[s.state]} title={stateText(s, status.at)} />
                </td>
                <td className="cv-key">
                  <button type="button" className="cv-row" aria-pressed={s.key === selected} onClick={() => onSelect(s.key)}>
                    <span className="truncate text-[#e4e5e7]">{storeName(s)}</span>
                    <span className="ml-2 chip">{ROLE[s.role]}</span>
                    {s.id && <span className="ml-2 hidden font-mono text-[10px] text-[#7b7e88] sm:inline">{s.id.slice(0, 8)}</span>}
                  </button>
                </td>
                <td className="cell-mono min-w-[96px] sm:whitespace-nowrap">{stateText(s, status.at)}</td>
                <td className="num cell-mono whitespace-nowrap">{f ? sizeText(f.bytes) : "—"}</td>
                <td className="num cell-mono hidden sm:table-cell">{f && f.bytes ? `${Math.round((f.free / f.bytes) * 100)}%` : "—"}</td>
                <td className="hidden md:table-cell">
                  <span className="inline-flex items-center gap-2">
                    <Sparkline values={spark} width={84} height={20} />
                    <span className="cell-mono">{perSec(s.rates.writes)}</span>
                  </span>
                </td>
                <td className={`num cell-mono hidden lg:table-cell ${behind ? "!text-[#aab8d2]" : ""}`}>{behind == null ? "—" : countText(behind)}</td>
                <td className="cell-mono hidden whitespace-nowrap md:table-cell">{s.heard ? <RelTime ts={Math.floor(s.heard / 1000)} /> : "—"}</td>
                <td className="cell-mono hidden xl:table-cell">{s.version ?? "—"}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

type Window = "3600" | "21600" | "86400";
const WINDOWS: { value: Window; label: string }[] = [
  { value: "3600", label: "1 h" },
  { value: "21600", label: "6 h" },
  { value: "86400", label: "24 h" },
];

function StoreDashboard({ status, store }: { status: ConvoStatus; store: ConvoStore }) {
  const [win, setWin] = React.useState<Window>("3600");
  const notes = status.notes.filter((n) => n.key === store.key);
  return (
    <section className="mt-10">
      <SectionHeading
        label="Dashboard"
        title={store.role === "server" ? "The server's store" : store.role === "mirror" ? `${store.machine}'s copy of the server's store` : `${store.machine}'s store`}
        aside={<Segmented label="Window" value={win} onChange={setWin} options={WINDOWS} />}
      />
      <StoreKpis status={status} store={store} />
      <Loadable label="its series" fallback={<ChartsHold />}>
        <StoreCharts store={store} win={Number(win)} />
      </Loadable>
      <div className="mt-4 grid gap-4 lg:grid-cols-2">
        <Panel eyebrow="Details" title="As it stands">
          <Facts items={detailsOf(status, store)} />
        </Panel>
        <Panel eyebrow="Activity" title="What happened lately" description={notes.length ? undefined : "Nothing yet since the server started: what changes is noted here as it happens."}>
          <Notes notes={notes} />
        </Panel>
      </div>
    </section>
  );
}

function StoreKpis({ status, store }: { status: ConvoStatus; store: ConvoStore }) {
  const f = store.report?.file;
  const behind = store.feed?.behind ?? store.mirror?.behind ?? null;
  const p99 = waitQuantile(store.report?.waits, 0.99);
  const blank = " ";
  return (
    <div className="kpi-row cv-kpis !grid-cols-2 md:!grid-cols-3 xl:!grid-cols-6">
      <div className="kpi">
        <div className="label">File</div>
        <div className="kpi-value">{f ? sizeText(f.bytes) : blank}</div>
        <div className="kpi-foot">{f ? `${sizeText(f.free)} free · log ${sizeText(f.wal)}` : "not reported"}</div>
      </div>
      <div className="kpi">
        <div className="label">Sessions</div>
        <div className="kpi-value">{f?.sessions != null ? f.sessions.toLocaleString("en-US") : "—"}</div>
        <div className="kpi-foot">{f ? `#${countText(f.top)} writes so far` : blank}</div>
      </div>
      <div className="kpi">
        <div className="label">Writes</div>
        <div className="kpi-value">{perSec(store.rates.writes)}</div>
        <div className="kpi-foot">over the last minute</div>
      </div>
      <div className="kpi">
        <div className="label">Writer busy</div>
        <div className="kpi-value">{store.rates.busy != null ? `${Math.round(store.rates.busy * 100)}%` : "—"}</div>
        <div className="kpi-foot">{p99 == null ? blank : `reads wait under ${p99 < 1000 ? `${p99} ms` : `${p99 / 1000} s`} (p99)`}</div>
      </div>
      <div className="kpi">
        <div className="label">{store.role === "server" ? "Feeds in" : "Rows to come"}</div>
        <div className="kpi-value">
          {store.role === "server" ? perSec(status.stores.reduce((a, s) => a + (s.role === "runner" ? (s.rates.batches ?? 0) : 0), 0)) : behind == null ? "—" : countText(behind)}
        </div>
        <div className="kpi-foot">{store.role === "server" ? "batches taken" : store.state === "offline" ? "not linked" : stateText(store, status.at)}</div>
      </div>
      <div className="kpi">
        <div className="label">On the wire</div>
        <div className="kpi-value">{store.rates.wire == null ? "—" : `${formatBytes(store.rates.wire, 1)}/s`}</div>
        <div className="kpi-foot">{store.role === "mirror" ? "sent to it" : store.role === "server" ? "taken from runners" : "sent to the server"}</div>
      </div>
    </div>
  );
}

function detailsOf(status: ConvoStatus, s: ConvoStore): { label: string; value: React.ReactNode; tone?: "pass" | "fail" | "error" | "pending" }[] {
  const now = status.at;
  const r = s.report;
  const out: { label: string; value: React.ReactNode; tone?: "pass" | "fail" | "error" | "pending" }[] = [
    { label: "Store", value: s.id ?? "none" },
    { label: "Kiln", value: s.version ?? "—" },
    { label: "Heard", value: s.heard ? `${agoText(now - s.heard)} ago` : "not since the server started" },
  ];
  if (r) {
    out.push({ label: "Commits", value: `${r.commits.toLocaleString("en-US")} (${r.rows.toLocaleString("en-US")} rows)` });
    out.push({ label: "Reads", value: r.jobs.toLocaleString("en-US") });
    out.push({ label: "Dropped · timed out", value: `${r.dropped} · ${r.timeouts}`, tone: r.dropped || r.timeouts ? "fail" : undefined });
    const c = r.file.compacted;
    out.push({ label: "Made compact", value: c ? `${sizeText(c.before)} → ${sizeText(c.after)} in ${Math.round(c.ms / 1000)} s, ${agoText(now - c.at)} ago` : "not since its kiln started" });
  }
  const i = s.feed?.intake;
  if (s.feed) out.push({ label: "The server has it through", value: `#${countText(s.feed.rev)}` });
  if (i) {
    out.push({ label: "Batches taken", value: `${i.batches.toLocaleString("en-US")} (${i.rows.toLocaleString("en-US")} rows, ${sizeText(i.bytes)})` });
    out.push({ label: "Gaps · errors", value: `${i.gaps} · ${i.errors}`, tone: i.errors ? "fail" : undefined });
    if (i.fenced) out.push({ label: "Left out", value: `${i.fenced.toLocaleString("en-US")} rows of sessions that went on elsewhere` });
  }
  const ship = s.feed?.ship ?? s.mirror?.ship;
  if (ship) {
    out.push({ label: s.role === "mirror" ? "The server sent" : "It sent", value: `${ship.batches.toLocaleString("en-US")} batches, through #${countText(ship.sent)}` });
    out.push({ label: "Quiet spells", value: `${ship.stalls}`, tone: ship.stalls ? "error" : undefined });
  }
  if (s.mirror) out.push({ label: "Chosen", value: `${agoText(now - s.mirror.chosen_at)} ago` });
  return out;
}

function Notes({ notes }: { notes: ConvoNote[] }) {
  if (!notes.length) return null;
  return (
    <ul className="cv-notes">
      {notes.slice(0, 30).map((n, i) => (
        <li key={`${n.at}-${i}`}>
          <i className="dot" data-status={n.level === "error" ? "failed" : n.level === "warn" ? "timed_out" : "succeeded"} aria-label={n.level} />
          <span>{n.text}</span>
          <RelTime ts={Math.floor(n.at / 1000)} />
        </li>
      ))}
    </ul>
  );
}

function ChartsHold() {
  return (
    <div className="mt-4 grid gap-4 lg:grid-cols-2">
      {["Writes", "File", "Feed", "Writer"].map((t) => (
        <Panel key={t} eyebrow={t}>
          <Hold height={160} />
        </Panel>
      ))}
    </div>
  );
}

function StoreCharts({ store, win }: { store: ConvoStore; win: number }) {
  const s: ConvoSeries = useSuspenseResource(`conversations/series/${store.key}/${win}`, (sig) => api.convoSeries(store.key, win, sig), { pollMs: win <= 3600 ? 5_000 : 30_000 }).data;
  const time = win > 6 * 3600 ? "hour" : "time";
  const used = s.bytes.map((b, i) => (b == null ? null : b - (s.free[i] ?? 0)));
  const busy = s.busy.map((b) => (b == null ? null : b * 100));
  const last = <T,>(xs: (T | null)[]) => {
    for (let i = xs.length - 1; i >= 0; i--) if (xs[i] != null) return xs[i];
    return null;
  };
  const feed = store.role === "server";
  return (
    <div className="mt-4 grid gap-4 lg:grid-cols-2">
      <Panel eyebrow="Writes" title={perSec(last(s.writes))} description="Rows written into the store, per second; commits on the right.">
        <Chart
          ariaLabel="Writes per second"
          timeFormat={time}
          data={[s.t, s.writes, s.commits]}
          height={160}
          yFormat={tick}
          y2Format={tick}
          series={[
            { label: "Writes/s", color: SERIES.violet, fill: true },
            { label: "Commits/s", color: SERIES.grey, width: 1, dash: [2, 4], scale: "y2" },
          ]}
        />
      </Panel>
      <Panel eyebrow="File" title={last(s.bytes) == null ? "—" : sizeText(last(s.bytes)!)} description="In use, free within it (taken up again before the file grows), and its log.">
        <Chart
          ariaLabel="File size"
          timeFormat={time}
          data={[s.t, used, s.free, s.wal]}
          height={160}
          yFormat={(v) => formatBytes(v, 0)}
          series={[
            { label: "In use", color: SERIES.blue, fill: true },
            { label: "Free", color: SERIES.teal },
            { label: "Log", color: SERIES.grey, width: 1, dash: [2, 4] },
          ]}
        />
      </Panel>
      {feed ? (
        <Panel eyebrow="Feeds" title={perSec(last(s.batches))} description="Batches taken from every runner's store, per second; their bytes on the right.">
          <Chart
            ariaLabel="Batches per second"
            timeFormat={time}
            data={[s.t, s.batches, s.wire]}
            height={160}
            yFormat={tick}
            y2Format={(v) => formatBytes(v, 0)}
            series={[
              { label: "Batches/s", color: SERIES.sand, fill: true },
              { label: "Bytes/s", color: SERIES.rose, scale: "y2" },
            ]}
          />
        </Panel>
      ) : (
        <Panel
          eyebrow="Feed"
          title={last(s.behind) == null ? "—" : `${countText(last(s.behind)!)} rows to come`}
          description={store.role === "mirror" ? "Rows of the server's store its copy lacks; batches sent on the right." : "Rows of this store the server lacks; batches it took on the right."}
        >
          <Chart
            ariaLabel="Rows to come"
            timeFormat={time}
            data={[s.t, s.behind, s.batches]}
            height={160}
            yFormat={(v) => countText(Math.round(v))}
            y2Format={tick}
            series={[
              { label: "Rows to come", color: SERIES.warn, fill: true },
              { label: "Batches/s", color: SERIES.grey, width: 1, scale: "y2" },
            ]}
          />
        </Panel>
      )}
      <Panel
        eyebrow="Writer"
        title={last(busy) == null ? "—" : `${Math.round(last(busy)!)}% busy`}
        description="How much of the time its one writer thread was busy; how long reads waited (p50, p99) on the right."
      >
        <Chart
          ariaLabel="Writer busy"
          timeFormat={time}
          data={[s.t, busy, s.p50, s.p99]}
          height={160}
          yRange={[0, 100]}
          yFormat={(v) => `${Math.round(v)}%`}
          y2Format={(v) => (v < 1000 ? `${Math.round(v)} ms` : `${(v / 1000).toFixed(1)} s`)}
          series={[
            { label: "Busy", color: SERIES.pending, fill: true },
            { label: "Wait p50", color: SERIES.teal, width: 1, scale: "y2" },
            { label: "Wait p99", color: SERIES.rose, width: 1, scale: "y2" },
          ]}
        />
      </Panel>
    </div>
  );
}

/** The replication log while it loads: its tools' row and its lines' box. */
function LogHold() {
  return (
    <section className="rp-log">
      <SectionHeading label="Log" title="Replication log" />
      <Hold height={36} label="Loading the replication log" />
      <Hold height={260} className="mt-3" />
    </section>
  );
}

/** The section while it loads: the figure's frame, the log, the table's head. */
export function StoresHold() {
  return (
    <>
      <div className="rp-figure">
        <Hold height={520} label="Loading the conversation stores" />
      </div>
      <LogHold />
      <div className="mt-6">
        <TableHold head={<StoresHead />} rows={5} rowHeight={41} />
      </div>
    </>
  );
}
