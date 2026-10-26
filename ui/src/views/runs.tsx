import * as React from "react";
import { Link, useNavigate, useSearchParams } from "@/lib/router";
import { ChartLine, GitBranch, Search, Activity } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { RunRow, Status } from "@/api/types";
import { Button } from "@/components/ui/button";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { EmptyState, ErrorState, PageIntro, SelectField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { reposRes, useRepos, warm } from "@/lib/data";
import { firstLine, shortSha, statusLabel } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { useWindowedRows } from "@/lib/virtual";
import { EventIcon, LiveDuration, RelTime } from "./shared";

const PAGE = 50;
/** A row's height until the first one is measured (a list of a million scrolls as one of forty: W21-AC04). */
const ROW = 41;
/** The table's columns, for the rows that stand in for those not rendered. */
const COLUMNS = 8;

type StatusFilter = "all" | "active" | "failed" | "succeeded" | "cancelled";
const STATUS_GROUPS: Record<StatusFilter, Status[] | null> = {
  all: null,
  active: ["running", "queued", "pending"],
  failed: ["failed", "timed_out"],
  succeeded: ["succeeded"],
  cancelled: ["cancelled", "skipped"],
};

/** The facets the list filters by (W21-R03), each a URL parameter of its own, comma-separated. */
const FACETS: { id: "repo" | "workflow" | "event" | "branch" | "author" | "runner" | "executor"; label: string; of: (r: RunRow) => string[] }[] = [
  { id: "repo", label: "Repository", of: (r) => [r.repo_name] },
  { id: "workflow", label: "Workflow", of: (r) => [r.pipeline] },
  { id: "event", label: "Event", of: (r) => [r.event] },
  { id: "branch", label: "Branch", of: (r) => [r.branch] },
  { id: "author", label: "Author", of: (r) => [r.author] },
  // A run's legs may run on several (W21-R03).
  { id: "runner", label: "Runner", of: (r) => r.runners ?? [] },
  { id: "executor", label: "Executor", of: (r) => r.executors ?? [] },
];

type Sort = "newest" | "oldest" | "longest" | "shortest";
const SORTS: { value: Sort; label: string }[] = [
  { value: "newest", label: "Newest first" },
  { value: "oldest", label: "Oldest first" },
  { value: "longest", label: "Longest first" },
  { value: "shortest", label: "Shortest first" },
];

export default function Runs() {
  return (
    <div className="container-console page">
      <title>Runs · kiln</title>
      <PageIntro
        index={2}
        label="Runs"
        title="Runs"
        lead="Every pipeline run across your repositories, newest first. Filters stay in the URL so you can share a view."
        actions={
          <>
            <Button asChild variant="secondary" size="sm">
              <Link to="/logs">
                <Search /> Search logs
              </Link>
            </Button>
            <Button asChild variant="secondary" size="sm">
              <Link to="/insights">
                <ChartLine /> Insights
              </Link>
            </Button>
          </>
        }
      />
      <Loadable label="runs" fallback={<RunsHold />}>
        <RunsBody />
      </Loadable>
    </div>
  );
}

/** Older pages of one list (`key`: the repository filter they belong to). */
interface Older {
  key: string;
  runs: RunRow[];
  hasMore: boolean | null;
  loading: boolean;
  error: string | null;
}

function RunsBody() {
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const status = (params.get("status") as StatusFilter | null) ?? "all";
  const sort = (SORTS.find((x) => x.value === params.get("sort"))?.value ?? "newest") as Sort;
  // What's chosen, from the URL: each facet's values (a repository by name or id).
  const chosen: Chosen = Object.fromEntries(FACETS.map((f): [string, string[]] => [f.id, (params.get(f.id) ?? "").split(",").filter(Boolean)]).filter(([, v]) => v.length > 0));
  // One repository is the server's to narrow by; several are narrowed here.
  const repo = chosen.repo?.length === 1 ? (chosen.repo[0] ?? "") : "";
  // What's typed matches runs' words at once.
  const [query, setQuery] = React.useState("");

  // The server orders the list (W21-R03): newest first pages by time, the other orders by offset.
  const key = `${repo || "all"}/${sort}`;
  const ordered = sort === "newest" ? undefined : sort;
  const firstRes = { key: `runs/list/${key}`, fetch: (s: AbortSignal) => api.runs({ repo: repo || undefined, limit: PAGE, sort: ordered }, s), opts: { pollMs: 60_000 } };
  warm(reposRes, firstRes);
  const repos = useRepos().data;
  const first = useSuspenseResource(firstRes.key, firstRes.fetch, firstRes.opts);
  useServerEvents((ev) => {
    if (ev.type !== "run") return;
    const r = ev.run;
    if (repo && r.repo_id !== repo && r.repo_name !== repo) return;
    first.setData((cur) => {
      const i = cur.findIndex((x) => x.id === r.id);
      if (i !== -1) {
        const copy = cur.slice();
        // What the list knows that an event doesn't say (its runners and executors), kept.
        copy[i] = { ...cur[i], ...r };
        return copy;
      }
      // A new run leads only the newest-first list; the others find it when they load again.
      const newest = cur[0];
      return !ordered && (!newest || r.created_at >= newest.created_at) ? [r, ...cur] : cur;
    });
  });

  const [olderState, setOlder] = React.useState<Older>({ key, runs: [], hasMore: null, loading: false, error: null });
  // Another repository's list starts without the older pages of the last one.
  const older = React.useMemo<Older>(() => (olderState.key === key ? olderState : { key, runs: [], hasMore: null, loading: false, error: null }), [olderState, key]);
  const runs = React.useMemo(() => {
    const seen = new Set(first.data.map((r) => r.id));
    return [...first.data, ...older.runs.filter((r) => !seen.has(r.id))];
  }, [first.data, older.runs]);
  const hasMore = older.hasMore ?? first.data.length >= PAGE;

  const loadMore = React.useCallback(() => {
    const last = runs[runs.length - 1];
    if (!last || older.loading || !hasMore) return;
    setOlder({ ...older, loading: true, error: null });
    // An answer for a list since replaced (another repository) is dropped.
    api.runs({ repo: repo || undefined, limit: PAGE, ...(ordered ? { sort: ordered, offset: runs.length } : { before: last.created_at }) }).then(
      (more) => setOlder((o) => (o.key !== key ? o : { key, runs: [...o.runs, ...more], hasMore: more.length >= PAGE, loading: false, error: null })),
      (e: unknown) => setOlder((o) => (o.key !== key ? o : { ...o, loading: false, error: errorMessage(e) })),
    );
  }, [runs, older, hasMore, repo, key, ordered]);

  // Infinite scroll: loads when the end nears the viewport, and keeps going while it stays near.
  const sentinel = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el || !hasMore || older.loading || older.error) return;
      const io = new IntersectionObserver((entries) => entries.some((e) => e.isIntersecting) && loadMore(), { rootMargin: "600px 0px" });
      io.observe(el);
      return () => io.disconnect();
    },
    [hasMore, older.loading, older.error, loadMore],
  );

  const setParam = (k: string, v: string) => {
    const next = new URLSearchParams(params);
    if (v && v !== "all" && !(k === "sort" && v === "newest")) next.set(k, v);
    else next.delete(k);
    setParams(next, { replace: true });
  };
  const setChosen = (c: Chosen) => {
    const next = new URLSearchParams(params);
    for (const f of FACETS) {
      const v = c[f.id] ?? [];
      if (v.length) next.set(f.id, v.join(","));
      else next.delete(f.id);
    }
    setParams(next, { replace: true });
  };

  const group = STATUS_GROUPS[status] ?? null;
  const words = query.trim().toLowerCase();
  // A repository may be chosen by its id (an older link): its name is what rows carry.
  const named = (f: (typeof FACETS)[number], v: string) => (f.id === "repo" ? (repos.find((r) => r.id === v)?.name ?? v) : v);
  const passes = React.useCallback(
    (r: RunRow, except?: string) =>
      FACETS.every((f) => f.id === except || !chosen[f.id]?.length || (chosen[f.id] ?? []).some((v) => f.of(r).includes(named(f, v)))) &&
      (!words || [r.message, r.branch, r.pipeline, r.sha, r.author, r.repo_name].some((x) => x.toLowerCase().includes(words))),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [params, words, repos],
  );
  const visible = React.useMemo(() => runs.filter((r) => (!group || group.includes(r.status)) && passes(r)), [runs, group, passes]);
  const counts = React.useMemo(() => {
    const c: Record<StatusFilter, number> = { all: 0, active: 0, failed: 0, succeeded: 0, cancelled: 0 };
    for (const r of runs) {
      if (!passes(r)) continue;
      c.all++;
      for (const k of ["active", "failed", "succeeded", "cancelled"] as const) if (STATUS_GROUPS[k]!.includes(r.status)) c[k]++;
    }
    return c;
  }, [runs, passes]);
  // Each facet's values among the runs loaded, each with how many it would show (the other facets as they are).
  const facets: Facet[] = React.useMemo(
    () =>
      FACETS.map((f) => {
        const n = new Map<string, number>();
        for (const r of runs) if ((!group || group.includes(r.status)) && passes(r, f.id)) for (const v of f.of(r)) n.set(v, (n.get(v) ?? 0) + 1);
        if (f.id === "repo") for (const r of repos) if (!n.has(r.name)) n.set(r.name, 0);
        return {
          id: f.id,
          label: f.label,
          options: [...n.entries()]
            .filter(([v]) => v)
            .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
            .map(([v, count]) => ({ id: v, label: v, count })),
        };
      }),
    [runs, repos, group, passes],
  );

  const filtered = Boolean(Object.keys(chosen).length || status !== "all" || words);
  // Only the rows near the viewport are rendered (W21-AC04), at the height a row has here.
  const [rowHeight, setRowHeight] = React.useState(ROW);
  const [listRef, win] = useWindowedRows(visible.length, rowHeight);
  // Once: rows a pixel apart where they fall (WebKit rounds by position) would measure again and again.
  const measured = React.useRef(false);
  const measureRow = React.useCallback((el: HTMLTableRowElement | null) => {
    const h = el?.getBoundingClientRect().height ?? 0;
    if (measured.current || h <= 0) return;
    measured.current = true;
    setRowHeight((cur) => (Math.abs(h - cur) > 0.5 ? h : cur));
  }, []);

  return (
    <>
      <RunsToolbar
        status={status}
        onStatus={(v) => setParam("status", v)}
        counts={counts}
        facets={facets}
        chosen={chosen}
        onChosen={setChosen}
        query={query}
        onQuery={setQuery}
        sort={sort}
        onSort={(v) => setParam("sort", v)}
      />

      {visible.length === 0 && !hasMore ? (
        filtered ? (
          <EmptyState
            icon={<Search size={24} />}
            title="No runs match."
            action={
              <Button
                variant="secondary"
                onClick={() => {
                  setQuery("");
                  setParams(new URLSearchParams(), { replace: true });
                }}
              >
                Clear filters
              </Button>
            }
          >
            Try another repository, status, branch or word.
          </EmptyState>
        ) : (
          <EmptyState
            icon={<Activity size={24} />}
            title="No runs yet."
            action={
              <Link className="text-link" to="/repos">
                Add a repository
              </Link>
            }
          >
            Runs appear here as soon as kiln sees a push, a pull request, or a manual trigger.
          </EmptyState>
        )
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <RunsHead />
            <tbody ref={listRef}>
              {win.start > 0 && (
                <tr aria-hidden="true" style={{ height: win.start * rowHeight, background: "transparent" }}>
                  <td colSpan={COLUMNS} className="!border-0 !p-0" />
                </tr>
              )}
              {visible.slice(win.start, win.end).map((r, i) => (
                <tr
                  key={r.id}
                  ref={i === 0 ? measureRow : undefined}
                  data-href
                  tabIndex={0}
                  onClick={(e) => {
                    if ((e.target as HTMLElement).closest("a")) return;
                    navigate(`/runs/${r.id}`);
                  }}
                  onKeyDown={(e) => e.key === "Enter" && navigate(`/runs/${r.id}`)}
                  aria-label={`${r.repo_name} run ${r.number}, ${statusLabel(r.status)}`}
                >
                  <td>
                    <StatusDot status={r.status} />
                  </td>
                  <td className="whitespace-nowrap">
                    <Link to={`/runs/${r.id}`} className="hover:text-white">
                      <span className="text-[#8c8f98] max-sm:hidden">{r.repo_name.split("/")[0]}/</span>
                      <span className="text-[#e4e5e7]">{r.repo_name.split("/").slice(1).join("/")}</span>
                      <span className="ml-1.5 font-mono text-[11px] text-[#a1a4ad]">#{r.number}</span>
                    </Link>
                  </td>
                  <td className="cell-mono hidden lg:table-cell">{r.pipeline}</td>
                  <td className="hidden max-w-[200px] md:table-cell">
                    <span className="inline-flex max-w-full items-center gap-1.5 truncate font-mono text-[11px] text-[#b5b8c0]">
                      <GitBranch size={11} className="shrink-0 text-[#7b7e88]" />
                      <span className="truncate">{r.branch}</span>
                    </span>
                  </td>
                  <td className="truncate-cell">
                    <span className="font-mono text-[11px] text-[#a6b2c6]">{shortSha(r.sha)}</span>
                    <span className="ml-2 text-[#b9bbc2]" title={r.message}>
                      {firstLine(r.message)}
                    </span>
                    <span className="ml-2 hidden text-[11px] text-[#7b7e88] 2xl:inline">{r.author}</span>
                  </td>
                  <td className="hidden whitespace-nowrap xl:table-cell">
                    <span className="inline-flex items-center gap-1.5 font-mono text-[10px] text-[#8c8f98]">
                      <EventIcon event={r.event} />
                      {r.event.replace("_", " ")}
                    </span>
                  </td>
                  <td className="num cell-mono whitespace-nowrap">
                    <LiveDuration start={r.started_at} end={r.finished_at} />
                  </td>
                  <td className="num cell-dim hidden font-mono text-[11px] whitespace-nowrap sm:table-cell">
                    <RelTime ts={r.created_at} />
                  </td>
                </tr>
              ))}
              {win.end < visible.length && (
                <tr aria-hidden="true" style={{ height: (visible.length - win.end) * rowHeight, background: "transparent" }}>
                  <td colSpan={COLUMNS} className="!border-0 !p-0" />
                </tr>
              )}
            </tbody>
          </table>
        </div>
      )}
      <div ref={sentinel} className="h-px" aria-hidden="true" />
      {older.error ? (
        <div className="mt-4">
          <ErrorState error={older.error} onRetry={loadMore} compact />
        </div>
      ) : hasMore ? (
        <div className="flex min-h-[84px] items-center justify-center">
          {older.loading ? (
            <span className="label">Loading more…</span>
          ) : (
            <Button variant="ghost" size="sm" onClick={loadMore}>
              Load older runs
            </Button>
          )}
        </div>
      ) : (
        visible.length > 0 && (
          <div className="py-8 text-center">
            <span className="tiny-cross">+</span>
            <div className="label mt-2">End of history · {runs.length} runs</div>
          </div>
        )
      )}
    </>
  );
}

