import * as React from "react";
import { RefreshCw } from "lucide-react";
import { api } from "@/api/client";
import type { AuditEntry, SystemInfo } from "@/api/types";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { CopyButton, EmptyState, PageIntro, Panel, SearchField, SectionHeading } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Button } from "@/components/ui/button";
import { useRes, warm, type Res } from "@/lib/data";
import { absoluteTime, formatUptime } from "@/lib/format";
import { invalidate } from "@/lib/resource";
import { childStatus, RelTime } from "./shared";

const systemRes: Res<SystemInfo> = { key: "system", fetch: (s) => api.system(s), opts: { pollMs: 15_000 } };
const auditRes: Res<AuditEntry[]> = { key: "audit", fetch: (s) => api.audit(300, s), opts: { pollMs: 60_000 } };

/** Lightweight JSON syntax colouring using the site's prose palette. */
function JsonView({ value }: { value: unknown }) {
  const text = React.useMemo(() => JSON.stringify(value, null, 2) ?? "null", [value]);
  const parts = React.useMemo(() => {
    const re = /("(?:\\.|[^"\\])*")(\s*:)?|\b(true|false|null)\b|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)/g;
    const out: React.ReactNode[] = [];
    let last = 0;
    let m: RegExpExecArray | null;
    let i = 0;
    while ((m = re.exec(text))) {
      if (m.index > last) out.push(text.slice(last, m.index));
      if (m[1]) {
        out.push(
          <span key={i++} className={m[2] ? "text-[#adc2df]" : "text-[#a8c5bb]"}>
            {m[1]}
          </span>,
        );
        if (m[2]) out.push(m[2]);
      } else if (m[3]) out.push(<span key={i++} className="text-[#c6b2e1]">{m[3]}</span>);
      else if (m[4]) out.push(<span key={i++} className="text-[#d2b9a0]">{m[4]}</span>);
      last = re.lastIndex;
    }
    out.push(text.slice(last));
    return out;
  }, [text]);
  return (
    <div className="relative">
      <div className="absolute top-3 right-3">
        <CopyButton text={text} label="Copy" />
      </div>
      <pre className="max-h-[560px] overflow-auto rounded-md border border-[#2b2e39] bg-[#101217] p-4 font-mono text-[11.5px] leading-[1.7] text-[#bcc3d1]">{parts}</pre>
    </div>
  );
}

export default function System() {
  return (
    <div className="container-console page">
      <title>System · kiln</title>
      <PageIntro
        index={9}
        label="System"
        title="System"
        lead="kiln runs every subsystem under an Erlang-style supervisor; `kiln supervise` restarts the whole server with backoff. Generation increments on each process restart."
        actions={
          <Button
            variant="ghost"
            onClick={() => {
              invalidate("system");
              invalidate("audit");
            }}
          >
            <RefreshCw /> Refresh
          </Button>
        }
      />
      <Loadable label="the system" fallback={<SystemHold />}>
        <SystemBody />
      </Loadable>
    </div>
  );
}

function Kpis({ d }: { d: SystemInfo | null }) {
  const blank = "\u00a0";
  return (
    <div className="kpi-row !grid-cols-2 md:!grid-cols-4">
      <div className="kpi">
        <div className="label">Version</div>
        <div className="kpi-value">{d ? d.version : blank}</div>
      </div>
      <div className="kpi">
        <div className="label">Uptime</div>
        <div className="kpi-value">{d ? formatUptime(d.uptime_s) : blank}</div>
      </div>
      <div className="kpi">
        <div className="label">Generation</div>
        <div className="kpi-value">{d ? d.generation : blank}</div>
        <div className="kpi-foot">process restarts by supervise</div>
      </div>
      <div className="kpi">
        <div className="label">Subsystems</div>
        <div className="kpi-value">
          {d ? (
            <>
              {d.children.filter((c) => c.state === "running").length}
              <small>/ {d.children.length} running</small>
            </>
          ) : (
            blank
          )}
        </div>
      </div>
    </div>
  );
}

function ChildrenHead() {
  return (
    <thead>
      <tr>
        <th className="w-6" aria-label="Health" />
        <th>Child</th>
        <th>State</th>
        <th className="num">Restarts</th>
        <th className="hidden sm:table-cell">Since</th>
        <th>Last error</th>
      </tr>
    </thead>
  );
}

