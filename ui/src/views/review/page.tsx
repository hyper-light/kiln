/**
 * A diff or change request under review (W21-R09, W14-R06): what it is and where it stands, its
 * checks, what keeps it from landing (or the land control), and its files, read version by version,
 * with threads where they're about, the files you've viewed ticked off, and next and previous
 * unresolved a key away (n, p). Your comments wait as drafts until your review sends them together.
 */
import * as React from "react";
import { ArrowDown, ArrowUp } from "lucide-react";
import { Link, useParams } from "@/lib/router";
import { dependenciesRes, diffRes, filesRes, inboxRes, readinessRes, reviewApi, STATE_WORD, threadsRes, whoSaid, type DiffDetail, type ReviewAction, type Thread } from "@/api/review";
import { Button } from "@/components/ui/button";
import { Loadable, Loading } from "@/components/ui/loading";
import { SelectField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { useAction, useHold } from "@/lib/action";
import { useRes } from "@/lib/data";
import { relativeTime, shortSha } from "@/lib/format";
import { errorMessage, useMediaQuery } from "@/lib/hooks";
import { change, invalidate } from "@/lib/resource";
import { useListener } from "@/views/harness/dom";
import { AboutContext } from "@/components/code/about";
import { anchorOf, FileDiff, ThreadView, type Pick } from "./diff";
import { Gates } from "./gates";
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
          // An agent's diff: what's in it can be asked about, the agent answering (components/code/about.tsx).
          <AboutContext.Provider value={d.author.startsWith("agent:") ? { id: d.author.slice("agent:".length), name: whoSaid(d.author), subagents: true, busy: false } : null}>
            <Files diffKey={diffKey} version={version} split={split} whitespace={whitespace} threads={threads} />
          </AboutContext.Provider>
        )}
      </Loadable>
      <Loadable label="Loading its dependency changes" fallback={<></>}>
        <Dependencies diffKey={diffKey} to={compare == null ? version : Math.max(compare, version)} from={compare == null ? null : Math.min(compare, version)} />
      </Loadable>
      <ReviewForm d={d} drafts={drafts} />
    </>
  );
}

/** What the version changes in the dependencies its lockfiles pin (docs/ci.md W18-R03): each
 * lockfile's packages added, removed and changed. Nothing when none did. */
function Dependencies({ diffKey, to, from }: { diffKey: string; to: number; from: number | null }) {
  const deps = useRes(dependenciesRes(diffKey, to, from)).data;
  if (!deps.lockfiles.length) return null;
  return (
    <section className="flex flex-col gap-2 py-3" aria-label="Dependency changes">
      <h3 className="font-mono text-[11px] tracking-[0.08em] text-[#93a1b4] uppercase">Dependencies, from {deps.from}</h3>
      {deps.lockfiles.map((f) => (
        <details key={f.path} open={deps.lockfiles.length === 1} className="min-w-0">
          <summary className="cursor-pointer text-[12.5px] text-[#c5cdd8]">
            <code className="font-mono text-[#e4e5e7]">{f.path}</code>{" "}
            <span className="text-[#8c8f98]">{[f.added && `${f.added} added`, f.removed && `${f.removed} removed`, f.changed && `${f.changed} changed`].filter(Boolean).join(" · ")}</span>
          </summary>
          <ul className="mt-1.5 flex flex-col gap-0.5 pl-4">
            {f.changes.map((c) => (
              <li key={`${c.ecosystem}/${c.name}`} className="flex min-w-0 flex-wrap gap-x-2 font-mono text-[11.5px] text-[#a6b2c6]">
                <span className="[overflow-wrap:anywhere] text-[#d4dde8]">{c.name}</span>
                <span>{c.from.length === 0 ? `added ${c.to.join(", ")}` : c.to.length === 0 ? `removed ${c.from.join(", ")}` : `${c.from.join(", ")} → ${c.to.join(", ")}`}</span>
              </li>
            ))}
          </ul>
        </details>
      ))}
    </section>
  );
}

/** What a diff's author does to it from its head. */
type DiffAct = "ready" | "abandon" | "reclaim";

function Head({ d }: { d: DiffDetail }) {
  // The author's actions are the author's: on yours, or your agents'.
  const you = useRes(inboxRes).data.you;
  const yours = d.author === you || d.author_for === you;
  // Ready, Reclaim and Abandon change one diff's state: one at a time (W21-R15), the one pressed
  // saying it's on its way and the others held until it's back.
  const state = useHold("That didn't work");
  const act = (a: DiffAct) =>
    state.change(a, async () => {
      await reviewApi.act(d.name, a);
      refresh(d.name);
    });
  const hold = (a: DiffAct) => ({ pending: state.doing(a), disabled: state.held(a) });
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
      {yours && (
        <div className="rv-acts">
          {d.state === "draft" && (
            <Button size="xs" variant="secondary" {...hold("ready")} onClick={() => act("ready")}>
              Ready for review
            </Button>
          )}
          {d.state === "abandoned" ? (
            <Button size="xs" variant="ghost" {...hold("reclaim")} onClick={() => act("reclaim")}>
              Reclaim
            </Button>
          ) : (
            d.state !== "landed" && (
              <Button size="xs" variant="ghost" {...hold("abandon")} onClick={() => act("abandon")}>
                Abandon
              </Button>
            )
          )}
        </div>
      )}
    </header>
  );
}

