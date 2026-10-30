/**
 * Files and the Conversation: a Ship item's dropdown's first section (docs/ci.md W26-R07, R08,
 * R13), whatever the item's stage. A kiln change's Files draw its threads on their lines, its
 * conflicts where they fall, the kinds row and Approve by the gate's words, and the vorpal search;
 * a host's pull request reads and writes through the forge seam; a commit with no change shows
 * its diff from its parent, no Approve. Data through Suspense, mutations through React Actions.
 */
import * as React from "react";
import {
  blobTextRes,
  commitBlobTextRes,
  commitFilesRes,
  commentsRes,
  conflictsRes,
  diffRes,
  filesRes,
  hostPullRes,
  hostPullsApi,
  inboxRes,
  pullSearchRes,
  reviewApi,
  searchRes,
  whoSaid,
  type HostPullDetail,
  type Thread,
} from "@/api/review";
import type { ShipCommit } from "@/api/ship";
import { AboutContext, type AboutAgent } from "@/components/code/about";
import { toast } from "@/components/ui/toast";
import { useRes, warm } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate, setData } from "@/lib/resource";
import { parsePatch, type PatchFile } from "@/views/review/patch";
import { Conversation, ConvoBox, Description } from "./convo";
import { FileDiff, makeLinesOf, useThreadMap, WholeFile, type FilesCtx } from "./diffs";
import { rowsOf } from "./fold";
import { GateRow, HostGateRow } from "./gate";
import type { DraftPlace, Ops } from "./ops";
import { applyThreadPatch, pendingComment, pendingThread, type ThreadPatch } from "./ops";
import { counted, useStable } from "./perf";
import { SearchBox, type Jump } from "./search";
import "./review.css";

export const ReviewFiles = React.memo(function ReviewFiles({ item }: { item: ShipCommit }) {
  counted("ship/reviewfiles");
  warmReview(item);
  // Files is one `<section aria-label="Files">`, focusable, once its review is drawn or has failed:
  // Ship's Diff button waits for it to appear and brings it into view, focused. While it loads there
  // is none, so the focus never lands on a placeholder that the review then replaces. It suspends
  // until then, under the dropdown's one boundary (Ship's Dropdown): every section waits for Files,
  // so the dropdown grows once and nothing drawn is pushed down when the review lands (W26-R15).
  // Once drawn, it stays: what a changed item reads anew (a new revision's files, a commit become
  // a change) loads behind the review shown, which the dropdown's loader never replaces.
  const shown = React.useDeferredValue(item);
  const ch = shown.change;
  const host = ch?.host;
  return <ReviewBoundary>{ch && host === "kiln" ? <KilnFiles item={shown} /> : ch && host ? <HostFiles item={shown} /> : <CommitFiles item={shown} />}</ReviewBoundary>;
});

/** A host's pull request's number, from its change's name ("!6", "#12"). */
const pullNumber = (name: string) => Number(name.replace(/\D/g, ""));

/**
 * Starts loading an item's review, every key it reads at once (lib/data.ts's `warm`): one round
 * trip, where reading them in turn waits for each before asking for the next. Ship's rows call it
 * as the pointer comes onto one or the focus into it, so the review is on its way before it opens.
 */
export function warmReview(item: ShipCommit) {
  const ch = item.change;
  if (ch && ch.host === "kiln") warm(inboxRes, diffRes(ch.name), commentsRes(ch.name), filesRes(ch.name, ch.revision), conflictsRes(ch.name));
  else if (ch && ch.host) warm(inboxRes, hostPullRes(item.repo, pullNumber(ch.name)));
  else warm(inboxRes, commitFilesRes(item.repo, item.sha));
}

/** A change the server doesn't keep (a mock's, a pruned one): the section says so, nothing else breaks. */
class ReviewBoundary extends React.Component<{ children: React.ReactNode }, { err: unknown }> {
  state = { err: null as unknown };
  static getDerivedStateFromError(err: unknown) {
    return { err };
  }
  render() {
    if (this.state.err)
      return (
        <section className="rv-s" aria-label="Files" tabIndex={-1}>
          <h3 className="rv-h">Files</h3>
          <p className="rv-sub">{this.state.err instanceof Error ? this.state.err.message : "its review can't be read"}</p>
        </section>
      );
    return this.props.children;
  }
}

