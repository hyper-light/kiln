import * as React from "react";
import { RefreshCw } from "lucide-react";
import { api } from "@/api/client";
import type { AuditEntry, SystemInfo } from "@/api/types";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { CopyButton, EmptyState, PageIntro, Panel, SectionHeading } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { Button } from "@/components/ui/button";
import { useRes, warm, type Res } from "@/lib/data";
import { absoluteTime, formatUptime } from "@/lib/format";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { childStatus, RelTime } from "./shared";
import { StoresHold, StoresSection } from "./stores/section";

const systemRes: Res<SystemInfo> = { key: "system", fetch: (s) => api.system(s), opts: { pollMs: 15_000 } };

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
      } else if (m[3])
        out.push(
          <span key={i++} className="text-[#c6b2e1]">
            {m[3]}
          </span>,
        );
      else if (m[4])
        out.push(
          <span key={i++} className="text-[#d2b9a0]">
            {m[4]}
          </span>,
        );
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
      <pre className="max-h-[560px] overflow-y-auto rounded-md border border-[#2b2e39] bg-[#101217] p-4 font-mono text-[11.5px] leading-[1.7] [overflow-wrap:anywhere] whitespace-pre-wrap text-[#bcc3d1]">
        {parts}
      </pre>
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
              invalidate("conversations");
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
        <th>Who</th>
        <th>Action</th>
        <th className="hidden md:table-cell">Result</th>
        <th className="hidden lg:table-cell">Scope</th>
        <th className="hidden xl:table-cell">Address</th>
        <th>Detail</th>
      </tr>
    </thead>
  );
}

/** The audit log's windows: how far back it reads (docs/ci.md W19-R05). */
type AuditWindow = "1" | "7" | "30" | "all";
const AUDIT_WINDOWS: { value: AuditWindow; label: string }[] = [
  { value: "1", label: "24 hours" },
  { value: "7", label: "7 days" },
  { value: "30", label: "30 days" },
  { value: "all", label: "All" },
];

/** The fields the log filters by, as facets. */
const AUDIT_FACETS: { id: "principal" | "on_behalf_of" | "action" | "scope" | "result" | "ip" | "token"; label: string }[] = [
  { id: "principal", label: "Who" },
  { id: "on_behalf_of", label: "For whom" },
  { id: "action", label: "Action" },
  { id: "scope", label: "Scope" },
  { id: "result", label: "Result" },
  { id: "ip", label: "Address" },
  { id: "token", label: "Token" },
];

/** Whether a row has what's chosen in every facet but `skip`, and the words. */
function auditTakes(a: AuditEntry, chosen: Chosen, q: string, skip?: string) {
  for (const f of AUDIT_FACETS) {
    const want = chosen[f.id];
    if (f.id !== skip && want?.length && !want.includes(a[f.id] ?? "")) return false;
  }
  const words = q.trim().toLowerCase();
  return !words || [a.principal, a.on_behalf_of, a.action, a.target, a.detail, a.ip, a.result].some((v) => v?.toLowerCase().includes(words));
}

function AuditSection() {
  const [win, setWin] = React.useState<AuditWindow>("7");
  return (
    <section className="mt-12">
      <SectionHeading index="03" label="Audit" title="Audit log" aside={<Segmented label="Window" value={win} onChange={setWin} options={AUDIT_WINDOWS} />} />
      <Loadable label="the audit log" fallback={<TableHold head={<AuditHead />} rows={8} rowHeight={39} />}>
        <AuditTable win={win} />
      </Loadable>
    </section>
  );
}

function AuditTable({ win }: { win: AuditWindow }) {
  const since = win === "all" ? undefined : Math.floor(Date.now() / 1000 / 3600) * 3600 - Number(win) * 86400;
  const rows = useSuspenseResource(`audit/${win}`, (s) => api.audit({ since, limit: 1000 }, s), { pollMs: 60_000 }).data;
  const [chosen, setChosen] = React.useState<Chosen>({});
  const [q, setQ] = React.useState("");
  const facets: Facet[] = AUDIT_FACETS.map((f) => {
    const counts = new Map<string, number>();
    for (const a of rows) {
      const v = a[f.id];
      if (v && auditTakes(a, chosen, q, f.id)) counts.set(v, (counts.get(v) ?? 0) + 1);
    }
    return { id: f.id, label: f.label, options: [...counts.entries()].sort((x, y) => y[1] - x[1]).map(([v, n]) => ({ id: v, label: v, count: n })) };
  });
  const shown = rows.filter((a) => auditTakes(a, chosen, q));
  return (
    <>
      <FacetBox
        className="mb-4"
        label="Filter the audit log"
        placeholder="Filter by who, action, scope, result, address, or words…"
        query={q}
        onQuery={setQ}
        facets={facets}
        chosen={chosen}
        onChosen={setChosen}
      />
      {shown.length === 0 ? (
        <EmptyState title={rows.length ? "No entries match." : "No audit entries in this window."} />
      ) : (
        <div className="table-wrap max-h-[640px] overflow-y-auto">
          <table className="data-table">
            <AuditHead />
            <tbody>
              {shown.map((a) => (
                <tr key={a.id}>
                  <td className="cell-mono whitespace-nowrap" title={absoluteTime(a.ts)}>
                    <RelTime ts={a.ts} />
                  </td>
                  <td className="cell-mono max-w-[220px]">
                    <div className="truncate">{a.principal}</div>
                    {a.on_behalf_of && <div className="truncate text-[10px] text-[#7b7e88]">for {a.on_behalf_of}</div>}
                  </td>
                  <td className="max-w-[320px]">
                    <span className="chip max-w-full truncate">{a.action}</span>
                    {a.target && !a.action.endsWith(a.target) && <div className="mt-1 truncate font-mono text-[10px] text-[#7b7e88]">{a.target}</div>}
                  </td>
                  <td className={`cell-mono hidden md:table-cell ${/^[45]/.test(a.result) ? "!text-[#d1aaad]" : ""}`}>{a.result}</td>
                  <td className="cell-mono hidden max-w-[160px] truncate lg:table-cell">{a.scope ?? "—"}</td>
                  <td className="cell-mono hidden xl:table-cell">{a.ip ?? "—"}</td>
                  <td className="truncate-cell text-[#b9bbc2]" title={a.detail}>
                    {a.detail || <span className="text-[#6d7078]">—</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

function SystemBody() {
  warm(systemRes);
  const d = useRes(systemRes).data;

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
            <pre className="max-h-[420px] overflow-y-auto rounded-md border border-[#2b2e39] bg-[#101217] p-4 font-mono text-[11px] leading-[1.7] [overflow-wrap:anywhere] whitespace-pre-wrap text-[#d8c8b0]">
              {d.crash_log.join("\n")}
            </pre>
          )}
        </Panel>
        <Panel eyebrow="Read-only" title="Effective configuration" description="Secrets are redacted by the server.">
          <JsonView value={d.config} />
        </Panel>
      </section>

      <StoresSection index="02" />

      <AuditSection />
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
        <SectionHeading index="02" label="Conversations" title="Conversation stores" />
        <StoresHold />
      </section>
      <section className="mt-12">
        <SectionHeading index="03" label="Audit" title="Audit log" />
        <TableHold head={<AuditHead />} rows={8} rowHeight={39} />
      </section>
    </>
  );
}
