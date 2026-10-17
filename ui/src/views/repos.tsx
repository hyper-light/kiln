import * as React from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { DropdownMenu } from "radix-ui";
import { AlertTriangle, ArrowUpRight, GitBranch, MoreHorizontal, Play, Plus, RefreshCw, Trash2 } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Repo, Run } from "@/api/types";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { EmptyState, ErrorState, PageIntro, SearchField, SelectField, SkeletonRows, TextField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { Tip } from "@/components/ui/tooltip";
import { formatDuration } from "@/lib/format";
import { errorMessage, useDocumentTitle, useResource } from "@/lib/hooks";
import { RelTime } from "./shared";

export default function Repos() {
  useDocumentTitle("Repositories");
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const repos = useResource((s) => api.repos(s), [], { pollMs: 30_000 });
  const lastRuns = useResource((s) => api.runs({ limit: 200 }, s), []);
  const [q, setQ] = React.useState("");
  const [addOpen, setAddOpen] = React.useState(params.get("add") === "1");
  const [trigger, setTrigger] = React.useState<Repo | null>(null);
  const [del, setDel] = React.useState<Repo | null>(null);
  const [polling, setPolling] = React.useState<Set<string>>(new Set());

  React.useEffect(() => {
    if (params.get("add") === "1") {
      setAddOpen(true);
      const n = new URLSearchParams(params);
      n.delete("add");
      setParams(n, { replace: true });
    }
  }, [params, setParams]);

  useServerEvents((ev) => {
    if (ev.type === "run") lastRuns.setData((l) => (l ? [ev.run, ...l.filter((r) => r.id !== ev.run.id)] : l));
  });

  const latestByRepo = React.useMemo(() => {
    const m = new Map<string, Run>();
    for (const r of lastRuns.data ?? []) {
      const cur = m.get(r.repo_id);
      if (!cur || r.created_at > cur.created_at) m.set(r.repo_id, r);
    }
    return m;
  }, [lastRuns.data]);

  const poll = async (r: Repo) => {
    setPolling((p) => new Set(p).add(r.id));
    try {
      await api.pollRepo(r.id);
      toast({ title: `Polling ${r.name}`, tone: "info" });
      window.setTimeout(repos.reload, 1500);
    } catch (e) {
      toast.error(`Poll failed for ${r.name}`, errorMessage(e));
    } finally {
      setPolling((p) => {
        const n = new Set(p);
        n.delete(r.id);
        return n;
      });
    }
  };

  const list = (repos.data ?? []).filter((r) => r.name.toLowerCase().includes(q.trim().toLowerCase()));

  return (
    <div className="container-console page">
      <PageIntro
        index={4}
        label="Repositories"
        title="Repositories"
        lead="kiln polls each repository with conditional requests (ETag), so idle repos cost nothing against the GitHub rate limit. Pipelines live in .kiln/*.yaml."
        actions={
          <Button onClick={() => setAddOpen(true)}>
            <Plus /> Add repository
          </Button>
        }
      />

      <div className="mb-5 flex flex-wrap items-center gap-3">
        <SearchField className="w-full sm:w-72" label="Search repositories" placeholder="Find a repository…" value={q} onChange={setQ} />
        <span className="label ml-auto">{repos.data ? `${list.length} of ${repos.data.length}` : ""}</span>
      </div>

      {repos.error && !repos.data ? (
        <ErrorState error={repos.error} onRetry={repos.reload} />
      ) : !repos.data ? (
        <SkeletonRows rows={8} cols={5} />
      ) : repos.data.length === 0 ? (
        <EmptyState
          icon={<GitBranch size={24} />}
          title="No repositories yet."
          action={
            <Button variant="secondary" onClick={() => setAddOpen(true)}>
              <Plus /> Add your first repository
            </Button>
          }
        >
          Register an owner/name and the branches to watch. kiln starts polling immediately.
        </EmptyState>
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <thead>
              <tr>
                <th>Repository</th>
                <th className="hidden md:table-cell">Branches</th>
                <th className="hidden lg:table-cell">Latest run</th>
                <th className="num hidden sm:table-cell">Interval</th>
                <th className="num">Last poll</th>
                <th className="w-0 text-right">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {list.map((r) => {
                const latest = latestByRepo.get(r.id);
                return (
                  <tr key={r.id}>
                    <td className="max-w-[360px]">
                      <div className="flex items-center gap-2.5">
                        <StatusDot status={!r.enabled ? "cancelled" : r.last_error ? "timed_out" : "succeeded"} title={!r.enabled ? "disabled" : r.last_error ? "poll error" : "healthy"} />
                        <Link to={`/runs?repo=${encodeURIComponent(r.id)}`} className="min-w-0 truncate hover:text-white">
                          <span className="text-[#8c8f98]">{r.name.split("/")[0]}/</span>
                          <span className="text-[#e4e5e7]">{r.name.split("/").slice(1).join("/")}</span>
                        </Link>
                      </div>
                      {r.last_error && (
                        <div className="mt-1 flex items-start gap-1.5 pl-[18px] font-mono text-[10px] leading-snug text-[#cbb69a]">
                          <AlertTriangle size={11} className="mt-px" /> <span className="line-clamp-2 break-all">{r.last_error}</span>
                        </div>
                      )}
                    </td>
                    <td className="hidden md:table-cell">
                      <span className="inline-flex flex-wrap gap-1">
                        {r.branches.length ? r.branches.slice(0, 4).map((b) => <span key={b} className="chip">{b}</span>) : <span className="cell-dim">default</span>}
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
                    <td className="num cell-mono whitespace-nowrap">
                      <RelTime ts={r.last_polled_at} />
                    </td>
                    <td className="text-right whitespace-nowrap">
                      <div className="inline-flex items-center gap-0.5">
                        <Tip content="Poll now">
                          <button className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Poll ${r.name} now`} disabled={polling.has(r.id)} onClick={() => poll(r)}>
                            <RefreshCw size={14} className={polling.has(r.id) ? "animate-spin" : undefined} />
                          </button>
                        </Tip>
                        <Tip content="Trigger run">
                          <button className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Trigger a run of ${r.name}`} onClick={() => setTrigger(r)}>
                            <Play size={14} />
                          </button>
                        </Tip>
                        <DropdownMenu.Root>
                          <DropdownMenu.Trigger asChild>
                            <button className="icon-btn" data-variant="bare" data-size="sm" aria-label={`More actions for ${r.name}`}>
                              <MoreHorizontal size={15} />
                            </button>
                          </DropdownMenu.Trigger>
                          <DropdownMenu.Portal>
                            <DropdownMenu.Content className="popover" align="end" sideOffset={4}>
                              <DropdownMenu.Item className="menu-item" onSelect={() => navigate(`/runs?repo=${encodeURIComponent(r.id)}`)}>
                                <ArrowUpRight size={14} /> View runs
                              </DropdownMenu.Item>
                              <DropdownMenu.Item className="menu-item" onSelect={() => window.open(r.clone_url.replace(/\.git$/, ""), "_blank", "noopener")}>
                                <GitBranch size={14} /> Open on GitHub
                              </DropdownMenu.Item>
                              <DropdownMenu.Separator className="my-1 h-px bg-white/10" />
                              <DropdownMenu.Item className="menu-item" data-danger onSelect={() => setDel(r)}>
                                <Trash2 size={14} /> Delete…
                              </DropdownMenu.Item>
                            </DropdownMenu.Content>
                          </DropdownMenu.Portal>
                        </DropdownMenu.Root>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      <AddRepoDialog open={addOpen} onOpenChange={setAddOpen} onAdded={(r) => repos.setData((l) => [...(l ?? []), r])} />
      {trigger && <TriggerDialog repo={trigger} onClose={() => setTrigger(null)} />}
      {del && (
        <ConfirmDialog
          open
          onOpenChange={(o) => !o && setDel(null)}
          title={`Delete ${del.name}?`}
          description="kiln stops polling it. Existing runs and logs are removed with it."
          confirmLabel="Delete repository"
          danger
          onConfirm={async () => {
            try {
              await api.deleteRepo(del.id);
              repos.setData((l) => l?.filter((x) => x.id !== del.id));
              toast({ title: `${del.name} deleted`, tone: "info" });
            } catch (e) {
              toast.error("Delete failed", errorMessage(e));
            }
          }}
        />
      )}
    </div>
  );
}

function AddRepoDialog({ open, onOpenChange, onAdded }: { open: boolean; onOpenChange: (o: boolean) => void; onAdded: (r: Repo) => void }) {
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
      toast({ title: `${repo.name} added`, description: "First poll is on its way.", tone: "succeeded" });
      onOpenChange(false);
      setName("");
      setCloneUrl("");
    } catch (e2) {
      setErr(errorMessage(e2));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title="Add repository" eyebrow="Repositories / New" description="kiln polls GitHub for new commits on the listed branches and pull requests.">
        <form onSubmit={submit} className="space-y-4">
          <TextField label="Repository" placeholder="hyper-light/hyper-raft" value={name} onChange={(e) => setName(e.target.value)} autoFocus hint="owner/name on GitHub" />
          <TextField label="Clone URL (optional)" placeholder={name ? `https://github.com/${name.trim()}.git` : "https://github.com/owner/name.git"} value={cloneUrl} onChange={(e) => setCloneUrl(e.target.value)} />
          <div className="grid grid-cols-[1fr_120px] gap-3">
            <TextField label="Branches" placeholder="main, release/*" value={branches} onChange={(e) => setBranches(e.target.value)} hint="Comma-separated; globs allowed" />
            <TextField label="Poll (s)" type="number" min={10} value={poll} onChange={(e) => setPoll(e.target.value)} />
          </div>
          {err && <ErrorState error={err} compact />}
          <div className="flex justify-end gap-2 pt-2">
            <Button variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={!valid || busy}>
              {busy ? "Adding…" : "Add repository"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function TriggerDialog({ repo, onClose }: { repo: Repo; onClose: () => void }) {
  const navigate = useNavigate();
  const [branch, setBranch] = React.useState(repo.branches.find((b) => !b.includes("*")) ?? "");
  const [custom, setCustom] = React.useState("");
  const [pipeline, setPipeline] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const concrete = repo.branches.filter((b) => !b.includes("*"));

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    try {
      const b = (branch === "__custom" ? custom : branch).trim();
      const runs = await api.triggerRepo(repo.id, { branch: b || undefined, pipeline: pipeline.trim() || undefined });
      onClose();
      if (runs.length === 0) {
        toast({ title: "Nothing to run", description: "No pipeline matched that branch.", tone: "info" });
      } else {
        toast({
          title: `Started ${runs.length} run${runs.length > 1 ? "s" : ""} on ${repo.name}`,
          description: runs.map((r) => `#${r.number} ${r.pipeline}`).join(" · "),
          tone: "running",
          href: `/runs/${runs[0]!.id}`,
        });
        navigate(`/runs/${runs[0]!.id}`);
      }
    } catch (e2) {
      setErr(errorMessage(e2));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={`Run ${repo.name}`} eyebrow="Trigger / Manual" description="Starts a manual run at the branch head. Leave pipeline empty to run every pipeline that matches.">
        <form onSubmit={submit} className="space-y-4">
          <SelectField
            label="Branch"
            value={branch}
            onChange={setBranch}
            options={[...(concrete.length ? [] : [{ value: "", label: "Default branch" }]), ...concrete.map((b) => ({ value: b, label: b })), { value: "__custom", label: "Other branch…" }]}
          />
          {branch === "__custom" && <TextField label="Branch name" value={custom} onChange={(e) => setCustom(e.target.value)} placeholder="feature/xyz" autoFocus />}
          <TextField label="Pipeline (optional)" placeholder="ci" value={pipeline} onChange={(e) => setPipeline(e.target.value)} hint="Name from .kiln/<name>.yaml" />
          {err && <ErrorState error={err} compact />}
          <div className="flex justify-end gap-2 pt-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy || (branch === "__custom" && !custom.trim())}>
              <Play /> {busy ? "Starting…" : "Start run"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
