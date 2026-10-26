/**
 * Going back (docs/cd.md, "Rollback"), in the app's page.
 *
 * - **Roll back…** opens in place, under the app's actions: every deployment that succeeded and
 *   can be rendered again, newest first, one row each with its commit, the tags that name it and
 *   its message's first line. A tag, a commit or words of the message find one. The one chosen
 *   shows its whole commit message beside the list (under it on a phone) before anything is asked.
 *   The list and the message each keep one height, so choosing, finding and reading move nothing.
 * - **Rollback** in the app's lines says whether kiln goes back on its own when a deploy fails, and
 *   who said so; changing it, for the app or for every app of its repository, opens in place too.
 */
import * as React from "react";
import { Search, Undo2 } from "lucide-react";

import type { CdAppDetail, CdAutoRollback, CdCommit, CdDeployment, CdRollbackTargets } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Confirm } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { useRes, type Res } from "@/lib/data";
import { RelTime } from "@/views/shared";
import { TRIGGER_WORD, shortRev } from "./parts";

/** A row of the list, in px (`.cd-rb-row`): the list is as tall as its rows, at most this many of them. */
const ROW = 58;
const ROWS = 6;

/** The commit a deployment ran, short: its commit's when kiln read it, else its revision. */
const shaOf = (d: CdDeployment) => shortRev(d.commit?.sha ?? d.revision) || "no revision";

/** What a deployment is called on its button: its first tag, else its commit. */
export const rollbackName = (d: CdDeployment) => d.tags?.[0] ?? shaOf(d);

/** A repository as people say it: hyper-light/deploy, not its URL. */
export const repoName = (url: string) => url.replace(/^[a-z+]+:\/\/(www\.)?(github\.com\/)?/i, "").replace(/\.git$/, "");

/** The tags that name a commit, as chips: the first few, then how many more. */
export function TagChips({ tags, max = 3 }: { tags?: string[]; max?: number }) {
  if (!tags?.length) return null;
  const shown = tags.slice(0, max);
  return (
    <span className="cd-tags">
      {shown.map((t) => (
        <span key={t} className="chip cd-tag" title={`tag ${t}`}>
          {t}
        </span>
      ))}
      {tags.length > shown.length && (
        <span className="cd-meta" title={tags.slice(max).join(", ")}>
          +{tags.length - shown.length}
        </span>
      )}
    </span>
  );
}

/** A commit's whole message: its first line, then the rest as it was written. */
export function CommitMessage({ c, revision }: { c?: CdCommit; revision?: string | null }) {
  if (!c) {
    return (
      <p className="cd-dim">
        {revision ? `kiln has no commit message for ${shortRev(revision)}: it isn't a commit kiln can read (a chart's version, or a repository it can't reach).` : "kiln has no commit for it."}
      </p>
    );
  }
  return (
    <div className="cd-commit">
      <p className="cd-commit-subject">{c.subject || "(no message)"}</p>
      {c.body && <p className="cd-commit-body">{c.body}</p>}
      <p className="cd-meta">
        {c.author} · committed <RelTime ts={Math.floor(c.at_ms / 1000)} /> · {c.sha}
      </p>
    </div>
  );
}

/** Whether a deployment is what `q` asks for: its tag, its commit's first digits, its number, or words of its message. */
function finds(d: CdDeployment, q: string) {
  if (!q) return true;
  const lower = q.toLowerCase();
  const shas = [d.revision, d.commit?.sha, ...(d.commits ?? [])].filter((s): s is string => !!s).map((s) => s.toLowerCase());
  return (
    shas.some((s) => s.startsWith(lower)) ||
    (d.tags ?? []).some((t) => t.toLowerCase().startsWith(lower)) ||
    `#${d.id}` === lower ||
    String(d.id) === lower ||
    `${d.commit?.subject ?? ""}\n${d.commit?.body ?? ""}`.toLowerCase().includes(lower)
  );
}

/**
 * Roll back, asked in place: what runs now, the deployments it can go back to (found by tag,
 * commit or message), the chosen one's whole commit message, prune, and the button that goes.
 */
