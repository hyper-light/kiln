/**
 * Putting a repository on machines, in place under it: cloned with git (commits only, from kiln's
 * mirror, a machine that has it, or its remote), or kept in sync (each new folder cloned, then kept
 * the same as the others, uncommitted work included). Any machines at once, picked one by one or
 * by OS, architecture or label, each with a folder of its own. Nothing goes but over kiln's links.
 */
import * as React from "react";
import { api } from "@/api/client";
import type { Place, PlaceMachine, PlaceTask, TaskSource } from "@/api/types";
import { Button } from "@/components/ui/button";
import { SelectField, TextField } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { errorMessage } from "@/lib/hooks";
import { type Entry, defaultPath, osLabel, resolvedOn, tildeOf } from "./model";

export type PutMode = "clone" | "sync";

interface Source {
  id: string;
  label: string;
  from: TaskSource;
  /** The place it is, for a machine's folder. */
  place?: Place;
  /** Its commits come with git (else: a plain folder, for sync alone). */
  git: boolean;
}

/**
 * Where it can come from, for each way of putting it on machines, the likeliest first: for a clone,
 * kiln's mirror (a clean checkout of its remote); for a sync, a folder in step (its working tree).
 */
function sourcesOf(e: Entry, machines: Map<string, PlaceMachine>, mode: PutMode): Source[] {
  const out: Source[] = [];
  const kiln: Source | null = e.built ? { id: "kiln", label: "kiln's mirror of it, a clean checkout", from: { kind: "kiln", repo: e.built.id }, git: true } : null;
  if (kiln && mode === "clone") out.push(kiln);
  const synced = e.synced !== null;
  for (const p of e.places) {
    const m = machines.get(p.machine);
    if (!p.online || !m) continue;
    // A synced repository's new folders take its working tree: they come from one of its copies in step.
    if (mode === "sync" && synced && !(p.copy && !p.copy.moving && p.copy.state === "in step")) continue;
    if (!p.git && (mode === "clone" || !p.copy)) continue;
    if (p.git && !m.can_clone) continue;
    const where = tildeOf(p.path, m.home) ?? p.path;
    const g = p.git;
    const about = g ? [g.branch ?? "detached", g.changed > 0 && `${g.changed} changed`].filter(Boolean).join(", ") : "a folder";
    out.push({ id: `place\n${p.machine}\n${p.path}`, label: `${p.machine} · ${where} (${about})`, from: { kind: "machine", machine: p.machine, path: p.path }, place: p, git: Boolean(g) });
  }
  if (kiln && mode === "sync") out.push(kiln);
  if (e.origin && /^(https?|ssh|git):\/\/|^[^/\s]+@[^:\s]+:/.test(e.origin)) {
    out.push({ id: "url", label: `its remote, each machine fetching it itself (${e.origin})`, from: { kind: "url", url: e.origin }, git: true });
  }
  return out;
}

interface Pick {
  on: boolean;
  path: string;
  /** Typed by hand: a new source no longer changes it. */
  typed: boolean;
}

