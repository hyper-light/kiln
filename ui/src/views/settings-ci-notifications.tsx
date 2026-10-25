/**
 * Settings → You → CI notifications (docs/ci.md W19-R06): a person's own rules, each one or more
 * repository events sent to their inbox (on the review page) or their email, for every repository
 * they may read or the ones named, and only what's about them when they say so; and the address
 * email goes to. Signed in as the owner, it says where kiln's rules are instead.
 */
import * as React from "react";
import { ci, type MyNotifications, type MyRule } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useRes, type Res } from "@/lib/data";

const ID = "you.notifications";
const TITLE = "CI notifications";
const ABOUT = "What kiln tells you of runs and reviews: in your inbox, on the review page, or by email.";
const WORDS = "notify notifications email inbox subscribe runs failed fixed review mention reply";

export const myNotificationsRes: Res<MyNotifications> = { key: "me/notifications", fetch: (s) => ci.myNotifications(s) };
const deliveriesRes: Res<Awaited<ReturnType<typeof ci.deliveries>>> = { key: "ci/notify/deliveries", fetch: (s) => ci.deliveries(30, s) };

export function MyNotificationsHold() {
  return (
    <SettingsGroup id={ID} title={TITLE} about={ABOUT} words={WORDS}>
      <Hold height={120} />
    </SettingsGroup>
  );
}

/** A rule, in a line: its events, where it goes, and what it's held to. */
function described(r: MyRule): { what: string; meta: string } {
  const to = r.to.map((t) => (t === "inbox" ? "your inbox" : "your email")).join(" and ");
  const scope = [r.about_me ? "only about you" : "", r.repos?.length ? r.repos.join(", ") : "every repository you read"].filter(Boolean).join(" · ");
  return { what: r.on.join(", "), meta: `to ${to} · ${scope}` };
}

export function MyNotificationsGroup() {
  const res = useRes(myNotificationsRes);
  const d = res.data;
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const save = async (rules: MyRule[], email?: string | null) => {
    setBusy(true);
    setProblem(null);
    try {
      const next = await ci.setMyNotifications(rules, email);
      res.setData(() => next);
      return true;
    } catch (e) {
      setProblem(errorMessage(e));
      return false;
    } finally {
      setBusy(false);
    }
  };
  if (!d.person) {
    return (
      <SettingsGroup id={ID} title={TITLE} about={ABOUT} words={WORDS}>
        <p className="st-note">
          Signed in as the owner, kiln&rsquo;s rules are the server&rsquo;s ([[ci.notify]] in kiln.toml) and each repository&rsquo;s (.kiln/notify.yaml). People keep their own here.
        </p>
      </SettingsGroup>
    );
  }
  return (
    <SettingsGroup id={ID} title={TITLE} about={ABOUT} words={WORDS}>
      <EmailRow email={d.email} busy={busy} onSave={(e) => save(d.rules, e)} />
      {d.rules.length > 0 && (
        <ul className="st-list mt-2" aria-label="Your rules">
          {d.rules.map((r, i) => {
            const t = described(r);
            return (
              <li key={i}>
                <div className="st-item">
                  <span className="st-item-name font-mono text-[12.5px]">{t.what}</span>
                  <span className="st-item-meta">{t.meta}</span>
                  <span className="st-item-acts">
                    <Button size="xs" variant="ghost" disabled={busy} onClick={() => void save(d.rules.filter((_, j) => j !== i))}>
                      Remove
                    </Button>
                  </span>
                </div>
              </li>
            );
          })}
        </ul>
      )}
      <AddRule events={d.events} hasEmail={Boolean(d.email)} busy={busy} onAdd={(r) => save([...d.rules, r])} />
      {problem && (
        <p className="st-note" data-tone="error" role="status">
          {problem}
        </p>
      )}
    </SettingsGroup>
  );
}

function EmailRow({ email, busy, onSave }: { email: string | null; busy: boolean; onSave: (email: string | null) => Promise<boolean> }) {
  const [draft, setDraft] = React.useState(email ?? "");
  const trimmed = draft.trim();
  return (
    <form
      noValidate
      onSubmit={(e) => {
        e.preventDefault();
        void onSave(trimmed || null);
      }}
    >
      <SettingRow label="Email" htmlFor="st-my-email" help="Where rules that send to your email go.">
        <div className="st-control-line">
          <div className="field st-wide">
            <input id="st-my-email" type="email" inputMode="email" autoComplete="email" placeholder="you@example.com" value={draft} onChange={(e) => setDraft(e.target.value)} />
          </div>
          <Button type="submit" size="xs" variant="secondary" disabled={busy || trimmed === (email ?? "")}>
            Save
          </Button>
        </div>
      </SettingRow>
    </form>
  );
}

