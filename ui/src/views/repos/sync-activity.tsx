/**
 * Sync, as the Repositories page shows it in a section that opens and closes (docs/sync.md,
 * "Watching it"): the folders kiln keeps the same on several machines, each copy's state (how far
 * behind, what it waits to send or fetch, when it was last heard from, how long changes take to
 * reach it, what moved and what failed), and what happened, newest first: each change one machine
 * made and the copies that have it, conflicts, copies going offline and back, errors, steps let go,
 * things tried again, and agent sessions moving with their files. Closed, one line says how it is.
 */
import * as React from "react";
import { ChevronDown } from "lucide-react";
import { api } from "@/api/client";
import type { SyncCopy, SyncEvent, SyncFileEvent, SyncRepo, SyncTone, SyncView } from "@/api/types";
import { SelectField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { clockTime, formatBytes, formatNumber, relativeTime } from "@/lib/format";
import { useLocalStorage, useNow } from "@/lib/hooks";
import { useSearchParams } from "@/lib/router";
import { DOT } from "../sync";
import "./sync-activity.css";

/** Events kept here at the most, and shown before "more". */
const KEPT = 500;
const SHOWN = 60;
/** How often what happened is asked again while the section is open. */
const POLL_MS = 2_500;
/** A step's files listed before the rest fold away. */
const FILES_OPEN = 4;

type Show = "all" | "changes" | "trouble" | "moves";
const SHOWS: { value: Show; label: string }[] = [
  { value: "all", label: "Everything" },
  { value: "changes", label: "File changes" },
  { value: "trouble", label: "Problems" },
  { value: "moves", label: "Agent moves" },
];

interface Seen {
  /** Newest first. */
  events: SyncEvent[];
  error: string | null;
}

/**
 * What happened, read every few seconds while something shows it (and the tab is shown): only
 * what's new each time, merged in, newest first. Kept between openings, so the section opens on
 * what it had.
 */
class ActivityStore {
  private seen: Seen = { events: [], error: null };
  private next = 0;
  private readonly fns = new Set<() => void>();
  private timer = 0;
  private ctl: AbortController | null = null;

  subscribe = (fn: () => void) => {
    this.fns.add(fn);
    if (this.fns.size === 1) void this.read();
    return () => {
      this.fns.delete(fn);
      if (this.fns.size === 0) this.stop();
    };
  };

  get = () => this.seen;

  private stop() {
    window.clearTimeout(this.timer);
    this.ctl?.abort();
    this.ctl = null;
  }

  private set(seen: Seen) {
    this.seen = seen;
    for (const fn of this.fns) fn();
  }

  private async read() {
    this.stop();
    if (document.hidden) {
      this.timer = window.setTimeout(() => void this.read(), POLL_MS);
      return;
    }
    const ctl = new AbortController();
    this.ctl = ctl;
    try {
      const got = await api.syncActivity(this.next, 200, ctl.signal);
      if (ctl.signal.aborted) return;
      // Numbered from the start again (another server's data): what's here is another history.
      const fresh = got.next < this.next;
      this.next = got.next;
      if (got.events.length > 0 || fresh || this.seen.error) {
        const have = new Set(got.events.map((e) => e.seq));
        const kept = fresh ? [] : this.seen.events.filter((e) => !have.has(e.seq));
        this.set({ events: [...got.events, ...kept].sort((a, b) => b.seq - a.seq).slice(0, KEPT), error: null });
      }
    } catch (e) {
      if (ctl.signal.aborted) return;
      this.set({ ...this.seen, error: e instanceof Error ? e.message : String(e) });
    }
    if (this.fns.size > 0) this.timer = window.setTimeout(() => void this.read(), POLL_MS);
  }
}

const activity = new ActivityStore();

function useActivity(): Seen {
  return React.useSyncExternalStore(activity.subscribe, activity.get, activity.get);
}

/** Each copy's state, counted, worst first, as one line says it. */
function tally(repos: SyncRepo[]): { line: string; dot: string } {
  const by = new Map<string, number>();
  for (const r of repos) for (const c of r.copies) by.set(c.state, (by.get(c.state) ?? 0) + 1);
  const order: SyncCopy["state"][] = ["error", "offline", "catching up", "fetching", "sending", "starting", "in step"];
  const parts = order.filter((s) => by.has(s)).map((s) => `${by.get(s)} ${s}`);
  const worst = order.find((s) => by.has(s));
  const conflicts = repos.reduce((n, r) => n + r.conflicts.length, 0);
  if (conflicts > 0) parts.push(`${conflicts} conflict${conflicts === 1 ? "" : "s"}`);
  return { line: parts.join(" · "), dot: worst ? DOT[worst] : "skipped" };
}

/** Milliseconds, as a person reads them. */
function took(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return "—";
  if (ms < 1_000) return `${Math.max(0, Math.round(ms))} ms`;
  if (ms < 60_000) return `${(ms / 1_000).toFixed(ms < 10_000 ? 1 : 0)} s`;
  return `${Math.floor(ms / 60_000)} min ${Math.round((ms % 60_000) / 1_000)} s`;
}

function seconds(ms: number | null | undefined): number | null {
  return ms ? Math.floor(ms / 1000) : null;
}

/** How long ago `ms` (unix ms) was, as a person says it. */
function ago(ms: number | null | undefined, now: number): string {
  const r = relativeTime(seconds(ms), now);
  return r === "now" ? "just now" : r;
}

const TROUBLE: SyncEvent["kind"][] = ["error", "dropped", "retry", "state"];

function shown(e: SyncEvent, show: Show, machine: string): boolean {
  if (machine && e.machine !== machine) return false;
  switch (show) {
    case "all":
      return true;
    case "changes":
      return e.kind === "step" || e.kind === "resolved";
    case "trouble":
      return TROUBLE.includes(e.kind) || (e.kind === "step" && (e.step?.conflicts ?? 0) > 0) || e.tone === "fail" || e.tone === "warn";
    case "moves":
      return e.kind === "move";
  }
}

/**
 * The section: closed, one line; open, the copies and what happened. `?activity=<repo id>` opens
 * it on that repo (a synced repository's details link here).
 */
export function SyncActivity({ sync }: { sync: SyncView }) {
  const [stored, setStored] = useLocalStorage<boolean>("kiln.repos.sync.open", false);
  const [params, setParams] = useSearchParams();
  const focus = params.get("activity");
  const open = stored || focus !== null;
  const setOpen = (v: boolean) => {
    if (!v && focus !== null) {
      const n = new URLSearchParams(params);
      n.delete("activity");
      setParams(n, { replace: true });
    }
    setStored(v);
  };
  // Those synced only while a repository moves between machines too: named "(moving)", they go
  // once it has.
  const repos = sync.repos;
  const { line, dot } = tally(repos);
  const copies = repos.reduce((n, r) => n + r.copies.length, 0);
  const last = Math.max(0, ...repos.map((r) => r.stats?.last_step ?? 0)) || null;
  const now = useNow();
  const id = React.useId();
  return (
    <section id="sync-activity" className="sact" aria-label="Sync" data-open={open || undefined}>
      <button type="button" className="sact-toggle" aria-expanded={open} aria-controls={id} onClick={() => setOpen(!open)}>
        <span className="eyebrow sact-eyebrow">
          <span className="spectrum-rule" aria-hidden="true" />
          Sync
        </span>
        {repos.length === 0 ? (
          <span className="sact-line">No folder is kept in sync yet</span>
        ) : (
          <span className="sact-line">
            <StatusDot status={dot} />
            <span>
              {repos.length} {repos.length === 1 ? "repo" : "repos"} · {copies} {copies === 1 ? "copy" : "copies"}
              {line && ` · ${line}`}
            </span>
            {last ? <span className="sact-when">last change {ago(last, now)}</span> : null}
          </span>
        )}
        <ChevronDown className="sact-chev" aria-hidden="true" />
      </button>
      {open && (
        <div id={id} className="sact-body">
          {repos.length === 0 ? (
            <p className="sact-empty">
              Keep a folder the same on several machines with “Sync a folder” above, or “Sync to machines” under a repository. Every change any of them makes shows here, with the copies that have it.
            </p>
          ) : (
            <Opened key={focus ?? ""} repos={repos} focus={focus ?? ""} />
          )}
        </div>
      )}
    </section>
  );
}

function Opened({ repos, focus }: { repos: SyncRepo[]; focus: string }) {
  const { events, error } = useActivity();
  const [repo, setRepo] = React.useState(focus);
  const [machine, setMachine] = React.useState("");
  const [show, setShow] = React.useState<Show>("all");
  const [more, setMore] = React.useState(0);
  const machines = React.useMemo(() => [...new Set(repos.flatMap((r) => r.copies.map((c) => c.machine)))].sort(), [repos]);
  const byId = React.useMemo(() => new Map(repos.map((r) => [r.id, r])), [repos]);
  const picked = repo ? repos.filter((r) => r.id === repo) : repos;
  const list = events.filter((e) => byId.has(e.repo) && (!repo || e.repo === repo) && shown(e, show, machine));
  const visible = list.slice(0, SHOWN + more);
  return (
    <>
      <div className="sact-tools">
        <SelectField className="sact-select" label="Repository" hideLabel value={repo} onChange={setRepo} options={[{ value: "", label: "Every synced repo" }, ...repos.map((r) => ({ value: r.id, label: r.name }))]} />
        <SelectField className="sact-select" label="Machine" hideLabel value={machine} onChange={setMachine} options={[{ value: "", label: "Every machine" }, ...machines.map((m) => ({ value: m, label: m }))]} />
        <SelectField className="sact-select" label="Show" hideLabel value={show} onChange={(v) => setShow(v as Show)} options={SHOWS} />
      </div>
      {picked.map((r) => (
        <RepoCopies key={r.id} repo={r} events={events.filter((e) => e.repo === r.id)} machine={machine} />
      ))}
      <h3 className="field-label sact-feed-title">What happened</h3>
      {error && <p className="notice" data-tone="fail">Reading what happened failed: {error}</p>}
      {visible.length === 0 ? (
        <p className="sact-empty">{events.length === 0 ? "Nothing yet: every change a copy makes shows here as its machine sends it." : "Nothing matches."}</p>
      ) : (
        <ol className="sact-feed">
          {visible.map((e) => (
            <EventRow key={e.seq} event={e} repo={byId.get(e.repo)} named={!repo && repos.length > 1} />
          ))}
        </ol>
      )}
      {list.length > visible.length && (
        <button type="button" className="sact-more" onClick={() => setMore((m) => m + SHOWN)}>
          {Math.min(SHOWN, list.length - visible.length)} more of {formatNumber(list.length - visible.length)}
        </button>
      )}
    </>
  );
}

/** Changes per minute over the last half hour, from what happened. */
function perMinute(events: SyncEvent[], now: number): number[] {
  const out = new Array<number>(30).fill(0);
  for (const e of events) {
    if (e.kind !== "step" || !e.step) continue;
    const back = Math.floor((now - e.at) / 60_000);
    if (back >= 0 && back < 30) out[29 - back] = (out[29 - back] ?? 0) + e.step.added + e.step.changed + e.step.deleted;
  }
  return out;
}

/** A synced repo: what its log did since the server started, and each copy's state. */
function RepoCopies({ repo, events, machine }: { repo: SyncRepo; events: SyncEvent[]; machine: string }) {
  const now = useNow();
  const s = repo.stats;
  const minutes = perMinute(events, now);
  const peak = Math.max(1, ...minutes);
  const copies = machine ? repo.copies.filter((c) => c.machine === machine) : repo.copies;
  return (
    <div className="sact-repo">
      <div className="sact-repo-head">
        <h3>{repo.name}</h3>
        <span className="sact-meta">
          {formatNumber(repo.files)} files · log #{repo.index}
          {s && (
            <>
              {" "}
              · since kiln started {ago(s.since, now)}: {formatNumber(s.steps)} {s.steps === 1 ? "change" : "changes"} of {formatNumber(s.files)} {s.files === 1 ? "file" : "files"} ({formatBytes(s.bytes)})
              {s.conflicts > 0 && <span className="sact-warn"> · {s.conflicts} conflicting</span>}
              {s.dropped > 0 && <span className="sact-warn"> · {s.dropped} let go</span>}
              {s.retries > 0 && <> · {formatNumber(s.retries)} tried again</>}
              {s.errors > 0 && <span className="sact-fail"> · {s.errors} errors</span>}
            </>
          )}
        </span>
        <span className="sact-spark" title="Files changed each minute, the last 30 minutes" aria-label="Files changed each minute, the last 30 minutes">
          {minutes.map((n, i) => (
            <i key={i} style={{ height: `${n === 0 ? 1 : 2 + Math.round((n / peak) * 14)}px` }} data-on={n > 0 || undefined} />
          ))}
        </span>
      </div>
      {repo.error && <p className="notice" data-tone="fail">{repo.error}</p>}
      <table className="data-table sact-copies">
        <thead>
          <tr>
            <th>Copy</th>
            <th>State</th>
            <th className="num">Behind</th>
            <th className="num hidden sm:table-cell">To send</th>
            <th className="num hidden sm:table-cell">Fetching</th>
            <th className="num hidden md:table-cell">Changes arrive in</th>
            <th className="hidden lg:table-cell">Moved</th>
            <th className="hidden md:table-cell">Failed</th>
            <th className="num">Heard</th>
          </tr>
        </thead>
        <tbody>
          {copies.map((c) => (
            <CopyRow key={c.member} copy={c} now={now} />
          ))}
        </tbody>
      </table>
    </div>
  );
}

function CopyRow({ copy: c, now }: { copy: SyncCopy; now: number }) {
  const st = c.status;
  const t = st?.traffic;
  const lag = c.lag ?? 0;
  return (
    <tr>
      <td className="sact-copy">
        <span className="sact-machine">{c.machine}</span>
        <code className="sact-path" title={c.path}>
          {c.path}
        </code>
      </td>
      <td>
        <span className="sact-state">
          <StatusDot status={DOT[c.state]} />
          {c.state}
        </span>
        {st?.error && <div className="sact-copy-error">{st.error}</div>}
      </td>
      <td className="num cell-mono">{st ? formatNumber(lag) : "—"}</td>
      <td className="num cell-mono hidden sm:table-cell">{st ? formatNumber(st.unsent) : "—"}</td>
      <td className="num cell-mono hidden sm:table-cell">{st ? formatNumber(st.waiting) : "—"}</td>
      <td className="num cell-mono hidden md:table-cell" title="From the look on the machine that made the change to the file here">
        {took(st?.lag_ms)}
      </td>
      <td className="cell-mono hidden lg:table-cell">
        {t ? (
          <span title={`${formatNumber(t.sent)} sent (${formatBytes(t.sent_bytes)}), ${formatNumber(t.fetched)} fetched (${formatBytes(t.fetched_bytes)}) since its runner started`}>
            ↑ {formatNumber(t.sent)} · {formatBytes(t.sent_bytes)} &nbsp;↓ {formatNumber(t.fetched)} · {formatBytes(t.fetched_bytes)}
          </span>
        ) : (
          "—"
        )}
      </td>
      <td className="cell-mono hidden md:table-cell">
        {t && (t.failed > 0 || t.resent > 0) ? (
          <span className={t.failed > 0 ? "sact-warn" : undefined} title={t.last_failure ? `Last: ${t.last_failure} (${relativeTime(t.failed_at, now)})` : undefined}>
            {formatNumber(t.failed)} failed · {formatNumber(t.retried + t.resent)} again
          </span>
        ) : t ? (
          "none"
        ) : (
          "—"
        )}
      </td>
      <td className="num cell-mono">{c.heard ? ago(c.heard, now) : "—"}</td>
    </tr>
  );
}

const TONE_DOT: Record<SyncTone, string> = { info: "queued", ok: "succeeded", warn: "timed_out", fail: "failed" };

/** Something that happened: a change and the copies that have it, or a line of what else. */
function EventRow({ event: e, repo, named }: { event: SyncEvent; repo: SyncRepo | undefined; named: boolean }) {
  const now = useNow();
  return (
    <li className="sact-event" data-kind={e.kind} data-tone={e.tone}>
      <time className="sact-time" dateTime={new Date(e.at).toISOString()} title={new Date(e.at).toLocaleString()}>
        {now - e.at < 3_600_000 ? ago(e.at, now) : clockTime(e.at)}
      </time>
      <div className="sact-what">
        {e.step ? <StepLine event={e} repo={repo} named={named} /> : (
          <p className="sact-text">
            <StatusDot status={TONE_DOT[e.tone]} />
            {named && repo && <span className="sact-repo-name">{repo.name}</span>}
            <span>{e.text}</span>
          </p>
        )}
      </div>
    </li>
  );
}

const SIGN: Record<SyncFileEvent["kind"], string> = { added: "+", changed: "~", deleted: "−" };

function StepLine({ event: e, repo, named }: { event: SyncEvent; repo: SyncRepo | undefined; named: boolean }) {
  const s = e.step;
  if (!s) return null;
  const n = s.added + s.changed + s.deleted;
  // Every copy but the one that made it: which have it, which don't yet.
  const others = (repo?.copies ?? []).filter((c) => c.machine !== e.machine);
  const have = others.filter((c) => (c.status?.applied ?? 0) >= s.index);
  const counts = [s.added && `${s.added} added`, s.changed && `${s.changed} changed`, s.deleted && `${s.deleted} deleted`].filter(Boolean).join(", ");
  const head = s.files.slice(0, FILES_OPEN);
  const rest = s.files.slice(FILES_OPEN);
  const unlisted = n - s.files.length;
  const inLog = s.seen > 0 ? e.at - s.seen : null;
  return (
    <>
      <p className="sact-step">
        {named && repo && <span className="sact-repo-name">{repo.name}</span>}
        <span className="sact-machine">{e.machine}</span>
        <span>
          {n} {n === 1 ? "file" : "files"}
          {counts && <span className="sact-dim"> ({counts})</span>}
          {s.bytes > 0 && <span className="sact-dim"> · {formatBytes(s.bytes)}</span>}
        </span>
        {s.conflicts > 0 && <span className="sact-warn">{s.conflicts} conflict{s.conflicts === 1 ? "" : "s"}</span>}
        <span className="sact-delivery" title={inLog !== null ? `In the log ${took(inLog)} after ${e.machine} saw it (commit ${took(s.commit_ms)})` : undefined}>
          {others.length === 0 ? (
            <span className="sact-dim">no other copy</span>
          ) : (
            others.map((c) => {
              const got = (c.status?.applied ?? 0) >= s.index;
              return (
                <span key={c.member} className="sact-to" data-got={got || undefined} title={got ? `${c.machine} has it` : `${c.machine}: on its way`}>
                  <i aria-hidden="true" />
                  {c.machine}
                </span>
              );
            })
          )}
          {others.length > 0 && <span className="sr-only">{have.length === others.length ? "every copy has it" : `${others.length - have.length} copies don't have it yet`}</span>}
        </span>
      </p>
      <ul className="sact-files">
        {head.map((f) => (
          <FileLine key={f.path} file={f} />
        ))}
      </ul>
      {(rest.length > 0 || unlisted > 0) && (
        <details className="sact-rest">
          <summary>{rest.length + unlisted} more</summary>
          <ul className="sact-files">
            {rest.map((f) => (
              <FileLine key={f.path} file={f} />
            ))}
            {unlisted > 0 && <li className="sact-dim">and {unlisted} not listed</li>}
          </ul>
        </details>
      )}
    </>
  );
}

function FileLine({ file: f }: { file: SyncFileEvent }) {
  return (
    <li className="sact-file" data-kind={f.kind} data-outcome={f.outcome}>
      <span className="sact-sign" aria-label={f.kind}>
        {SIGN[f.kind]}
      </span>
      <code className="sact-file-path" title={f.path}>
        {f.path}
        {f.link && " (link)"}
      </code>
      {f.size !== undefined && f.kind !== "deleted" && <span className="sact-dim">{formatBytes(f.size)}</span>}
      {f.outcome === "conflict" && <span className="sact-warn">{f.beside ? `conflict: kept beside as ${f.beside.split("/").pop()}` : "conflict: the delete was let go"}</span>}
      {f.outcome === "refused" && <span className="sact-fail">refused: not a path kiln writes</span>}
      {f.outcome === "same" && <span className="sact-dim">already so</span>}
    </li>
  );
}
