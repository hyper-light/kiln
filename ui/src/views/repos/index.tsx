/**
 * Repositories: the code kiln works with, one list. The repositories it builds (polled with
 * conditional requests, so idle ones cost nothing against GitHub's rate limit; pipelines live in
 * .kiln/*.yaml), the folders it keeps the same on several machines, and every git repository the
 * machines hold, each once, with the machines it is on. One filter field (kind, machine, OS,
 * architecture, labels, state, each with counts) and a sort, as the agents list has. A repository
 * opens under its row: where it is, the tasks that put it there, and putting it on more machines
 * (cloned, or kept in sync), moving it or giving it up, all in place. Above the list, a section that
 * opens shows sync at work: each copy's state and every change as it goes between machines.
 */
import * as React from "react";
import { Link, useSearchParams } from "@/lib/router";
import { AlertTriangle, ArrowUpRight, FolderSync, HardDriveDownload, Play, Plus, RefreshCw, ScanSearch, GitBranch, Trash2 } from "lucide-react";
import { WatchedBy } from "@/views/attach/here";
import { api } from "@/api/client";
import type { PlaceMachine, PlacesView, PlaceTask, SyncView } from "@/api/types";
import { Button } from "@/components/ui/button";
import { FacetBox } from "@/components/ui/facet-box";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { EmptyState, PageIntro, SelectField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { Tip } from "@/components/ui/tooltip";
import { recentRunsRes, reposRes, useRecentRuns, useRepos, useRes, warm, type Res } from "@/lib/data";
import { errorMessage, useLocalStorage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { RelTime } from "../shared";
import { DOT, NewRepo, RepoBlock } from "../sync";
import { AddRepoForm, DeleteRepo, type Notice, RunForm } from "./forms";
import { DEFAULT_VIEW, type Entry, type ListView, SORTS, type SortId, browseUrl, dimsOf, entriesOf, facetsOf, isActive, isFiltered, machinesOf, matches, sortEntries, tildeOf } from "./model";
import { PlaceList, TaskList, taskLine } from "./places";
import { PutForm, type PutMode } from "./put";
import { SyncActivity } from "./sync-activity";
import { ScopeSecrets } from "../settings-ci";
import { RepoKeep } from "../keep/repo";
import { Providers } from "./providers";
import "./repos.css";

/** Folders kept the same on several machines; read often, they move by the second. */
export const syncRes: Res<SyncView> = { key: "sync", fetch: (s) => api.sync(s), opts: { pollMs: 3_000 } };
/** Where every repository is, and the tasks moving them: read again whenever the server says it changed. */
export const placesRes: Res<PlacesView> = { key: "places", fetch: (s) => api.places(s), opts: { pollMs: 15_000, on: ["places"] } };

/** What is open under a repository's row. */
type Panel = { key: string; what: "details" | PutMode | "run" | "delete" };

/** Machines shown in a row before "+N". */
const CHIPS = 3;

/**
 * The page's toggles, in the URL: which form is open (`?add=1`, `?sync=1`), a kind to show
 * (`?show=built|synced`, until the filter changes) and a repository to open (`?repo=<key>`).
 */
function useRepoParams() {
  const [params, setParams] = useSearchParams();
  const set = (patch: Record<string, string | null>) => {
    const n = new URLSearchParams(params);
    for (const [k, v] of Object.entries(patch)) {
      if (v) n.set(k, v);
      else n.delete(k);
    }
    setParams(n, { replace: true });
  };
  return { adding: params.get("add") === "1", syncing: params.get("sync") === "1", show: params.get("show"), repo: params.get("repo"), set };
}

/** The Sync section, opened on one repo's activity, brought into view. */
function showActivity(set: (patch: Record<string, string | null>) => void, repo: string) {
  set({ activity: repo });
  document.getElementById("sync-activity")?.scrollIntoView({ block: "start", behavior: "smooth" });
}

export default function Repos() {
  const { adding, syncing, set } = useRepoParams();
  const look = async () => {
    try {
      const { asked } = await api.placesLook();
      toast({ title: asked ? `${asked} ${asked === 1 ? "machine looks" : "machines look"} at their repositories again` : "No connected machine can look yet", tone: "info" });
      window.setTimeout(() => invalidate("places"), 4_000);
    } catch (err) {
      toast.error("Looking again failed", errorMessage(err));
    }
  };
  return (
    <div className="container-console page">
      <title>Repositories · kiln</title>
      <PageIntro
        index={4}
        label="Repositories"
        title="Repositories"
        lead="The code kiln works with, on every machine: repositories it builds (polled with conditional requests, so idle ones cost nothing against GitHub's rate limit; pipelines live in .kiln/*.yaml), folders it keeps the same on several machines, and the repositories your machines hold. Clone or sync any of them to more machines, or move one, over kiln's own links."
        actions={
          <>
            <Button size="sm" onClick={() => set({ add: "1" })} disabled={adding}>
              <Plus /> Add repository
            </Button>
            <Button size="sm" variant="secondary" onClick={() => set({ sync: "1" })} disabled={syncing}>
              <FolderSync /> Sync a folder
            </Button>
            <Button asChild size="sm" variant="secondary">
              <Link to="/review">Review</Link>
            </Button>
            <Tip content="Every machine looks at its repositories again now">
              <Button size="sm" variant="ghost" onClick={() => void look()}>
                <ScanSearch /> Look again
              </Button>
            </Tip>
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
  const { adding, syncing, show, repo, set } = useRepoParams();
  warm(reposRes, recentRunsRes(200), syncRes, placesRes);
  const repos = useRepos();
  const lastRuns = useRecentRuns(200).data;
  const sync = useRes(syncRes);
  const places = useRes(placesRes);
  const [stored, setStored] = useLocalStorage<Partial<ListView>>("kiln.repos.list", DEFAULT_VIEW);
  const base: ListView = { ...DEFAULT_VIEW, ...stored };
  const view: ListView = show === "built" || show === "synced" ? { ...base, facets: { ...base.facets, kind: [show] } } : base;
  const setView = (v: ListView) => {
    setStored(v);
    if (show) set({ show: null });
  };
  const [q, setQ] = React.useState("");
  const [opened, setOpened] = React.useState<Panel | null>(null);
  const open: Panel | null = opened ?? (repo ? { key: repo, what: "details" } : null);
  const setOpen = (p: Panel | null) => {
    setOpened(p);
    if (repo) set({ repo: null });
  };
  const [polling, setPolling] = React.useState<Set<string>>(new Set());
  const [notice, setNoticeState] = React.useState<Notice>(null);
  // A success says so for a while; a failure stays until the next action.
  const setNotice = (n: Notice) => {
    setNoticeState(n);
    if (n?.tone === "ok") window.setTimeout(() => setNoticeState((cur) => (cur === n ? null : cur)), 8_000);
  };

  const entries = React.useMemo(() => entriesOf(places.data, repos.data, sync.data, lastRuns), [places.data, repos.data, sync.data, lastRuns]);
  const machines = React.useMemo(() => new Map(places.data.machines.map((m) => [m.name, m])), [places.data]);
  const machineNames = React.useMemo(() => [...machines.keys()].sort(), [machines]);
  const dims = React.useMemo(() => new Map(entries.map((e) => [e.key, dimsOf(e, machines)])), [entries, machines]);
  const facets = React.useMemo(() => facetsOf(entries, dims, q, view.facets), [entries, dims, q, view.facets]);
  const list = sortEntries(
    entries.filter((e) => matches(q, view.facets, e, dims.get(e.key) ?? {})),
    view.sort,
  );
  const filtered = isFiltered(q, view.facets);
  const underway = places.data.tasks.filter(isActive);

  const poll = async (e: Entry) => {
    const r = e.built;
    if (!r) return;
    setPolling((p) => new Set(p).add(r.id));
    setNotice(null);
    try {
      await api.pollRepo(r.id);
      setNotice({ tone: "ok", text: `Polling ${r.name}.` });
      window.setTimeout(() => invalidate("repos"), 1500);
    } catch (err) {
      setNotice({ tone: "fail", text: `Polling ${r.name} failed: ${errorMessage(err)}` });
    } finally {
      setPolling((p) => {
        const n = new Set(p);
        n.delete(r.id);
        return n;
      });
    }
  };

  const started = (tasks: PlaceTask[], e: Entry) => {
    const first = tasks[0];
    if (first) setNotice({ tone: "ok", text: `${taskLine(first)}${tasks.length > 1 ? ` and ${tasks.length - 1} more` : ""}: under way for ${e.name}.` });
    setOpen({ key: e.key, what: "details" });
    places.reload();
  };
  const toggle = (key: string, what: Panel["what"]) => setOpen(open?.key === key && open.what === what ? (what === "details" ? null : { key, what: "details" }) : { key, what });
  const changed = () => {
    sync.reload();
    places.reload();
  };

  return (
    <>
      <ReposToolbar q={q} onQ={setQ} view={view} onView={setView} facets={facets} count={filtered ? `${list.length} of ${entries.length}` : `${entries.length} ${entries.length === 1 ? "repository" : "repositories"}`} />
      {notice && (
        <p role="status" className="notice" data-tone={notice.tone}>
          {notice.text}
        </p>
      )}
      {sync.data.error && <p className="notice" data-tone="fail">{sync.data.error}</p>}
      {adding && (
        <AddRepoForm
          onAdded={(r) => {
            repos.setData((l) => [...l, r]);
            set({ add: null });
            setNotice({ tone: "ok", text: `${r.name} added; its first poll is on its way.` });
            places.reload();
          }}
          onCancel={() => set({ add: null })}
        />
      )}
      {syncing && (
        <div className="mb-8">
          <NewRepo
            machines={machineNames}
            onDone={() => {
              set({ sync: null });
              changed();
            }}
            onCancel={() => set({ sync: null })}
          />
        </div>
      )}

      {underway.length > 0 && (
        <section className="repos-underway" aria-label="Under way">
          <h2 className="field-label">Under way</h2>
          <ul>
            {underway.slice(0, 6).map((t) => (
              <li key={t.id}>
                <button type="button" onClick={() => setOpen({ key: t.key, what: "details" })}>
                  <StatusDot status="running" />
                  <span className="repos-underway-name">{t.name}</span>
                  <span>{taskLine(t)}</span>
                  <span className="repos-underway-detail">
                    {t.state}
                    {t.detail ? `: ${t.detail}` : ""}
                  </span>
                </button>
              </li>
            ))}
            {underway.length > 6 && <li className="repos-underway-more">and {underway.length - 6} more</li>}
          </ul>
        </section>
      )}

      <SyncActivity sync={sync.data} />

      {entries.length === 0 ? (
        <EmptyState
          icon={<GitBranch size={24} />}
          title="No repositories yet."
          action={
            <div className="flex flex-wrap justify-center gap-2">
              <Button size="sm" variant="secondary" onClick={() => set({ add: "1" })}>
                <Plus /> Add your first repository
              </Button>
              <Button size="sm" variant="ghost" onClick={() => set({ sync: "1" })}>
                <FolderSync /> Sync a folder
              </Button>
            </div>
          }
        >
          Register an owner/name and the branches to watch, and kiln starts polling at once. The repositories on your machines show here as their runners find them.
        </EmptyState>
      ) : list.length === 0 ? (
        <EmptyState
          title="No repository matches."
          action={
            <Button
              variant="secondary"
              onClick={() => {
                setView({ ...view, facets: {} });
                setQ("");
              }}
            >
              Clear filters
            </Button>
          }
        />
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <ReposHead />
            <tbody>
              {list.map((e) => (
                <Row key={e.key} entry={e} machines={machines} open={open?.key === e.key ? open.what : null} polling={e.built ? polling.has(e.built.id) : false} onToggle={(what) => toggle(e.key, what)} onPoll={() => void poll(e)}>
                  {open?.key === e.key && (
                    <Details
                      entry={e}
                      machines={machines}
                      machineNames={machineNames}
                      panel={open.what}
                      onPanel={(what) => setOpen({ key: e.key, what })}
                      onClose={() => setOpen(null)}
                      onStarted={(t) => started(t, e)}
                      onNotice={setNotice}
                      onDeleted={() => {
                        repos.setData((l) => l.filter((x) => x.id !== e.built?.id));
                        setOpen(null);
                        setNotice({ tone: "ok", text: `${e.name} deleted.` });
                        places.reload();
                      }}
                      onChange={changed}
                      onActivity={(id) => showActivity(set, id)}
                    />
                  )}
                </Row>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

function ReposToolbar({ q, onQ, view, onView, facets, count }: { q: string; onQ: (v: string) => void; view: ListView; onView: (v: ListView) => void; facets: React.ComponentProps<typeof FacetBox>["facets"]; count: string }) {
  return (
    <div className="repos-tools">
      <FacetBox
        className="repos-filter"
        label="Filter repositories"
        placeholder="Filter by name, machine, OS, architecture, kind, state…"
        query={q}
        onQuery={onQ}
        facets={facets}
        chosen={view.facets}
        onChosen={(f) => onView({ ...view, facets: f })}
      />
      <SelectField className="repos-sort" label="Sort repositories" hideLabel value={view.sort} onChange={(v) => onView({ ...view, sort: v as SortId })} options={SORTS} />
      <span className="label repos-count tabular-nums">{count}</span>
    </div>
  );
}

function ReposHead() {
  return (
    <thead>
      <tr>
        <th>Repository</th>
        <th className="hidden md:table-cell">On machines</th>
        <th className="hidden lg:table-cell">Latest run</th>
        <th className="num hidden sm:table-cell">Last poll</th>
        <th className="w-0 text-right">
          <span className="sr-only">Actions</span>
        </th>
      </tr>
    </thead>
  );
}

/** The page while it loads: its toolbar, the table's head and its rows' space. */
function ReposHold() {
  const noop = () => undefined;
  return (
    <>
      <ReposToolbar q="" onQ={noop} view={DEFAULT_VIEW} onView={noop} facets={[]} count={" "} />
      <TableHold head={<ReposHead />} rows={8} rowHeight={45} label="Loading the repositories" />
    </>
  );
}

/** A repository's state as its dot shows it: a poll error, its copies, or that it is on machines. */
function entryDot(e: Entry): { status: string; title: string } {
  if (e.built && !e.built.enabled) return { status: "cancelled", title: "disabled" };
  if (e.built?.last_error) return { status: "timed_out", title: "poll error" };
  if (e.tasks.some(isActive)) return { status: "running", title: "under way" };
  const copies = e.places.filter((p) => p.copy);
  if (copies.some((p) => p.copy?.state === "error")) return { status: "failed", title: "a copy has an error" };
  if (e.synced?.conflicts.length) return { status: "timed_out", title: "conflicts" };
  if (copies.some((p) => p.copy && p.copy.state !== "in step")) return { status: "running", title: "syncing" };
  if (e.built || e.places.length > 0) return { status: "succeeded", title: e.built ? "healthy" : "on machines" };
  return { status: "skipped", title: "on no machine" };
}

function Row({
  entry: e,
  machines,
  open,
  polling,
  onToggle,
  onPoll,
  children,
}: {
  entry: Entry;
  machines: Map<string, PlaceMachine>;
  open: Panel["what"] | null;
  polling: boolean;
  onToggle: (what: Panel["what"]) => void;
  onPoll: () => void;
  children: React.ReactNode;
}) {
  const r = e.built;
  const dot = entryDot(e);
  const [owner, ...rest] = e.name.split("/");
  const on = machinesOf(e);
  const remote = browseUrl(e.origin);
  return (
    <>
      <tr
        data-href
        tabIndex={0}
        aria-expanded={open !== null}
        onClick={(ev) => !(ev.target as HTMLElement).closest("button,a,input,select,label") && onToggle("details")}
        onKeyDown={(ev) => ev.key === "Enter" && ev.target === ev.currentTarget && onToggle("details")}
      >
        <td className="max-w-[220px] sm:max-w-[380px]">
          <div className="flex items-center gap-2.5">
            <StatusDot status={dot.status} title={dot.title} />
            <span className="min-w-0 truncate">
              {rest.length > 0 ? (
                <>
                  <span className="text-[#8c8f98]">{owner}/</span>
                  <span className="text-[#e4e5e7]">{rest.join("/")}</span>
                </>
              ) : (
                <span className="text-[#e4e5e7]">{e.name}</span>
              )}
            </span>
            {e.synced && <span className="chip shrink-0">synced</span>}
          </div>
          {r?.last_error && (
            <div className="mt-1 flex items-start gap-1.5 pl-[18px] font-mono text-[10px] leading-snug text-[#cbb69a]">
              <AlertTriangle size={11} className="mt-px" /> <span className="line-clamp-2 break-all">{r.last_error}</span>
            </div>
          )}
          <div className="repo-row-machines md:hidden">{on.length ? `on ${on.join(", ")}` : "on no machine"}</div>
        </td>
        <td className="hidden md:table-cell">
          {on.length === 0 ? (
            <span className="cell-dim">—</span>
          ) : (
            <span className="repo-machines">
              {on.slice(0, CHIPS).map((name) => {
                const ps = e.places.filter((p) => p.machine === name);
                const p = ps[0];
                const status = p?.copy ? DOT[p.copy.state] : p?.online ? "succeeded" : "skipped";
                const where = ps.map((x) => tildeOf(x.path, machines.get(name)?.home) ?? x.path).join("\n");
                return (
                  <span key={name} className="chip repo-machine" title={where}>
                    <StatusDot status={status} />
                    {name}
                    {ps.length > 1 && <span className="repo-machine-n">×{ps.length}</span>}
                  </span>
                );
              })}
              {on.length > CHIPS && (
                <span className="chip" title={on.slice(CHIPS).join(", ")}>
                  +{on.length - CHIPS}
                </span>
              )}
            </span>
          )}
        </td>
        <td className="hidden lg:table-cell">
          {e.latest ? (
            <Link to={`/runs/${e.latest.id}`} className="inline-flex items-center gap-2 font-mono text-[11px] text-[#b5b8c0] hover:text-white">
              <StatusDot status={e.latest.status} /> #{e.latest.number} · {e.latest.branch}
              <span className="text-[#7b7e88]">
                <RelTime ts={e.latest.created_at} />
              </span>
            </Link>
          ) : (
            <span className="cell-dim">—</span>
          )}
        </td>
        <td className="num cell-mono hidden whitespace-nowrap sm:table-cell">{r ? <RelTime ts={r.last_polled_at} /> : <span className="cell-dim">—</span>}</td>
        <td className="text-right whitespace-nowrap">
          <div className="inline-flex items-center gap-0.5">
            {r && (
              <Tip content="Poll now">
                <button className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Poll ${r.name} now`} disabled={polling} onClick={onPoll}>
                  <RefreshCw size={14} className={polling ? "animate-spin motion-reduce:animate-none" : undefined} />
                </button>
              </Tip>
            )}
            {r && (
              <Tip content="Start a run">
                <button className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Start a run of ${r.name}`} aria-expanded={open === "run"} onClick={() => onToggle("run")}>
                  <Play size={14} />
                </button>
              </Tip>
            )}
            <Tip content="Put it on machines: clone or sync">
              <button className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Put ${e.name} on machines`} aria-expanded={open === "clone" || open === "sync"} onClick={() => onToggle(e.synced ? "sync" : "clone")}>
                <HardDriveDownload size={14} />
              </button>
            </Tip>
            {remote && (
              <Tip content="Open its remote">
                <a className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Open ${e.name}'s remote`} href={remote} target="_blank" rel="noopener noreferrer">
                  <ArrowUpRight size={14} />
                </a>
              </Tip>
            )}
            {r && (
              <Tip content="Delete">
                <button className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Delete ${r.name}`} aria-expanded={open === "delete"} onClick={() => onToggle("delete")}>
                  <Trash2 size={14} />
                </button>
              </Tip>
            )}
          </div>
        </td>
      </tr>
      {children && (
        <tr className="repo-detail-row">
          <td colSpan={5}>{children}</td>
        </tr>
      )}
    </>
  );
}

/** A repository opened under its row: its forms, its tasks, where it is, and its sync. */
function Details({
  entry: e,
  machines,
  machineNames,
  panel,
  onPanel,
  onClose,
  onStarted,
  onNotice,
  onDeleted,
  onChange,
  onActivity,
}: {
  entry: Entry;
  machines: Map<string, PlaceMachine>;
  machineNames: string[];
  panel: Panel["what"];
  onPanel: (what: Panel["what"]) => void;
  onClose: () => void;
  onStarted: (tasks: PlaceTask[]) => void;
  onNotice: (n: Notice) => void;
  onDeleted: () => void;
  onChange: () => void;
  /** Open the Sync section on a synced repo's activity. */
  onActivity: (repo: string) => void;
}) {
  const r = e.built;
  return (
    <div className="repo-details">
      <div className="repo-details-head">
        <div className="min-w-0">
          <h3>{e.name}</h3>
          <p className="repo-details-origin">{e.origin ?? (e.key.startsWith("here:") ? "no remote: it is on one machine alone" : e.key)}</p>
        </div>
        <div className="repo-actions">
          <Button size="xs" variant="secondary" aria-pressed={panel === "clone"} title="Clone it to machines" onClick={() => onPanel(panel === "clone" ? "details" : "clone")}>
            Clone
          </Button>
          <Button size="xs" variant="secondary" aria-pressed={panel === "sync"} title="Keep it in sync on machines" onClick={() => onPanel(panel === "sync" ? "details" : "sync")}>
            Sync
          </Button>
          {r && (
            <Button size="xs" variant="secondary" asChild>
              <Link to={`/runs?repo=${encodeURIComponent(r.id)}`} title="Its runs">
                Runs
              </Link>
            </Button>
          )}
          <Button size="xs" variant="secondary" onClick={onClose}>
            Close
          </Button>
        </div>
      </div>
      {(panel === "clone" || panel === "sync") && <PutForm key={panel} entry={e} machines={[...machines.values()]} mode={panel} onStarted={onStarted} onCancel={() => onPanel("details")} />}
      {panel === "run" && r && <RunForm repo={r} onCancel={() => onPanel("details")} onNotice={onNotice} />}
      {panel === "delete" && r && <DeleteRepo repo={r} onCancel={() => onPanel("details")} onDeleted={onDeleted} />}
      <TaskList tasks={e.tasks} machines={machines} onChange={onChange} />
      {r && <WatchedBy on={`repo:${r.name}`} watches={(on) => on === `repo:${r.name}` || on.startsWith(`workflow:${r.name}:`) || on.startsWith(`environment:${r.name}:`)} />}
      <section className="repo-section" aria-label="On machines">
        <h4 className="field-label">On machines</h4>
        <PlaceList entry={e} machines={machines} synced={e.synced} onStarted={onStarted} />
      </section>
      {r && (
        // What runs its CI (W23-R07), when its host makes a choice possible.
        <Loadable label="what runs its CI" fallback={<></>}>
          <Providers repo={r.name} />
        </Loadable>
      )}
      {!e.key.startsWith("here:") && (
        // The copies of its trees runners keep for others, and its rules for them (docs/reach.md §5.1).
        <Loadable label="its copies on runners" fallback={<Hold height={160} />}>
          <RepoKeep repoKey={e.key} />
        </Loadable>
      )}
      {r && (
        // Its CI secrets and variables (W09-R11).
        <section className="repo-section" aria-label="Its secrets and variables">
          <Loadable label="its secrets and variables" fallback={<Hold height={120} />}>
            <ScopeSecrets scope={`repo:${r.name}`} of="Its" />
          </Loadable>
        </section>
      )}
      {e.synced && (
        <section className="repo-section">
          <RepoBlock repo={e.synced} machines={machineNames} onChange={onChange} title="Kept in sync" copies={false} />
          <div>
            <Button size="xs" variant="ghost" onClick={() => e.synced && onActivity(e.synced.id)}>
              Its sync activity: each change and the copies that have it
            </Button>
          </div>
        </section>
      )}
    </div>
  );
}