/* ------------------------------------------------------------- shared pieces */

function useOps(item: ShipCommit): { me: string; ops: Omit<Ops, "commentAt" | "reply" | "converse" | "resolve" | "edit" | "remove" | "setKind" | "apply"> } {
  const me = useRes(inboxRes).data.you;
  const ch = item.change;
  // One ops object while what it names stays the same (W26-R15): the memoized parts below it skip a
  // render that changes nothing.
  const ops = React.useMemo(
    () => ({
      me,
      item: ch ? `${item.repo}/${ch.name}` : `${item.repo}/${item.sha}`,
      key: ch && ch.host === "kiln" ? ch.name : null,
      version: ch?.revision ?? 1,
      kiln: ch?.host === "kiln",
    }),
    [me, ch, item.repo, item.sha],
  );
  return { me, ops };
}

const refresh = () => {
  invalidate("review/");
  invalidate("ship/");
};

/** Shared empties and a no-op, so stable references stay stable (W26-R15). */
const EMPTY_THREADS: Thread[] = [];

/** Who asking about code goes to (components/code/about.tsx): its author, where an agent wrote it
 * (`agent:<id>`, as its agent token pushed it); none for a person's, as on the review page. */
function aboutOf(author: string): AboutAgent | null {
  const id = author.startsWith("agent:") ? (author.slice("agent:".length).split(" ")[0] ?? "") : "";
  return id ? { id, name: whoSaid(author), subagents: true, busy: false } : null;
}
const noop = () => {};

interface FrameP {
  sum: string;
  search?: { res: Parameters<typeof SearchBox>[0]["hits"]; q: string; onQ: (q: string) => void };
  onJump?: (j: Jump) => void;
  bar?: React.ReactNode;
  children: React.ReactNode;
  convo: React.ReactNode;
}

/**
 * The Conversation head's agent fixes switch, where the mockup draws it (Ship's mockups plan, step
 * 13): a repository's setting that lets agents fix code from agents' feedback. Nothing acts on it
 * until Agents in review builds that, so it shows off and can't be turned on yet, and says why.
 */
function AgentFixes() {
  return (
    <button type="button" className="rv-switch" role="switch" aria-checked={false} disabled title="Agents fix code from agents' feedback · repository setting. Not yet: Agents in review builds it.">
      <i aria-hidden="true" />
      <span>agent fixes</span>
    </button>
  );
}

/** The two sections the Dropdown mounts, in their order: Files, then the Conversation. */
function Frame(p: FrameP) {
  return (
    <>
      <section className="rv-s" aria-label="Files" tabIndex={-1}>
        <h3 className="rv-h">
          Files <span className="rv-sum">{p.sum}</span>
        </h3>
        {p.bar}
        {p.search && <SearchBox q={p.search.q} onQ={p.search.onQ} hits={p.search.res} onJump={p.onJump ?? (() => {})} />}
        <div className="rv-files">{p.children}</div>
      </section>
      <section className="rv-s rv-s-convo" aria-label="Conversation">
        <h3 className="rv-h">Conversation</h3>
        <AgentFixes />
        {p.convo}
      </section>
    </>
  );
}

/* ------------------------------------------------------------- a kiln change */

