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
  type HostPullDetail,
  type Thread,
} from "@/api/review";
import type { ShipCommit } from "@/api/ship";
import { Loadable } from "@/components/ui/loading";
import { useRes } from "@/lib/data";
import { invalidate } from "@/lib/resource";
import { parsePatch, type PatchFile } from "@/views/review/patch";
import { Conversation, ConvoBox, Description } from "./convo";
import { FileDiff, makeLinesOf, useThreadMap, WholeFile, type FilesCtx } from "./diffs";
import { rowsOf } from "./fold";
import { GateRow, HostGateRow } from "./gate";
import type { DraftPlace, Ops } from "./ops";
import { SearchBox, type Jump } from "./search";
import "./review.css";

export const ReviewFiles = React.memo(function ReviewFiles({ item }: { item: ShipCommit }) {
  const ch = item.change;
  const host = ch?.host;
  return (
    <Loadable label="Loading its review">
      <ReviewBoundary>{ch && host === "kiln" ? <KilnFiles item={item} /> : ch && host ? <HostFiles item={item} /> : <CommitFiles item={item} />}</ReviewBoundary>
    </Loadable>
  );
});

/** A change the server doesn't keep (a mock's, a pruned one): the section says so, nothing else breaks. */
class ReviewBoundary extends React.Component<{ children: React.ReactNode }, { err: unknown }> {
  state = { err: null as unknown };
  static getDerivedStateFromError(err: unknown) {
    return { err };
  }
  render() {
    if (this.state.err)
      return (
        <section className="rv-s" aria-label="Files">
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
  return {
    me,
    ops: {
      me,
      item: ch ? `${item.repo}/${ch.name}` : `${item.repo}/${item.sha}`,
      key: ch && ch.host === "kiln" ? ch.name : null,
      version: ch?.revision ?? 1,
      kiln: ch?.host === "kiln",
    },
  };
}

const refresh = () => {
  invalidate("review/");
  invalidate("ship/");
};

interface FrameP {
  sum: string;
  search?: { res: Parameters<typeof SearchBox>[0]["hits"]; q: string; onQ: (q: string) => void };
  onJump?: (j: Jump) => void;
  bar?: React.ReactNode;
  children: React.ReactNode;
  convo: React.ReactNode;
}

/** The two sections the Dropdown mounts, in their order: Files, then the Conversation. */
function Frame(p: FrameP) {
  return (
    <>
      <section className="rv-s" aria-label="Files">
        <h3 className="rv-h">
          Files <span className="rv-sum">{p.sum}</span>
        </h3>
        {p.bar}
        {p.search && <SearchBox q={p.search.q} onQ={p.search.onQ} hits={p.search.res} onJump={p.onJump ?? (() => {})} />}
        <div className="rv-files">{p.children}</div>
      </section>
      <section className="rv-s" aria-label="Conversation">
        <h3 className="rv-h">Conversation</h3>
        {p.convo}
      </section>
    </>
  );
}

/* ------------------------------------------------------------- a kiln change */

function KilnFiles({ item }: { item: ShipCommit }) {
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
  const list = React.useMemo(() => parsePatch(files.patch), [files.patch]);
  const rowsByFile = React.useMemo(() => new Map(list.map((f) => [f.path, rowsOf(f)])), [list]);
  const map = useThreadMap(comments.threads, version);
  const meta = React.useMemo(() => new Map(files.files.map((f) => [f.path, f])), [files.files]);
  const linesOf = React.useMemo(() => makeLinesOf(rowsByFile), [rowsByFile]);

  const fullOps: Ops = {
    ...ops,
    commentAt: async (file, line, side, body, kind) => {
      await reviewApi.comment(key, { body, version, file, side, start_line: line, end_line: line, kind, publish: true });
      refresh();
    },
    reply: async (t, body) => {
      await reviewApi.comment(key, { body, thread: t.id, publish: true });
      refresh();
    },
    converse: async (body) => {
      await reviewApi.comment(key, { body, publish: true });
      refresh();
    },
    resolve: async (t, on) => {
      const c = t.comments.at(-1) ?? t.comments[0];
      if (c) await reviewApi.editComment(c.id, { state: on ? "resolved" : "open" });
      refresh();
    },
    edit: async (id, body) => {
      await reviewApi.editComment(id, { body });
      refresh();
    },
    remove: async (id) => {
      await reviewApi.deleteComment(id);
      refresh();
    },
    setKind: async (t, kind) => {
      const c = t.comments[0];
      if (c) await reviewApi.editComment(c.id, { kind });
      refresh();
    },
    apply: async (id, index) => {
      const r = await reviewApi.applySuggestion(id, index);
      refresh();
      return { version: r.version };
    },
  };

  const ctx: FilesCtx = { ops: fullOps, threads: comments.threads, conflicts: cf.conflicts, rowsByFile, version, draft, setDraft, open: setWhole, q };
  const jump = (j: Jump) => {
    if (j.whole) setWhole(j.file);
    else document.querySelector(`[data-rvl="${j.file}|new:${j.line}"]`)?.scrollIntoView({ block: "center" });
  };
  const unresolved = cf.unresolved;
  const adds = list.reduce((a, f) => a + f.hunks.reduce((n, h) => n + h.lines.filter((l) => l.startsWith("+")).length, 0), 0);
  const dels = list.reduce((a, f) => a + f.hunks.reduce((n, h) => n + h.lines.filter((l) => l.startsWith("-")).length, 0), 0);
  const general = comments.threads.filter((t) => t.file == null || !anchorHere(t, map));

  return (
    <Frame
      sum={`${list.length} file${list.length === 1 ? "" : "s"} · +${adds} −${dels} · v${version}${ch.latest > version ? ` of ${ch.latest}` : ""}`}
      bar={<GateRow gate={comments.gate} conflicts={{ unresolved, ready: cf.ready, total: cf.conflicts.length }} diffKey={key} superseded={ch.superseded_by} />}
      search={{ res: q.trim() ? searchRes(key, version, q) : null, q, onQ: setQ }}
      onJump={jump}
      convo={
        <Conversation ops={fullOps} linesOf={linesOf} threads={general} draft={draft?.kind === "reply" ? draft.thread : null} setDraft={(t) => setDraft(t ? { kind: "reply", thread: t } : null)}>
          <Description author={d.author} title={d.title} body={d.summary} at={d.created_at} />
          <ConvoBox ops={fullOps} />
        </Conversation>
      }
    >
      {whole ? (
        <WholeFile file={list.find((f) => f.path === whole)!} ctx={ctx} map={map} text={blobTextRes(key, version, whole, "new")} />
      ) : (
        list.map((f) => <FileDiff key={f.path} file={f} ctx={ctx} map={map} meta={meta.get(f.path)} />)
      )}
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
  const n = Number(ch.name.replace(/\D/g, ""));
  const { me, ops } = useOps(item);
  const pull = useRes(hostPullRes(item.repo, n)).data;
  const [q, setQ] = React.useState("");
  const [draft, setDraft] = React.useState<DraftPlace | null>(null);
  const list = React.useMemo(() => pull.files.map((f) => hostFile(f)).filter((f): f is PatchFile => f != null), [pull.files]);
  const rowsByFile = React.useMemo(() => new Map(list.map((f) => [f.path, rowsOf(f)])), [list]);
  const threads = React.useMemo(() => hostThreads(pull), [pull]);
  const map = useThreadMap(threads, 1);
  const linesOf = React.useMemo(() => makeLinesOf(rowsByFile), [rowsByFile]);

  const fullOps: Ops = {
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
  };
  const ctx: FilesCtx = { ops: fullOps, threads, conflicts: [], rowsByFile, version: 1, draft, setDraft, open: () => {}, q };
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
          draft={draft?.kind === "reply" ? draft.thread : null}
          setDraft={(t) => setDraft(t ? { kind: "reply", thread: t } : null)}
        >
          <Description author={pull.author} title={pull.title} body={pull.body} at={pull.updated_at ?? undefined} />
          <ConvoBox ops={fullOps} />
        </Conversation>
      }
    >
      {list.map((f) => (
        <FileDiff key={f.path} file={f} ctx={ctx} map={map} meta={{ status: "M" }} />
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
  const map = useThreadMap([], 1);
  const fullOps: Ops = {
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
  };
  const ctx: FilesCtx = { ops: fullOps, threads: [], conflicts: [], rowsByFile, version: 1, draft, setDraft, open: setWhole, q: "" };
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
      {whole ? (
        <WholeFile file={list.find((f) => f.path === whole)!} ctx={ctx} map={map} text={commitBlobTextRes(item.repo, item.sha, whole)} />
      ) : (
        list.map((f) => <FileDiff key={f.path} file={f} ctx={ctx} map={map} />)
      )}
    </Frame>
  );
}
