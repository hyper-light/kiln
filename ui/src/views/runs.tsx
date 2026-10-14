import * as React from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { GitBranch, Search, Activity } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Run, Status } from "@/api/types";
import { Button } from "@/components/ui/button";
import { EmptyState, ErrorState, PageIntro, SearchField, SelectField, SkeletonRows } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { firstLine, shortSha, statusLabel } from "@/lib/format";
import { errorMessage, useDocumentTitle, useResource } from "@/lib/hooks";
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
  useDocumentTitle("Runs");
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const repo = params.get("repo") ?? "";
  const status = (params.get("status") as StatusFilter | null) ?? "all";
  const branch = params.get("branch") ?? "";

  const repos = useResource((s) => api.repos(s), []);
  const [runs, setRuns] = React.useState<Run[] | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const [loadingMore, setLoadingMore] = React.useState(false);
  const [hasMore, setHasMore] = React.useState(true);
  const sentinel = React.useRef<HTMLDivElement>(null);
  const reqId = React.useRef(0);

  const setParam = (k: string, v: string) => {
    const next = new URLSearchParams(params);
    if (v && v !== "all") next.set(k, v);
    else next.delete(k);
    setParams(next, { replace: true });
  };

  const loadFirst = React.useCallback(() => {
    const id = ++reqId.current;
    setRuns(null);
    setError(null);
    setHasMore(true);
    api
      .runs({ repo: repo || undefined, limit: PAGE })
      .then((r) => {
        if (id !== reqId.current) return;
        setRuns(r);
        setHasMore(r.length >= PAGE);
      })
      .catch((e) => id === reqId.current && setError(errorMessage(e)));
  }, [repo]);

  React.useEffect(loadFirst, [loadFirst]);

  const loadMore = React.useCallback(() => {
    if (!runs || loadingMore || !hasMore) return;
    const last = runs[runs.length - 1];
    if (!last) return;
    const id = reqId.current;
    setLoadingMore(true);
    api
      .runs({ repo: repo || undefined, limit: PAGE, before: last.created_at })
      .then((more) => {
        if (id !== reqId.current) return;
        setRuns((cur) => {
          const seen = new Set((cur ?? []).map((r) => r.id));
          return [...(cur ?? []), ...more.filter((r) => !seen.has(r.id))];
        });
        setHasMore(more.length >= PAGE);
      })
      .catch((e) => setError(errorMessage(e)))
      .finally(() => setLoadingMore(false));
  }, [runs, loadingMore, hasMore, repo]);

  // Infinite scroll: load when the sentinel nears the viewport; keep going while it stays visible.
  React.useEffect(() => {
    const el = sentinel.current;
    if (!el) return;
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) loadMore();
    }, { rootMargin: "600px 0px" });
    io.observe(el);
    return () => io.disconnect();
  }, [loadMore]);
  React.useEffect(() => {
    const el = sentinel.current;
    if (!el || loadingMore || !hasMore || !runs) return;
    if (el.getBoundingClientRect().top < window.innerHeight + 600) loadMore();
  }, [runs, loadingMore, hasMore, loadMore]);

  useServerEvents((ev) => {
    if (ev.type !== "run") return;
    const r = ev.run;
    if (repo && r.repo_id !== repo && r.repo_name !== repo) return;
    setRuns((cur) => {
      if (!cur) return cur;
      const i = cur.findIndex((x) => x.id === r.id);
      if (i !== -1) {
        const copy = cur.slice();
        copy[i] = r;
        return copy;
      }
      const newest = cur[0];
      if (!newest || r.created_at >= newest.created_at) return [r, ...cur];
      return cur;
    });
  });

  const group = STATUS_GROUPS[status] ?? null;
  const b = branch.trim().toLowerCase();
  const visible = React.useMemo(
    () => (runs ?? []).filter((r) => (!group || group.includes(r.status)) && (!b || r.branch.toLowerCase().includes(b))),
    [runs, group, b],
  );
  const counts = React.useMemo(() => {
    const c: Record<StatusFilter, number> = { all: 0, active: 0, failed: 0, succeeded: 0, cancelled: 0 };
    for (const r of runs ?? []) {
      if (b && !r.branch.toLowerCase().includes(b)) continue;
      c.all++;
      for (const k of ["active", "failed", "succeeded", "cancelled"] as const) if (STATUS_GROUPS[k]!.includes(r.status)) c[k]++;
    }
    return c;
  }, [runs, b]);

  const filtered = Boolean(repo || status !== "all" || branch);

  return (
    <div className="container-console page">
      <PageIntro
        index={2}
        label="Runs"
        title="Runs"
        lead="Every pipeline run across your repositories, newest first. Filters stay in the URL so you can share a view."
      />

      <div className="mb-5 flex flex-wrap items-end gap-3">
        <Segmented
          label="Filter by status"
          value={status}
          onChange={(v) => setParam("status", v)}
          options={[
            { value: "all", label: "All", count: counts.all },
            { value: "active", label: "Active", count: counts.active },
            { value: "failed", label: "Failed", count: counts.failed },
            { value: "succeeded", label: "Passed", count: counts.succeeded },
            { value: "cancelled", label: "Cancelled", count: counts.cancelled },
          ]}
        />
        <div className="ml-auto flex w-full flex-wrap gap-3 sm:w-auto">
          <SelectField
            label="Repository"
            hideLabel
            className="min-w-0 flex-1 sm:w-56 sm:flex-none"
            value={repo}
            onChange={(v) => setParam("repo", v)}
            options={[{ value: "", label: "All repositories" }, ...(repos.data ?? []).map((r) => ({ value: r.id, label: r.name }))]}
          />
          <SearchField className="min-w-0 flex-1 sm:w-52 sm:flex-none" label="Branch" placeholder="Branch…" value={branch} onChange={(v) => setParam("branch", v)} />
        </div>
      </div>

      {error && !runs ? (
        <ErrorState error={error} onRetry={loadFirst} />
      ) : !runs ? (
        <SkeletonRows rows={12} />
      ) : visible.length === 0 && !hasMore ? (
        filtered ? (
          <EmptyState
            icon={<Search size={24} />}
            title="No runs match."
            action={
              <Button variant="secondary" onClick={() => setParams(new URLSearchParams(), { replace: true })}>
                Clear filters
              </Button>
            }
          >
            Try another repository, status, or branch.
          </EmptyState>
        ) : (
          <EmptyState icon={<Activity size={24} />} title="No runs yet." action={<Link className="text-link" to="/repos">Add a repository</Link>}>
            Runs appear here as soon as kiln sees a push, a pull request, or a manual trigger.
          </EmptyState>
        )
      ) : (
        <div className="table-wrap">
          <table className="data-table">
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
                      <span className="text-[#8c8f98]">{r.repo_name.split("/")[0]}/</span>
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
                  <td className="num cell-mono">
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
      {runs && (loadingMore || hasMore) && (
        <div className="py-6 text-center">
          {loadingMore ? (
            <span className="label">Loading more…</span>
          ) : (
            <Button variant="ghost" size="sm" onClick={loadMore}>
              Load older runs
            </Button>
          )}
        </div>
      )}
      {runs && !hasMore && visible.length > 0 && (
        <div className="py-8 text-center">
          <span className="tiny-cross">+</span>
          <div className="label mt-2">End of history · {runs.length} runs</div>
        </div>
      )}
      {error && runs && (
        <div className="mt-4">
          <ErrorState error={error} onRetry={loadMore} compact />
        </div>
      )}
    </div>
  );
}
