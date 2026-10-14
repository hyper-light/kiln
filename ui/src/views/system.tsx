import * as React from "react";
import { RefreshCw } from "lucide-react";
import { api } from "@/api/client";
import { CopyButton, EmptyState, ErrorState, PageIntro, Panel, SearchField, SectionHeading, Skeleton, SkeletonRows } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Button } from "@/components/ui/button";
import { absoluteTime, formatUptime } from "@/lib/format";
import { useDocumentTitle, useResource } from "@/lib/hooks";
import { childStatus, RelTime } from "./shared";

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
  useDocumentTitle("System");
  const sys = useResource((s) => api.system(s), [], { pollMs: 15_000 });
  const audit = useResource((s) => api.audit(300, s), []);
  const [q, setQ] = React.useState("");
  const d = sys.data;

  const auditRows = (audit.data ?? []).filter((a) => !q || `${a.actor} ${a.action} ${a.detail}`.toLowerCase().includes(q.toLowerCase()));

  return (
    <div className="container-console page">
      <PageIntro
        index={7}
        label="System"
        title="System"
        lead="kiln runs every subsystem under an Erlang-style supervisor; `kiln supervise` restarts the whole server with backoff. Generation increments on each process restart."
        actions={
          <Button variant="ghost" onClick={() => { sys.reload(); audit.reload(); }}>
            <RefreshCw /> Refresh
          </Button>
        }
      />

      {sys.error && !d && <ErrorState error={sys.error} onRetry={sys.reload} />}

      <div className="kpi-row !grid-cols-2 md:!grid-cols-4">
        <div className="kpi">
          <div className="label">Version</div>
          <div className="kpi-value">{d ? d.version : <Skeleton className="h-8 w-20" />}</div>
        </div>
        <div className="kpi">
          <div className="label">Uptime</div>
          <div className="kpi-value">{d ? formatUptime(d.uptime_s) : <Skeleton className="h-8 w-20" />}</div>
        </div>
        <div className="kpi">
          <div className="label">Generation</div>
          <div className="kpi-value">{d ? d.generation : <Skeleton className="h-8 w-12" />}</div>
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
              <Skeleton className="h-8 w-16" />
            )}
          </div>
        </div>
      </div>

      <section className="mt-12">
        <SectionHeading index="01" label="Supervisor" title="Children" />
        {!d ? (
          <SkeletonRows rows={6} cols={4} />
        ) : (
          <div className="table-wrap">
            <table className="data-table">
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
        )}
      </section>

      <section className="mt-12 grid gap-4 lg:grid-cols-2">
        <Panel eyebrow="Process" title="Crash log" description="Recorded by kiln supervise when the server exits abnormally.">
          {!d ? (
            <Skeleton className="h-40 w-full" />
          ) : d.crash_log.length === 0 ? (
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
          {!d ? <Skeleton className="h-40 w-full" /> : <JsonView value={d.config} />}
        </Panel>
      </section>

      <section className="mt-12">
        <SectionHeading
          index="02"
          label="Audit"
          title="Audit log"
          aside={<SearchField className="w-56" label="Filter audit log" placeholder="Filter…" value={q} onChange={setQ} />}
        />
        {audit.error && !audit.data ? (
          <ErrorState error={audit.error} onRetry={audit.reload} />
        ) : !audit.data ? (
          <SkeletonRows rows={8} cols={4} />
        ) : auditRows.length === 0 ? (
          <EmptyState title={q ? "No entries match." : "No audit entries yet."} />
        ) : (
          <div className="table-wrap max-h-[640px] overflow-y-auto">
            <table className="data-table">
              <thead>
                <tr>
                  <th>Time</th>
                  <th>Actor</th>
                  <th>Action</th>
                  <th>Detail</th>
                </tr>
              </thead>
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
    </div>
  );
}
