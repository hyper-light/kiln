/**
 * An environment's page (docs/ci.md W15-R08, W21-R10), in place: what it deploys to and where
 * (its kind, and pickers of kiln CD's clusters, the runners' labels and kiln's integrations), who
 * may deploy there and when (its protection), what waits to deploy, kiln CD's apps there, its
 * deployments, and its secrets and variables. One from `.kiln/environments.yaml` is changed there;
 * one made here (or named by a job) is changed and removed here.
 */
import * as React from "react";
import { Link, useNavigate, useParams, useSearchParams } from "@/lib/router";

import { api } from "@/api/client";
import { cdApi } from "@/api/cd";
import { deployApi } from "@/api/deploy";
import { ci, isProviderEnvironment, type CiEnvironment, type CiEnvKind, type CiEnvTargets } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { Confirm, Crumbs, Facts, Panel, SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { agentsRes, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { ScopeSecrets } from "@/views/settings-ci";
import { Deployed, EnvApps, PreviewOf, Waits } from "./board";
import { ENV_EVENTS, environmentsHref, KINDS, kindLabel, protectionWords, SOURCE_WORDS, whereWords } from "./shared";
import "@/views/deployments/deployments.css";
import "./environments.css";

/** Names in a field, separated by commas. */
const names = (s: string) =>
  s
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);

/** kiln CD's clusters, as checkboxes; none known (CD off), a field of names. */
function Clusters({ value, onChange, disabled }: { value: string[]; onChange: (v: string[]) => void; disabled: boolean }) {
  const known = useSuspenseResource("cd/clusters", (s) => cdApi.clusters(s).catch(() => ({ clusters: [] })), { pollMs: 60_000 }).data.clusters;
  const [text, setText] = React.useState(value.join(", "));
  if (!known.length) {
    return (
      <TextField
        label="Clusters"
        hint="kiln CD's clusters it deploys to, by name, separated by commas."
        value={text}
        disabled={disabled}
        onChange={(e) => {
          setText(e.target.value);
          onChange(names(e.target.value));
        }}
      />
    );
  }
  const all = [...new Set([...known.map((c) => c.name), ...value])];
  return (
    <fieldset className="env-checks" disabled={disabled}>
      <legend className="field-label">Clusters</legend>
      {all.map((c) => (
        <label key={c} className="env-check">
          <input type="checkbox" checked={value.includes(c)} onChange={(e) => onChange(e.target.checked ? [...value, c] : value.filter((x) => x !== c))} />
          <span>{c}</span>
          {!known.some((k) => k.name === c) && <span className="env-quiet">not one kiln CD knows</span>}
        </label>
      ))}
    </fieldset>
  );
}

/** The runners' labels, to pick one the environment's runners carry. */
function LabelPicker({ onPick, disabled }: { onPick: (label: string) => void; disabled: boolean }) {
  const agents = useRes(agentsRes).data;
  const labels = [...new Set(agents.flatMap((a) => a.labels))].sort();
  if (!labels.length) return null;
  return (
    <SelectField
      label="Or pick a label the runners carry"
      value=""
      onChange={(l) => l && !disabled && onPick(l)}
      options={[{ value: "", label: "A label…" }, ...labels.map((l) => ({ value: l, label: l }))]}
      className="env-field"
    />
  );
}

/** kiln's integrations, to pick the one a platform environment deploys through. */
function IntegrationPicker({ value, onChange, disabled }: { value: string; onChange: (v: string) => void; disabled: boolean }) {
  const list = useSuspenseResource("ci/integrations", (s) => api.integrations(s)).data.integrations;
  if (!list.length && !value) return <p className="env-quiet">kiln has no integrations yet: add one under Settings → Deployments.</p>;
  const options = [{ value: "", label: "An integration…" }, ...list.map((i) => ({ value: i.name, label: `${i.name} (${i.kind})` }))];
  if (value && !list.some((i) => i.name === value)) options.push({ value, label: `${value} (not one kiln has)` });
  return <SelectField label="Integration" value={value} onChange={(v) => !disabled && onChange(v)} options={options} className="env-field" />;
}

/** U10: an explicit binding shares policy, never merely a matching display name. */
function ServerPolicyPicker({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const environments = useSuspenseResource("deploy/environments", (s) => deployApi.environments(s)).data;
  return (
    <SelectField
      label="Server environment"
      value={value}
      onChange={onChange}
      options={[{ value: "", label: "Choose an environment…" }, ...environments.map((e) => ({ value: e.name, label: e.name }))]}
    />
  );
}

function SharedFlow({ repo, e }: { repo: string; e: CiEnvironment }) {
  const [run, setRun] = React.useState("");
  const [app, setApp] = React.useState("");
  const [version, setVersion] = React.useState("");
  const [id, setId] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  if (e.protection.policy?.scope.kind !== "server") return null;
  const create = async () => {
    setBusy(true);
    try {
      const flow = await ci.createEnvironmentFlow(repo, e.name, { run, app, version });
      setId(flow.id);
      toast.success("Shared protected action declared");
    } catch (error) {
      toast.error("Couldn't declare the protected action", errorMessage(error));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="env-group" aria-label="Shared protected action">
      <h3 className="env-legend">One approval for a CI-to-release flow</h3>
      <p className="env-quiet">
        Declare the exact run, app and version before its environment job passes protection. Reviewers must be allowed to approve both the run and the release. Other releases keep their own approvals.
      </p>
      <div className="env-fields">
        <TextField label="CI run" value={run} onChange={(ev) => setRun(ev.target.value)} />
        <TextField label="Release app" value={app} onChange={(ev) => setApp(ev.target.value)} />
        <TextField label="Exact release version" value={version} onChange={(ev) => setVersion(ev.target.value)} />
      </div>
      <Button size="sm" disabled={busy || !run.trim() || !app.trim() || !version.trim()} onClick={() => void create()}>
        Declare shared action
      </Button>
      {id && (
        <div role="status">
          <TextField label="Release protected_action" value={id} readOnly hint="Pass this action to the release request. Its app, version and environment must match." />
        </div>
      )}
    </section>
  );
}

function EnvForm({ e, repo, others }: { e: CiEnvironment; repo: string; others: string[] }) {
  // A preview's are its `previews:` entry's, in the file too (W15-R07).
  const file = e.source === "file" || e.source === "preview";
  const p = e.protection;
  const [binding, setBinding] = React.useState<"repository" | "server">(p.policy?.scope.kind === "server" ? "server" : "repository");
  const [serverPolicy, setServerPolicy] = React.useState(p.policy?.scope.kind === "server" ? p.policy.name : "");
  const [kind, setKind] = React.useState<CiEnvKind | "none">(e.kind ?? "none");
  const [clusters, setClusters] = React.useState<string[]>(e.targets.clusters ?? []);
  const [namespace, setNamespace] = React.useState(e.targets.namespace ?? "");
  const [runners, setRunners] = React.useState(e.targets.runners ?? "");
  const [integration, setIntegration] = React.useState(e.targets.integration ?? "");
  const [reviewers, setReviewers] = React.useState((p.reviewers ?? []).join(", "));
  const [selfReview, setSelfReview] = React.useState(Boolean(p["prevent-self-review"]));
  const [waitTimer, setWaitTimer] = React.useState(String(p["wait-timer"] ?? 0));
  const [branches, setBranches] = React.useState((p.branches ?? []).join(", "));
  const [tags, setTags] = React.useState((p.tags ?? []).join(", "));
  const [after, setAfter] = React.useState(p.after ?? "");
  const [soak, setSoak] = React.useState(p.soak ?? "");
  const [concurrency, setConcurrency] = React.useState(e.concurrency ? String(e.concurrency) : "");
  const [url, setUrl] = React.useState(e.url ?? "");
  const [busy, setBusy] = React.useState(false);
  const [said, setSaid] = React.useState<{ warnings: string[]; trunk: string | null } | null>(null);

  const save = async () => {
    setBusy(true);
    try {
      const targets: CiEnvTargets = { clusters, namespace: namespace.trim() || undefined, runners: runners.trim() || undefined, integration: integration || undefined };
      const r = await ci.saveEnvironment(repo, e.name, {
        kind: binding === "server" || kind === "none" ? null : kind,
        targets: binding === "server" ? {} : targets,
        // Its windows and gates as they are: they're kept in `.kiln/environments.yaml`'s terms.
        protection:
          binding === "server"
            ? { policy: { scope: { kind: "server" }, name: serverPolicy } }
            : {
                ...p,
                policy: undefined,
                reviewers: names(reviewers),
                "prevent-self-review": selfReview,
                "wait-timer": Math.max(0, Number(waitTimer) || 0),
                branches: names(branches).length ? names(branches) : null,
                tags: names(tags).length ? names(tags) : null,
                after: after || undefined,
                soak: after && soak.trim() ? soak.trim() : undefined,
              },
        url: url.trim() || null,
        concurrency: binding === "server" ? null : Number(concurrency) > 0 ? Math.floor(Number(concurrency)) : null,
      });
      setSaid({ warnings: (r.diagnostics ?? []).map((d) => d.message), trunk: r.preview ? `A deployment from ${r.preview.trunk} ${r.preview.a_deployment_from_the_trunk}.` : null });
      toast.success(`Saved ${e.name}`);
      invalidate("ci/environment");
    } catch (err) {
      toast.error(`Couldn't save ${e.name}`, errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const about = KINDS.find((k) => k.value === kind)?.about;
  return (
    <form
      noValidate
      className="env-form"
      onSubmit={(ev) => {
        ev.preventDefault();
        if (!file) void save();
      }}
    >
      {file && (
        <p className="env-note">
          {e.preview ? `Made from ${e.preview.template} in .kiln/environments.yaml's previews on the trunk: change it there.` : "Defined in .kiln/environments.yaml on the trunk: change it there."}
        </p>
      )}
      <fieldset className="env-group" disabled={file || busy}>
        <legend className="env-legend">Policy identity</legend>
        <SelectField
          label="Policy owner"
          value={binding}
          onChange={(value) => setBinding(value === "server" ? "server" : "repository")}
          options={[
            { value: "repository", label: "This repository" },
            { value: "server", label: "Shared server environment" },
          ]}
        />
        {binding === "server" && (
          <>
            <Loadable label="server environment policies">
              <ServerPolicyPicker value={serverPolicy} onChange={setServerPolicy} />
            </Loadable>
            <p className="env-quiet">
              Targets, approvals, windows, prerequisites and capacity come from this one server policy. Saving replaces this repository's local rules with that explicit binding.
            </p>
            {serverPolicy && <Link to={`/deployments/releases/environments/${encodeURIComponent(serverPolicy)}`}>Edit the shared policy</Link>}
          </>
        )}
      </fieldset>
      <fieldset className="env-group" disabled={file || busy || binding === "server"}>
        <legend className="env-legend">Where it deploys</legend>
        <Segmented<CiEnvKind | "none">
          label="Kind"
          value={kind}
          onChange={(k) => !file && setKind(k)}
          options={[{ value: "none", label: "None" }, ...KINDS.map((k) => ({ value: k.value, label: k.label }))]}
        />
        <p className="env-quiet">{about ?? "No targets of its own, as a GitHub environment: its jobs deploy however their steps do."}</p>
        {kind === "kubernetes" && (
          <>
            <Loadable label="kiln CD's clusters" fallback={null}>
              <Clusters value={clusters} onChange={setClusters} disabled={file || busy} />
            </Loadable>
            <TextField label="Namespace" value={namespace} onChange={(ev) => setNamespace(ev.target.value)} placeholder="web" className="env-field" />
          </>
        )}
        {(kind === "host" || kind === "container" || kind === "microvm") && (
          <>
            <TextField
              label="Runners"
              hint="A constraint the runners it deploys on meet: label deploy-web, os == linux && label web."
              value={runners}
              onChange={(ev) => setRunners(ev.target.value)}
              placeholder="label deploy-web"
              spellCheck={false}
              className="env-field"
            />
            <Loadable label="the runners' labels" fallback={null}>
              <LabelPicker onPick={(l) => setRunners(`label ${l}`)} disabled={file || busy} />
            </Loadable>
          </>
        )}
        {kind === "platform" && (
          <Loadable label="kiln's integrations" fallback={null}>
            <IntegrationPicker value={integration} onChange={setIntegration} disabled={file || busy} />
          </Loadable>
        )}
      </fieldset>
      <fieldset className="env-group" disabled={file || busy || binding === "server"}>
        <legend className="env-legend">Who may deploy, and when</legend>
        <div className="env-fields">
          <TextField
            label="Required reviewers"
            hint="People or team:name, separated by commas; any one of them approves."
            value={reviewers}
            onChange={(ev) => setReviewers(ev.target.value)}
            placeholder="ada, team:web"
          />
          <TextField label="Wait timer" hint="Minutes a job waits before it deploys." type="number" min={0} value={waitTimer} onChange={(ev) => setWaitTimer(ev.target.value)} />
          <TextField
            label="Branches"
            hint="Patterns that may deploy; with none (and no tags), any ref may."
            value={branches}
            onChange={(ev) => setBranches(ev.target.value)}
            placeholder="main, release/*"
          />
          <TextField label="Tags" hint="Tag patterns that may deploy." value={tags} onChange={(ev) => setTags(ev.target.value)} placeholder="v*" />
          <SelectField label="After" value={after} onChange={setAfter} options={[{ value: "", label: "No environment first" }, ...others.map((o) => ({ value: o, label: `Once in ${o}` }))]} />
          {after && <TextField label="For at least" hint="How long a commit runs there first: 30m, 2h." value={soak} onChange={(ev) => setSoak(ev.target.value)} placeholder="30m" />}
          <TextField
            label="At most at once"
            hint="Deployments past its rules at once; the rest wait their turn."
            type="number"
            min={1}
            value={concurrency}
            onChange={(ev) => setConcurrency(ev.target.value)}
            placeholder="any"
          />
          <TextField label="URL" hint="Where it is; a job's environment.url wins." value={url} onChange={(ev) => setUrl(ev.target.value)} placeholder="https://staging.example.com" />
        </div>
        <label className="env-check">
          <input type="checkbox" checked={selfReview} onChange={(ev) => setSelfReview(ev.target.checked)} />
          <span>Whoever started the run can't approve its deployment</span>
        </label>
        {(p.windows?.length ?? 0) + (p.gates?.length ?? 0) > 0 && (
          <p className="env-quiet">Its windows and gates are kept as they are: {protectionWords({ windows: p.windows, gates: p.gates }, null).join(" · ")}.</p>
        )}
      </fieldset>
      {!file && (
        <div className="env-actions">
          <Button type="submit" size="sm" disabled={busy || (binding === "server" && !serverPolicy)}>
            Save {e.name}
          </Button>
        </div>
      )}
      {said && (
        <div className="env-said" role="status">
          {said.trunk && <p>{said.trunk}</p>}
          {said.warnings.map((w) => (
            <p key={w} data-tone="warning">
              {w}
            </p>
          ))}
        </div>
      )}
    </form>
  );
}

function History({ repo, name }: { repo: string; name: string }) {
  const list = useSuspenseResource(`ci/environment/${repo}/${name}/deployments`, (s) => ci.deployments(repo, name, s), { on: ENV_EVENTS, pollMs: 15_000 }).data.deployments;
  if (!list.length) return <p className="env-quiet">No deployments yet.</p>;
  return (
    <ul className="env-history" aria-label={`${name}'s deployments`}>
      {list.map((d) => (
        <li key={d.id}>
          <Deployed d={d} />
        </li>
      ))}
    </ul>
  );
}

function Remove({ e, repo }: { e: CiEnvironment; repo: string }) {
  const navigate = useNavigate();
  const [asked, setAsked] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  if (e.source === "file" || e.preview?.removed) return null;
  const remove = async () => {
    setBusy(true);
    try {
      await ci.deleteEnvironment(repo, e.name);
      toast.success(`Removed ${e.name}`);
      invalidate("ci/environment");
      navigate(environmentsHref);
    } catch (err) {
      toast.error(`Couldn't remove ${e.name}`, errorMessage(err));
      setBusy(false);
    }
  };
  if (!asked) {
    return (
      <div className="env-remove">
        <Button size="sm" variant="ghost" onClick={() => setAsked(true)}>
          Remove {e.name}
        </Button>
      </div>
    );
  }
  return (
    <Confirm
      className="env-remove"
      label={`Remove ${e.name}`}
      tone="danger"
      actions={
        <>
          <Button size="xs" variant="primary" disabled={busy} onClick={() => void remove()}>
            Remove {e.name}
          </Button>
          <Button size="xs" variant="ghost" disabled={busy} onClick={() => setAsked(false)}>
            Keep it
          </Button>
        </>
      }
    >
      {e.preview
        ? "Its deployments are marked inactive, and kiln takes down what it deployed on its runners. Its change's next run makes it again."
        : "Its rules go; its deployments stay in its history. A job that names it again makes it again, with no rules."}
    </Confirm>
  );
}

function EnvBody({ repo, name }: { repo: string; name: string }) {
  const e = useSuspenseResource(`ci/environment/${repo}/${name}`, (s) => ci.environment(repo, name, s), { on: ENV_EVENTS, pollMs: 10_000 }).data;
  const others = useSuspenseResource(`ci/environment/${repo}/list`, (s) => ci.environments(repo, s), { on: ENV_EVENTS })
    .data.environments.filter((x): x is CiEnvironment => !isProviderEnvironment(x))
    .map((x) => x.name)
    .filter((n) => n.toLowerCase() !== e.name.toLowerCase());
  const where = whereWords(e);
  return (
    <>
      <Facts
        className="env-facts"
        items={[
          { label: "Kind", value: kindLabel(e.kind) ?? "none of its own" },
          { label: "Where", value: where ?? "—" },
          { label: "Rules", value: protectionWords(e.protection, e.concurrency).join(" · ") || "anyone may deploy, any ref" },
          { label: "Defined", value: SOURCE_WORDS[e.source] ?? e.source },
          { label: "Policy owner", value: e.identity?.scope.kind === "server" ? `Server · ${e.identity.name}` : `Repository · ${repo} · ${e.name}` },
          ...(e.preview ? [{ label: "For", value: <PreviewOf p={e.preview} /> }] : []),
        ]}
      />
      <Panel eyebrow="Waiting" title="What waits to deploy here" description="Approve or reject on each run's page.">
        <Waits e={e} />
      </Panel>
      <Panel eyebrow="Running" title="What runs here">
        {e.latest_deployment ? <Deployed d={e.latest_deployment} /> : <p className="env-quiet">Nothing deployed yet.</p>}
        <Loadable label={`kiln CD's apps in ${e.name}`} fallback={null}>
          <EnvApps env={e.name} repo={repo} />
        </Loadable>
      </Panel>
      <Panel eyebrow="Settings" title={`${e.name}'s settings`}>
        <EnvForm key={`${e.source}/${e.kind ?? ""}/${e.protection.policy?.name ?? ""}`} e={e} repo={repo} others={others} />
        <SharedFlow repo={repo} e={e} />
        <Remove e={e} repo={repo} />
      </Panel>
      <Panel eyebrow="History" title="Deployments">
        <Loadable label={`${e.name}'s deployments`}>
          <History repo={repo} name={e.name} />
        </Loadable>
      </Panel>
      <Loadable label={`${e.name}'s secrets and variables`}>
        <ScopeSecrets scope={`env:${repo}:${e.name}`} of={e.name} />
      </Loadable>
    </>
  );
}

export default function EnvironmentPage() {
  const { name = "" } = useParams();
  const [search] = useSearchParams();
  const repo = search.get("repo") ?? "";
  return (
    <div className="container-console page cd-page">
      <title>{`${name} · Environments · kiln`}</title>
      <header className="cd-head">
        <Crumbs index={2} items={[{ label: "Deployments", to: "/deployments" }, { label: "Environments", to: environmentsHref }, { label: name }]} />
        <h1 className="cd-title">{name}</h1>
        <p className="cd-note">
          {repo ? (
            <>
              An environment of <Link to={`/runs?repo=${encodeURIComponent(repo)}`}>{repo}</Link>.
            </>
          ) : (
            "Which repository's? Open it from the environments board."
          )}
        </p>
      </header>
      {repo && (
        <Loadable label={`the environment ${name}`}>
          <EnvBody repo={repo} name={name} />
        </Loadable>
      )}
    </div>
  );
}
