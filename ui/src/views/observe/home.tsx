/**
 * Observe at a glance: what is in trouble (monitors alerting, warning, without data), what just
 * happened (the newest events), and the store itself: what it keeps, from when, on which machines,
 * and whether every replica holds the same.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { observeApi, type ObserveStatus } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Facts, PageIntro, Panel } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { useRes, warm } from "@/lib/data";
import { formatBytes } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { ObserveNav, StateBadge, monitorsRes, recordsRes, statusRes } from "./parts";
import { Stream } from "./widgets";

const RANK: Record<string, number> = { ALERT: 0, WARN: 1, "NO DATA": 2 };

export default function ObserveHome() {
  warm(monitorsRes, statusRes, recordsRes("source:kiln.event", "1d", 50));
  return (
    <div className="container-console page">
      <title>Observe · kiln</title>
      <PageIntro index={8} label="Observe" title="Observe" lead="kiln's own metrics, logs and events, kept on every runner in one replicated store: query them, chart them, alert on them." />
      <ObserveNav />
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Loadable label="the monitors" fallback={<Hold height={300} />}>
          <Trouble />
        </Loadable>
        <Panel
          eyebrow="Events"
          title="The newest"
          actions={
            <Link to="/observe/events" className="text-link text-[12px]">
              Every event
            </Link>
          }
        >
          <Loadable label="the events" fallback={<Hold height={260} />}>
            <Stream query="source:kiln.event" range="1d" height={260} kind="events" />
          </Loadable>
        </Panel>
      </div>
      <Loadable label="the store" fallback={<Hold height={320} />}>
        <Store />
      </Loadable>
    </div>
  );
}

function Trouble() {
  const all = useRes(monitorsRes).data;
  const now = Date.now();
  const trouble = all.filter((m) => m.state !== "OK").sort((a, b) => (RANK[a.state] ?? 3) - (RANK[b.state] ?? 3) || a.name.localeCompare(b.name));
  const n = (s: string) => all.filter((m) => m.state === s).length;
  const title =
    all.length === 0
      ? "No monitors yet"
      : trouble.length === 0
        ? `All ${all.length} OK`
        : [n("ALERT") && `${n("ALERT")} alerting`, n("WARN") && `${n("WARN")} warning`, n("NO DATA") && `${n("NO DATA")} without data`].filter(Boolean).join(" · ");
  return (
    <Panel
      eyebrow="Monitors"
      title={title}
      actions={
        <Link to="/observe/monitors" className="text-link text-[12px]">
          Every monitor
        </Link>
      }
    >
      {all.length === 0 ? (
        <p className="empty-line flex min-h-[220px] items-center justify-center text-center">
          <span>
            A monitor watches a query and says when it crosses a threshold.{" "}
            <Link to="/observe/monitors/new" className="text-link">
              Make one
            </Link>
            .
          </span>
        </p>
      ) : trouble.length === 0 ? (
        <p className="empty-line flex min-h-[220px] items-center justify-center">Nothing is in trouble.</p>
      ) : (
        <ul className="divide-y divide-[#1d1f23]">
          {trouble.slice(0, 8).map((m) => (
            <li key={m.id} className="flex items-center justify-between gap-3 py-2">
              <Link to={`/observe/monitors/${encodeURIComponent(m.id)}`} className="min-w-0 truncate text-[13px] text-[#e4e5e7] hover:text-white">
                {m.name}
              </Link>
              <StateBadge state={m.state} muted={m.muted_until !== null && m.muted_until > now} />
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

/** Whether a replica holds what this one does: the same digest at the same entry of the log. */
function agreement(mine: ObserveStatus["verified_hex"], theirs: ObserveStatus["verified_hex"]): { word: string; tone: string } {
  if (!mine || !theirs) return { word: "not checked", tone: "skipped" };
  if (mine[0] !== theirs[0]) return { word: "checking", tone: "pending" };
  return mine[1] === theirs[1] ? { word: "the same", tone: "succeeded" } : { word: "differs", tone: "failed" };
}

function Store() {
  const res = useRes(statusRes);
  const s = res.data;
  const [busy, setBusy] = React.useState(false);
  const leaderApplied = s.applied;
  return (
    <section className="mt-10" aria-label="The store">
      <Panel
        eyebrow="The store"
        title={`${s.series.toLocaleString()} series · ${formatBytes(s.disk_bytes)} on this machine`}
        description={`Every machine keeps a replica, kept alike through the server's log (Raft): a query is answered where it is asked. ${s.role === "server" ? "This is the server's." : "This is a runner's."}`}
        actions={
          <Button
            variant="secondary"
            size="sm"
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              try {
                await observeApi.verify();
                // Each replica takes its digest as it reaches the entry asked about: a moment.
                window.setTimeout(() => {
                  invalidate("observe/status");
                  setBusy(false);
                }, 2_500);
              } catch (e) {
                toast({ title: "Couldn't ask the replicas", description: errorMessage(e), tone: "failed" });
                setBusy(false);
              }
            }}
          >
            Compare replicas
          </Button>
        }
      >
        <Facts
          items={[
            { label: "Applied", value: `entry ${s.applied.toLocaleString()} (term ${s.term})` },
            { label: "Kept from", value: s.cutoff_ms > 0 ? new Date(s.cutoff_ms).toLocaleString() : "its start" },
            {
              label: "Writes",
              value: `${s.written.toLocaleString()} applied${s.waiting ? ` · ${s.waiting.toLocaleString()} points waiting` : ""}${s.dropped ? ` · ${s.dropped.toLocaleString()} dropped` : ""}`,
              tone: s.dropped ? "fail" : undefined,
            },
            ...(s.error ? [{ label: "Error", value: s.error, tone: "error" as const }] : []),
          ]}
        />
        {s.machines.length > 0 && (
          <div className="table-wrap mt-6">
            <table className="data-table">
              <thead>
                <tr>
                  <th>Machine</th>
                  <th className="num">Behind</th>
                  <th className="num">Series</th>
                  <th className="num">On disk</th>
                  <th>Contents</th>
                </tr>
              </thead>
              <tbody>
                {s.machines.map((m) => {
                  const a = agreement(s.verified_hex, m.status?.verified_hex ?? null);
                  return (
                    <tr key={m.machine}>
                      <td>
                        <StatusDot status={m.up ? "succeeded" : "skipped"} title={m.up ? "Connected" : "Away"} className="mr-2" />
                        {m.machine}
                        {m.status?.fetching && <span className="ml-2 text-[#8c8f98]">fetching a snapshot: {Math.round((100 * m.status.fetching[0]) / Math.max(1, m.status.fetching[1]))}%</span>}
                        {m.status?.error && <p className="text-[11px] text-[#e2c4c6]">{m.status.error}</p>}
                      </td>
                      <td className="num cell-mono">{m.status ? Math.max(0, leaderApplied - m.status.applied).toLocaleString() : "—"}</td>
                      <td className="num cell-mono">{m.status ? m.status.series.toLocaleString() : "—"}</td>
                      <td className="num cell-mono">{m.status ? formatBytes(m.status.disk_bytes) : "—"}</td>
                      <td>
                        <span className="status">
                          <i className="dot" data-status={a.tone} aria-hidden="true" />
                          <span className="status-text" data-status={a.tone}>
                            {a.word}
                          </span>
                        </span>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </section>
  );
}