export function RollbackPanel({
  res,
  name,
  busy,
  onRollback,
  onCancel,
}: {
  res: Res<CdRollbackTargets>;
  name: string;
  busy: boolean;
  onRollback: (d: CdDeployment, prune: boolean) => void;
  onCancel: () => void;
}) {
  const { targets, current, kept } = useRes(res).data;
  const [q, setQ] = React.useState("");
  const [picked, setPicked] = React.useState<number | null>(null);
  const [prune, setPrune] = React.useState(false);
  const inputId = React.useId();
  const query = q.trim();
  const shown = targets.filter((d) => finds(d, query));
  const chosen = targets.find((d) => d.id === picked) ?? null;
  // One height whatever is found or chosen: as tall as all its rows, at most ROWS of them.
  const box = { "--cd-rb-h": `${Math.max(Math.min(targets.length, ROWS), 2) * ROW + 2}px` } as React.CSSProperties;
  return (
    <Confirm
      className="cd-rollback"
      label={`Roll back ${name}`}
      actions={
        <>
          <Button size="xs" onClick={() => chosen && onRollback(chosen, prune)} disabled={busy || !chosen}>
            <Undo2 aria-hidden="true" /> {chosen ? `Roll back to ${rollbackName(chosen)}` : "Roll back"}
          </Button>
          <Button size="xs" variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
        </>
      }
    >
      <p>
        Go back to a deployment that succeeded. kiln renders its commits again and syncs them, with the artifacts it deployed then; {name} holds there until git changes it, then follows git again.
      </p>
      {current && (
        <p className="cd-rb-now">
          <span className="cd-rb-now-label">Runs now</span>
          <span className="cd-mono">
            #{current.id} · {shaOf(current)}
          </span>
          <TagChips tags={current.tags} max={2} />
          <span className="cd-rb-now-text">{current.commit?.subject ?? current.revision ?? ""}</span>
        </p>
      )}
      {targets.length === 0 ? (
        <p className="cd-empty cd-rb-none">
          Nothing earlier to roll back to: no other deployment of {name} succeeded{kept ? ` among the last ${kept} kiln keeps` : ""}.
        </p>
      ) : (
        <div className="cd-rb">
          <div className="cd-rb-find">
            <label htmlFor={inputId} className="field-label">
              Find a tag, commit or message
            </label>
            <div className="field">
              <Search size={14} aria-hidden="true" />
              <input
                id={inputId}
                type="search"
                value={q}
                onChange={(e) => setQ(e.target.value)}
                placeholder="v1.4.2, 3f2a9c1, fix the probe"
                autoComplete="off"
                autoCapitalize="off"
                spellCheck={false}
              />
            </div>
          </div>
          <div className="cd-rb-body" style={box}>
            <div className="cd-rb-list" role="radiogroup" aria-label="Deployments to roll back to">
              {shown.length === 0 ? (
                <p className="cd-rb-nomatch">
                  No deployment kiln can go back to ran {query}. kiln rolls back only to what it deployed and kept: the last {kept} deployments of {name}.
                </p>
              ) : (
                shown.map((d) => (
                  <label key={d.id} className="cd-rb-row" data-picked={d.id === picked ? "" : undefined}>
                    <input type="radio" className="cd-radio" name={`${inputId}-to`} checked={d.id === picked} onChange={() => setPicked(d.id)} />
                    <span className="cd-rb-what">
                      <span className="cd-rb-line">
                        <span className="cd-mono cd-rb-sha">{shaOf(d)}</span>
                        <TagChips tags={d.tags} max={2} />
                        <span className="cd-meta cd-rb-id">#{d.id}</span>
                      </span>
                      <span className="cd-rb-subject">{d.commit?.subject ?? d.revision ?? ""}</span>
                    </span>
                    <RelTime ts={Math.floor(d.at_ms / 1000)} className="cd-meta cd-rb-at" />
                  </label>
                ))
              )}
            </div>
            <div className="cd-rb-detail" aria-live="polite">
              {chosen ? (
                <>
                  <p className="cd-rb-detail-head">
                    <span className="cd-mono">
                      Deployment #{chosen.id} · {shaOf(chosen)}
                    </span>
                    <TagChips tags={chosen.tags} max={6} />
                  </p>
                  <CommitMessage c={chosen.commit} revision={chosen.revision} />
                  <p className="cd-meta">
                    Deployed <RelTime ts={Math.floor(chosen.at_ms / 1000)} />
                    {chosen.by ? ` by ${chosen.by}` : ""} · {(TRIGGER_WORD[chosen.trigger] ?? chosen.trigger).toLowerCase()} · {chosen.applied} applied
                    {chosen.pruned ? ` · ${chosen.pruned} pruned` : ""}
                  </p>
                </>
              ) : (
                <p className="cd-rb-hint">Choose a deployment to read its commit's whole message here.</p>
              )}
            </div>
          </div>
          <p className="cd-meta cd-rb-count">
            {query ? `${shown.length} of ${targets.length}` : `${targets.length}`} to go back to · kiln keeps the last {kept} deployments
          </p>
        </div>
      )}
      <label className="cd-opt">
        <input type="checkbox" className="check" checked={prune} onChange={(e) => setPrune(e.target.checked)} /> Also delete what it didn't have (prune)
      </label>
    </Confirm>
  );
}

