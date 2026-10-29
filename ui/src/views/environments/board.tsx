/**
 * The environments board (docs/ci.md W15-R09, W21-R10): every environment of every repository
 * you may read, a row each, by repository: what it deploys to, what runs there (its latest
 * deployment, and kiln CD's apps in it with their revision, health and drift) and what waits to
 * deploy there (for a review, a wait timer, a window, a promotion, a gate or its turn). Each opens
 * its page; a repository's dev, staging and production are made here in one step.
 */
import * as React from "react";
import { Link } from "@/lib/router";

import { appHref, cdApi } from "@/api/cd";
import { ci, waitSentence, type CiDeployment, type CiEnvironment, type CiPreview } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, EmptyState, Panel, SelectField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { useAuth } from "@/lib/auth";
import { reposRes, useRes, type Res } from "@/lib/data";
import { shortSha } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { RelTime } from "@/views/shared";
import { deploymentStatus, ENV_EVENTS, environmentHref, kindLabel, previewOfHref, protectionWords, refName, whereWords } from "./shared";
import "@/views/deployments/deployments.css";
import "./environments.css";

export const boardRes: Res<{ environments: CiEnvironment[] }> = { key: "ci/environments/board", fetch: (s) => ci.environmentsBoard(s), opts: { on: ENV_EVENTS, pollMs: 15_000 } };

