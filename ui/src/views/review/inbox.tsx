/**
 * Review's front page (W21-R09): what waits on you first (threads whose last word isn't yours,
 * diffs whose review is asked of you), then your own diffs and what waits on others. One list,
 * each row a link to the diff, the line or the thread it's about.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { inboxRes, STATE_WORD, whoSaid, type Inbox, type InboxThread } from "@/api/review";
import { Button } from "@/components/ui/button";
import { Loadable, Loading } from "@/components/ui/loading";
import { PageIntro } from "@/components/ui/misc";
import { useRes } from "@/lib/data";
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
  const nothing = inbox.waiting_for_you.length + inbox.review_requested.length + inbox.your_diffs.length + inbox.waiting_for_others.length === 0;
  if (nothing) return <p className="rv-sub">Nothing waits on you, and you have no open diffs.</p>;
  return (
    <div className="rv-inbox">
      <Section title="Waiting for you" n={inbox.waiting_for_you.length + inbox.review_requested.length}>
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