function KilnFiles({ item }: { item: ShipCommit }) {
  counted("ship/kilnfiles");
  const ch = item.change!;
  const key = ch.name;
  const version = ch.revision;
  const { ops } = useOps(item);
  const d = useRes(diffRes(key)).data;
  const comments = useRes(commentsRes(key)).data;
  const files = useRes(filesRes(key, version)).data;
  const cf = useRes(conflictsRes(key)).data;
  const [q, setQ] = React.useState("");
  const [draft, setDraft] = React.useState<DraftPlace | null>(null);
  const [whole, setWhole] = React.useState<string | null>(null);
  // What a refetch brings that reads the same isn't a change: the threads, conflicts and file list
  // keep their last references, so the memoized diffs and threads under them don't render (W26-R15).
  const threads = useStable(comments.threads, JSON.stringify(comments.threads));
  const conflicts = useStable(cf.conflicts, JSON.stringify(cf.conflicts));
  const fileList = useStable(files.files, JSON.stringify(files.files));
  const list = React.useMemo(() => parsePatch(files.patch), [files.patch]);
  const rowsByFile = React.useMemo(() => new Map(list.map((f) => [f.path, rowsOf(f)])), [list]);
  // Mutations are React Actions with optimistic state: the change shows at once, then the server's
  // answer is written into the resource (setData) before the refresh it asks for lands.
  const [shown, patchThreads] = React.useOptimistic(threads, applyThreadPatch);
  const map = useThreadMap(shown, version);
  const meta = React.useMemo(() => new Map(fileList.map((f) => [f.path, f])), [fileList]);
  const linesOf = React.useMemo(() => makeLinesOf(rowsByFile), [rowsByFile]);
  const act = React.useCallback(
    (f: (patch: (p: ThreadPatch) => void) => Promise<void>) =>
      new Promise<void>((res, rej) =>
        React.startTransition(async () => {
          try {
            await f(patchThreads);
            res();
          } catch (e) {
            rej(e);
          }
        }),
      ),
    [patchThreads],
  );

  const fullOps: Ops = React.useMemo(() => {
    const merge = (p: ThreadPatch) => setData<{ threads: Thread[] }>(`review/comments/${key}`, (s) => ({ ...s, threads: applyThreadPatch(s.threads, p) }));
    return {
      ...ops,
      // What goes out new (a comment, a thread) is written optimistically into the resource at
      // once and swapped for the server's ids when they land, or taken back out when refused;
      // what changes in place (resolve, kind, edit, delete) goes through useOptimistic's patch.
      commentAt: (file, line, side, body, kind, start) =>
        act(async () => {
          const t = pendingThread(ops.me, version, { file, side, line, start }, kind, pendingComment(ops.me, body));
          merge({ type: "thread", t });
          try {
            const r = await reviewApi.comment(key, { body, version, file, side, start_line: start ?? line, end_line: line, kind, publish: true });
            merge({ type: "retagThread", id: t.id, t: { ...t, id: r.thread, comments: t.comments.map((c) => ({ ...c, id: r.id })) } });
          } catch (e) {
            merge({ type: "unthread", id: t.id });
            throw e;
          }
          refresh();
        }),
      reply: (t, body) =>
        act(async () => {
          const c = pendingComment(ops.me, body);
          merge({ type: "reply", thread: t.id, c });
          try {
            const r = await reviewApi.comment(key, { body, thread: t.id, publish: true });
            merge({ type: "retag", thread: t.id, id: c.id, c: { ...c, id: r.id } });
          } catch (e) {
            merge({ type: "unreply", thread: t.id, id: c.id });
            throw e;
          }
          refresh();
        }),
      converse: (body) =>
        act(async () => {
          const t = pendingThread(ops.me, version, { file: null, side: "new", line: null }, "question", pendingComment(ops.me, body));
          merge({ type: "thread", t });
          try {
            const r = await reviewApi.comment(key, { body, publish: true });
            merge({ type: "retagThread", id: t.id, t: { ...t, id: r.thread, comments: t.comments.map((c) => ({ ...c, id: r.id })) } });
          } catch (e) {
            merge({ type: "unthread", id: t.id });
            throw e;
          }
          refresh();
        }),
      resolve: (t, on) =>
        act(async (patch) => {
          patch({ type: "resolve", thread: t.id, on, me: ops.me });
          const c = t.comments.at(-1) ?? t.comments[0];
          if (c) await reviewApi.editComment(c.id, { state: on ? "resolved" : "open" });
          merge({ type: "resolve", thread: t.id, on, me: ops.me });
          refresh();
        }),
      edit: (id, body) =>
        act(async (patch) => {
          patch({ type: "edit", id, body });
          await reviewApi.editComment(id, { body });
          merge({ type: "edit", id, body });
          refresh();
        }),
      remove: (id) =>
        act(async (patch) => {
          patch({ type: "remove", id });
          await reviewApi.deleteComment(id);
          merge({ type: "remove", id });
          refresh();
        }),
      setKind: (t, kind) =>
        act(async (patch) => {
          patch({ type: "kind", thread: t.id, kind });
          const c = t.comments[0];
          if (c) await reviewApi.editComment(c.id, { kind });
          merge({ type: "kind", thread: t.id, kind });
          refresh();
        }),
      apply: async (id, index) => {
        const r = await reviewApi.applySuggestion(id, index);
        refresh();
        return { version: r.version };
      },
    };
  }, [ops, act, key, version]);

  // Viewed, kept by kiln for the change: the file folds at once, and its files are read again so it
  // opens folded next time (Changes plan 01a17bd748ff73d98f09996ecdc6873d, step 2).
  const viewed = React.useCallback(
    (file: string, on: boolean) =>
      void reviewApi.viewed(key, file, on).then(
        () => invalidate(`review/files/${key}/`),
        (e: unknown) => toast.error("Couldn't mark it viewed", errorMessage(e)),
      ),
    [key],
  );
  const about = React.useMemo(() => aboutOf(d.author), [d.author]);
  const ctx: FilesCtx = React.useMemo(
    () => ({ ops: fullOps, threads: shown, conflicts, rowsByFile, version, draft, setDraft, open: setWhole, q, linesOf, viewed }),
    [fullOps, shown, conflicts, rowsByFile, version, draft, q, linesOf, viewed],
  );
  const jump = (j: Jump) => {
    if (j.whole) setWhole(j.file);
    else document.querySelector(`[data-rvl="${j.file}|new:${j.line}"]`)?.scrollIntoView({ block: "center" });
  };
  const unresolved = cf.unresolved;
  const adds = list.reduce((a, f) => a + f.hunks.reduce((n, h) => n + h.lines.filter((l) => l.startsWith("+")).length, 0), 0);
  const dels = list.reduce((a, f) => a + f.hunks.reduce((n, h) => n + h.lines.filter((l) => l.startsWith("-")).length, 0), 0);
  const general = shown.filter((t) => t.file == null || !anchorHere(t, map));

  return (
    <Frame
      sum={`${list.length} file${list.length === 1 ? "" : "s"} · +${adds} −${dels} · v${version}${ch.latest > version ? ` of ${ch.latest}` : ""}`}
      bar={<GateRow gate={comments.gate} conflicts={{ unresolved, ready: cf.ready, total: cf.conflicts.length }} diffKey={key} superseded={ch.superseded_by} />}
      search={{ res: q.trim() ? searchRes(key, version, q) : null, q, onQ: setQ }}
      onJump={jump}
      convo={
        <Conversation
          ops={fullOps}
          linesOf={linesOf}
          threads={general}
          box={<ConvoBox ops={fullOps} />}
          draft={draft?.kind === "reply" ? draft.thread : null}
          setDraft={(t) => setDraft(t ? { kind: "reply", thread: t } : null)}
        >
          <Description item={fullOps.item} author={d.author} title={d.title} body={d.summary} at={d.created_at} />
        </Conversation>
      }
    >
      <AboutContext.Provider value={about}>
        {whole ? (
          <WholeFile file={list.find((f) => f.path === whole)!} ctx={ctx} map={map} text={blobTextRes(key, version, whole, "new")} />
        ) : (
          list.map((f) => <FileDiff key={f.path} file={f} ctx={ctx} map={map} meta={meta.get(f.path)} />)
        )}
      </AboutContext.Provider>
    </Frame>
  );
}

