/**
 * Pull requests on a repository's host (docs/ci.md W23-R06, AC03): GitHub's pull requests, GitLab's
 * merge requests and Bitbucket's pull requests, reviewed here as kiln's own diffs are. A list of a
 * repository's open ones, and one with its checks, conversation and files, commented on line by
 * line, reviewed and merged by the methods the host allows. What's done goes to the host at once,
 * as you when you signed in with that host, else with kiln's credentials there.
 */
import * as React from "react";
import { Link, useParams } from "@/lib/router";
import { HOST_WORD, hostNumber, hostPullRes, hostPullsApi, hostPullsRes, type HostComment, type HostPullDetail, type HostThread, type Thread } from "@/api/review";
import { Button } from "@/components/ui/button";
import { Loadable, Loading } from "@/components/ui/loading";
import { PageIntro, SelectField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { relativeTime, shortSha } from "@/lib/format";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { FileDiff, ThreadView, type Pick } from "./diff";
import { findMoves, parsePatch, type PatchFile } from "./patch";
import "./review.css";

const Markdown = React.lazy(() => import("@/lib/markdown").then((m) => ({ default: m.MarkdownBody })));

const STATE: Record<string, string> = {
  open: "Open",
  merged: "Merged",
  closed: "Closed",
};
const STATE_DATA: Record<string, string> = {
  open: "needs-review",
  merged: "landed",
  closed: "abandoned",
};
const VERDICTS = [
  { value: "comment", label: "Comment" },
  { value: "approve", label: "Approve" },
  { value: "request_changes", label: "Request changes" },
];

const acted = (as: "person" | "kiln") => (as === "person" ? "as you" : "with kiln's credentials");

/** `/review/hosts/:repo`: the repository's open pull requests on its host. */
export function HostPullsPage() {
  const { repo = "" } = useParams();
  return (
    <div className="container-console page rv-page">
      <title>{`${repo} · Pull requests · kiln`}</title>
      <PageIntro index={4} label={`Repositories / Review / ${repo}`} title="Pull requests." lead="Open on the repository's host, reviewed from here." />
      <Loadable label="Loading its pull requests" fallback={<Loading label="Loading its pull requests" block />}>
        <PullList repo={repo} />
      </Loadable>
    </div>
  );
}

function PullList({ repo }: { repo: string }) {
  const pulls = useRes(hostPullsRes(repo)).data;
  if (pulls.length === 0) return <p className="rv-sub">No open pull requests on its host.</p>;
  return (
    <ul className="rv-inbox-list">
      {pulls.map((p) => (
        <li key={p.number}>
          <Link to={`/review/hosts/${encodeURIComponent(repo)}/${p.number}`} className="rv-inbox-row">
            <span className="rv-inbox-what">
              <b>{hostNumber(p.host, p.number)}</b> {p.title}
            </span>
            <span className="rv-sub">
              {HOST_WORD[p.host]} · {p.author} · {p.head} → {p.base}
              {p.draft ? " · draft" : ""}
              {p.updated_at ? ` · updated ${relativeTime(p.updated_at)}` : ""}
            </span>
          </Link>
        </li>
      ))}
    </ul>
  );
}

/** `/review/hosts/:repo/:number`: one pull request. */
export default function HostPullPage() {
  const { repo = "", number = "" } = useParams();
  const n = Number(number);
  return (
    <div className="container-console page rv-page">
      <title>{`${repo} ${number} · Review · kiln`}</title>
      <Loadable label="Loading the pull request" fallback={<Loading label="Loading the pull request" block />}>
        <Pull key={`${repo}/${n}`} repo={repo} n={n} />
      </Loadable>
    </div>
  );
}

const refresh = (repo: string, n: number) => {
  invalidate(`review/host/${repo}/${n}`);
  invalidate(`review/host/${repo}`);
  invalidate("review/inbox");
};

/** A host's thread in the shape kiln's diff view draws: one version, on one line of one side. */
function asThread(t: HostThread): Thread {
  const first = t.comments[0];
  return {
    id: t.id,
    version: 1,
    file: t.path,
    side: t.side === "left" ? "old" : "new",
    start_line: t.line,
    end_line: t.line,
    state: t.resolved ? "resolved" : "open",
    created_by: first?.author ?? "",
    created_at: first?.at ?? 0,
    outdated: t.path != null && t.line == null,
    now: null,
    comments: t.comments.map((c) => ({
      id: c.id,
      author: c.author,
      body: c.body,
      suggestion: null,
      created_at: c.at ?? 0,
      draft: false,
    })),
  };
}

const STATUS: Record<string, PatchFile["status"]> = {
  added: "added",
  removed: "deleted",
  deleted: "deleted",
  renamed: "renamed",
  copied: "copied",
};

/** A host's file in the shape kiln's diff view draws: its hunks read from the host's patch. */
function asFile(f: HostPullDetail["files"][number]): PatchFile {
  const old = f.old_path ?? f.path;
  const parsed = f.patch ? parsePatch(`diff --git a/${old} b/${f.path}\n${f.patch}`)[0] : undefined;
  return {
    path: f.path,
    oldPath: f.old_path,
    status: STATUS[f.status] ?? "modified",
    similarity: null,
    // The hosts leave out a binary file's patch (and a very big one's).
    binary: !f.patch,
    modeChange: null,
    hunks: parsed?.hunks ?? [],
  };
}

function Pull({ repo, n }: { repo: string; n: number }) {
  const p = useRes(hostPullRes(repo, n)).data;
  const phone = useMediaQuery("(max-width: 760px)");
  const [splitPref, setSplitPref] = React.useState(false);
  const split = splitPref && !phone;
  const [whitespace, setWhitespace] = React.useState(false);
  const threads = React.useMemo(() => p.threads.map(asThread), [p.threads]);
  const files = React.useMemo(() => p.files.map(asFile), [p.files]);
  const moves = React.useMemo(() => findMoves(files), [files]);
  const reply = async (t: Thread, body: string) => {
    const r = await hostPullsApi.comment(repo, n, {
      body,
      reply_to: t.comments.at(-1)?.id ?? t.id,
    });
    toast({ title: `Replied ${acted(r.as)}`, tone: "succeeded" });
    refresh(repo, n);
  };
  const comment = async (path: string, pick: Pick, body: string) => {
    // A comment sits on one line on the hosts: a range's last.
    const r = await hostPullsApi.comment(repo, n, {
      body,
      path,
      line: pick.end,
      side: pick.side === "old" ? "left" : "right",
    });
    toast({
      title: `Commented on ${path}:${pick.end} ${acted(r.as)}`,
      tone: "succeeded",
    });
    refresh(repo, n);
  };
  return (
    <>
      <Head repo={repo} p={p} />
      <Checks p={p} />
      <Merge repo={repo} p={p} />
      <Reviews p={p} />
      <Conversation repo={repo} p={p} threads={threads.filter((t) => t.file == null)} onReply={reply} />
      <div className="rv-bar" role="toolbar" aria-label="How the diff shows">
        {!phone && (
          <SelectField
            label="View"
            value={split ? "split" : "unified"}
            onChange={(v) => setSplitPref(v === "split")}
            options={[
              { value: "unified", label: "Unified" },
              { value: "split", label: "Side by side" },
            ]}
          />
        )}
        <label className="rv-check">
          <input type="checkbox" className="check" checked={whitespace} onChange={(e) => setWhitespace(e.target.checked)} />
          Hide whitespace changes
        </label>
      </div>
      <div className="rv-filebar">
        <span className="rv-sub">
          {files.length} file{files.length === 1 ? "" : "s"}
        </span>
      </div>
      {files.map((f, i) => (
        <FileDiff
          key={f.path}
          idx={i}
          file={f}
          meta={undefined}
          diffKey={`${repo}/${n}`}
          version={1}
          split={split}
          whitespace={whitespace}
          moves={moves}
          pathOf={(k) => files[k]?.path ?? ""}
          threads={threads.filter((t) => t.file === f.path)}
          onViewed={() => undefined}
          onComment={(pick, body) => comment(f.path, pick, body)}
          onReply={(t, body) => reply(t, body)}
          now
        />
      ))}
      <ReviewForm repo={repo} p={p} />
    </>
  );
}

function Head({ repo, p }: { repo: string; p: HostPullDetail }) {
  return (
    <header className="rv-head">
      <p className="rv-eyebrow">
        <Link to="/repos">Repositories</Link> / <Link to="/review">Review</Link> / <Link to={`/review/hosts/${encodeURIComponent(repo)}`}>{repo}</Link> / {hostNumber(p.host, p.number)}
      </p>
      <h1 className="rv-title">{p.title}</h1>
      <p className="rv-meta">
        <span className="rv-state" data-state={STATE_DATA[p.state] ?? p.state}>
          {p.draft && p.state === "open" ? "Draft" : (STATE[p.state] ?? p.state)}
        </span>
        <span>by {p.author}</span>
        <span>
          {p.head} → {p.base}
        </span>
        {p.sha && <span>{shortSha(p.sha)}</span>}
        {p.updated_at && <span>updated {relativeTime(p.updated_at)}</span>}
        <a href={p.url} className="rv-link" target="_blank" rel="noreferrer">
          On {HOST_WORD[p.host]}
        </a>
      </p>
      {p.body.trim() && (
        <div className="rv-summary">
          <React.Suspense fallback={<p className="rv-sub">{p.body}</p>}>
            <Markdown text={p.body} />
          </React.Suspense>
        </div>
      )}
    </header>
  );
}

/** Every provider's runs of its head commit (KilnCI's, and the host's own CI's kiln follows). */
function Checks({ p }: { p: HostPullDetail }) {
  const runs = p.checks.checks ?? [];
  if (p.checks.error) return <p className="rv-sub rv-checks">Its checks can't be read: {p.checks.error}</p>;
  if (runs.length === 0) return <p className="rv-sub rv-checks">No checks have run on its head commit.</p>;
  return (
    <ul className="rv-checks" aria-label="Checks on its head commit">
      {runs.map((r) => (
        <li key={r.run}>
          <StatusDot status={r.status} title={r.conclusion ?? r.status} />
          <Link to={`/runs/${r.run}`}>{r.name ?? `run ${r.number}`}</Link>
          <span className="rv-sub">
            {r.provider} · {r.conclusion ?? r.status}
          </span>
        </li>
      ))}
    </ul>
  );
}

function Merge({ repo, p }: { repo: string; p: HostPullDetail }) {
  const [method, setMethod] = React.useState(p.merge_methods[0] ?? "merge");
  const [busy, setBusy] = React.useState(false);
  if (p.state !== "open") return null;
  const merge = async () => {
    setBusy(true);
    try {
      const r = await hostPullsApi.merge(repo, p.number, method);
      toast({
        title: r.merged ? `Merged by ${method} ${acted(r.as)}${r.commit ? `: ${shortSha(r.commit)}` : ""}` : `${HOST_WORD[p.host]} took the merge ${acted(r.as)}`,
        tone: "succeeded",
      });
      refresh(repo, p.number);
    } catch (e) {
      toast.error("It didn't merge", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="rv-land" aria-label="Merging">
      <div className="rv-land-go">
        <p className="rv-land-title">{p.mergeable === false ? `${HOST_WORD[p.host]} says it can't merge yet` : "Merge"}</p>
        {p.merge_methods.length > 1 && (
          <SelectField
            label="How it merges"
            hideLabel
            value={method}
            onChange={setMethod}
            options={p.merge_methods.map((m) => ({
              value: m,
              label: m.replace(/-/g, " "),
            }))}
          />
        )}
        <Button size="sm" disabled={busy || p.merge_methods.length === 0} onClick={() => void merge()}>
          Merge {hostNumber(p.host, p.number)}
        </Button>
      </div>
    </section>
  );
}

function Reviews({ p }: { p: HostPullDetail }) {
  if (p.reviews.length === 0) return null;
  return (
    <ul className="rv-checks" aria-label="Its reviews">
      {p.reviews.map((r, i) => (
        <li key={`${r.author}:${i}`}>
          <StatusDot status={r.state === "approved" ? "succeeded" : r.state === "changes_requested" ? "failed" : "neutral"} title={r.state} />
          <b>{r.author}</b>
          <span className="rv-sub">
            {r.state.replace(/_/g, " ")}
            {r.at ? ` · ${relativeTime(r.at)}` : ""}
            {r.body ? ` · ${r.body}` : ""}
          </span>
        </li>
      ))}
    </ul>
  );
}

/** Comments on the change as a whole, its threads that aren't on a line, and a new comment. */
function Conversation({ repo, p, threads, onReply }: { repo: string; p: HostPullDetail; threads: Thread[]; onReply: (t: Thread, body: string) => Promise<void> }) {
  const [text, setText] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const send = async () => {
    setBusy(true);
    try {
      const r = await hostPullsApi.comment(repo, p.number, {
        body: text.trim(),
      });
      setText("");
      toast({ title: `Commented ${acted(r.as)}`, tone: "succeeded" });
      refresh(repo, p.number);
    } catch (e) {
      toast.error("Your comment didn't go", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="rv-general" aria-label="Conversation">
      {p.conversation.map((c) => (
        <Said key={c.id} c={c} />
      ))}
      {threads.map((t) => (
        <ThreadView key={t.id} t={t} onReply={onReply} now />
      ))}
      <div className="rv-compose">
        <textarea className="rv-compose-input" rows={2} value={text} placeholder="Comment on the change" onChange={(e) => setText(e.target.value)} />
        <div className="rv-compose-acts">
          <Button size="xs" disabled={busy || !text.trim()} onClick={() => void send()}>
            Comment
          </Button>
        </div>
      </div>
    </section>
  );
}

function Said({ c }: { c: HostComment }) {
  return (
    <div className="rv-thread">
      <div className="rv-comment">
        <p className="rv-comment-who">
          <b>{c.author}</b> {c.at && <span className="rv-sub">{relativeTime(c.at)}</span>}
        </p>
        <p className="rv-comment-body">{c.body}</p>
      </div>
    </div>
  );
}

function ReviewForm({ repo, p }: { repo: string; p: HostPullDetail }) {
  const [text, setText] = React.useState("");
  const [verdict, setVerdict] = React.useState<"approve" | "request_changes" | "comment">("comment");
  const [busy, setBusy] = React.useState(false);
  if (p.state !== "open") return null;
  const send = async () => {
    setBusy(true);
    try {
      const r = await hostPullsApi.review(repo, p.number, verdict, text.trim());
      setText("");
      toast({ title: `Review sent ${acted(r.as)}`, tone: "succeeded" });
      refresh(repo, p.number);
    } catch (e) {
      toast.error("Your review didn't go", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="rv-review" aria-label="Your review">
      <p className="rv-land-title">Your review</p>
      <textarea className="rv-compose-input" rows={3} value={text} placeholder="What you think of it, overall (optional to approve)" onChange={(e) => setText(e.target.value)} />
      <div className="rv-compose-acts">
        <SelectField label="Your review" hideLabel value={verdict} onChange={(v) => setVerdict(v as typeof verdict)} options={VERDICTS} />
        <Button size="sm" disabled={busy || (verdict !== "approve" && !text.trim())} onClick={() => void send()}>
          Send review
        </Button>
      </div>
    </section>
  );
}
