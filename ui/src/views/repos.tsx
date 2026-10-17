/**
 * Repositories: the code kiln works with. The ones it builds (polled with conditional requests, so
 * idle ones cost nothing against GitHub's rate limit; pipelines live in .kiln/*.yaml) and the
 * folders it keeps the same on several machines, on one page, with a repository that is both
 * linked to its other half. A search and a Show select narrow the page; adding, running and
 * deleting happen in place, under what they act on.
 */
import * as React from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { AlertTriangle, ArrowUpRight, FolderSync, GitBranch, Play, Plus, RefreshCw, Trash2 } from "lucide-react";
import { api } from "@/api/client";
import type { Repo, Run, SyncRepo, SyncView } from "@/api/types";
import { Button } from "@/components/ui/button";
import { RowsHold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, EmptyState, ErrorState, PageIntro, SearchField, SectionHeading, SelectField, TextField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Tip } from "@/components/ui/tooltip";
import { agentsRes, recentRunsRes, reposRes, useRecentRuns, useRepos, useRes, warm, type Res } from "@/lib/data";
import { formatDuration } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { RelTime } from "./shared";
import { SyncedRepos } from "./sync";

type Show = "all" | "built" | "synced";
type Notice = { tone: "ok" | "fail"; text: string } | null;

const SHOWS = [
  { value: "all", label: "All repositories" },
  { value: "built", label: "Built by kiln" },
  { value: "synced", label: "Synced to machines" },
];

/** Folders kept the same on several machines; read often, they move by the second. */
export const syncRes: Res<SyncView> = { key: "sync", fetch: (s) => api.sync(s), opts: { pollMs: 3_000 } };

/** The synced folder that is a built repository's other half: same name, or same last part. */
function syncedFor(r: Repo, synced: Map<string, SyncRepo>) {
  const name = r.name.toLowerCase();
  return synced.get(name) ?? synced.get(name.split("/").pop() ?? name);
}

/** The page's toggles, in the URL: what shows, and which form is open (`?add=1`, `?sync=1`). */
function useRepoParams() {
  const [params, setParams] = useSearchParams();
  const show = (["all", "built", "synced"].includes(params.get("show") ?? "") ? params.get("show") : "all") as Show;
  const set = (k: string, v: string | null) => {
    const n = new URLSearchParams(params);
    if (v) n.set(k, v);
    else n.delete(k);
    setParams(n, { replace: true });
  };
  return { show, adding: params.get("add") === "1", syncing: params.get("sync") === "1", set };
}

export default function Repos() {
  const { adding, syncing, set } = useRepoParams();
  return (
    <div className="container-console page">
      <title>Repositories · kiln</title>
      <PageIntro
        index={4}
        label="Repositories"
        title="Repositories"
        lead="The code kiln works with: repositories it builds, polled with conditional requests so idle ones cost nothing against GitHub's rate limit (pipelines live in .kiln/*.yaml), and folders it keeps the same on several machines."
        actions={
          <>
            <Button size="sm" onClick={() => set("add", "1")} disabled={adding}>
              <Plus /> Add repository
            </Button>
            <Button size="sm" variant="secondary" onClick={() => set("sync", "1")} disabled={syncing}>
              <FolderSync /> Sync a folder
            </Button>
          </>
        }
      />
      <Loadable label="the repositories" fallback={<ReposHold />}>
        <ReposBody />
      </Loadable>
    </div>
  );
}