function AuditHead() {
  return (
    <thead>
      <tr>
        <th>Time</th>
        <th>Actor</th>
        <th>Action</th>
        <th>Detail</th>
      </tr>
    </thead>
  );
}

function SystemBody() {
  warm(systemRes, auditRes);
  const d = useRes(systemRes).data;
  const audit = useRes(auditRes).data;
  const [q, setQ] = React.useState("");
  const auditRows = audit.filter((a) => !q || `${a.actor} ${a.action} ${a.detail}`.toLowerCase().includes(q.toLowerCase()));

  return (
    <>
      <Kpis d={d} />

      <section className="mt-12">
        <SectionHeading index="01" label="Supervisor" title="Children" />
        <div className="table-wrap">
          <table className="data-table">
            <ChildrenHead />
            <tbody>
              {d.children.map((c) => (
                <tr key={c.name}>
                  <td>
                    <StatusDot status={childStatus(c)} title={c.state} />
                  </td>
                  <td className="text-[#e4e5e7]">{c.name}</td>
                  <td className="cell-mono">{c.state}</td>
                  <td className={`num cell-mono ${c.restarts > 0 ? "!text-[#cbb69a]" : ""}`}>{c.restarts}</td>
                  <td className="cell-mono hidden whitespace-nowrap sm:table-cell" title={absoluteTime(c.since_unix)}>
                    <RelTime ts={c.since_unix} />
                  </td>
                  <td className="truncate-cell font-mono text-[11px] text-[#cbb69a]" title={c.last_error ?? undefined}>
                    {c.last_error ?? <span className="text-[#6d7078]">—</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>

      <section className="mt-12 grid gap-4 lg:grid-cols-2">
        <Panel eyebrow="Process" title="Crash log" description="Recorded by kiln supervise when the server exits abnormally.">
          {d.crash_log.length === 0 ? (
            <div className="flex items-center gap-2.5 py-6 font-mono text-[11px] text-[#a8c9bc]">
              <i className="dot" data-status="succeeded" /> No crashes recorded.
            </div>
          ) : (
            <pre className="max-h-[420px] overflow-auto rounded-md border border-[#2b2e39] bg-[#101217] p-4 font-mono text-[11px] leading-[1.7] whitespace-pre-wrap text-[#d8c8b0]">
              {d.crash_log.join("\n")}
            </pre>
          )}
        </Panel>
        <Panel eyebrow="Read-only" title="Effective configuration" description="Secrets are redacted by the server.">
          <JsonView value={d.config} />
        </Panel>
      </section>

      <section className="mt-12">
        <SectionHeading index="02" label="Audit" title="Audit log" aside={<SearchField className="w-56" label="Filter audit log" placeholder="Filter…" value={q} onChange={setQ} />} />
        {auditRows.length === 0 ? (
          <EmptyState title={q ? "No entries match." : "No audit entries yet."} />
        ) : (
          <div className="table-wrap max-h-[640px] overflow-y-auto">
            <table className="data-table">
              <AuditHead />
              <tbody>
                {auditRows.map((a, i) => (
                  <tr key={`${a.ts}-${i}`}>
                    <td className="cell-mono whitespace-nowrap" title={absoluteTime(a.ts)}>
                      <RelTime ts={a.ts} />
                    </td>
                    <td className="cell-mono whitespace-nowrap">{a.actor}</td>
                    <td className="whitespace-nowrap">
                      <span className="chip">{a.action}</span>
                    </td>
                    <td className="truncate-cell text-[#b9bbc2]" title={a.detail}>
                      {a.detail}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </>
  );
}

/** The page while it loads: its figures' labels, its tables' heads, its panels' frames. */
function SystemHold() {
  return (
    <>
      <Kpis d={null} />
      <section className="mt-12">
        <SectionHeading index="01" label="Supervisor" title="Children" />
        <TableHold head={<ChildrenHead />} rows={8} rowHeight={39} label="Loading the system" />
      </section>
      <section className="mt-12 grid gap-4 lg:grid-cols-2">
        <Panel eyebrow="Process" title="Crash log" description="Recorded by kiln supervise when the server exits abnormally.">
          <Hold height={160} />
        </Panel>
        <Panel eyebrow="Read-only" title="Effective configuration" description="Secrets are redacted by the server.">
          <Hold height={160} />
        </Panel>
      </section>
      <section className="mt-12">
        <SectionHeading index="02" label="Audit" title="Audit log" />
        <TableHold head={<AuditHead />} rows={8} rowHeight={39} />
      </section>
    </>
  );
}
