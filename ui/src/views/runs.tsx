import * as React from "react";
import { Link, useNavigate, useSearchParams } from "@/lib/router";
import { GitBranch, Search, Activity } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Repo, Run, Status } from "@/api/types";
import { Button } from "@/components/ui/button";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, ErrorState, PageIntro, SearchField, SelectField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { reposRes, useRepos, warm } from "@/lib/data";
import { firstLine, shortSha, statusLabel } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { EventIcon, LiveDuration, RelTime } from "./shared";

const PAGE = 50;

type StatusFilter = "all" | "active" | "failed" | "succeeded" | "cancelled";
const STATUS_GROUPS: Record<StatusFilter, Status[] | null> = {
  all: null,
  active: ["running", "queued", "pending"],
  failed: ["failed", "timed_out"],
  succeeded: ["succeeded"],
  cancelled: ["cancelled", "skipped"],
};

export default function Runs() {
  return (
    <div className="container-console page">
      <title>Runs · kiln</title>
      <PageIntro index={2} label="Runs" title="Runs" lead="Every pipeline run across your repositories, newest first. Filters stay in the URL so you can share a view." />
      <Loadable label="runs" fallback={<RunsHold />}>
        <RunsBody />
      </Loadable>
    </div>
  );
}

/** Older pages of one list (`key`: the repository filter they belong to). */
interface Older {
  key: string;
  runs: Run[];
  hasMore: boolean | null;
  loading: boolean;
  error: string | null;
}

function RunsBody() {
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const repo = params.get("repo") ?? "";
  const status = (params.get("status") as StatusFilter | null) ?? "all";
  // What's typed and chosen shows at once; the URL follows it.
  const [branch, setBranch] = React.useState(() => params.get("branch") ?? "");
  const [repoChoice, setRepoChoice] = React.useState(repo);
  // A repository chosen elsewhere (a link, the palette) is chosen here too.
  const [repoSeen, setRepoSeen] = React.useState(repo);
  if (repoSeen !== repo) {
    setRepoSeen(repo);
    setRepoChoice(repo);
  }

  const key = repo || "all";
  const firstRes = { key: `runs/list/${key}`, fetch: (s: AbortSignal) => api.runs({ repo: repo || undefined, limit: PAGE }, s), opts: { pollMs: 60_000 } };
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
        copy[i] = r;
        return copy;
      }
      const newest = cur[0];
      return !newest || r.created_at >= newest.created_at ? [r, ...cur] : cur;
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
    api.runs({ repo: repo || undefined, limit: PAGE, before: last.created_at }).then(
      (more) => setOlder((o) => (o.key !== key ? o : { key, runs: [...o.runs, ...more], hasMore: more.length >= PAGE, loading: false, error: null })),
      (e: unknown) => setOlder((o) => (o.key !== key ? o : { ...o, loading: false, error: errorMessage(e) })),
    );
  }, [runs, older, hasMore, repo, key]);

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
    if (v && v !== "all") next.set(k, v);
    else next.delete(k);
    setParams(next, { replace: true });
  };

  const group = STATUS_GROUPS[status] ?? null;
  const b = branch.trim().toLowerCase();
  const visible = React.useMemo(() => runs.filter((r) => (!group || group.includes(r.status)) && (!b || r.branch.toLowerCase().includes(b))), [runs, group, b]);
  const counts = React.useMemo(() => {
    const c: Record<StatusFilter, number> = { all: 0, active: 0, failed: 0, succeeded: 0, cancelled: 0 };
    for (const r of runs) {
      if (b && !r.branch.toLowerCase().includes(b)) continue;
      c.all++;
      for (const k of ["active", "failed", "succeeded", "cancelled"] as const) if (STATUS_GROUPS[k]!.includes(r.status)) c[k]++;
    }
    return c;
  }, [runs, b]);

  const filtered = Boolean(repo || status !== "all" || branch);
  // A repository chosen whose runs are still on their way: the list shown is the last one.
  const pending = repoChoice !== repo;

  return (
    <>
      <RunsToolbar
        status={status}
        onStatus={(v) => setParam("status", v)}
        counts={counts}
        repos={repos}
        repo={repoChoice}
        onRepo={(v) => {
          setRepoChoice(v);
          setParam("repo", v);
        }}
        branch={branch}
        onBranch={(v) => {
          setBranch(v);
          setParam("branch", v);
        }}
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
                  setBranch("");
                  setRepoChoice("");
                  setParams(new URLSearchParams(), { replace: true });
                }}
              >
                Clear filters
              </Button>
            }
          >
            Try another repository, status, or branch.
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
        <div className={cn("table-wrap transition-opacity", pending && "opacity-60")} aria-busy={pending || undefined}>
          <table className="data-table">
            <RunsHead />
            <tbody>
              {visible.map((r) => (
                <tr
                  key={r.id}
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
  repos,
  repo,
  onRepo,
  branch,
  onBranch,
}: {
  status: StatusFilter;
  onStatus: (v: StatusFilter) => void;
  counts: Record<StatusFilter, number> | null;
  repos: Repo[];
  repo: string;
  onRepo: (v: string) => void;
  branch: string;
  onBranch: (v: string) => void;
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
        <SelectField
          label="Repository"
          hideLabel
          className="min-w-0 flex-1 sm:w-56 sm:flex-none"
          value={repo}
          onChange={onRepo}
          options={[{ value: "", label: "All repositories" }, ...repos.map((r) => ({ value: r.id, label: r.name }))]}
        />
        <SearchField className="min-w-0 flex-1 sm:w-52 sm:flex-none" label="Branch" placeholder="Branch…" value={branch} onChange={onBranch} />
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
      <RunsToolbar status={(params.get("status") as StatusFilter | null) ?? "all"} onStatus={noop} counts={null} repos={[]} repo="" onRepo={noop} branch={params.get("branch") ?? ""} onBranch={noop} />
      <TableHold head={<RunsHead />} rows={12} rowHeight={41} label="Loading runs" />
    </>
  );
}
