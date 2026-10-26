import * as React from "react";
import { Download, RefreshCw } from "lucide-react";
import { api } from "@/api/client";
import type { AuditEntry, SystemInfo } from "@/api/types";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { CopyButton, EmptyState, FoldSection, PageIntro, SearchField, openFold } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { Button } from "@/components/ui/button";
import { updatesRes, useRes, warm, type Res } from "@/lib/data";
import { fileStamp, saveFile, toCsv } from "@/lib/download";
import { absoluteTime, formatUptime } from "@/lib/format";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { useLocation } from "@/lib/router";
import { KeepSection } from "./keep/section";
import { childStatus, RelTime } from "./shared";
import { StoresHold, StoresSection } from "./stores/section";
import { BuildsSection, UpdatesSection } from "./updates/section";

// Live while the page is open: the children, the crash log and the figures follow the server within seconds.
const systemRes: Res<SystemInfo> = { key: "system", fetch: (s) => api.system(s), opts: { pollMs: 5_000 } };

const CRASH_LEAD = "Recorded by kiln supervise when the server exits abnormally.";
const CONFIG_LEAD = "Secrets are redacted by the server.";

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
  warm(updatesRes);
  // At /system#<section>, arriving or following a link here: that one opens, and comes into view.
  const { hash } = useLocation();
  const arrived = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (el && hash) openFold(hash);
    },
    [hash],
  );
  return (
    <div ref={arrived} className="container-console page">
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
              invalidate("updates");
              invalidate("conversations");
              invalidate("reach/keep");
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
        <th className="w-7" aria-label="Health" />
        <th className="sm:w-[26%]">Child</th>
        <th className="w-px sm:w-[14%]">State</th>
        <th className="num w-px sm:w-[10%]">Restarts</th>
        <th className="hidden w-[14%] sm:table-cell">Since</th>
        <th className="hidden sm:table-cell">Last error</th>
      </tr>
    </thead>
  );
}

function AuditHead() {
  return (
    <thead>
      <tr>
        <th className="w-[7%]">Time</th>
        <th className="hidden w-[14%] sm:table-cell">Who</th>
        <th className="md:w-[31%]">Action</th>
        <th className="hidden w-[5%] md:table-cell">Result</th>
        <th className="hidden w-[8%] lg:table-cell">Scope</th>
        <th className="hidden w-[10%] xl:table-cell">Address</th>
        <th className="hidden md:table-cell">Detail</th>
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
    <FoldSection id="system.audit" index="08" label="Audit" title="Audit log" aside={<Segmented label="Window" value={win} onChange={setWin} options={AUDIT_WINDOWS} />}>
      <Loadable label="the audit log" fallback={<TableHold head={<AuditHead />} rows={8} rowHeight={39} />}>
        <AuditTable win={win} />
      </Loadable>
    </FoldSection>
  );
}

