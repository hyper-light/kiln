/**
 * Where a repository is: each folder on each machine (a synced copy, kiln's clone, or one that was
 * there), what git says of it and how it syncs, with moving it to another machine and giving it up
 * done in place under it; and the tasks under way or done lately that put it there.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { ArrowRightLeft, RotateCcw, SquareTerminal, Trash2, X } from "lucide-react";
import { api } from "@/api/client";
import type { Place, PlaceMachine, PlaceTask, SyncRepo } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Confirm, SelectField, TextField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Tip } from "@/components/ui/tooltip";
import { shortSha } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { RelTime } from "../shared";
import { copyDetails, DOT } from "../sync";
import { type Entry, defaultPath, isActive, osLabel, resolvedOn, tildeOf } from "./model";

const KIND: Record<Place["kind"], string> = { copy: "synced", clone: "kiln's clone", found: "found here" };

/** A place's state as a status dot shows it. */
function placeDot(p: Place) {
  if (p.copy) return DOT[p.copy.state];
  return p.online ? "succeeded" : "skipped";
}

/** What git says of a folder, in a few words. */
function gitWords(p: Place): string[] {
  const g = p.git;
  if (!g) return [];
  const at = g.head ? `${g.branch ?? "detached"} @ ${shortSha(g.head)}` : `${g.branch ?? "no branch"}, no commits yet`;
  return [
    at,
    g.changed > 0 && `${g.changed} changed`,
    g.ahead > 0 && `${g.ahead} ahead`,
    g.behind > 0 && `${g.behind} behind`,
    !g.upstream && g.head && "no upstream",
  ].filter((x): x is string => Boolean(x));
}