/** A deployment, in a line: its state, commit and ref, who made it and when, its run and URL. */
export function Deployed({ d }: { d: CiDeployment }) {
  return (
    <span className="env-deployed">
      <StatusDot status={deploymentStatus(d.state)} title={d.state ?? "no status yet"} />
      <code>{shortSha(d.sha)}</code>
      <span>{refName(d)}</span>
      <span className="env-quiet">
        {d.creator} · <RelTime ts={d.created_at} />
      </span>
      {d.run && (
        <Link to={`/runs/${d.run}${d.leg ? `/jobs/${d.leg}` : ""}`} className="env-link-quiet">
          its run
        </Link>
      )}
      {d.url && (
        <a href={d.url} target="_blank" rel="noreferrer" className="env-link-quiet">
          {d.url.replace(/^https?:\/\//, "")}
        </a>
      )}
    </span>
  );
}

/** What a preview is for (W15-R07), and when it goes unless it's deployed again, or why it went. */
export function PreviewOf({ p }: { p: CiPreview }) {
  const href = previewOfHref(p);
  const of = href ? (
    <Link to={href} className="env-link-quiet">
      {p.of}
    </Link>
  ) : (
    p.of
  );
  return (
    <span className="env-meta">
      Preview of {of}
      {p.kind === "stack" ? "'s stack" : ""}
      {p.removed ? (
        ` · removed: ${p.removed}`
      ) : (
        <>
          {" "}
          · goes <RelTime ts={p.expires_at} /> unless deployed again
        </>
      )}
    </span>
  );
}

/** Whether a deploy file's repository is `repo` (its name, or a URL ending in it). */
const sameRepo = (origin: string, repo: string) => {
  const o = origin.toLowerCase().replace(/\.git$/, "");
  const r = repo.toLowerCase();
  return o === r || o.endsWith(`/${r}`);
};

/** kiln CD's apps in an environment that `repo`'s deploy file makes: revision, health, drift. */
export function EnvApps({ env, repo }: { env: string; repo: string }) {
  const all = useSuspenseResource(`cd/apps/in/${env}`, (s) => cdApi.apps({ environment: [env], limit: 200 }, s).catch(() => ({ apps: [], next: null })), { on: ["cd_apps"], pollMs: 30_000 }).data.apps;
  const apps = all.filter((a) => a.origin.kind === "deploy" && sameRepo(a.origin.repo, repo));
  if (!apps.length) return null;
  return (
    <ul className="env-apps" aria-label={`kiln CD's apps in ${env}`}>
      {apps.map((a) => (
        <li key={a.app}>
          <StatusDot
            status={a.health === "Healthy" ? "succeeded" : a.health === "Degraded" || a.health === "Missing" ? "failed" : a.health === "Progressing" ? "running" : "pending"}
            title={a.health}
          />
          <Link to={appHref(a.app)} className="env-link-quiet">
            {a.name}
          </Link>
          {a.revision && <code>{shortSha(a.revision)}</code>}
          <span className="env-quiet">{a.health.toLowerCase()}</span>
          {a.sync === "OutOfSync" && (
            <span className="env-tag" data-tone="warn" title={`${a.out_of_sync ?? 0} of its objects differ from git`}>
              drifted
            </span>
          )}
          {a.operation?.by && <span className="env-quiet">by {a.operation.by}</span>}
        </li>
      ))}
    </ul>
  );
}

/** What waits to deploy to an environment, each leg with why. */
export function Waits({ e }: { e: CiEnvironment }) {
  const waits = e.waiting ?? [];
  if (!waits.length) return <span className="env-quiet">nothing</span>;
  return (
    <ul className="env-waits" aria-label={`Waiting to deploy to ${e.name}`}>
      {waits.map((w) => (
        <li key={w.leg}>
          <Link to={`/runs/${w.run}/jobs/${w.leg}`} className="env-link">
            #{w.number} {w.name ?? w.key}
          </Link>
          <span className="env-quiet">{waitSentence(w.wait)}</span>
        </li>
      ))}
    </ul>
  );
}

function Row({ e }: { e: CiEnvironment }) {
  const repo = e.repo ?? "";
  const where = whereWords(e);
  const rules = protectionWords(e.protection, e.concurrency);
  return (
    <li className="env-row">
      <div className="env-cell env-who">
        <Link to={environmentHref(repo, e.name)} className="env-name">
          {e.name}
        </Link>
        {e.kind && (
          <span className="env-meta">
            {kindLabel(e.kind)}
            {where ? ` · ${where}` : ""}
          </span>
        )}
        {e.preview && <PreviewOf p={e.preview} />}
        <span className="env-meta">{rules.length ? rules.join(" · ") : "anyone may deploy, any ref"}</span>
      </div>
      <div className="env-cell">
        <span className="env-label">What runs</span>
        {e.latest_deployment ? <Deployed d={e.latest_deployment} /> : <span className="env-quiet">nothing deployed yet</span>}
        <Loadable label={`kiln CD's apps in ${e.name}`} fallback={null}>
          <EnvApps env={e.name} repo={repo} />
        </Loadable>
      </div>
      <div className="env-cell">
        <span className="env-label">Waiting to deploy</span>
        <Waits e={e} />
      </div>
    </li>
  );
}

/** A repository's dev, staging and production in one step, for those who may make them. */
function Starter({ repos }: { repos: string[] }) {
  const [repo, setRepo] = React.useState(repos[0] ?? "");
  const [busy, setBusy] = React.useState(false);
  if (!repos.length) return null;
  const make = async () => {
    setBusy(true);
    try {
      const r = await ci.starterEnvironments(repo);
      toast.success(r.made.length ? `Made ${r.made.join(", ")} in ${r.repo}` : `${r.repo} has them all already`);
      invalidate("ci/environment");
    } catch (err) {
      toast.error(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="env-starter">
      <SelectField label="Repository" value={repo} onChange={setRepo} options={repos.map((r) => ({ value: r, label: r }))} className="env-starter-repo" />
      <Button size="sm" variant="secondary" disabled={busy || !repo} onClick={() => void make()}>
        Make dev, staging and production
      </Button>
    </div>
  );
}

function BoardBody() {
  const rows = useRes(boardRes).data.environments;
  const repos = useRes(reposRes).data;
  const can = useAuth((s) => (s.kind === "authed" ? s.me?.can : undefined));
  const mayMake = !can || Boolean(can["environments.manage"]);
  const byRepo = new Map<string, CiEnvironment[]>();
  for (const e of rows) byRepo.set(e.repo ?? "", [...(byRepo.get(e.repo ?? "") ?? []), e]);
  const starter = mayMake ? repos.map((r) => r.name).filter((n) => !["dev", "staging", "production"].every((x) => byRepo.get(n)?.some((e) => e.name.toLowerCase() === x))) : [];
  return (
    <>
      {starter.length > 0 && (
        <Panel
          eyebrow="Starter set"
          title="Environments for a repository, in one step"
          description="Dev, staging and production: staging and production take only the trunk, production once you approve."
        >
          <Starter repos={starter} />
        </Panel>
      )}
      {rows.length === 0 ? (
        <EmptyState title="No environments yet">
          A job that names an environment makes it, as on GitHub; <code>.kiln/environments.yaml</code> on the trunk defines them, and so do the starter set above and <code>kiln env create</code>.
        </EmptyState>
      ) : (
        [...byRepo.entries()].map(([repo, list]) => (
          <Panel key={repo} eyebrow="Repository" title={repo} className="env-panel">
            <ul className="env-rows" aria-label={`${repo}'s environments`}>
              {list.map((e) => (
                <Row key={e.name} e={e} />
              ))}
            </ul>
          </Panel>
        ))
      )}
    </>
  );
}

export default function EnvironmentsBoard() {
  return (
    <div className="container-console page cd-page">
      <title>Environments · kiln</title>
      <header className="cd-head">
        <Crumbs index={2} items={[{ label: "Deployments", to: "/deployments" }, { label: "Environments" }]} />
        <h1 className="cd-title">Environments</h1>
        <p className="cd-note">Every environment your repositories deploy to: what runs in each, what is drifting, and what waits to deploy there.</p>
      </header>
      <Loadable label="the environments">
        <BoardBody />
      </Loadable>
    </div>
  );
}