/** The thread shows in the Conversation when it isn't under a line in the version shown. */
function anchorHere(t: Thread, map: Map<string, { t: Thread }[]>): boolean {
  for (const v of map.values()) if (v.some((x) => x.t.id === t.id)) return true;
  return false;
}

/* ------------------------------------------------------------- a host's pull request */

function HostFiles({ item }: { item: ShipCommit }) {
  const ch = item.change!;
  const n = pullNumber(ch.name);
  const { me, ops } = useOps(item);
  const pull = useRes(hostPullRes(item.repo, n)).data;
  const [q, setQ] = React.useState("");
  const [draft, setDraft] = React.useState<DraftPlace | null>(null);
  const list = React.useMemo(() => pull.files.map((f) => hostFile(f)).filter((f): f is PatchFile => f != null), [pull.files]);
  const rowsByFile = React.useMemo(() => new Map(list.map((f) => [f.path, rowsOf(f)])), [list]);
  // The pull's threads as kiln's, on their last reference while they read the same (W26-R15).
  const host = useStable(hostThreads(pull), JSON.stringify(pull.threads));
  const threads = React.useMemo(() => host, [host]);
  const map = useThreadMap(threads, 1);
  const linesOf = React.useMemo(() => makeLinesOf(rowsByFile), [rowsByFile]);

  const fullOps: Ops = React.useMemo(
    () => ({
      ...ops,
      kiln: false,
      version: 1,
      pull: { repo: item.repo, n },
      commentAt: async (file, line, side, body) => {
        await hostPullsApi.comment(item.repo, n, { body, path: file, line, side: side === "old" ? "left" : "right" });
        refresh();
      },
      reply: async (t, body) => {
        await hostPullsApi.comment(item.repo, n, { body, reply_to: t.comments.at(-1)?.id ?? t.id });
        refresh();
      },
      converse: async (body) => {
        await hostPullsApi.comment(item.repo, n, { body });
        refresh();
      },
      resolve: async () => {},
      edit: async () => {},
      remove: async () => {},
      setKind: async () => {},
      apply: async () => ({ version: 0 }),
    }),
    [ops, item.repo, n],
  );
  const ctx: FilesCtx = React.useMemo(
    () => ({ ops: fullOps, threads, conflicts: [], rowsByFile, version: 1, draft, setDraft, open: noop, q, linesOf }),
    [fullOps, threads, rowsByFile, draft, q, linesOf],
  );
  const adds = pull.files.reduce((a, f) => a + (f.additions ?? 0), 0);
  const dels = pull.files.reduce((a, f) => a + (f.deletions ?? 0), 0);
  const general = threads.filter((t) => t.file == null);
  const approved = pull.reviews.some((r) => r.author === me || r.state === "approved");

  return (
    <Frame
      sum={`${pull.files.length} file${pull.files.length === 1 ? "" : "s"} · +${adds} −${dels} · ${ch.name}`}
      bar={<HostGateRow approved={approved} onApprove={() => hostPullsApi.review(item.repo, n, "approve", "").then(() => {})} />}
      search={{ res: q.trim() ? pullSearchRes(item.repo, n, q) : null, q, onQ: setQ }}
      convo={
        <Conversation
          ops={fullOps}
          linesOf={linesOf}
          threads={[...convoThreads(pull), ...general]}
          box={<ConvoBox ops={fullOps} />}
          draft={draft?.kind === "reply" ? draft.thread : null}
          setDraft={(t) => setDraft(t ? { kind: "reply", thread: t } : null)}
        >
          <Description item={fullOps.item} author={pull.author} title={pull.title} body={pull.body} at={pull.updated_at ?? undefined} />
        </Conversation>
      }
    >
      {list.map((f) => (
        <FileDiff key={f.path} file={f} ctx={ctx} map={map} meta={{ status: pull.files.find((x) => x.path === f.path)?.status }} />
      ))}
    </Frame>
  );
}

