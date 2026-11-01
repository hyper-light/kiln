/**
 * The Changes tab's Commits (docs/agents.md, "Commits"; the Changes panel plan 01a194444b1d, step
 * 4): its branch's commits since its trunk, newest first, what isn't committed yet on top and this
 * session's set apart, and below them what the picked ones change together. Picked as Mission
 * control picks agents (touch-select.ts): a tap shows one alone; press and hold picks it, or unpicks
 * it, and a drag goes on as the first went; with a mouse, Shift picks a run and ⌘/Ctrl adds or takes
 * away one; the This session band's ring picks all of the session's. On picks the branch.
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { Check, ChevronDown, GitBranch } from "lucide-react";
import type { CommitLog, LoggedCommit, PickedCommits } from "@/api/harness";
import { useSuspenseResource } from "@/lib/resource";
import { relativeTime } from "@/lib/format";
import { DiffStat } from "./diff";
import { FileList, rowOf, sortRows, type Row, type SortKey } from "./changes";
import type { ChangedFile } from "./derive";
import { touchSelect, type TouchSelect } from "./touch-select";
import { usePref } from "@/lib/prefs";

/** Where an agent's commits come from: its machine, asked (`agents.commits`). */
export interface CommitsSource {
  id: string;
  ask: (picks: readonly string[], branch?: string) => Promise<{ log: CommitLog; picked: PickedCommits | null }>;
}

/** Its machine's answer, or why there's none (a runner too old to say, a folder that isn't a repository). */
type Answer = { log: CommitLog; picked: PickedCommits | null; error?: undefined } | { error: string };

/** What isn't committed yet, as a pick names it. */
const NOW = "now";

const asked = (source: CommitsSource, picks: readonly string[], branch: string | undefined) => (): Promise<Answer> =>
  source.ask(picks, branch).then(
    (a) => a,
    (e: unknown) => ({ error: e instanceof Error ? e.message : String(e) }),
  );

/** Its branch's commits, read through Suspense (kept, and refreshed when shown again). */
function useLog(source: CommitsSource, branch?: string): Answer {
  return useSuspenseResource(`harness/commits/${source.id}/${branch ?? ""}`, asked(source, [], branch)).data;
}

/** The Commits mode's button: on once its machine lists them, else turned off with why. */
export function CommitsMode({ source, on, onPick }: { source: CommitsSource; on: boolean; onPick: () => void }) {
  const a = useLog(source);
  return (
    <button type="button" role="radio" aria-checked={on} disabled={a.error !== undefined} title={a.error} onClick={onPick}>
      Commits
    </button>
  );
}

/** A commit as a change: its files, as the picked commits' diff names them. */
const asChanged = (picked: PickedCommits): ChangedFile[] =>
  picked.files.map((f) => ({
    path: f.path,
    tree: f.tree,
    created: Boolean(f.created),
    deleted: Boolean(f.deleted),
    truncated: Boolean(f.truncated),
    add: f.added,
    del: f.removed,
    edits: [{ call: null, diff: { path: f.path, created: f.created, hunks: f.hunks } }],
    place: f.place,
    at: f.at,
  }));

const short = (sha: string) => sha.slice(0, 7);

/** `head`: the tab's mode row, above this view's own. */
export function CommitsView({ source, cwd, head }: { source: CommitsSource; cwd: string; head: React.ReactNode }) {
  const [branch, setBranch] = React.useState<string | undefined>(undefined);
  return (
    <React.Suspense
      fallback={
        <>
          <div className="cx-head">{head}</div>
          <div className="cx-none">Asking its machine for its commits…</div>
        </>
      }
    >
      <Commits source={source} cwd={cwd} head={head} branch={branch} onBranch={setBranch} />
    </React.Suspense>
  );
}