function ReposBody() {
  const { show, adding, syncing, set } = useRepoParams();
  warm(reposRes, recentRunsRes(200), syncRes, agentsRes);
  const repos = useRepos();
  const lastRuns = useRecentRuns(200).data;
  const sync = useRes(syncRes);
  const agents = useRes(agentsRes).data;
  const machines = React.useMemo(() => agents.map((a) => a.name).sort(), [agents]);
  const [q, setQ] = React.useState("");
  const [open, setOpen] = React.useState<{ id: string; what: "run" | "delete" } | null>(null);
  const [polling, setPolling] = React.useState<Set<string>>(new Set());
  const [notice, setNoticeState] = React.useState<Notice>(null);
  // A success says so for a while; a failure stays until the next action.
  const setNotice = (n: Notice) => {
    setNoticeState(n);
    if (n?.tone === "ok") window.setTimeout(() => setNoticeState((cur) => (cur === n ? null : cur)), 8_000);
  };

  const latestByRepo = React.useMemo(() => {
    const m = new Map<string, Run>();
    for (const r of lastRuns) {
      const cur = m.get(r.repo_id);
      if (!cur || r.created_at > cur.created_at) m.set(r.repo_id, r);
    }
    return m;
  }, [lastRuns]);
  const syncedByName = React.useMemo(() => new Map(sync.data.repos.map((s) => [s.name.toLowerCase(), s])), [sync.data]);
  const builtByName = React.useMemo(() => {
    const m = new Map<string, Repo>();
    for (const r of repos.data) {
      const s = syncedFor(r, syncedByName);
      if (s) m.set(s.name.toLowerCase(), r);
    }
    return m;
  }, [repos.data, syncedByName]);

  const poll = async (r: Repo) => {
    setPolling((p) => new Set(p).add(r.id));
    setNotice(null);
    try {
      await api.pollRepo(r.id);
      setNotice({ tone: "ok", text: `Polling ${r.name}.` });
      window.setTimeout(() => invalidate("repos"), 1500);
    } catch (e) {
      setNotice({ tone: "fail", text: `Polling ${r.name} failed: ${errorMessage(e)}` });
    } finally {
      setPolling((p) => {
        const n = new Set(p);
        n.delete(r.id);
        return n;
      });
    }
  };

  const toSynced = (s: SyncRepo) => {
    if (show === "built") set("show", null);
    requestAnimationFrame(() => document.getElementById(`sync-${s.id}`)?.scrollIntoView({ block: "start" }));
  };

  const needle = q.trim().toLowerCase();
  const list = repos.data.filter((r) => r.name.toLowerCase().includes(needle));

  return (
    <>
      <ReposToolbar q={q} onQ={setQ} show={show} onShow={(v) => set("show", v === "all" ? null : v)} counts={`${list.length} built · ${sync.data.repos.length} synced`} />
      {notice && (
        <p role="status" className="notice" data-tone={notice.tone}>
          {notice.text}
        </p>
      )}
      {adding && (
        <AddRepoForm
          onAdded={(r) => {
            repos.setData((l) => [...l, r]);
            set("add", null);
            setNotice({ tone: "ok", text: `${r.name} added; its first poll is on its way.` });
          }}
          onCancel={() => set("add", null)}
        />
      )}

      {show !== "synced" && (
        <section className="mb-12" aria-labelledby="repos-built">
          <SectionHeading label="Built by kiln" title={<span id="repos-built">Repositories kiln builds</span>} />
          {repos.data.length === 0 ? (
            <EmptyState
              icon={<GitBranch size={24} />}
              title="No repositories yet."
              action={
                <Button size="sm" variant="secondary" onClick={() => set("add", "1")}>
                  <Plus /> Add your first repository
                </Button>
              }
            >
              Register an owner/name and the branches to watch. kiln starts polling immediately.
            </EmptyState>
          ) : list.length === 0 ? (
            <p className="py-4 text-[13px] text-[#8c8f98]">No repository kiln builds matches.</p>
          ) : (
            <div className="table-wrap">
              <table className="data-table">
                <ReposHead />
                <tbody>
                  {list.map((r) => {
                    const latest = latestByRepo.get(r.id);
                    const synced = syncedFor(r, syncedByName);
                    const isOpen = open?.id === r.id ? open.what : null;
                    return (
                      <React.Fragment key={r.id}>
                        <tr>
                          <td className="max-w-[200px] sm:max-w-[360px]">
                            <div className="flex items-center gap-2.5">
                              <StatusDot status={!r.enabled ? "cancelled" : r.last_error ? "timed_out" : "succeeded"} title={!r.enabled ? "disabled" : r.last_error ? "poll error" : "healthy"} />
                              <Link to={`/runs?repo=${encodeURIComponent(r.id)}`} className="min-w-0 truncate hover:text-white">
                                <span className="text-[#8c8f98]">{r.name.split("/")[0]}/</span>
                                <span className="text-[#e4e5e7]">{r.name.split("/").slice(1).join("/")}</span>
                              </Link>
                              {synced && (
                                <button type="button" className="chip shrink-0 hover:text-white" onClick={() => toSynced(synced)} title="Its synced copies, below">
                                  synced · {synced.copies.length} {synced.copies.length === 1 ? "machine" : "machines"}
                                </button>
                              )}
                            </div>
                            {r.last_error && (
                              <div className="mt-1 flex items-start gap-1.5 pl-[18px] font-mono text-[10px] leading-snug text-[#cbb69a]">
                                <AlertTriangle size={11} className="mt-px" /> <span className="line-clamp-2 break-all">{r.last_error}</span>
                              </div>
                            )}
                          </td>
                          <td className="hidden md:table-cell">
                            <span className="inline-flex flex-wrap gap-1">
                              {r.branches.length ? (
                                r.branches.slice(0, 4).map((b) => (
                                  <span key={b} className="chip">
                                    {b}
                                  </span>
                                ))
                              ) : (
                                <span className="cell-dim">default</span>
                              )}
                              {r.branches.length > 4 && <span className="chip">+{r.branches.length - 4}</span>}
                            </span>
                          </td>
                          <td className="hidden lg:table-cell">
                            {latest ? (
                              <Link to={`/runs/${latest.id}`} className="inline-flex items-center gap-2 font-mono text-[11px] text-[#b5b8c0] hover:text-white">
                                <StatusDot status={latest.status} /> #{latest.number} · {latest.branch}
                                <span className="text-[#7b7e88]">
                                  <RelTime ts={latest.created_at} />
                                </span>
                              </Link>
                            ) : (
                              <span className="cell-dim">—</span>
                            )}
                          </td>
                          <td className="num cell-mono hidden sm:table-cell">{formatDuration(r.poll_seconds)}</td>
                          <td className="num cell-mono hidden whitespace-nowrap sm:table-cell">
                            <RelTime ts={r.last_polled_at} />
                          </td>
                          <td className="text-right whitespace-nowrap">
                            <div className="inline-flex items-center gap-0.5">
                              <Tip content="Poll now">
                                <button className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Poll ${r.name} now`} disabled={polling.has(r.id)} onClick={() => poll(r)}>
                                  <RefreshCw size={14} className={polling.has(r.id) ? "animate-spin motion-reduce:animate-none" : undefined} />
                                </button>
                              </Tip>
                              <Tip content="Start a run">
                                <button
                                  className="icon-btn"
                                  data-variant="bare"
                                  data-size="sm"
                                  aria-label={`Start a run of ${r.name}`}
                                  aria-expanded={isOpen === "run"}
                                  onClick={() => setOpen(isOpen === "run" ? null : { id: r.id, what: "run" })}
                                >
                                  <Play size={14} />
                                </button>
                              </Tip>
                              <Tip content="Open on GitHub">
                                <a className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Open ${r.name} on GitHub`} href={r.clone_url.replace(/\.git$/, "")} target="_blank" rel="noopener noreferrer">
                                  <ArrowUpRight size={14} />
                                </a>
                              </Tip>
                              <Tip content="Delete">
                                <button
                                  className="icon-btn"
                                  data-variant="bare"
                                  data-size="sm"
                                  aria-label={`Delete ${r.name}`}
                                  aria-expanded={isOpen === "delete"}
                                  onClick={() => setOpen(isOpen === "delete" ? null : { id: r.id, what: "delete" })}
                                >
                                  <Trash2 size={14} />
                                </button>
                              </Tip>
                            </div>
                          </td>
                        </tr>
                        {isOpen && (
                          <tr>
                            <td colSpan={6} className="bg-[#0b0c0e]">
                              {isOpen === "run" ? (
                                <RunForm repo={r} onCancel={() => setOpen(null)} onNotice={setNotice} />
                              ) : (
                                <DeleteRepo
                                  repo={r}
                                  onCancel={() => setOpen(null)}
                                  onDeleted={() => {
                                    repos.setData((l) => l.filter((x) => x.id !== r.id));
                                    setOpen(null);
                                    setNotice({ tone: "ok", text: `${r.name} deleted.` });
                                  }}
                                />
                              )}
                            </td>
                          </tr>
                        )}
                      </React.Fragment>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </section>
      )}

      {show !== "built" && (
        <section aria-labelledby="repos-synced">
          <SyncedHeading />
          <SyncedRepos
            view={sync}
            machines={machines}
            adding={syncing}
            onAdding={(on) => set("sync", on ? "1" : null)}
            q={q}
            builtBy={(name) => {
              const r = builtByName.get(name.toLowerCase());
              return r ? { name: r.name, href: `/runs?repo=${encodeURIComponent(r.id)}` } : undefined;
            }}
          />
        </section>
      )}
    </>
  );
}

function ReposToolbar({ q, onQ, show, onShow, counts }: { q: string; onQ: (v: string) => void; show: Show; onShow: (v: Show) => void; counts: string }) {
  return (
    <div className="mb-5 flex flex-wrap items-end gap-3">
      <SearchField className="w-full sm:w-72" label="Find a repository" placeholder="Find a repository…" value={q} onChange={onQ} />
      <SelectField label="Show" hideLabel value={show} onChange={(v) => onShow(v as Show)} options={SHOWS} className="w-full sm:w-56" />
      <span className="label tabular-nums sm:ml-auto">{counts}</span>
    </div>
  );
}

function ReposHead() {
  return (
    <thead>
      <tr>
        <th>Repository</th>
        <th className="hidden md:table-cell">Branches</th>
        <th className="hidden lg:table-cell">Latest run</th>
        <th className="num hidden sm:table-cell">Interval</th>
        <th className="num hidden sm:table-cell">Last poll</th>
        <th className="w-0 text-right">
          <span className="sr-only">Actions</span>
        </th>
      </tr>
    </thead>
  );
}

function SyncedHeading() {
  return (
    <>
      <SectionHeading label="Synced to machines" title={<span id="repos-synced">Folders kept the same on several machines</span>} />
      <p className="mb-4 max-w-[68ch] text-[12.5px] leading-relaxed text-[#9a9ca5]">
        A change on one machine reaches the others within seconds; when two machines change one file, both versions are kept.
      </p>
    </>
  );
}

/** The page while it loads: its toolbar, the table's head and rows' space, the synced folders' frame. */
function ReposHold() {
  const { show } = useRepoParams();
  const noop = () => undefined;
  return (
    <>
      <ReposToolbar q="" onQ={noop} show={show} onShow={noop} counts={"\u00a0"} />
      {show !== "synced" && (
        <section className="mb-12">
          <SectionHeading label="Built by kiln" title="Repositories kiln builds" />
          <TableHold head={<ReposHead />} rows={6} rowHeight={45} label="Loading the repositories" />
        </section>
      )}
      {show !== "built" && (
        <section>
          <SyncedHeading />
          <RowsHold rows={3} rowHeight={72} />
        </section>
      )}
    </>
  );
}

/** Adding a repository kiln builds: its name on GitHub, the branches to watch, how often to poll. */
function AddRepoForm({ onAdded, onCancel }: { onAdded: (r: Repo) => void; onCancel: () => void }) {
  const [name, setName] = React.useState("");
  const [cloneUrl, setCloneUrl] = React.useState("");
  const [branches, setBranches] = React.useState("main");
  const [poll, setPoll] = React.useState("60");
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const valid = /^[\w.-]+\/[\w.-]+$/.test(name.trim());

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!valid) return;
    setBusy(true);
    setErr(null);
    try {
      const repo = await api.addRepo({
        name: name.trim(),
        clone_url: cloneUrl.trim() || undefined,
        branches: branches
          .split(",")
          .map((b) => b.trim())
          .filter(Boolean),
        poll_seconds: Number(poll) > 0 ? Number(poll) : undefined,
      });
      onAdded(repo);
    } catch (e2) {
      setErr(errorMessage(e2));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} aria-label="Add a repository" className="mb-8 max-w-[640px] space-y-4 rounded-md border border-white/10 bg-[#0b0c0e] p-4">
      <p className="text-[12.5px] leading-relaxed text-[#aab3c1]">kiln polls GitHub for new commits on the listed branches, and for pull requests.</p>
      <TextField label="Repository" placeholder="hyper-light/hyper-raft" value={name} onChange={(e) => setName(e.target.value)} autoFocus hint="owner/name on GitHub" />
      <TextField label="Clone URL (optional)" placeholder={name ? `https://github.com/${name.trim()}.git` : "https://github.com/owner/name.git"} value={cloneUrl} onChange={(e) => setCloneUrl(e.target.value)} />
      <div className="grid grid-cols-[1fr_120px] gap-3">
        <TextField label="Branches" placeholder="main, release/*" value={branches} onChange={(e) => setBranches(e.target.value)} hint="Comma-separated; globs allowed" />
        <TextField label="Poll (s)" type="number" min={10} value={poll} onChange={(e) => setPoll(e.target.value)} />
      </div>
      {err && <ErrorState error={err} compact />}
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={!valid || busy}>
          {busy ? "Adding…" : "Add repository"}
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

/** Starting a run by hand, under the repository's row: a branch, and a pipeline if only one. */
function RunForm({ repo, onCancel, onNotice }: { repo: Repo; onCancel: () => void; onNotice: (n: Notice) => void }) {
  const navigate = useNavigate();
  const concrete = repo.branches.filter((b) => !b.includes("*"));
  const [branch, setBranch] = React.useState(concrete[0] ?? "");
  const [custom, setCustom] = React.useState("");
  const [pipeline, setPipeline] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      const b = (branch === "__custom" ? custom : branch).trim();
      const runs = await api.triggerRepo(repo.id, { branch: b || undefined, pipeline: pipeline.trim() || undefined });
      const first = runs[0];
      if (!first) {
        onNotice({ tone: "ok", text: `Nothing to run: no pipeline of ${repo.name} matched that branch.` });
        onCancel();
      } else {
        navigate(`/runs/${first.id}`);
      }
    } catch (e2) {
      setErr(errorMessage(e2));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} aria-label={`Start a run of ${repo.name}`} className="max-w-[640px] space-y-3 py-2">
      <p className="text-[12.5px] text-[#aab3c1]">Runs the branch head. Leave the pipeline empty to run every pipeline that matches.</p>
      <div className="grid gap-3 sm:grid-cols-2">
        <SelectField
          label="Branch"
          value={branch}
          onChange={setBranch}
          options={[...(concrete.length ? [] : [{ value: "", label: "Default branch" }]), ...concrete.map((b) => ({ value: b, label: b })), { value: "__custom", label: "Other branch…" }]}
        />
        <TextField label="Pipeline (optional)" placeholder="ci" value={pipeline} onChange={(e) => setPipeline(e.target.value)} hint="Name from .kiln/<name>.yaml" />
      </div>
      {branch === "__custom" && <TextField label="Branch name" value={custom} onChange={(e) => setCustom(e.target.value)} placeholder="feature/xyz" autoFocus />}
      {err && <ErrorState error={err} compact />}
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={busy || (branch === "__custom" && !custom.trim())}>
          <Play /> {busy ? "Starting…" : "Start run"}
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

/** Deleting a repository, confirmed under its row. */
function DeleteRepo({ repo, onCancel, onDeleted }: { repo: Repo; onCancel: () => void; onDeleted: () => void }) {
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  return (
    <div className="py-2">
      <Confirm
        label={`Delete ${repo.name}`}
        tone="danger"
        actions={
          <>
            <Button
              size="sm"
              variant="danger"
              disabled={busy}
              onClick={async () => {
                setBusy(true);
                setErr(null);
                try {
                  await api.deleteRepo(repo.id);
                  onDeleted();
                } catch (e) {
                  setErr(errorMessage(e));
                } finally {
                  setBusy(false);
                }
              }}
            >
              Delete {repo.name}
            </Button>
            <Button size="sm" variant="ghost" onClick={onCancel}>
              Cancel
            </Button>
          </>
        }
      >
        <p>kiln stops polling {repo.name}. Its runs and logs are removed with it; its synced copies, if any, keep syncing.</p>
        {err && <ErrorState error={err} compact />}
      </Confirm>
    </div>
  );
}
