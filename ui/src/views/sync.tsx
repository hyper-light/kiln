/**
 * Folders kept the same on several machines (docs/sync.md), as the Repositories page shows them
 * under a synced repository: starting one, what it holds and leaves out, its conflicts, and each
 * copy's own state (the page lists copies among the places a repository is).
 */
import * as React from "react";
import { api } from "@/api/client";
import type { SyncConflict, SyncCopy, SyncKeep, SyncMode, SyncRepo } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Confirm, SelectField, TextField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { errorMessage } from "@/lib/hooks";
import "./sync.css";

/** A copy's state as a status dot shows it. */
export const DOT: Record<SyncCopy["state"], string> = {
  "in step": "succeeded",
  "catching up": "running",
  fetching: "running",
  sending: "running",
  starting: "queued",
  offline: "skipped",
  error: "failed",
};

/** What a synced repo can keep the same, as a choice says it. */
export const MODES: { value: SyncMode; label: string }[] = [
  { value: "files", label: "Its files" },
  { value: "files_and_git", label: "Its files, branches and commits" },
  { value: "git", label: "Its branches and commits" },
];

/** What a synced repo keeps the same, in a few words. */
export function modeWords(mode: SyncMode | undefined): string {
  return mode === "git" ? "branches and commits" : mode === "files_and_git" ? "files, branches and commits" : "files";
}

/** A path of a synced repo as people say it: a branch's or tag's name, else the path. */
export function pathWords(path: string): string {
  if (path === ".git/HEAD") return "the branch checked out";
  if (path.startsWith(".git/refs/heads/")) return `branch ${path.slice(".git/refs/heads/".length)}`;
  if (path.startsWith(".git/refs/tags/")) return `tag ${path.slice(".git/refs/tags/".length)}`;
  return path;
}

/** Starting a synced repo: its name, where its copies are, what it keeps the same and leaves out. */
export function NewRepo({ machines, onDone, onCancel }: { machines: string[]; onDone: () => void; onCancel: () => void }) {
  const [name, setName] = React.useState("");
  const [mode, setMode] = React.useState<SyncMode>("files");
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
        mode,
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
      <div>
        <SelectField label="Keeps the same" value={mode} onChange={(v) => setMode(v as SyncMode)} options={MODES} />
        <div className="field-hint">
          {mode === "files" ? "Uncommitted work too, both ways, within seconds." : mode === "git" ? "Each machine's working tree stays its own. For a git repository." : "Uncommitted work too, and the branch checked out. For a git repository."}
        </div>
      </div>
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

/** What a copy says of itself, in words: what it has to take and send, conflicts, what it left out. */
export function copyDetails(copy: SyncCopy): string[] {
  const s = copy.status;
  if (!s) return [];
  return [
    s.adopting && "taking the others' files",
    s.behind > 0 && `${s.behind} to take`,
    s.unsent > 0 && `${s.unsent} to send`,
    s.waiting > 0 && `fetching ${s.waiting}`,
    s.conflicts > 0 && `${s.conflicts} conflict${s.conflicts === 1 ? "" : "s"}`,
    s.skipped.length > 0 && `${s.skipped.length} left out`,
  ].filter((x): x is string => Boolean(x));
}

/**
 * A synced repo: how many files, what it leaves out, its conflicts, adding a copy and stopping it.
 * `copies`: list its copies here too (the Repositories page lists them among the places instead).
 */
export function RepoBlock({ repo, machines, onChange, title, copies = true }: { repo: SyncRepo; machines: string[]; onChange: () => void; title?: string; copies?: boolean }) {
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
          <h2>{title ?? repo.name}</h2>
          <p className="sync-meta">
            {modeWords(repo.mode)} · {repo.mode !== "git" && `${repo.files.toLocaleString()} files · `}
            {summary(repo) || "no copies"}
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
        <Confirm
          label={`Stop syncing ${repo.name}`}
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" onClick={() => void act(() => api.syncDelete(repo.id))}>
                Stop syncing
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setStopping(false)}>
                Keep syncing
              </Button>
            </>
          }
        >
          <p>Stop syncing {repo.name}? Every copy keeps its files where they are.</p>
        </Confirm>
      )}
      {copies && (
        <ul className="sync-copies-list">
          {repo.copies.map((c) => (
            <CopyRow key={c.member} copy={c} onRemove={() => act(() => api.syncRemoveCopy(repo.id, c.member))} />
          ))}
        </ul>
      )}
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
  const details = copyDetails(copy);
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
        <Confirm
          label={`Remove the copy on ${copy.machine}`}
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" onClick={() => void onRemove().then((ok) => ok || setRemoving(false))}>
                Remove copy
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setRemoving(false)}>
                Keep it
              </Button>
            </>
          }
        >
          <p>
            Stop syncing the copy on {copy.machine}? Its files stay in <code>{copy.path}</code>.
          </p>
        </Confirm>
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
      <code className="sync-path">{pathWords(conflict.beside)}</code>
      <code className="sync-path sync-beside">{pathWords(conflict.path)}</code>
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
