/**
 * Review's front page (W21-R09): what notification rules put in your inbox (W19-R06), then what
 * waits on you (threads whose last word isn't yours, diffs whose review is asked of you), then
 * your own diffs and what waits on others. One list, each row a link to what it's about.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { HOST_WORD, hostNumber, inboxRes, reviewApi, STATE_WORD, whoSaid, type Inbox, type InboxNote, type InboxThread } from "@/api/review";
import { Button } from "@/components/ui/button";
import { Loadable, Loading } from "@/components/ui/loading";
import { PageIntro } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import "./review.css";

export default function InboxPage() {
  return (
    <div className="container-console page rv-page">
      <title>Review · kiln</title>
      <PageIntro
        index={4}
        label="Repositories / Review"
        title="Review."
        lead="Diffs and change requests: what waits on you, and where yours stand."
        actions={
          <Button asChild size="sm" variant="secondary">
            <Link to="/review/queue">Land queue</Link>
          </Button>
        }
      />
      <Loadable label="Loading your inbox" fallback={<Loading label="Loading your inbox" block />}>
        <InboxBody />
      </Loadable>
    </div>
  );
}

function InboxBody() {
  const inbox: Inbox = useRes(inboxRes).data;
  const notes = inbox.notifications ?? [];
  const elsewhere = inbox.elsewhere ?? [];
  const nothing = notes.length + inbox.waiting_for_you.length + inbox.review_requested.length + elsewhere.length + inbox.your_diffs.length + inbox.waiting_for_others.length === 0;
  if (nothing) return <p className="rv-sub">Nothing waits on you, and you have no open diffs.</p>;
  return (
    <div className="rv-inbox">
      <Notes notes={notes} unread={inbox.unread ?? 0} />
      <Section title="Waiting for you" n={inbox.waiting_for_you.length + inbox.review_requested.length + elsewhere.length}>
        {inbox.review_requested.map((r) => (
          <li key={`r${r.diff}`}>
            <Link to={`/review/${r.diff}`} className="rv-inbox-row">
              <span className="rv-inbox-what">
                <b>{r.diff}</b> {r.title}
              </span>
              <span className="rv-sub">
                {r.repo} · {whoSaid(r.author)} asks your review
              </span>
            </Link>
          </li>
        ))}
        {elsewhere.map((p) => (
          <li key={`h${p.repo}#${p.number}`}>
            <Link to={`/review/hosts/${encodeURIComponent(p.repo)}/${p.number}`} className="rv-inbox-row">
              <span className="rv-inbox-what">
                <b>{hostNumber(p.host, p.number)}</b> {p.title}
              </span>
              <span className="rv-sub">
                {p.repo} on {HOST_WORD[p.host]} · {p.author} asks your review
              </span>
            </Link>
          </li>
        ))}
        {inbox.waiting_for_you.map((t) => (
          <ThreadRow key={t.thread} t={t} />
        ))}
      </Section>
      <Section title="Yours" n={inbox.your_diffs.length}>
        {inbox.your_diffs.map((d) => (
          <li key={d.diff}>
            <Link to={`/review/${d.diff}`} className="rv-inbox-row">
              <span className="rv-inbox-what">
                <b>{d.diff}</b> {d.title}
              </span>
              <span className="rv-sub">
                {d.repo} ·{" "}
                <span className="rv-state" data-state={d.state}>
                  {STATE_WORD[d.state] ?? d.state}
                </span>
              </span>
            </Link>
          </li>
        ))}
      </Section>
      <Section title="Waiting for others" n={inbox.waiting_for_others.length}>
        {inbox.waiting_for_others.map((t) => (
          <ThreadRow key={t.thread} t={t} />
        ))}
      </Section>
    </div>
  );
}

/** A notification's link: kiln's own pages in the app, anything else as it is. */
function inApp(url: string): string | null {
  try {
    const u = new URL(url, window.location.origin);
    return u.origin === window.location.origin ? `${u.pathname}${u.search}${u.hash}` : null;
  } catch {
    return null;
  }
}

function Notes({ notes, unread }: { notes: InboxNote[]; unread: number }) {
  const [busy, setBusy] = React.useState(false);
  if (notes.length === 0) return null;
  const readAll = async () => {
    setBusy(true);
    try {
      await reviewApi.readAll();
      invalidate("review/inbox");
    } catch (e) {
      toast.error("That didn't work", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="rv-inbox-section">
      <h2 className="rv-inbox-title">
        Notifications <span className="rv-sub">{unread > 0 ? `${unread} new` : notes.length}</span>
        {unread > 0 && (
          <Button size="sm" variant="ghost" className="rv-inbox-read" disabled={busy} onClick={readAll}>
            Mark all read
          </Button>
        )}
      </h2>
      <ul className="rv-inbox-list">
        {notes.map((n) => {
          const path = inApp(n.url);
          // The row is the link: the line's own copy of it (for chat) isn't shown.
          const line = n.line.endsWith(` · ${n.url}`) ? n.line.slice(0, -(n.url.length + 3)) : n.line;
          const body = (
            <>
              <span className="rv-inbox-what">{n.title}</span>
              <span className="rv-sub">
                {relativeTime(n.at)} · {line}
              </span>
            </>
          );
          return (
            <li key={n.id} data-unread={n.read ? undefined : ""}>
              {path ? (
                <Link to={path} className="rv-inbox-row">
                  {body}
                </Link>
              ) : (
                <a href={n.url} className="rv-inbox-row">
                  {body}
                </a>
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}

function Section({ title, n, children }: { title: string; n: number; children: React.ReactNode }) {
  if (n === 0) return null;
  return (
    <section className="rv-inbox-section">
      <h2 className="rv-inbox-title">
        {title} <span className="rv-sub">{n}</span>
      </h2>
      <ul className="rv-inbox-list">{children}</ul>
    </section>
  );
}

function ThreadRow({ t }: { t: InboxThread }) {
  return (
    <li>
      <Link to={`/review/${t.diff}#rv-t-${t.thread}`} className="rv-inbox-row">
        <span className="rv-inbox-what">
          <b>{t.diff}</b> {t.title}
        </span>
        <span className="rv-sub">
          {t.repo}
          {t.file ? ` · ${t.file}${t.line ? `:${t.line}` : ""}` : ""} · {whoSaid(t.last_author)}: {t.last}
        </span>
      </Link>
    </li>
  );
}