export function PutForm({ entry, machines, mode: initial, onStarted, onCancel }: { entry: Entry; machines: PlaceMachine[]; mode: PutMode; onStarted: (tasks: PlaceTask[]) => void; onCancel: () => void }) {
  const byName = React.useMemo(() => new Map(machines.map((m) => [m.name, m])), [machines]);
  const [mode, setMode] = React.useState<PutMode>(initial);
  const sources = React.useMemo(() => sourcesOf(entry, byName, mode), [entry, byName, mode]);
  const [sourceId, setSourceId] = React.useState<string>("");
  const source = sources.find((s) => s.id === sourceId) ?? sources[0];
  const [branch, setBranch] = React.useState("");
  const [picks, setPicks] = React.useState<Record<string, Pick>>({});
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);

  const fits = (m: PlaceMachine) => m.connected && (mode === "clone" ? m.can_clone : m.can_sync && (!source?.git || m.can_clone));
  const why = (m: PlaceMachine) =>
    !m.connected ? "offline" : mode === "clone" && !m.can_clone ? "its runner is too old to clone: upgrade it" : mode === "sync" && !m.can_sync ? "its runner is too old to sync: upgrade it" : source?.git && !m.can_clone ? "its runner is too old to clone: upgrade it" : null;
  const sorted = [...machines].sort((a, b) => Number(b.connected) - Number(a.connected) || a.name.localeCompare(b.name));
  const fallback = defaultPath(entry, byName, source?.place);
  const pathOf = (m: string) => picks[m]?.path ?? fallback;
  const chosen = sorted.filter((m) => picks[m.name]?.on && fits(m));

  const toggle = (m: string, on: boolean) => setPicks((all) => ({ ...all, [m]: { on, path: all[m]?.path ?? fallback, typed: all[m]?.typed ?? false } }));
  const pickAll = (want: (m: PlaceMachine) => boolean) =>
    setPicks((all) => {
      const next = { ...all };
      for (const m of sorted) if (fits(m) && want(m)) next[m.name] = { on: true, path: all[m.name]?.path ?? fallback, typed: all[m.name]?.typed ?? false };
      return next;
    });
  // A new source moves the folders not typed by hand to where it suggests.
  const chooseSource = (id: string) => {
    setSourceId(id);
    const s = sources.find((x) => x.id === id);
    const path = defaultPath(entry, byName, s?.place);
    setPicks((all) => Object.fromEntries(Object.entries(all).map(([m, p]) => [m, p.typed ? p : { ...p, path }])));
  };

  // Quick picks: every machine of an OS, an architecture or a label.
  const ok = sorted.filter(fits);
  const count = (key: (m: PlaceMachine) => string[]) => {
    const c = new Map<string, number>();
    for (const m of ok) for (const v of key(m)) c.set(v, (c.get(v) ?? 0) + 1);
    return [...c.entries()];
  };
  const groups = [
    ...count((m) => [m.os]).map(([v, c]) => ({ id: `os:${v}`, label: osLabel(v), count: c, want: (m: PlaceMachine) => m.os === v })),
    ...count((m) => [m.arch]).map(([v, c]) => ({ id: `arch:${v}`, label: v, count: c, want: (m: PlaceMachine) => m.arch === v })),
    // Labels that only say a machine's OS or architecture again are left out.
    ...count((m) => m.labels.filter((l) => ![m.os, osLabel(m.os), m.arch, m.arch === "aarch64" ? "arm64" : m.arch, m.arch === "x86_64" ? "amd64" : m.arch].some((x) => x.toLowerCase() === l.toLowerCase()))).map(([v, c]) => ({
      id: `label:${v}`,
      label: v,
      count: c,
      want: (m: PlaceMachine) => m.labels.includes(v),
    })),
  ];

  const branches = (entry.built?.branches ?? []).filter((b) => !b.includes("*"));
  const fromBranchable = source?.from.kind === "kiln" || source?.from.kind === "url";

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!source) return;
    setBusy(true);
    setError(null);
    try {
      const tasks = await api.placesStart(mode, {
        key: entry.key,
        from: source.from,
        branch: fromBranchable && branch.trim() ? branch.trim() : undefined,
        to: chosen.map((m) => ({ machine: m.name, path: pathOf(m.name).trim() })),
      });
      onStarted(tasks);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const n = chosen.length;
  const verb = mode === "clone" ? "Clone" : "Sync";
  return (
    <form className="repo-put" onSubmit={submit} aria-label={`${verb} ${entry.name} to machines`}>
      <div className="repo-put-mode" role="radiogroup" aria-label="How">
        {(["clone", "sync"] as const).map((m) => (
          <button key={m} type="button" role="radio" aria-checked={mode === m} className={cn("repo-seg", mode === m && "repo-seg-on")} onClick={() => setMode(m)}>
            {m === "clone" ? "Clone with git" : "Keep in sync"}
          </button>
        ))}
      </div>
      <p className="repo-put-lead">
        {mode === "clone"
          ? "Each machine gets a clone of its own: the commits and branches, from where you choose. Work not committed stays where it is."
          : entry.synced
            ? "Each new folder is cloned, then kept the same as the copies it syncs with already: uncommitted work too, both ways, within seconds."
            : "The first folder starts a sync; each new one is cloned, then kept the same as it: uncommitted work too, both ways, within seconds."}
      </p>
      {sources.length === 0 ? (
        <p className="repo-error">
          {mode === "sync" && entry.synced ? "None of its synced copies is in step and online to start from." : "Nowhere to take it from: no machine that has it is online, and kiln doesn't build it."}
        </p>
      ) : (
        <div className="repo-put-from">
          <SelectField label="From" value={source?.id ?? ""} onChange={chooseSource} options={sources.map((s) => ({ value: s.id, label: s.label }))} />
          {fromBranchable && (
            <div>
              <TextField label="Branch" value={branch} onChange={(e) => setBranch(e.target.value)} placeholder={branches[0] ?? "the default branch"} list="repo-put-branches" spellCheck={false} />
              <datalist id="repo-put-branches">
                {branches.map((b) => (
                  <option key={b} value={b} />
                ))}
              </datalist>
            </div>
          )}
        </div>
      )}
      <fieldset className="repo-put-machines">
        <legend className="field-label">To</legend>
        <div className="repo-put-quick">
          <button type="button" className="chip" onClick={() => pickAll(() => true)}>
            all that can
          </button>
          <button type="button" className="chip" onClick={() => setPicks((all) => Object.fromEntries(Object.entries(all).map(([m, p]) => [m, { ...p, on: false }])))}>
            none
          </button>
          {groups.map((g) => (
            <button key={g.id} type="button" className="chip" onClick={() => pickAll(g.want)} title={`Every ${g.label} machine`}>
              {g.label} · {g.count}
            </button>
          ))}
        </div>
        <ul>
          {sorted.map((m) => {
            const can = fits(m);
            const on = Boolean(picks[m.name]?.on) && can;
            const has = entry.places.filter((p) => p.machine === m.name);
            const path = pathOf(m.name);
            const resolved = resolvedOn(path, m);
            return (
              <li key={m.name} className={cn("repo-put-machine", !can && "repo-put-off")}>
                <label className="repo-check">
                  <input type="checkbox" checked={on} disabled={!can} onChange={(e) => toggle(m.name, e.target.checked)} />
                  <span className="repo-place-machine">{m.name}</span>
                  <span className="repo-place-os">
                    {osLabel(m.os)} · {m.arch}
                  </span>
                  {why(m) && <span className="repo-place-state">{why(m)}</span>}
                  {has.length > 0 && <span className="repo-place-state">has it at {has.map((p) => tildeOf(p.path, m.home) ?? p.path).join(", ")}</span>}
                </label>
                {on && (
                  <TextField
                    label={`Folder on ${m.name}`}
                    value={path}
                    onChange={(e) => {
                      const v = e.target.value;
                      setPicks((all) => ({ ...all, [m.name]: { on: true, path: v, typed: true } }));
                    }}
                    spellCheck={false}
                    hint={resolved ? `→ ${resolved}` : path.trim().startsWith("~") && !m.home ? "it hasn't said where its home is yet: give a whole path" : undefined}
                  />
                )}
              </li>
            );
          })}
        </ul>
      </fieldset>
      {error && <p className="repo-error">{error}</p>}
      <div className="repo-actions">
        <Button type="submit" size="sm" disabled={busy || !source || n === 0 || chosen.some((m) => !pathOf(m.name).trim())}>
          {busy ? "Starting…" : n === 0 ? `${verb} to machines` : `${verb} to ${n === 1 ? chosen[0]!.name : `${n} machines`}`}
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}