function Commits({ source, cwd, head, branch, onBranch }: { source: CommitsSource; cwd: string; head: React.ReactNode; branch: string | undefined; onBranch: (b: string | undefined) => void }) {
  const answer = useLog(source, branch);
  const log = answer.error === undefined ? answer.log : null;
  const commits = React.useMemo(() => log?.commits ?? [], [log]);
  const ours = React.useMemo(() => commits.filter((c) => c.session).map((c) => c.sha), [commits]);
  // What's picked: this session's at first; once a person picks, what they picked.
  const [chosen, setChosen] = React.useState<{ branch: string | undefined; picks: string[] } | null>(null);
  const picks = chosen && chosen.branch === branch ? chosen.picks : ours;
  const [anchor, setAnchor] = React.useState<string | null>(null);
  const [sel, setSel] = React.useState<string | null>(null);
  const order = React.useMemo(() => [...(log?.dirty ? [NOW] : []), ...commits.map((c) => c.sha)], [log, commits]);
  const pick = React.useCallback((next: string[]) => setChosen({ branch, picks: order.filter((k) => next.includes(k)) }), [branch, order]);
  const picked = new Set(picks);
  // The rows' gestures ask what's picked now; their props are made once (as Mission control's are).
  const [touch] = React.useState(() => {
    const now: { s: TouchSelect; hold: (held: boolean) => void } = { s: { isSelected: () => false, set: () => undefined, selecting: false }, hold: () => undefined };
    return { now, props: touchSelect(() => now.s, { rows: "commit", main: ".cx-commit", onHold: (held) => now.hold(held) }) };
  });
  // Held, what's below stays as it was till the finger lifts: what it shows growing or shrinking
  // under a list being dragged over would move the rows under the finger.
  const [frozen, setFrozen] = React.useState<string | null>(null);
  touch.now.hold = (held) => setFrozen(held ? picks.join(",") : null);
  // A tap shows one alone (its click): never a pick of its own, as a tap is in Mission control while it selects.
  touch.now.s = { isSelected: (k) => picked.has(k), set: (ks, on) => pick(on ? [...picks, ...ks] : picks.filter((k) => !ks.includes(k))), selecting: false };
  const onRow = (k: string, e: React.MouseEvent) => {
    if (e.shiftKey && anchor && order.includes(anchor)) {
      const [i, j] = [order.indexOf(anchor), order.indexOf(k)];
      pick(order.slice(Math.min(i, j), Math.max(i, j) + 1));
    } else if (e.metaKey || e.ctrlKey) {
      pick(picked.has(k) ? picks.filter((p) => p !== k) : [...picks, k]);
      setAnchor(k);
    } else {
      pick([k]);
      setAnchor(k);
    }
    setSel(null);
  };
  const some = ours.filter((k) => picked.has(k)).length;
  const shown = frozen ?? picks.join(",");
  const deferred = React.useDeferredValue(shown);

  if (!log) return <ErrorView head={head} error={answer.error ?? ""} />;
  const listed = log.listed ?? log.branch ?? "its commit";
  const rows = commits;
  const firstOurs = rows.find((c) => c.session);
  const firstBefore = ours.length > 0 ? rows.find((c) => !c.session) : undefined;
  return (
    <>
      <div className="cx-head">
        {head}
        <div className="cx-hrow">
          <span className="cx-lab">On</span>
          <DropdownMenu.Root>
            <DropdownMenu.Trigger asChild>
              <button type="button" className="cx-val" aria-label={`On: ${listed}`}>
                <GitBranch size={13} aria-hidden="true" />
                <span>{listed}</span>
                <ChevronDown size={12} aria-hidden="true" className="cx-chev" />
              </button>
            </DropdownMenu.Trigger>
            <DropdownMenu.Portal>
              <DropdownMenu.Content className="popover cx-menu" sideOffset={6} align="start" collisionPadding={12}>
                <DropdownMenu.RadioGroup value={branch ?? log.branch ?? ""} onValueChange={(v) => onBranch(v === log.branch ? undefined : v)}>
                  {[...new Set([...(log.branch ? [log.branch] : []), ...log.branches])].map((b) => (
                    <DropdownMenu.RadioItem key={b} value={b} className="menu-item">
                      <span className="cx-mark">
                        <DropdownMenu.ItemIndicator>
                          <Check size={14} aria-hidden="true" />
                        </DropdownMenu.ItemIndicator>
                      </span>
                      <GitBranch size={14} aria-hidden="true" />
                      <span className="cx-item">
                        {b}
                        {b === log.branch && <small>it's on this one</small>}
                      </span>
                    </DropdownMenu.RadioItem>
                  ))}
                </DropdownMenu.RadioGroup>
              </DropdownMenu.Content>
            </DropdownMenu.Portal>
          </DropdownMenu.Root>
          <span className="cx-total">
            {shown !== "" && deferred && (
              <React.Suspense fallback={null}>
                <PickedTotal source={source} branch={branch} picks={deferred} />
              </React.Suspense>
            )}
          </span>
        </div>
      </div>
      {order.length === 0 ? (
        <div className="cx-none">{log.trunk ? `Nothing on ${listed} since ${log.trunk}.` : "No commits here yet."}</div>
      ) : (
        <div className="cx-commits" {...touch.props} aria-label="Its commits">
          {log.dirty && (
            <button
              type="button"
              className="cx-commit"
              data-commit={NOW}
              data-now=""
              data-first=""
              data-on={picked.has(NOW) || undefined}
              aria-pressed={picked.has(NOW)}
              onClick={(e) => onRow(NOW, e)}
            >
              <span className="cx-ring" />
              <span className="cx-sha">now</span>
              <span className="cx-subj">Not committed yet</span>
              <span className="cx-age">{log.dirty.files === 1 ? "1 file" : `${log.dirty.files} files`}</span>
            </button>
          )}
          {rows.map((c, i) => (
            <React.Fragment key={c.sha}>
              {c === firstOurs && (
                <div className="cx-cband">
                  <button
                    type="button"
                    role="checkbox"
                    className="cx-all"
                    aria-label="Pick this session's commits"
                    aria-checked={some === ours.length ? true : some > 0 ? "mixed" : false}
                    data-on={some === ours.length || undefined}
                    data-some={(some > 0 && some < ours.length) || undefined}
                    onClick={() => pick(some === ours.length ? picks.filter((k) => !ours.includes(k)) : [...picks, ...ours])}
                  />
                  <span>This session</span>
                </div>
              )}
              {c === firstBefore && (
                <div className="cx-cband">
                  <span />
                  <span>Before this session</span>
                </div>
              )}
              <button
                type="button"
                className="cx-commit"
                data-commit={c.sha}
                data-on={picked.has(c.sha) || undefined}
                data-before={(ours.length > 0 && !c.session) || undefined}
                data-first={(i === 0 && !log.dirty) || undefined}
                data-last={i === rows.length - 1 || undefined}
                aria-pressed={picked.has(c.sha)}
                title={`${c.sha}\n${c.subject}\n${c.author}`}
                onClick={(e) => onRow(c.sha, e)}
              >
                <span className="cx-ring" />
                <span className="cx-sha">{short(c.sha)}</span>
                <span className="cx-subj">{c.subject}</span>
                <span className="cx-age">{relativeTime(Math.floor(c.at / 1000)).replace(/ ago$/, "")}</span>
              </button>
            </React.Fragment>
          ))}
        </div>
      )}
      {shown === "" ? (
        <div className="cx-none">Pick a commit to see what it changed.</div>
      ) : (
        <React.Suspense fallback={<div className="cx-none">Putting the picked commits together…</div>}>
          {deferred ? (
            <PickedFiles source={source} branch={branch} picks={deferred} cwd={cwd} sel={sel} onSel={setSel} commits={commits} />
          ) : (
            <div className="cx-none">Putting the picked commits together…</div>
          )}
        </React.Suspense>
      )}
    </>
  );
}

/** Commits greyed out after all (its machine answered with why): the head, and why. */
function ErrorView({ head, error }: { head: React.ReactNode; error: string }) {
  return (
    <>
      <div className="cx-head">{head}</div>
      <div className="cx-none">{error}</div>
    </>
  );
}

/** The picked commits' change, together, read through Suspense (kept per pick). */
function usePicked(source: CommitsSource, branch: string | undefined, picks: string): Answer {
  return useSuspenseResource(`harness/commits/${source.id}/${branch ?? ""}/${picks}`, asked(source, picks.split(","), branch)).data;
}

function PickedTotal({ source, branch, picks }: { source: CommitsSource; branch: string | undefined; picks: string }) {
  const a = usePicked(source, branch, picks);
  const files = a.error === undefined ? (a.picked?.files ?? []) : [];
  if (a.error !== undefined || a.picked?.needs || files.length === 0) return null;
  return <DiffStat add={files.reduce((n, f) => n + f.added, 0)} del={files.reduce((n, f) => n + f.removed, 0)} />;
}

function PickedFiles({
  source,
  branch,
  picks,
  cwd,
  sel,
  onSel,
  commits,
}: {
  source: CommitsSource;
  branch: string | undefined;
  picks: string;
  cwd: string;
  sel: string | null;
  onSel: (k: string | null) => void;
  commits: readonly LoggedCommit[];
}) {
  const [sort] = usePref<SortKey>("kiln.changes.sort", "path");
  const a = usePicked(source, branch, picks);
  if (a.error !== undefined) return <div className="cx-none">{a.error}</div>;
  const needs = a.picked?.needs;
  if (needs) {
    const name = (sha: string | null) => {
      const c = commits.find((x) => x.sha === sha);
      return sha === NOW ? "What isn't committed yet" : c ? `${short(c.sha)} “${c.subject}”` : sha ? short(sha) : "";
    };
    return (
      <div className="cx-none">
        {needs.commit ? `${name(needs.pick)} needs ${short(needs.commit)} “${needs.subject ?? ""}”, which isn't picked.` : `${name(needs.pick)} doesn't go on the commits picked before it.`}
      </div>
    );
  }
  const rows: Row[] = sortRows(
    asChanged(a.picked ?? { from: "", files: [] }).map((f) => rowOf(f, cwd)),
    sort,
  );
  return <FileList rows={rows} line={() => ""} sel={sel} onSel={onSel} none="The picked commits change nothing together." />;
}