function AuditTable({ win }: { win: AuditWindow }) {
  const since = win === "all" ? undefined : Math.floor(Date.now() / 1000 / 3600) * 3600 - Number(win) * 86400;
  // Live while its section is open (folded, it isn't rendered and doesn't ask).
  const rows = useSuspenseResource(`audit/${win}`, (s) => api.audit({ since, limit: 1000 }, s), { pollMs: 10_000 }).data;
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
      <div className="log-tools">
        <FacetBox
          className="min-w-0 flex-1"
          label="Filter the audit log"
          placeholder="Filter by who, action, scope, result, address, or words…"
          query={q}
          onQuery={setQ}
          facets={facets}
          chosen={chosen}
          onChosen={setChosen}
        />
        <Button variant="ghost" size="sm" disabled={shown.length === 0} title="Download the entries shown, as CSV" onClick={() => saveFile(`kiln-audit-${fileStamp()}.csv`, auditCsv(shown), "text/csv;charset=utf-8")}>
          <Download /> Download
        </Button>
      </div>
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
                    {/* On a phone who did it reads under when. */}
                    <div className="max-w-[112px] truncate text-[10px] text-[#7b7e88] sm:hidden">{a.principal}</div>
                  </td>
                  <td className="cell-mono hidden max-w-0 sm:table-cell">
                    <div className="truncate">{a.principal}</div>
                    {a.on_behalf_of && <div className="truncate text-[10px] text-[#7b7e88]">for {a.on_behalf_of}</div>}
                  </td>
                  <td className="max-w-0">
                    <span className="chip max-w-full truncate">{a.action}</span>
                    {a.target && !a.action.endsWith(a.target) && <div className="mt-1 truncate font-mono text-[10px] text-[#7b7e88]">{a.target}</div>}
                    {/* Without room for its own column, its detail reads in full here. */}
                    {a.detail && <div className="mt-1 text-[11px] whitespace-normal text-[#b9bbc2] [overflow-wrap:anywhere] md:hidden">{a.detail}</div>}
                  </td>
                  <td className={`cell-mono hidden md:table-cell ${/^[45]/.test(a.result) ? "!text-[#d1aaad]" : ""}`}>{a.result}</td>
                  <td className="cell-mono hidden max-w-0 truncate lg:table-cell">{a.scope ?? "—"}</td>
                  <td className="cell-mono hidden whitespace-nowrap xl:table-cell">{a.ip ?? "—"}</td>
                  {/* Cut to its share of the width, the whole of it on hover. */}
                  <td className="hidden max-w-0 truncate text-[#b9bbc2] md:table-cell" title={a.detail}>
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

/** The audit entries as CSV, a column per field. */
function auditCsv(rows: AuditEntry[]): string {
  return toCsv(
    ["time", "who", "for", "action", "target", "result", "scope", "address", "token", "detail"],
    rows.map((a) => [new Date(a.ts * 1000).toISOString(), a.principal, a.on_behalf_of, a.action, a.target, a.result, a.scope, a.ip, a.token, a.detail]),
  );
}

/** The supervisor's crash log, newest first: filtered as you type, downloaded as what it shows. */
function CrashLog({ lines }: { lines: string[] }) {
  const [q, setQ] = React.useState("");
  if (lines.length === 0) {
    return (
      <div className="flex items-center gap-2.5 py-2 font-mono text-[11px] text-[#a8c9bc]">
        <i className="dot" data-status="succeeded" /> No crashes recorded.
      </div>
    );
  }
  const words = q.trim().toLowerCase();
  const shown = words ? lines.filter((l) => l.toLowerCase().includes(words)) : lines;
  return (
    <>
      <div className="log-tools">
        <SearchField className="min-w-0 flex-1" label="Filter the crash log" placeholder="Filter by generation, signal, or words…" value={q} onChange={setQ} />
        <span className="log-count">{words ? `${shown.length} of ${lines.length}` : lines.length} lines</span>
        <Button variant="ghost" size="sm" disabled={shown.length === 0} title="Download the lines shown" onClick={() => saveFile(`kiln-crash-log-${fileStamp()}.txt`, shown.join("\n") + "\n")}>
          <Download /> Download
        </Button>
      </div>
      {shown.length === 0 ? (
        <EmptyState title="No lines match." />
      ) : (
        <pre className="max-h-[420px] overflow-y-auto rounded-md border border-[#2b2e39] bg-[#101217] p-4 font-mono text-[11px] leading-[1.7] [overflow-wrap:anywhere] whitespace-pre-wrap text-[#d8c8b0]">
          {shown.join("\n")}
        </pre>
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

      <FoldSection id="system.children" index="01" label="Supervisor" title="Children">
        <div className="table-wrap">
          <table className="data-table">
            <ChildrenHead />
            <tbody>
              {d.children.map((c) => (
                <tr key={c.name}>
                  <td>
                    <StatusDot status={childStatus(c)} title={c.state} />
                  </td>
                  <td className="whitespace-nowrap text-[#e4e5e7]">
                    {c.name}
                    {/* On a phone its error reads in full under its name: a column that narrow shows nothing of it. */}
                    {c.last_error && <div className="mt-0.5 font-mono text-[10.5px] whitespace-normal text-[#cbb69a] [overflow-wrap:anywhere] sm:hidden">{c.last_error}</div>}
                  </td>
                  <td className="cell-mono whitespace-nowrap">{c.state}</td>
                  <td className={`num cell-mono ${c.restarts > 0 ? "!text-[#cbb69a]" : ""}`}>{c.restarts}</td>
                  <td className="cell-mono hidden whitespace-nowrap sm:table-cell" title={absoluteTime(c.since_unix)}>
                    <RelTime ts={c.since_unix} />
                  </td>
                  {/* Cut to its share of the width: the columns before it keep theirs. */}
                  <td className="hidden max-w-0 truncate font-mono text-[11px] text-[#cbb69a] sm:table-cell" title={c.last_error ?? undefined}>
                    {c.last_error ?? <span className="text-[#6d7078]">—</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </FoldSection>

      <UpdatesSection />

      <BuildsSection />

      <FoldSection id="system.crash" index="04" label="Process" title="Crash log">
        <p className="section-lead">{CRASH_LEAD}</p>
        <CrashLog lines={d.crash_log} />
      </FoldSection>

      <FoldSection id="system.config" index="05" label="Read-only" title="Effective configuration">
        <p className="section-lead">{CONFIG_LEAD}</p>
        <JsonView value={d.config} />
      </FoldSection>

      <StoresSection index="06" />

      <KeepSection index="07" />

      <AuditSection />
    </>
  );
}

/** The page while it loads: its figures' labels, its tables' heads, its panels' frames. */
function SystemHold() {
  return (
    <>
      <Kpis d={null} />
      <FoldSection id="system.children" index="01" label="Supervisor" title="Children">
        <TableHold head={<ChildrenHead />} rows={8} rowHeight={39} label="Loading the system" />
      </FoldSection>
      <UpdatesSection hold />
      <BuildsSection hold />
      <FoldSection id="system.crash" index="04" label="Process" title="Crash log">
        <p className="section-lead">{CRASH_LEAD}</p>
        <Hold height={160} />
      </FoldSection>
      <FoldSection id="system.config" index="05" label="Read-only" title="Effective configuration">
        <p className="section-lead">{CONFIG_LEAD}</p>
        <Hold height={160} />
      </FoldSection>
      <FoldSection id="system.stores" index="06" label="Conversations" title="Conversation stores">
        <StoresHold />
      </FoldSection>
      <KeepSection index="07" hold />
      <FoldSection id="system.audit" index="08" label="Audit" title="Audit log">
        <TableHold head={<AuditHead />} rows={8} rowHeight={39} />
      </FoldSection>
    </>
  );
}