export function PlaceList({
  entry,
  machines,
  synced,
  onStarted,
}: {
  entry: Entry;
  machines: Map<string, PlaceMachine>;
  /** The synced repo, for each copy's own state. */
  synced: SyncRepo | null;
  onStarted: (tasks: PlaceTask[]) => void;
}) {
  const [open, setOpen] = React.useState<{ at: string; what: "move" | "remove" } | null>(null);
  if (entry.places.length === 0) {
    return <p className="empty-line">It is on no machine yet: clone it to some, or sync it, from above.</p>;
  }
  return (
    <ul className="repo-places" aria-label={`Where ${entry.name} is`}>
      {entry.places.map((p) => {
        const at = `${p.machine}\n${p.path}`;
        const m = machines.get(p.machine);
        const copy = p.copy && synced ? synced.copies.find((c) => c.member === p.copy!.member) : undefined;
        const words = [...gitWords(p), ...(copy ? copyDetails(copy) : [])];
        const what = open?.at === at ? open.what : null;
        const busy = entry.tasks.some((t) => isActive(t) && ((t.machine === p.machine && t.path === p.path) || (t.from.kind === "machine" && t.from.machine === p.machine && t.from.path === p.path)));
        return (
          <li key={at} className="repo-place">
            <div className="repo-place-line">
              <StatusDot status={placeDot(p)} title={p.copy ? p.copy.state : p.online ? "online" : "offline"} />
              <span className="repo-place-machine">{p.machine}</span>
              {m && (
                <span className="repo-place-os">
                  {osLabel(m.os)} · {m.arch}
                </span>
              )}
              <span className="chip">{p.copy?.moving ? "moving" : KIND[p.kind]}</span>
              {p.copy && <span className="repo-place-state">{p.copy.state}</span>}
              {!p.online && <span className="repo-place-state">offline</span>}
              <span className="repo-place-acts">
                <Tip content={p.online ? "Open a terminal there" : "Offline"}>
                  {p.online ? (
                    <Link className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Open a terminal on ${p.machine}`} to={`/terminal?open=${encodeURIComponent(`agent:${p.machine}`)}`}>
                      <SquareTerminal size={14} />
                    </Link>
                  ) : (
                    <span className="icon-btn" data-variant="bare" data-size="sm" aria-disabled="true">
                      <SquareTerminal size={14} />
                    </span>
                  )}
                </Tip>
                <Tip content="Move it to another machine or folder">
                  <button
                    type="button"
                    className="icon-btn"
                    data-variant="bare"
                    data-size="sm"
                    aria-label={`Move ${entry.name} from ${p.machine}`}
                    aria-expanded={what === "move"}
                    disabled={busy || !p.online || p.copy?.moving}
                    onClick={() => setOpen(what === "move" ? null : { at, what: "move" })}
                  >
                    <ArrowRightLeft size={14} />
                  </button>
                </Tip>
                <Tip content={p.kind === "copy" ? "Stop syncing it here" : p.kind === "clone" ? "Forget or delete it" : "Delete it"}>
                  <button
                    type="button"
                    className="icon-btn"
                    data-variant="bare"
                    data-size="sm"
                    aria-label={`Give up ${entry.name} on ${p.machine}`}
                    aria-expanded={what === "remove"}
                    disabled={busy || p.copy?.moving}
                    onClick={() => setOpen(what === "remove" ? null : { at, what: "remove" })}
                  >
                    <Trash2 size={14} />
                  </button>
                </Tip>
              </span>
            </div>
            <code className="repo-place-path" title={p.path}>
              {tildeOf(p.path, m?.home) ?? p.path}
            </code>
            {(words.length > 0 || p.git?.active) && (
              <p className="repo-place-meta">
                {words.join(" · ")}
                {p.git?.active ? (
                  <>
                    {words.length > 0 ? " · " : ""}active <RelTime ts={p.git.active} />
                  </>
                ) : null}
              </p>
            )}
            {copy?.status?.error && <p className="repo-error">{copy.status.error}</p>}
            {what === "move" && <MoveForm entry={entry} place={p} machines={machines} onCancel={() => setOpen(null)} onStarted={(t) => {
                  setOpen(null);
                  onStarted(t);
                }} />}
            {what === "remove" && <RemoveForm entry={entry} place={p} onCancel={() => setOpen(null)} onStarted={(t) => {
                  setOpen(null);
                  onStarted(t);
                }} />}
          </li>
        );
      })}
    </ul>
  );
}

/** Moving a place to another machine (or folder): it syncs there, then leaves here. */
function MoveForm({ entry, place, machines, onCancel, onStarted }: { entry: Entry; place: Place; machines: Map<string, PlaceMachine>; onCancel: () => void; onStarted: (t: PlaceTask[]) => void }) {
  const git = place.git !== null;
  const has = (m: string) => entry.places.some((p) => p.machine === m);
  // Machines it isn't on yet first, then the ones it is (another folder there), its own last.
  const rank = (m: PlaceMachine) => (m.name === place.machine ? 2 : has(m.name) ? 1 : 0);
  const eligible = [...machines.values()].filter((m) => m.connected && m.can_sync && (!git || m.can_clone)).sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
  const [to, setTo] = React.useState(eligible[0]?.name ?? "");
  const [path, setPath] = React.useState(() => defaultPath(entry, machines, place));
  const [del, setDel] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const resolved = resolvedOn(path, machines.get(to));
  const changed = place.git?.changed ?? 0;
  const ahead = place.git?.ahead ?? 0;

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      onStarted(await api.placesStart("move", { key: entry.key, from: { kind: "machine", machine: place.machine, path: place.path }, to: [{ machine: to, path: path.trim() }], delete: del }));
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  if (eligible.length === 0) {
    return (
      <Confirm
        label={`Move ${entry.name} from ${place.machine}`}
        className="repo-inline"
        actions={
          <Button size="xs" variant="ghost" onClick={onCancel}>
            Close
          </Button>
        }
      >
        <p>No connected machine can take it: each needs a runner that syncs{git ? " and clones" : ""}.</p>
      </Confirm>
    );
  }
  return (
    <form className="confirm repo-inline repo-move" onSubmit={submit} aria-label={`Move ${entry.name} from ${place.machine}`}>
      <p>
        {git ? "Its commits go as a git bundle and its working tree through sync, uncommitted work included" : "Its files go through sync"}, over kiln's own links. Once the new folder is in step, the one on {place.machine}{" "}
        {entry.synced && place.copy ? "stops syncing" : "is let go"}.
      </p>
      <div className="repo-move-fields">
        <SelectField
          label="To"
          value={to}
          onChange={setTo}
          options={eligible.map((m) => ({ value: m.name, label: `${m.name} · ${osLabel(m.os)} ${m.arch}${m.name === place.machine ? " · another folder here" : has(m.name) ? " · has it already" : ""}` }))}
        />
        <TextField label="Folder" value={path} onChange={(e) => setPath(e.target.value)} spellCheck={false} hint={resolved ? `→ ${resolved}` : "A whole path, or ~/… on that machine"} />
      </div>
      <label className="repo-check">
        <input type="checkbox" checked={del} onChange={(e) => setDel(e.target.checked)} />
        <span>
          Delete the old folder on {place.machine} once it has moved
          {(changed > 0 || ahead > 0) && (
            <em>
              {" "}
              (it has {[changed > 0 && `${changed} changes not committed`, ahead > 0 && `${ahead} commits not pushed`].filter(Boolean).join(" and ")}: they move too)
            </em>
          )}
        </span>
      </label>
      {error && <p className="repo-error">{error}</p>}
      <div className="repo-actions">
        <Button type="submit" size="xs" disabled={busy || !to || !path.trim()}>
          {busy ? "Starting…" : `Move to ${to}`}
        </Button>
        <Button type="button" size="xs" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

/** Giving a place up: a copy stops syncing there, kiln forgets its clone, and the folder goes if asked. */
function RemoveForm({ entry, place, onCancel, onStarted }: { entry: Entry; place: Place; onCancel: () => void; onStarted: (t: PlaceTask[]) => void }) {
  const found = place.kind === "found";
  const [del, setDel] = React.useState(found);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const changed = place.git?.changed ?? 0;
  const ahead = place.git?.ahead ?? 0;
  const go = async () => {
    setBusy(true);
    setError(null);
    try {
      onStarted(await api.placesStart("remove", { key: entry.key, from: { kind: "machine", machine: place.machine, path: place.path }, delete: del }));
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };
  const keep = place.kind === "copy" ? "Stop syncing it here" : "Forget kiln's clone";
  return (
    <Confirm
      label={`Give up ${entry.name} on ${place.machine}`}
      tone="danger"
      className="repo-inline"
      actions={
        <>
          <Button size="xs" variant="danger" disabled={busy} onClick={() => void go()}>
            {busy ? "Starting…" : del ? `Delete it on ${place.machine}` : keep}
          </Button>
          <Button size="xs" variant="ghost" onClick={onCancel}>
            Keep it
          </Button>
        </>
      }
    >
      <p>
        {found
          ? `kiln didn't make this folder; it can delete it on ${place.machine}.`
          : place.kind === "copy"
            ? `${place.machine} stops syncing ${entry.name}. Its files stay unless you delete them.`
            : `kiln stops keeping its clone on ${place.machine}. Its files stay unless you delete them.`}
        {(changed > 0 || ahead > 0) && (
          <strong className="repo-warn">
            {" "}
            It has {[changed > 0 && `${changed} changes not committed`, ahead > 0 && `${ahead} commits not pushed`].filter(Boolean).join(" and ")}.
          </strong>
        )}
      </p>
      {!found && (
        <label className="repo-check">
          <input type="checkbox" checked={del} onChange={(e) => setDel(e.target.checked)} />
          <span>Delete the folder too</span>
        </label>
      )}
      {error && <p className="repo-error">{error}</p>}
    </Confirm>
  );
}

const VERB: Record<PlaceTask["kind"], string> = { clone: "Clone", sync: "Sync", move: "Move", remove: "Remove" };

/** A task's words: what, and where to (or from). */
export function taskLine(t: PlaceTask): string {
  return `${VERB[t.kind]} ${t.kind === "remove" ? "from" : "to"} ${t.machine}`;
}

const TASK_DOT: Record<string, string> = { done: "succeeded", failed: "failed", queued: "queued" };

/** The tasks under way and done lately for a repository, newest first. */
export function TaskList({ tasks, machines, onChange }: { tasks: PlaceTask[]; machines: Map<string, PlaceMachine>; onChange: () => void }) {
  const [error, setError] = React.useState<string | null>(null);
  const shown = [...tasks.filter(isActive), ...tasks.filter((t) => !isActive(t)).slice(0, 4)];
  if (shown.length === 0) return null;
  const act = async (f: () => Promise<unknown>) => {
    setError(null);
    try {
      await f();
      onChange();
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  return (
    <div className="repo-tasks">
      <h4 className="field-label">Tasks</h4>
      <ul>
        {shown.map((t) => (
          <li key={t.id} className="repo-task">
            <div className="repo-task-line">
              <StatusDot status={TASK_DOT[t.state] ?? "running"} title={t.state} />
              <span className="repo-task-what">{taskLine(t)}</span>
              <code className="repo-task-path">{tildeOf(t.path, machines.get(t.machine)?.home) ?? t.path}</code>
              <span className="repo-task-state">{t.state}</span>
              <span className="repo-task-when">
                <RelTime ts={t.updated} />
              </span>
              <span className="repo-place-acts">
                {t.state === "failed" && (
                  <Tip content="Start it again">
                    <button type="button" className="icon-btn" data-variant="bare" data-size="sm" aria-label="Start it again" onClick={() => void act(() => api.placesRetry(t.id))}>
                      <RotateCcw size={14} />
                    </button>
                  </Tip>
                )}
                {!isActive(t) && (
                  <Tip content="Dismiss">
                    <button type="button" className="icon-btn" data-variant="bare" data-size="sm" aria-label="Dismiss" onClick={() => void act(() => api.placesDismiss(t.id))}>
                      <X size={14} />
                    </button>
                  </Tip>
                )}
              </span>
            </div>
            {t.state !== "done" && !t.error && t.detail && <p className="repo-place-meta">{t.detail}</p>}
            {t.state === "done" && t.detail && <p className="repo-place-meta">{t.detail}</p>}
            {t.error && <p className="repo-error">{t.error}</p>}
            {t.notes.length > 1 && (
              <details className="repo-notes">
                <summary>What happened</summary>
                <ol>
                  {t.notes.map((n, i) => (
                    <li key={i}>
                      <span className="repo-task-when">
                        <RelTime ts={n.at} />
                      </span>{" "}
                      {n.text}
                    </li>
                  ))}
                </ol>
              </details>
            )}
          </li>
        ))}
      </ul>
      {error && <p className="repo-error">{error}</p>}
    </div>
  );
}
