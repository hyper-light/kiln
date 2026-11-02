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
import { ci, type CiEnvironment, type CiEnvKind, type CiEnvTargets } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { Confirm, Crumbs, Facts, Panel, SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useHold, type Hold } from "@/lib/action";
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

function EnvForm({ e, repo, others, hold }: { e: CiEnvironment; repo: string; others: string[]; hold: Hold }) {
  // A preview's are its `previews:` entry's, in the file too (W15-R07).
  const file = e.source === "file" || e.source === "preview";
  const p = e.protection;
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
  const [said, setSaid] = React.useState<{ warnings: string[]; trunk: string | null } | null>(null);

  // Save says it's on its way; the fields go on taking what's typed (W21-R15).
  const save = () =>
    hold.change("save", async () => {
      try {
        const targets: CiEnvTargets = { clusters, namespace: namespace.trim() || undefined, runners: runners.trim() || undefined, integration: integration || undefined };
        const r = await ci.saveEnvironment(repo, e.name, {
          kind: kind === "none" ? null : kind,
          targets,
          // Its windows and gates as they are: they're kept in `.kiln/environments.yaml`'s terms.
          protection: {
            ...p,
            reviewers: names(reviewers),
            "prevent-self-review": selfReview,
            "wait-timer": Math.max(0, Number(waitTimer) || 0),
            branches: names(branches).length ? names(branches) : null,
            tags: names(tags).length ? names(tags) : null,
            after: after || undefined,
            soak: after && soak.trim() ? soak.trim() : undefined,
          },
          url: url.trim() || null,
          concurrency: Number(concurrency) > 0 ? Math.floor(Number(concurrency)) : null,
        });
        setSaid({ warnings: (r.diagnostics ?? []).map((d) => d.message), trunk: r.preview ? `A deployment from ${r.preview.trunk} ${r.preview.a_deployment_from_the_trunk}.` : null });
        toast.success(`Saved ${e.name}`);
        invalidate("ci/environment");
      } catch (err) {
        toast.error(`Couldn't save ${e.name}`, errorMessage(err));
      }
    });

  const about = KINDS.find((k) => k.value === kind)?.about;
  return (
    <form
      noValidate
      className="env-form"
      onSubmit={(ev) => {
        ev.preventDefault();
        if (!file) save();
      }}
    >
      {file && (
        <p className="env-note">
          {e.preview ? `Made from ${e.preview.template} in .kiln/environments.yaml's previews on the trunk: change it there.` : "Defined in .kiln/environments.yaml on the trunk: change it there."}
        </p>
      )}
      <fieldset className="env-group" disabled={file}>
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
              <Clusters value={clusters} onChange={setClusters} disabled={file} />
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
              <LabelPicker onPick={(l) => setRunners(`label ${l}`)} disabled={file} />
            </Loadable>
          </>
        )}
        {kind === "platform" && (
          <Loadable label="kiln's integrations" fallback={null}>
            <IntegrationPicker value={integration} onChange={setIntegration} disabled={file} />
          </Loadable>
        )}
      </fieldset>
      <fieldset className="env-group" disabled={file}>
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
          <Button type="submit" size="sm" pending={hold.doing("save")} disabled={hold.held("save")}>
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

function Remove({ e, repo, hold }: { e: CiEnvironment; repo: string; hold: Hold }) {
  const navigate = useNavigate();
  const [asked, setAsked] = React.useState(false);
  // Removed, it goes back to the board; until the board shows, Remove says it's on its way, where it
  // was asked and, once Keep it has closed the question, where it's asked from (W21-R15).
  const remove = () =>
    hold.change("remove", async () => {
      try {
        await ci.deleteEnvironment(repo, e.name);
        toast.success(`Removed ${e.name}`);
        invalidate("ci/environment");
        await navigate(environmentsHref);
      } catch (err) {
        toast.error(`Couldn't remove ${e.name}`, errorMessage(err));
      }
    });
  const removing = hold.doing("remove");
  if (e.source === "file" || e.preview?.removed) return null;
  if (!asked) {
    return (
      <div className="env-remove">
        <Button size="sm" variant="ghost" pending={removing} onClick={() => setAsked(true)}>
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
          <Button size="xs" variant="primary" pending={removing} disabled={hold.held("remove")} onClick={remove}>
            Remove {e.name}
          </Button>
          <Button size="xs" variant="ghost" onClick={() => setAsked(false)}>
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
    .data.environments.map((x) => x.name)
    .filter((n) => n.toLowerCase() !== e.name.toLowerCase());
  const where = whereWords(e);
  // Its changes (its settings saved, its removal) go one at a time: the one pressed says it's on its
  // way and the other waits for it (W21-R15).
  const hold = useHold();
  return (
    <>
      <Facts
        className="env-facts"
        items={[
          { label: "Kind", value: kindLabel(e.kind) ?? "none of its own" },
          { label: "Where", value: where ?? "—" },
          { label: "Rules", value: protectionWords(e.protection, e.concurrency).join(" · ") || "anyone may deploy, any ref" },
          { label: "Defined", value: SOURCE_WORDS[e.source] ?? e.source },
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
        <EnvForm key={`${e.source}/${e.kind ?? ""}`} e={e} repo={repo} others={others} hold={hold} />
        <Remove e={e} repo={repo} hold={hold} />
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