function Checks({ d }: { d: DiffDetail }) {
  if (d.runs.length === 0) return <p className="rv-sub rv-checks">No checks have run on its latest version.</p>;
  const gates = d.runs.flatMap((r) => r.gates ?? []);
  return (
    <>
      <ul className="rv-checks" aria-label="Checks on its latest version">
        {d.runs.map((r) => (
          <li key={r.id}>
            <StatusDot status={r.conclusion ?? r.status} title={r.conclusion ?? r.status} />
            <Link to={`/runs/${r.id}`}>{r.workflow ?? r.name ?? `run ${r.number ?? ""}`}</Link>
            <span className="rv-sub">{r.conclusion ?? r.status}</span>
          </li>
        ))}
      </ul>
      {gates.length > 0 && <Gates gates={gates} />}
    </>
  );
}

function Land({ d }: { d: DiffDetail }) {
  const r = useRes(readinessRes(d.name)).data;
  const [strategy, setStrategy] = React.useState(r.default ?? r.strategies[0] ?? "");
  const [land, landing] = useAction(async () => {
    await reviewApi.land(d.name, strategy || undefined);
    toast({ title: `${d.name} is in the land queue`, tone: "running" });
    refresh(d.name);
  }, "It can't land yet");
  if (d.state === "landed" || d.state === "abandoned") return null;
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
          <Button size="sm" pending={landing} onClick={land}>
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

/** Resolved (or open again) at once (W21-R15), and as it was if that doesn't go: the promise fails then, for the thread to unfold. */
function resolve(key: string, t: Thread, on: boolean): Promise<void> {
  const c = t.comments.at(-1) ?? t.comments[0];
  if (!c) return Promise.resolve();
  change<Thread[]>(threadsRes(key).key, (ts) => ts.map((x) => (x.id === t.id ? { ...x, state: on ? "resolved" : "open" } : x)));
  return reviewApi.editComment(c.id, { state: on ? "resolved" : "open" }).then(
    () => refresh(key),
    (e: unknown) => {
      toast.error(on ? "Couldn't resolve it" : "Couldn't reopen it", errorMessage(e));
      invalidate(`review/threads/${key}`);
      throw e;
    },
  );
}

function Files({ diffKey, version, split, whitespace, threads }: { diffKey: string; version: number; split: boolean; whitespace: boolean; threads: Thread[] }) {
  const res = useRes(filesRes(diffKey, version));
  const v = res.data;
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
  // Ticked at once (W21-R15), and as it was if that doesn't go (the promise fails then, for the file to unfold).
  const viewedOne = (path: string, on: boolean) => {
    res.change((d) => ({ ...d, files: d.files.map((f) => (f.path === path ? { ...f, viewed: on } : f)) }));
    return reviewApi.viewed(diffKey, path, on).then(
      () => invalidate(`review/files/${diffKey}/${version}`),
      (e: unknown) => {
        toast.error("Couldn't mark it viewed", errorMessage(e));
        invalidate(`review/files/${diffKey}/${version}`);
        throw e;
      },
    );
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
          onViewed={viewedOne}
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
  const [send, sending] = useAction(async () => {
    const sent = text;
    const r = await reviewApi.review(d.name, action, text.trim());
    // What's written since stays (W21-R15).
    setText((t) => (t === sent ? "" : t));
    toast({ title: `Review sent: ${d.name} ${STATE_WORD[r.state]?.toLowerCase() ?? r.state}`, tone: "succeeded" });
    refresh(d.name);
  }, "Your review didn't go");
  if (d.state === "landed" || d.state === "abandoned") return null;
  return (
    <section className="rv-review" aria-label="Your review">
      <p className="rv-land-title">Your review</p>
      <textarea className="rv-compose-input" rows={3} value={text} placeholder="What you think of it, overall (optional for Accept)" onChange={(e) => setText(e.target.value)} />
      <div className="rv-compose-acts">
        <SelectField label="Your review" hideLabel value={action} onChange={(v) => setAction(v as ReviewAction)} options={ACTIONS} />
        <Button size="sm" disabled={action !== "accept" && !text.trim() && drafts === 0} pending={sending} onClick={send}>
          Send review{drafts > 0 ? ` with ${drafts} comment${drafts === 1 ? "" : "s"}` : ""}
        </Button>
      </div>
    </section>
  );
}
