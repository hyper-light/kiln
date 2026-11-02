/**
 * Error Tracking (docs/observe.md §8.8): errors grouped into issues as Datadog groups them, a
 * service's errors of one type, alike in their words, thrown at one place. The issues over the
 * range, the most errors first; an issue's page: its errors over time by version, its latest error
 * whole (its stack, its trace), its status, and what changed it.
 */
import { observeApi, type Issue, type IssueDetail, type IssueList, type IssueStatus } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, PageIntro, Panel } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useParams } from "@/lib/router";
import { SearchForm } from "./logs";
import { ObserveNav, QueryChart, RANGES, RangePicker, compact, pollFor, useParam, useRange, type Range } from "./parts";

const LABEL: Record<IssueStatus, string> = { for_review: "For review", reviewed: "Reviewed", resolved: "Resolved", ignored: "Ignored" };
const FILTERS: { value: IssueStatus | "all"; label: string }[] = [
  { value: "for_review", label: "For review" },
  { value: "reviewed", label: "Reviewed" },
  { value: "resolved", label: "Resolved" },
  { value: "ignored", label: "Ignored" },
  { value: "all", label: "All" },
];

const issuesRes = (range: Range, status: string, query: string): Res<IssueList> => ({
  key: `observe/errors/${range}/${status}/${query}`,
  fetch: (s) => {
    const to = Date.now();
    return observeApi.issues(to - RANGES[range], to, { status: status === "all" ? undefined : status, query: query || undefined }, s);
  },
  opts: { pollMs: pollFor(range) },
});

const issueRes = (id: string, range: Range): Res<IssueDetail> => ({
  key: `observe/errors/issue/${id}/${range}`,
  fetch: (s) => {
    const to = Date.now();
    return observeApi.issue(id, to - RANGES[range], to, s);
  },
  opts: { pollMs: pollFor(range) },
});

/** Where an issue's errors were thrown: its file's name, and its function. */
function place(i: Issue) {
  const file = i.file.split(/[\\/]/).pop() ?? i.file;
  if (!file) return i.function || "—";
  return i.function ? `${file} · ${i.function}` : file;
}

const ago = (ms: number) => relativeTime(Math.floor(ms / 1000));

function Regression() {
  return <span className="ml-2 rounded-full border border-[#f27a7a]/40 px-2 py-px text-[10.5px] tracking-wide text-[#f27a7a] uppercase">Regression</span>;
}

export default function ErrorTracking() {
  const [range, setRange] = useRange("1d");
  const [status, setStatus] = useParam("status", "for_review");
  const [query, setQuery] = useParam("q", "");
  return (
    <div className="container-console page">
      <title>Error Tracking · Observe · kiln</title>
      <PageIntro
        index={9}
        label="Observe"
        title="Error Tracking"
        lead="Errors grouped into issues: a service's errors of one type, alike in their words, thrown at one place. Every error of a request that failed with it is counted, from its spans and its logs."
      />
      <ObserveNav />
      <div className="mb-4 flex flex-wrap items-center gap-3">
        <RangePicker value={range} onChange={setRange} />
        <Segmented label="Status" value={status as IssueStatus | "all"} onChange={setStatus} options={FILTERS} />
      </div>
      <SearchForm key={query} initial={query} onSearch={setQuery} placeholder="Words of its type, message, service or place" />
      <Loadable key={`${range}/${status}/${query}`} label="the issues" fallback={<TableHold head={<IssuesHead />} rows={6} />}>
        <IssuesTable range={range} status={status} query={query} />
      </Loadable>
    </div>
  );
}

function IssuesHead() {
  return (
    <tr>
      <th>Issue</th>
      <th className="hidden md:table-cell">Service</th>
      <th className="text-right">Errors</th>
      <th className="hidden text-right sm:table-cell">Last seen</th>
      <th className="hidden text-right lg:table-cell">First seen</th>
    </tr>
  );
}