/** A host file's hunk-only patch wrapped as a patch file parsePatch reads. */
function hostFile(f: HostPullDetail["files"][number]): PatchFile | null {
  if (!f.patch) return { path: f.path, oldPath: f.old_path, status: "modified", similarity: null, binary: true, modeChange: null, hunks: [] };
  return parsePatch(`diff --git a/${f.old_path ?? f.path} b/${f.path}\n--- a/${f.old_path ?? f.path}\n+++ b/${f.path}\n${f.patch}\n`)[0] ?? null;
}

/** A host's line threads as kiln's Thread shape (version 1, kiln keeps no kind). */
function hostThreads(p: HostPullDetail): Thread[] {
  return p.threads.map((t) => ({
    id: t.id,
    version: 1,
    file: t.path,
    side: t.side === "left" ? "old" : "new",
    start_line: t.line,
    end_line: t.line,
    state: t.resolved ? "resolved" : "open",
    created_by: t.comments[0]?.author ?? "",
    created_at: t.comments[0]?.at ?? 0,
    outdated: false,
    now: null,
    comments: t.comments.map((c) => ({ id: c.id, author: c.author, body: c.body, suggestion: null, created_at: c.at ?? 0, draft: false })),
  }));
}

/** The PR's conversation as threads of one comment each. */
function convoThreads(p: HostPullDetail): Thread[] {
  return p.conversation.map((c) => ({
    id: c.id,
    version: 1,
    file: null,
    side: "new",
    start_line: null,
    end_line: null,
    state: "open",
    created_by: c.author,
    created_at: c.at ?? 0,
    outdated: false,
    now: null,
    comments: [{ id: c.id, author: c.author, body: c.body, suggestion: null, created_at: c.at ?? 0, draft: false }],
  }));
}