function AddRule({ events, hasEmail, busy, onAdd }: { events: string[]; hasEmail: boolean; busy: boolean; onAdd: (r: MyRule) => Promise<boolean> }) {
  const [on, setOn] = React.useState<string[]>([]);
  const [aboutMe, setAboutMe] = React.useState(true);
  const [repos, setRepos] = React.useState("");
  const [to, setTo] = React.useState<("inbox" | "email")[]>(["inbox"]);
  const flip = <T,>(list: T[], v: T, yes: boolean) => (yes ? [...list.filter((x) => x !== v), v] : list.filter((x) => x !== v));
  const ready = on.length > 0 && to.length > 0;
  return (
    <form
      noValidate
      className="mt-4"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!ready) return;
        const list = repos
          .split(",")
          .map((r) => r.trim())
          .filter(Boolean);
        if (await onAdd({ on, to, about_me: aboutMe, ...(list.length ? { repos: list } : {}) })) {
          setOn([]);
          setRepos("");
        }
      }}
    >
      <SettingRow label="Tell me of" help="One rule takes any of these.">
        <fieldset className="flex flex-wrap gap-x-4 gap-y-2" disabled={busy}>
          <legend className="sr-only">Events</legend>
          {events.map((ev) => (
            <label key={ev} className="flex items-center gap-2 font-mono text-[12px]">
              <input type="checkbox" className="check" checked={on.includes(ev)} onChange={(e) => setOn(flip(on, ev, e.target.checked))} />
              {ev}
            </label>
          ))}
        </fieldset>
      </SettingRow>
      <SettingRow inline htmlFor="st-my-about" label="Only what's about me" help="Runs you started, reviews asked of you, replies to and mentions of you.">
        <input id="st-my-about" type="checkbox" className="check" checked={aboutMe} disabled={busy} onChange={(e) => setAboutMe(e.target.checked)} />
      </SettingRow>
      <SettingRow label="Repositories" htmlFor="st-my-repos" help="Names or globs, comma-separated; every one you may read when left empty.">
        <div className="field st-wide">
          <input id="st-my-repos" type="text" placeholder="acme/*" value={repos} disabled={busy} onChange={(e) => setRepos(e.target.value)} />
        </div>
      </SettingRow>
      <SettingRow label="Send to" help={hasEmail ? undefined : "Save your email above to send there."}>
        <fieldset className="flex flex-wrap gap-x-4 gap-y-2" disabled={busy}>
          <legend className="sr-only">Where it goes</legend>
          <label className="flex items-center gap-2">
            <input type="checkbox" className="check" checked={to.includes("inbox")} onChange={(e) => setTo(flip(to, "inbox", e.target.checked))} />
            Inbox
          </label>
          <label className="flex items-center gap-2">
            <input type="checkbox" className="check" checked={to.includes("email")} disabled={!hasEmail} onChange={(e) => setTo(flip(to, "email", e.target.checked))} />
            Email
          </label>
        </fieldset>
      </SettingRow>
      <div className="mt-3 flex justify-end">
        <Button type="submit" size="xs" variant="secondary" disabled={busy || !ready}>
          Add the rule
        </Button>
      </div>
    </form>
  );
}

export function DeliveriesHold() {
  return (
    <SettingsGroup id="ci.deliveries" title="Notifications sent" words="notifications deliveries sent failed channel">
      <Hold height={96} />
    </SettingsGroup>
  );
}

/** What the server's, the repositories' and people's rules sent (W19-R06), newest first, and why any didn't go. */
export function DeliveriesGroup() {
  const d = useRes(deliveriesRes).data;
  return (
    <SettingsGroup
      id="ci.deliveries"
      title="Notifications sent"
      about="The last deliveries of every rule: the server's ([[ci.notify]]), each repository's (.kiln/notify.yaml) and people's own."
      words="notifications deliveries sent failed channel rule"
    >
      {d.deliveries.length === 0 ? (
        <p className="st-note">Nothing sent yet.</p>
      ) : (
        <ul className="st-list" aria-label="Deliveries">
          {d.deliveries.map((x, i) => (
            <li key={`${x.ts}-${i}`}>
              <div className="st-item">
                <i className="dot" data-status={x.status === "sent" ? "succeeded" : "failed"} aria-hidden="true" />
                <span className="st-item-name font-mono text-[12px]">{x.event}</span>
                <span className="st-item-meta">
                  to {x.channel} · {x.rule} · {relativeTime(x.ts)}
                  {x.status === "sent" ? "" : ` · didn't go${x.error ? `: ${x.error}` : ""}${x.attempts > 1 ? ` (${x.attempts} tries)` : ""}`}
                </span>
              </div>
            </li>
          ))}
        </ul>
      )}
    </SettingsGroup>
  );
}