function RunsToolbar({
  status,
  onStatus,
  counts,
  facets,
  chosen,
  onChosen,
  query,
  onQuery,
  sort,
  onSort,
}: {
  status: StatusFilter;
  onStatus: (v: StatusFilter) => void;
  counts: Record<StatusFilter, number> | null;
  facets: Facet[];
  chosen: Chosen;
  onChosen: (c: Chosen) => void;
  query: string;
  onQuery: (q: string) => void;
  sort: Sort;
  onSort: (v: Sort) => void;
}) {
  return (
    <div className="mb-5 flex flex-wrap items-end gap-3">
      <Segmented
        label="Filter by status"
        value={status}
        onChange={onStatus}
        options={[
          { value: "all", label: "All", count: counts?.all },
          { value: "active", label: "Active", count: counts?.active },
          { value: "failed", label: "Failed", count: counts?.failed },
          { value: "succeeded", label: "Passed", count: counts?.succeeded },
          { value: "cancelled", label: "Cancelled", count: counts?.cancelled },
        ]}
      />
      <div className="ml-auto flex w-full flex-wrap gap-3 sm:w-auto">
        <FacetBox
          className="min-w-0 flex-1 sm:w-80 sm:flex-none"
          label="Filter runs"
          placeholder="Repository, workflow, branch, event, author, or words…"
          query={query}
          onQuery={onQuery}
          facets={facets}
          chosen={chosen}
          onChosen={onChosen}
        />
        <SelectField label="Sort" hideLabel className="min-w-0 sm:w-40" value={sort} onChange={(v) => onSort(v as Sort)} options={SORTS} />
      </div>
    </div>
  );
}

function RunsHead() {
  return (
    <thead>
      <tr>
        <th className="w-6" aria-label="Status" />
        <th>Run</th>
        <th className="hidden lg:table-cell">Pipeline</th>
        <th className="hidden md:table-cell">Branch</th>
        <th>Commit</th>
        <th className="hidden xl:table-cell">Event</th>
        <th className="num">Duration</th>
        <th className="num hidden sm:table-cell">Created</th>
      </tr>
    </thead>
  );
}

/** The runs while they load: the same toolbar, the table's head and its rows' space. */
function RunsHold() {
  const [params] = useSearchParams();
  const noop = () => undefined;
  return (
    <>
      <RunsToolbar
        status={(params.get("status") as StatusFilter | null) ?? "all"}
        onStatus={noop}
        counts={null}
        facets={[]}
        chosen={{}}
        onChosen={noop}
        query=""
        onQuery={noop}
        sort="newest"
        onSort={noop}
      />
      <TableHold head={<RunsHead />} rows={12} rowHeight={41} label="Loading runs" />
    </>
  );
}