/** What a scope left unset falls back to: the repository's, the deploy file's, or kiln's default; with its name, and its choice's label. */
function inherited(a: CdAutoRollback, scope: "app" | "repo") {
  if (scope === "app" && a.repo_setting) return { on: a.repo_setting.on, from: repoName(a.repo ?? ""), label: `As ${repoName(a.repo ?? "")}` };
  if (a.deploy_file !== null) return { on: a.deploy_file, from: "its deploy file", label: "As its deploy file" };
  return { on: true, from: "kiln's default", label: "kiln's default" };
}

type Choice = "inherit" | "on" | "off";
const choiceOf = (s: { on: boolean } | null): Choice => (s ? (s.on ? "on" : "off") : "inherit");
const onOf = (c: Choice) => (c === "inherit" ? null : c === "on");

/** Whether kiln goes back on its own, in the app's lines: on or off, who said so, and Change. */
export function AutoRollbackLine({ a, open, onToggle }: { a: CdAppDetail; open: boolean; onToggle: () => void }) {
  const r = a.auto_rollback;
  if (a.shadow || a.origin.kind === "argo") return <span className="cd-dim">Argo CD's: kiln never deploys this app, so it never rolls it back on its own</span>;
  if (!r) return <span className="cd-dim">Automatic: back to what ran before when a deploy fails</span>;
  return (
    <span className="cd-policy">
      <span>
        {r.on ? "Automatic: back to what ran before when a deploy fails" : "Off: a deploy that fails stays until someone rolls it back"}
        <span className="cd-dim"> · {r.why}</span>
      </span>
      <button type="button" className="cd-link" onClick={onToggle} aria-expanded={open}>
        Change…
      </button>
    </span>
  );
}

/**
 * Auto-rollback, changed in place: for this app (or following its repository), and for every app
 * of its repository. Each change is made as it is chosen, as the sync policy's switches are.
 */
export function AutoRollbackPanel({
  a,
  name,
  busy,
  onSetApp,
  onSetRepo,
  onClose,
}: {
  a: CdAppDetail;
  name: string;
  busy: boolean;
  onSetApp: (on: boolean | null, said: string) => void;
  onSetRepo: (repo: string, on: boolean | null, said: string) => void;
  onClose: () => void;
}) {
  const r = a.auto_rollback;
  if (!r) return null;
  const repo = r.repo ? repoName(r.repo) : null;
  const app = inherited(r, "app");
  const fromRepo = inherited(r, "repo");
  const word = (on: boolean) => (on ? "on" : "off");
  return (
    <Confirm
      className="cd-autorb"
      label="Auto-rollback"
      actions={
        <Button size="xs" variant="ghost" onClick={onClose}>
          Done
        </Button>
      }
    >
      <p>
        When a deploy of {name} fails (its rollout aborts, a release fails after its commit, or kiln itself doesn't come back after a restart), kiln goes back to what ran before and holds there until
        git changes. Off, what failed stays as it is until someone rolls it back; a kiln that can't start stays down.
      </p>
      <div className="cd-autorb-field">
        <span className="field-label">This app</span>
        <Segmented
          label={`Auto-rollback for ${name}`}
          value={choiceOf(r.app)}
          onChange={(c) =>
            !busy && c !== choiceOf(r.app) && onSetApp(onOf(c), c === "inherit" ? `${name} follows ${app.from} again: auto-rollback ${word(app.on)}.` : `Auto-rollback ${c} for ${name}.`)
          }
          options={[
            { value: "inherit", label: `${app.label} (${word(app.on)})` },
            { value: "on", label: "On" },
            { value: "off", label: "Off" },
          ]}
        />
      </div>
      {repo && r.repo && (
        <div className="cd-autorb-field">
          <span className="field-label">Every app of {repo}</span>
          <Segmented
            label={`Auto-rollback for every app of ${repo}`}
            value={choiceOf(r.repo_setting)}
            onChange={(c) =>
              !busy &&
              c !== choiceOf(r.repo_setting) &&
              r.repo &&
              onSetRepo(r.repo, onOf(c), c === "inherit" ? `${repo} no longer says: its apps follow ${fromRepo.from} (${word(fromRepo.on)}).` : `Auto-rollback ${c} for every app of ${repo}.`)
            }
            options={[
              { value: "inherit", label: `Not set (${word(fromRepo.on)})` },
              { value: "on", label: "On" },
              { value: "off", label: "Off" },
            ]}
          />
          {r.app && <p className="field-hint">{name} says for itself, so its repository's doesn't change it.</p>}
        </div>
      )}
    </Confirm>
  );
}
