import * as React from "react";
import { api } from "@/api/client";
import type { Agent, SyncConflict, SyncCopy, SyncKeep, SyncRepo } from "@/api/types";
import { Button } from "@/components/ui/button";
import { EmptyState, ErrorState, PageIntro, SelectField, SkeletonRows, TextField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { errorMessage, useDocumentTitle, useResource } from "@/lib/hooks";
import "./sync.css";

/** A copy's state as a status dot shows it. */
const DOT: Record<SyncCopy["state"], string> = {
  "in step": "succeeded",
  "catching up": "running",
  fetching: "running",
  sending: "running",
  starting: "queued",
  offline: "skipped",
  error: "failed",
};

export default function Sync() {
  useDocumentTitle("Sync");
  const view = useResource((s) => api.sync(s), [], { pollMs: 3_000 });
  const agents = useResource((s) => api.agents(s), [], { pollMs: 30_000 });
  const [adding, setAdding] = React.useState(false);
  const machines = React.useMemo(() => (agents.data ?? []).map((a: Agent) => a.name).sort(), [agents.data]);
  const repos = view.data?.repos ?? [];

  return (
    <div className="container-console page sync-page">
      <PageIntro
        index={5}
        label="Sync"
        title="Synced repos"
        lead="A folder kept the same on several machines. A change on one reaches the others within seconds; when two machines change one file, both versions are kept."
        actions={
          !adding && (
            <Button size="sm" variant="secondary" onClick={() => setAdding(true)}>
              Sync a folder
            </Button>
          )
        }
      />
      {view.data?.error && <p className="sync-off">{view.data.error}</p>}
      {adding && (
        <NewRepo
          machines={machines}
          onDone={() => {
            setAdding(false);
            view.reload();
          }}
          onCancel={() => setAdding(false)}
        />
      )}
      {view.error && !view.data ? (
        <ErrorState error={view.error} onRetry={view.reload} />
      ) : !view.data ? (
        <SkeletonRows rows={3} cols={3} />
      ) : repos.length === 0 && !adding ? (
        <EmptyState
          title="Nothing is synced yet"
          action={
            <Button size="sm" variant="secondary" onClick={() => setAdding(true)}>
              Sync a folder
            </Button>
          }
        >
          Pick a folder on two or more machines, a repository or anything else, and kiln keeps them the same.
        </EmptyState>
      ) : (
        <div className="sync-list">
          {repos.map((r) => (
            <RepoBlock key={r.id} repo={r} machines={machines} onChange={view.reload} />
          ))}
        </div>
      )}
    </div>
  );
}

/** Starting a synced repo: its name, where its copies are, and what to leave out. */
function NewRepo({ machines, onDone, onCancel }: { machines: string[]; onDone: () => void; onCancel: () => void }) {
  const [name, setName] = React.useState("");
  const [copies, setCopies] = React.useState<{ machine: string; path: string }[]>([
    { machine: "", path: "" },
    { machine: "", path: "" },
  ]);
  const [exclude, setExclude] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const filled = copies.filter((c) => c.machine && c.path.trim());

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.syncCreate({
        name: name.trim(),
        copies: filled.map((c) => ({ machine: c.machine, path: c.path.trim() })),
        exclude: exclude
          .split(",")
          .map((x) => x.trim())
          .filter(Boolean),
      });
      onDone();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form className="sync-form" onSubmit={submit} aria-label="Sync a folder">
      <TextField label="Name" value={name} onChange={(e) => setName(e.target.value)} placeholder="kiln" required autoFocus />
      <fieldset className="sync-copies">
        <legend className="field-label">Copies</legend>
        {copies.map((c, i) => (
          <CopyFields
            key={i}
            machines={machines}
            value={c}
            onChange={(v) => setCopies((all) => all.map((x, j) => (j === i ? v : x)))}
            onRemove={copies.length > 1 ? () => setCopies((all) => all.filter((_, j) => j !== i)) : undefined}
          />
        ))}
        <Button type="button" size="xs" variant="ghost" onClick={() => setCopies((all) => [...all, { machine: "", path: "" }])}>
          Another copy
        </Button>
      </fieldset>
      <TextField
        label="Leave out"
        value={exclude}
        onChange={(e) => setExclude(e.target.value)}
        placeholder="dist, .cache"
        hint="Names or folders, besides .git, node_modules, target and what .gitignore says."
      />
      {error && <p className="sync-error">{error}</p>}
      <div className="sync-actions">
        <Button type="submit" size="sm" disabled={busy || !name.trim() || filled.length === 0}>
          {busy ? "Starting…" : "Start syncing"}
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

/** One copy's machine and folder. */
function CopyFields({
  machines,
  value,
  onChange,
  onRemove,
}: {
  machines: string[];
  value: { machine: string; path: string };
  onChange: (v: { machine: string; path: string }) => void;
  onRemove?: () => void;
}) {
  return (
    <div className="sync-copy-fields">
      <SelectField
        label="Machine"
        value={value.machine}
        onChange={(machine) => onChange({ ...value, machine })}
        options={[{ value: "", label: "Choose a machine" }, ...machines.map((m) => ({ value: m, label: m }))]}
      />
      <TextField label="Folder" value={value.path} onChange={(e) => onChange({ ...value, path: e.target.value })} placeholder="/home/you/src/kiln" spellCheck={false} />
      {onRemove && (
        <Button type="button" size="xs" variant="ghost" onClick={onRemove} aria-label="Remove this copy">
          Remove
        </Button>
      )}
    </div>
  );
}

function summary(r: SyncRepo) {
  const by = new Map<string, number>();
  for (const c of r.copies) by.set(c.state, (by.get(c.state) ?? 0) + 1);
  return [...by.entries()].map(([state, n]) => `${n} ${state}`).join(" · ");
}

function RepoBlock({ repo, machines, onChange }: { repo: SyncRepo; machines: string[]; onChange: () => void }) {
  const [adding, setAdding] = React.useState(false);
  const [stopping, setStopping] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const act = async (f: () => Promise<unknown>) => {
    setError(null);
    try {
      await f();
      onChange();
      return true;
    } catch (e) {
      setError(errorMessage(e));
      return false;
    }
  };

  return (
    <section className="sync-repo" aria-label={repo.name}>
      <header className="sync-repo-head">
        <div className="min-w-0">
          <h2>{repo.name}</h2>
          <p className="sync-meta">
            {repo.files.toLocaleString()} files · {summary(repo) || "no copies"}
            {repo.exclude.length > 0 && ` · leaves out ${repo.exclude.join(", ")}`}
          </p>
        </div>
        <div className="sync-actions">
          {!adding && (
            <Button size="xs" variant="secondary" onClick={() => setAdding(true)}>
              Add a copy
            </Button>
          )}
          {!stopping && (
            <Button size="xs" variant="ghost" onClick={() => setStopping(true)}>
              Stop syncing
            </Button>
          )}
        </div>
      </header>
      {repo.error && <p className="sync-error">{repo.error}</p>}
      {error && <p className="sync-error">{error}</p>}
      {stopping && (
        <div className="sync-confirm" role="group" aria-label={`Stop syncing ${repo.name}`}>
          <p>Stop syncing {repo.name}? Every copy keeps its files where they are.</p>
          <div className="sync-actions">
            <Button size="xs" variant="danger" onClick={() => void act(() => api.syncDelete(repo.id))}>
              Stop syncing
            </Button>
            <Button size="xs" variant="ghost" onClick={() => setStopping(false)}>
              Keep syncing
            </Button>
          </div>
        </div>
      )}
      <ul className="sync-copies-list">
        {repo.copies.map((c) => (
          <CopyRow key={c.member} copy={c} onRemove={() => act(() => api.syncRemoveCopy(repo.id, c.member))} />
        ))}
      </ul>
      {adding && (
        <AddCopy
          machines={machines}
          onAdd={async (copy) => {
            if (await act(() => api.syncAddCopy(repo.id, copy))) setAdding(false);
          }}
          onCancel={() => setAdding(false)}
        />
      )}
      {repo.conflicts.length > 0 && (
        <div className="sync-conflicts">
          <h3 className="field-label">
            {repo.conflicts.length === 1 ? "1 conflict" : `${repo.conflicts.length} conflicts`}
          </h3>
          <ul>
            {repo.conflicts.map((c) => (
              <ConflictRow key={c.path} conflict={c} onResolve={(keep) => act(() => api.syncResolve(repo.id, c.path, keep))} />
            ))}
          </ul>
        </div>
      )}
    </section>
  );
}

function CopyRow({ copy, onRemove }: { copy: SyncCopy; onRemove: () => Promise<boolean> }) {
  const [removing, setRemoving] = React.useState(false);
  const s = copy.status;
  const details = s
    ? [
        s.behind > 0 && `${s.behind} to take`,
        s.unsent > 0 && `${s.unsent} to send`,
        s.waiting > 0 && `fetching ${s.waiting}`,
        s.conflicts > 0 && `${s.conflicts} conflict${s.conflicts === 1 ? "" : "s"}`,
        s.skipped.length > 0 && `${s.skipped.length} left out`,
      ].filter(Boolean)
    : [];
  return (
    <li className="sync-copy">
      <div className="sync-copy-line">
        <StatusDot status={DOT[copy.state]} title={copy.state} />
        <span className="sync-machine">{copy.machine}</span>
        <span className="sync-state">{copy.state}</span>
        {!removing && (
          <Button size="xs" variant="ghost" className="sync-remove" onClick={() => setRemoving(true)}>
            Remove
          </Button>
        )}
      </div>
      <code className="sync-path">{copy.path}</code>
      {details.length > 0 && <p className="sync-meta">{details.join(" · ")}</p>}
      {s?.error && <p className="sync-error">{s.error}</p>}
      {s && s.skipped.length > 0 && (
        <details className="sync-skipped">
          <summary>Left out</summary>
          <ul>
            {s.skipped.map((k) => (
              <li key={k.path}>
                <code>{k.path}</code> {k.why}
              </li>
            ))}
          </ul>
        </details>
      )}
      {removing && (
        <div className="sync-confirm" role="group" aria-label={`Remove the copy on ${copy.machine}`}>
          <p>
            Stop syncing the copy on {copy.machine}? Its files stay in <code>{copy.path}</code>.
          </p>
          <div className="sync-actions">
            <Button size="xs" variant="danger" onClick={() => void onRemove().then((ok) => ok || setRemoving(false))}>
              Remove copy
            </Button>
            <Button size="xs" variant="ghost" onClick={() => setRemoving(false)}>
              Keep it
            </Button>
          </div>
        </div>
      )}
    </li>
  );
}

function AddCopy({ machines, onAdd, onCancel }: { machines: string[]; onAdd: (copy: { machine: string; path: string }) => Promise<void>; onCancel: () => void }) {
  const [copy, setCopy] = React.useState({ machine: "", path: "" });
  const [busy, setBusy] = React.useState(false);
  return (
    <form
      className="sync-form sync-add"
      aria-label="Add a copy"
      onSubmit={(e) => {
        e.preventDefault();
        setBusy(true);
        void onAdd({ machine: copy.machine, path: copy.path.trim() }).finally(() => setBusy(false));
      }}
    >
      <CopyFields machines={machines} value={copy} onChange={setCopy} />
      <div className="sync-actions">
        <Button type="submit" size="xs" disabled={busy || !copy.machine || !copy.path.trim()}>
          Add copy
        </Button>
        <Button type="button" size="xs" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

const KEEP: { value: SyncKeep; label: string }[] = [
  { value: "name", label: "Keep the file's version" },
  { value: "copy", label: "Keep the conflict copy's" },
  { value: "both", label: "Keep both" },
];

function ConflictRow({ conflict, onResolve }: { conflict: SyncConflict; onResolve: (keep: SyncKeep) => Promise<boolean> }) {
  const [keep, setKeep] = React.useState<SyncKeep>("name");
  const [busy, setBusy] = React.useState(false);
  return (
    <li className="sync-conflict">
      <code className="sync-path">{conflict.beside}</code>
      <code className="sync-path sync-beside">{conflict.path}</code>
      <div className="sync-resolve">
        <SelectField label="Resolve" hideLabel value={keep} onChange={(v) => setKeep(v as SyncKeep)} options={KEEP} />
        <Button
          size="xs"
          variant="secondary"
          disabled={busy}
          onClick={() => {
            setBusy(true);
            void onResolve(keep).finally(() => setBusy(false));
          }}
        >
          Resolve
        </Button>
      </div>
    </li>
  );
}
