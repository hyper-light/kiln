/** The forms of a repository kiln builds, in place under what they act on: adding one, starting a run, deleting it. */
import * as React from "react";
import { useNavigate } from "@/lib/router";
import { Play } from "lucide-react";
import { api } from "@/api/client";
import type { Repo } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Confirm, ErrorState, SelectField, TextField } from "@/components/ui/misc";
import { errorMessage } from "@/lib/hooks";

export type Notice = { tone: "ok" | "fail"; text: string } | null;

/** Adding a repository kiln builds: its name on GitHub, the branches to watch, how often to poll. */
export function AddRepoForm({ onAdded, onCancel }: { onAdded: (r: Repo) => void; onCancel: () => void }) {
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
export function RunForm({ repo, onCancel, onNotice }: { repo: Repo; onCancel: () => void; onNotice: (n: Notice) => void }) {
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
export function DeleteRepo({ repo, onCancel, onDeleted }: { repo: Repo; onCancel: () => void; onDeleted: () => void }) {
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