function IssuesTable({ range, status, query }: { range: Range; status: string; query: string }) {
  const l = useRes(issuesRes(range, status, query)).data;
  if (l.issues.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        No issues here. An issue is the error a request failed with: a service's entry span in error with its error's type, message and stack, or an error's log with its kind or stack.
      </p>
    );
  return (
    <div className="table-wrap">
      <table className="data-table">
        <thead>
          <IssuesHead />
        </thead>
        <tbody>
          {l.issues.map((i) => (
            <tr key={i.id}>
              <td className="truncate-cell">
                <Link to={`/observe/errors/${encodeURIComponent(i.id)}?range=${range}`} className="block min-w-0 hover:text-white">
                  <span className="flex min-w-0 items-center">
                    <span className="truncate font-mono text-[12.5px] text-[#e4e5e7]" title={i.type}>
                      {i.type || "(no type)"}
                    </span>
                    {i.regression && <Regression />}
                  </span>
                  <span className="block truncate text-[12.5px] text-[#b4b6bd]" title={i.message}>
                    {i.message || "(no message)"}
                  </span>
                  <span className="block truncate font-mono text-[11px] text-[#7b7e88]" title={`${i.file} ${i.function}`}>
                    {place(i)}
                    <span className="md:hidden"> · {i.service}</span>
                  </span>
                </Link>
              </td>
              <td className="hidden truncate-cell font-mono text-[12px] text-[#b4b6bd] md:table-cell">{i.service}</td>
              <td className="text-right font-mono text-[12px] whitespace-nowrap">{i.errors === null ? "—" : compact(i.errors)}</td>
              <td className="hidden text-right text-[12px] whitespace-nowrap text-[#b4b6bd] sm:table-cell">{ago(i.last_seen)}</td>
              <td className="hidden text-right text-[12px] whitespace-nowrap text-[#8c8f98] lg:table-cell">{ago(i.first_seen)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {l.total > l.issues.length && (
        <p className="mt-3 text-[12px] text-[#8c8f98]">
          The first {l.issues.length} of {l.total.toLocaleString()}: a search narrows them.
        </p>
      )}
    </div>
  );
}

export function IssuePage() {
  const { id = "" } = useParams();
  const [range, setRange] = useRange("1d");
  return (
    <div className="container-console page">
      <title>Issue · Error Tracking · Observe · kiln</title>
      <Loadable key={`${id}/${range}`} label="the issue" fallback={<Hold height={560} label="Loading the issue" />}>
        <IssueView id={id} range={range} onRange={setRange} />
      </Loadable>
    </div>
  );
}

function IssueView({ id, range, onRange }: { id: string; range: Range; onRange: (r: Range) => void }) {
  const res = useRes(issueRes(id, range));
  const d = res.data;
  const i = d.issue;
  const setStatus = async (status: IssueStatus) => {
    try {
      const after = await observeApi.setIssueStatus(i.id, status);
      res.setData((prev) => ({ ...prev, issue: { ...after, errors: prev.issue.errors } }));
      invalidate("observe/errors");
    } catch (e) {
      toast({ title: "Couldn't change the issue's status", description: errorMessage(e), tone: "failed" });
    }
  };
  const stack = d.latest?.attributes["error.stack"] ?? d.latest?.attributes["exception.stacktrace"] ?? "";
  const filter = `issue.id:${i.id}`;
  return (
    <>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={9} items={[{ label: "Observe", to: "/observe" }, { label: "Error Tracking", to: `/observe/errors?range=${range}` }, { label: i.service }]} />
          <h1 className="flex min-w-0 items-center font-mono">
            <span className="truncate" title={i.type}>
              {i.type || "(no type)"}
            </span>
            {i.regression && <Regression />}
          </h1>
          <p className="mt-2 text-[14px] break-words text-[#b4b6bd]">{i.message || "(no message)"}</p>
          <p className="mt-1 font-mono text-[12px] break-all text-[#7b7e88]">{[i.file, i.function].filter(Boolean).join(" · ")}</p>
        </div>
      </header>
      <div className="mb-5 flex flex-wrap items-center gap-3">
        <RangePicker value={range} onChange={onRange} />
        <Segmented label="Its status" value={i.status} onChange={(s) => void setStatus(s)} options={FILTERS.filter((f): f is { value: IssueStatus; label: string } => f.value !== "all")} />
      </div>
      <div className="space-y-6">
        <Facts
          items={[
            { label: "Errors", value: i.errors === null ? "—" : compact(i.errors), tone: (i.errors ?? 0) > 0 ? "fail" : undefined },
            { label: "Service", value: i.service },
            { label: "From", value: i.source === "log" ? "Its logs" : "Its spans" },
            { label: "Last seen", value: ago(i.last_seen) },
            { label: "First seen", value: ago(i.first_seen) },
            { label: "Status", value: LABEL[i.status] },
          ]}
        />
        <Panel title="Errors" description="Each window's errors of the issue, by the version that threw them.">
          <Loadable label="its errors" fallback={<Hold height={180} />}>
            <QueryChart queries={[`sum:error_tracking.errors{${filter}} by {version}.as_count()`]} range={range} display="bars" />
          </Loadable>
        </Panel>
        <Panel
          title="Its latest error"
          description={d.latest ? `${ago(Math.floor(d.latest.ts_ns / 1e6))}, on ${d.latest.host || "its host"}` : "None in this range."}
          actions={
            d.latest?.trace_id ? (
              <Button variant="secondary" asChild>
                <Link to={`/observe/traces/${d.latest.trace_id}`}>Its trace</Link>
              </Button>
            ) : undefined
          }
        >
          {stack ? (
            <pre className="max-h-[420px] overflow-auto rounded-xl border border-white/10 bg-black/30 p-4 font-mono text-[11.5px] leading-relaxed whitespace-pre text-[#d6d7da]">{stack}</pre>
          ) : (
            <p className="empty-line py-6 text-center">{d.latest ? "It says no stack." : "No error of it in this range."}</p>
          )}
        </Panel>
        <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
          <Panel title="Versions" description="Its errors in the range by the version that threw them, and every version it has been seen in.">
            <Breakdown rows={d.versions.map((v) => ({ key: v.version, errors: v.errors }))} empty="No errors in this range." />
            {i.versions.length > 0 && <p className="mt-3 font-mono text-[11.5px] break-words text-[#8c8f98]">Seen in {i.versions.join(", ")}</p>}
          </Panel>
          <Panel title="Activity" description="Its status, and what changed it.">
            <ul className="space-y-2">
              {[...i.activity].reverse().map((a, n) => (
                <li key={`${a.at}/${n}`} className="flex flex-wrap items-baseline gap-x-3 text-[12.5px]">
                  <span className="text-[#e4e5e7]">{LABEL[a.status]}</span>
                  <span className="text-[#8c8f98]">{a.by === "user" ? "by a user" : a.why || "by kiln"}</span>
                  <span className="ml-auto text-[11.5px] text-[#7b7e88]">{ago(a.at)}</span>
                </li>
              ))}
            </ul>
          </Panel>
        </div>
      </div>
    </>
  );
}

function Breakdown({ rows, empty }: { rows: { key: string; errors: number }[]; empty: string }) {
  if (rows.length === 0) return <p className="empty-line py-6 text-center">{empty}</p>;
  return (
    <div className="table-wrap">
      <table className="data-table">
        <tbody>
          {rows.map((r) => (
            <tr key={r.key}>
              <td className="truncate-cell font-mono text-[12px] text-[#e4e5e7]">{r.key === "N/A" ? "(none said)" : r.key}</td>
              <td className="text-right font-mono text-[12px]">{compact(r.errors)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
