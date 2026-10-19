/**
 * A diff or change request under review (W21-R09, W14-R06): what it is and where it stands, its
 * checks, what keeps it from landing (or the land control), and its files, read version by version,
 * with threads where they're about, the files you've viewed ticked off, and next and previous
 * unresolved a key away (n, p). Your comments wait as drafts until your review sends them together.
 */
import * as React from "react";
import { ArrowDown, ArrowUp } from "lucide-react";
import { Link, useParams } from "@/lib/router";
import { diffRes, filesRes, readinessRes, reviewApi, STATE_WORD, threadsRes, whoSaid, type DiffDetail, type ReviewAction, type Thread } from "@/api/review";
import { Button } from "@/components/ui/button";
import { Loadable, Loading } from "@/components/ui/loading";
import { SelectField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { relativeTime, shortSha } from "@/lib/format";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { useListener } from "@/views/harness/dom";
import { anchorOf, FileDiff, ThreadView, type Pick } from "./diff";
import { findMoves, parsePatch } from "./patch";
import "./review.css";

const Markdown = React.lazy(() => import("@/lib/markdown").then((m) => ({ default: m.MarkdownBody })));

export default function ReviewPage() {
  const { key = "" } = useParams();
  return (
    <div className="container-console page rv-page">
      <title>{`${key} · Review · kiln`}</title>
      <Loadable label="Loading the review" fallback={<Loading label="Loading the review" block />}>
        <Review key={key} diffKey={key} />
      </Loadable>
    </div>
  );
}

const refresh = (key: string) => {
  invalidate(`review/diff/${key}`);
  invalidate(`review/threads/${key}`);
  invalidate(`review/land/${key}`);
  invalidate("review/inbox");
};

function Review({ diffKey }: { diffKey: string }) {
  const d = useRes(diffRes(diffKey)).data;
  const threads = useRes(threadsRes(diffKey)).data;
  const latest = d.versions.at(-1)?.n ?? 1;
  const [version, setVersion] = React.useState(latest);
  const [compare, setCompare] = React.useState<number | null>(null);
  const phone = useMediaQuery("(max-width: 760px)");
  const [splitPref, setSplitPref] = React.useState(false);
  const split = splitPref && !phone;
  const [whitespace, setWhitespace] = React.useState(false);
  const general = threads.filter((t) => t.file == null);
  const drafts = threads.reduce((a, t) => a + t.comments.filter((c) => c.draft).length, 0);
  const versionOptions = d.versions.map((v) => ({ value: String(v.n), label: `v${v.n}${v.n === latest ? " (latest)" : ""}${v.rebase_only ? " · rebase only" : ""} · ${shortSha(v.commit)}` }));
  return (
    <>
      <Head d={d} />
      <Checks d={d} />
      <React.Suspense fallback={null}>
        <Land d={d} />
      </React.Suspense>
      <div className="rv-bar" role="toolbar" aria-label="How the diff shows">
        <SelectField label="Version" value={String(version)} onChange={(v) => setVersion(Number(v))} options={versionOptions} />
        {d.versions.length > 1 && (
          <SelectField
            label="Compare"
            value={compare == null ? "" : String(compare)}
            onChange={(v) => setCompare(v ? Number(v) : null)}
            options={[{ value: "", label: "Its changes" }, ...d.versions.filter((v) => v.n !== version).map((v) => ({ value: String(v.n), label: `With v${v.n}` }))]}
          />
        )}
        {!phone && compare == null && (
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
        {compare == null && (
          <label className="rv-check">
            <input type="checkbox" className="check" checked={whitespace} onChange={(e) => setWhitespace(e.target.checked)} />
            Hide whitespace changes
          </label>
        )}
      </div>
      <section className="rv-general">
        {general.map((t) => (
          <ThreadView key={t.id} t={t} onReply={(th, body, publish) => reply(diffKey, th, body, publish)} onResolve={(th, on) => resolve(diffKey, th, on)} />
        ))}
      </section>
      <Loadable label="Loading its files" fallback={<Loading label="Loading its files" block />}>
        {compare != null ? (
          <Interdiff diffKey={diffKey} from={Math.min(compare, version)} to={Math.max(compare, version)} />
        ) : (
          <Files diffKey={diffKey} version={version} split={split} whitespace={whitespace} threads={threads} />
        )}
      </Loadable>
      <ReviewForm d={d} drafts={drafts} />
    </>
  );
}

function Head({ d }: { d: DiffDetail }) {
  const [busy, setBusy] = React.useState(false);
  const act = async (a: "ready" | "abandon" | "reclaim" | "commandeer") => {
    setBusy(true);
    try {
      await reviewApi.act(d.name, a);
      refresh(d.name);
    } catch (e) {
      toast.error("That didn't work", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const agent = d.author_kind === "agent" || d.author.startsWith("agent:");
  return (
    <header className="rv-head">
      <p className="rv-eyebrow">
        <Link to="/repos">Repositories</Link> / <Link to="/review">Review</Link> / {d.repo} / {d.name}
      </p>
      <h1 className="rv-title">{d.title || d.name}</h1>
      <p className="rv-meta">
        <span className="rv-state" data-state={d.state}>
          {STATE_WORD[d.state] ?? d.state}
        </span>
        <span>
          by {whoSaid(d.author)}
          {agent && d.author_for ? ` for ${whoSaid(d.author_for)}` : ""}
        </span>
        <span>
          {d.kind === "change_request" && d.branch ? `${d.branch} → ` : "onto "}
          {d.trunk}
        </span>
        <span>updated {relativeTime(d.updated_at)}</span>
        {d.stack && (
          <Link to={`/review/stacks/${encodeURIComponent(d.stack)}`} className="rv-link">
            {d.parent ? `In a stack, above ${d.parent}` : "In a stack"}
          </Link>
        )}
      </p>
      {d.summary.trim() && (
        <div className="rv-summary">
          <React.Suspense fallback={<p className="rv-sub">{d.summary}</p>}>
            <Markdown text={d.summary} />
          </React.Suspense>
        </div>
      )}
      <div className="rv-acts">
        {d.state === "draft" && (
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => void act("ready")}>
            Ready for review
          </Button>
        )}
        {d.state === "abandoned" ? (
          <Button size="xs" variant="ghost" disabled={busy} onClick={() => void act("reclaim")}>
            Reclaim
          </Button>
        ) : (
          d.state !== "landed" && (
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => void act("abandon")}>
              Abandon
            </Button>
          )
        )}
      </div>
    </header>
  );
}

function Checks({ d }: { d: DiffDetail }) {
  if (d.runs.length === 0) return <p className="rv-sub rv-checks">No checks have run on its latest version.</p>;
  return (
    <ul className="rv-checks" aria-label="Checks on its latest version">
      {d.runs.map((r) => (
        <li key={r.id}>
          <StatusDot status={r.conclusion ?? r.status} title={r.conclusion ?? r.status} />
          <Link to={`/runs/${r.id}`}>{r.workflow ?? r.name ?? `run ${r.number ?? ""}`}</Link>
          <span className="rv-sub">{r.conclusion ?? r.status}</span>
        </li>
      ))}
    </ul>
  );
}

function Land({ d }: { d: DiffDetail }) {
  const r = useRes(readinessRes(d.name)).data;
  const [strategy, setStrategy] = React.useState(r.default ?? r.strategies[0] ?? "");
  const [busy, setBusy] = React.useState(false);
  if (d.state === "landed" || d.state === "abandoned") return null;
  const land = async () => {
    setBusy(true);
    try {
      await reviewApi.land(d.name, strategy || undefined);
      toast({ title: `${d.name} is in the land queue`, tone: "running" });
      refresh(d.name);
    } catch (e) {
      toast.error("It can't land yet", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="rv-land" aria-label="Landing">
      {r.blockers.length > 0 ? (
        <>
          <p className="rv-land-title">Can't land yet</p>
          <ul className="rv-blockers">
            {r.blockers.map((b) => (
              <li key={b}>{b}</li>
            ))}
          </ul>
        </>
      ) : (
        <div className="rv-land-go">
          <p className="rv-land-title">Ready to land</p>
          {r.strategies.length > 1 && (
            <SelectField label="How it lands" hideLabel value={strategy} onChange={setStrategy} options={r.strategies.map((s) => ({ value: s, label: s.replace(/-/g, " ") }))} />
          )}
          <Button size="sm" disabled={busy} onClick={() => void land()}>
            Land {d.name}
          </Button>
        </div>
      )}
    </section>
  );
}

async function reply(key: string, t: Thread, body: string, publish: boolean) {
  await reviewApi.comment(key, { body, publish, thread: t.id });
  refresh(key);
}

async function resolve(key: string, t: Thread, on: boolean) {
  const c = t.comments.at(-1) ?? t.comments[0];
  if (!c) return;
  await reviewApi.editComment(c.id, { state: on ? "resolved" : "open" });
  refresh(key);
}

function Files({ diffKey, version, split, whitespace, threads }: { diffKey: string; version: number; split: boolean; whitespace: boolean; threads: Thread[] }) {
  const v = useRes(filesRes(diffKey, version)).data;
  const files = React.useMemo(() => parsePatch(v.patch), [v.patch]);
  const moves = React.useMemo(() => findMoves(files), [files]);
  const meta = React.useMemo(() => new Map(v.files.map((f) => [f.path, f])), [v.files]);
  const viewed = v.files.filter((f) => f.viewed).length;
  // Open threads in the order they come down the page, for n and p.
  const order = React.useMemo(() => {
    const at = new Map(files.map((f, i) => [f.path, i]));
    return threads
      .filter((t) => t.state !== "resolved" && t.file != null && anchorOf(t, version))
      .sort((a, b) => (at.get(a.file ?? "") ?? 0) - (at.get(b.file ?? "") ?? 0) || (anchorOf(a, version)?.end ?? 0) - (anchorOf(b, version)?.end ?? 0))
      .map((t) => t.id);
  }, [threads, files, version]);
  const [cursor, setCursor] = React.useState(-1);
  const go = (by: 1 | -1) => {
    if (order.length === 0) return;
    const next = (cursor + by + order.length) % order.length;
    setCursor(next);
    document.getElementById(`rv-t-${order[next] ?? ""}`)?.scrollIntoView({ block: "center", behavior: "smooth" });
  };
  useListener<KeyboardEvent>(window, "keydown", (e) => {
    const t = e.target as HTMLElement | null;
    if (e.metaKey || e.ctrlKey || e.altKey || t?.closest("input, textarea, select, [contenteditable]")) return;
    if (e.key === "n") go(1);
    else if (e.key === "p") go(-1);
  });
  const viewedOne = async (path: string, on: boolean) => {
    try {
      await reviewApi.viewed(diffKey, path, on);
      invalidate(`review/files/${diffKey}/${version}`);
    } catch (e) {
      toast.error("Couldn't mark it viewed", errorMessage(e));
    }
  };
  const comment = async (path: string, pick: Pick, body: string, publish: boolean) => {
    await reviewApi.comment(diffKey, { body, publish, version, file: path, side: pick.side, start_line: pick.start, end_line: pick.end });
    refresh(diffKey);
  };
  return (
    <>
      <div className="rv-filebar">
        <span className="rv-sub">
          {files.length} file{files.length === 1 ? "" : "s"} · {viewed} viewed
        </span>
        {order.length > 0 && (
          <span className="rv-unresolved">
            <span className="rv-sub">
              {order.length} unresolved {order.length === 1 ? "thread" : "threads"}
            </span>
            <Button size="xs" variant="ghost" onClick={() => go(-1)} aria-label="Previous unresolved thread (p)">
              <ArrowUp /> Previous
            </Button>
            <Button size="xs" variant="ghost" onClick={() => go(1)} aria-label="Next unresolved thread (n)">
              <ArrowDown /> Next
            </Button>
          </span>
        )}
      </div>
      {v.truncated && <p className="st-note">This patch is too big to show whole: its first 8 MiB are below.</p>}
      {files.map((f, i) => (
        <FileDiff
          key={`${version}:${f.path}`}
          idx={i}
          file={f}
          meta={meta.get(f.path)}
          diffKey={diffKey}
          version={version}
          split={split}
          whitespace={whitespace}
          moves={moves}
          pathOf={(k) => files[k]?.path ?? ""}
          threads={threads.filter((t) => t.file === f.path)}
          onViewed={(path, on) => void viewedOne(path, on)}
          onComment={(pick, body, publish) => comment(f.path, pick, body, publish)}
          onReply={(t, body, publish) => reply(diffKey, t, body, publish)}
          onResolve={(t, on) => resolve(diffKey, t, on)}
        />
      ))}
    </>
  );
}

function Interdiff({ diffKey, from, to }: { diffKey: string; from: number; to: number }) {
  const r = useRes({ key: `review/interdiff/${diffKey}/${from}/${to}`, fetch: (s) => reviewApi.interdiff(diffKey, from, to, s) }).data;
  return (
    <section className="rv-interdiff">
      <p className="rv-sub">
        v{from} → v{to}: {r.same_patch ? "the same patch, on a new base (a rebase only)." : "what changed between them, commit by commit (rebase noise left out)."}
      </p>
      {!r.same_patch && <pre className="rv-range">{r.range_diff || "No differences."}</pre>}
    </section>
  );
}

const ACTIONS: { value: ReviewAction; label: string }[] = [
  { value: "comment", label: "Comment" },
  { value: "accept", label: "Accept" },
  { value: "request_changes", label: "Request changes" },
];

/** Your review: a few words, how it comes down, and your drafts sent with it. */
function ReviewForm({ d, drafts }: { d: DiffDetail; drafts: number }) {
  const [text, setText] = React.useState("");
  const [action, setAction] = React.useState<ReviewAction>("comment");
  const [busy, setBusy] = React.useState(false);
  if (d.state === "landed" || d.state === "abandoned") return null;
  const send = async () => {
    setBusy(true);
    try {
      const r = await reviewApi.review(d.name, action, text.trim());
      setText("");
      toast({ title: `Review sent: ${d.name} ${STATE_WORD[r.state]?.toLowerCase() ?? r.state}`, tone: "succeeded" });
      refresh(d.name);
    } catch (e) {
      toast.error("Your review didn't go", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="rv-review" aria-label="Your review">
      <p className="rv-land-title">Your review</p>
      <textarea className="rv-compose-input" rows={3} value={text} placeholder="What you think of it, overall (optional for Accept)" onChange={(e) => setText(e.target.value)} />
      <div className="rv-compose-acts">
        <SelectField label="Your review" hideLabel value={action} onChange={(v) => setAction(v as ReviewAction)} options={ACTIONS} />
        <Button size="sm" disabled={busy || (action !== "accept" && !text.trim() && drafts === 0)} onClick={() => void send()}>
          Send review{drafts > 0 ? ` with ${drafts} comment${drafts === 1 ? "" : "s"}` : ""}
        </Button>
      </div>
    </section>
  );
}