/* ------------------------------------------------------------- a commit with no change */

function CommitFiles({ item }: { item: ShipCommit }) {
  const { ops } = useOps(item);
  const v = useRes(commitFilesRes(item.repo, item.sha)).data;
  const [draft, setDraft] = React.useState<DraftPlace | null>(null);
  const [whole, setWhole] = React.useState<string | null>(null);
  const list = React.useMemo(() => parsePatch(v.patch), [v.patch]);
  const rowsByFile = React.useMemo(() => new Map(list.map((f) => [f.path, rowsOf(f)])), [list]);
  const map = useThreadMap(EMPTY_THREADS, 1);
  const linesOf = React.useMemo(() => makeLinesOf(rowsByFile), [rowsByFile]);
  const fullOps: Ops = React.useMemo(
    () => ({
      ...ops,
      kiln: false,
      version: 1,
      commentAt: async () => {},
      reply: async () => {},
      converse: async () => {},
      resolve: async () => {},
      edit: async () => {},
      remove: async () => {},
      setKind: async () => {},
      apply: async () => ({ version: 0 }),
    }),
    [ops],
  );
  const about = React.useMemo(() => aboutOf(item.author), [item.author]);
  const ctx: FilesCtx = React.useMemo(
    () => ({ ops: fullOps, threads: EMPTY_THREADS, conflicts: [], rowsByFile, version: 1, draft, setDraft, open: setWhole, q: "", linesOf }),
    [fullOps, rowsByFile, draft, linesOf],
  );
  const adds = list.reduce((a, f) => a + f.hunks.reduce((n, h) => n + h.lines.filter((l) => l.startsWith("+")).length, 0), 0);
  const dels = list.reduce((a, f) => a + f.hunks.reduce((n, h) => n + h.lines.filter((l) => l.startsWith("-")).length, 0), 0);
  return (
    <Frame
      sum={`${list.length} file${list.length === 1 ? "" : "s"} · +${adds} −${dels} · ${item.short}`}
      convo={
        <ul className="rv-convo">
          <Description author={item.author} body={item.message} at={item.at} />
        </ul>
      }
    >
      <AboutContext.Provider value={about}>
        {whole ? (
          <WholeFile file={list.find((f) => f.path === whole)!} ctx={ctx} map={map} text={commitBlobTextRes(item.repo, item.sha, whole)} />
        ) : (
          list.map((f) => <FileDiff key={f.path} file={f} ctx={ctx} map={map} />)
        )}
      </AboutContext.Provider>
    </Frame>
  );
}
