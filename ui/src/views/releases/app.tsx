/**
 * One app across its environments, side by side: what runs in each, what is on its way, what its
 * provider says; and from each, a release: a new version, new values, a promotion of what another
 * environment runs, or a rollback to one of the releases before. Below, its latest releases, what
 * it is, and how it deploys: a Helm chart, a path in git, or a platform with its definition.
 */
import * as React from "react";
import { Link, useNavigate, useParams } from "@/lib/router";
import { Pause, Play, Plus, Rocket } from "lucide-react";

import { cdApi } from "@/api/cd";
import { boardHref, deployApi, describe, packageWords, providerName, registrationHref, releaseHref, STATE_DOT, STATE_WORDS, type App, type AppPatch, type Package, type Registries, type ReleaseAsk } from "@/api/deploy";
import type { CdProvider } from "@/api/types";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { Loadable } from "@/components/ui/loading";
import { CopyButton, Crumbs, Facts, Panel, SelectField, TextField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { RelTime } from "@/views/shared";
import { useRes, type Res } from "@/lib/data";
import { DEPLOY_EVENTS, environmentsRes, ReleaseState, templatesRes } from "./index";
import { toYaml, yamlMap, YamlEditor } from "./yaml";
import "@/views/deployments/deployments.css";
import "./releases.css";

type Mode = "version" | "values" | "promote" | "rollback";

/** The platforms KilnCD deploys to, each with what its definition takes. */
const providersRes: Res<{ providers: CdProvider[] }> = { key: "cd/providers", fetch: (s) => cdApi.providers(s) };

/** `repository:tag` or `repository@sha256:…` as an artifact. */
function artifactOf(text: string) {
  const t = text.trim();
  if (!t) return undefined;
  const at = t.indexOf("@");
  if (at > 0) return { repository: t.slice(0, at), tag: "", digest: t.slice(at + 1) };
  const colon = t.lastIndexOf(":");
  return colon > t.lastIndexOf("/") ? { repository: t.slice(0, colon), tag: t.slice(colon + 1), digest: "" } : { repository: t, tag: "", digest: "" };
}

/** The releases a rollback can go back to, one to choose. */
function Candidates({ app, env, to, onPick }: { app: string; env: string; to: number | null; onPick: (id: number) => void }) {
  const list = useSuspenseResource(`deploy/candidates/${app}/${env}`, () => deployApi.candidates(app, env), { on: DEPLOY_EVENTS }).data;
  if (list.length === 0) return <p className="rel-quiet">Nothing earlier to roll back to here.</p>;
  return (
    <fieldset className="rel-candidates">
      <legend className="field-label">Back to</legend>
      {list.map((c) => (
        <label key={c.id}>
          <input type="radio" name="rel-to" checked={to === c.id} onChange={() => onPick(c.id)} />
          <span>
            release {c.id}: {describe(c)} · <RelTime ts={c.ended_at ?? c.created_at} />
          </span>
        </label>
      ))}
    </fieldset>
  );
}

/** A release, asked for: from one of four kinds, then followed on its own page. */
function ReleaseDialog({ app, env, others, open, onOpenChange }: { app: string; env: string; others: string[]; open: boolean; onOpenChange: (o: boolean) => void }) {
  const navigate = useNavigate();
  const [mode, setMode] = React.useState<Mode>("version");
  const [version, setVersion] = React.useState("");
  const [image, setImage] = React.useState("");
  const [values, setValues] = React.useState("");
  const [from, setFrom] = React.useState(others[0] ?? "");
  const [to, setTo] = React.useState<number | null>(null);
  const [busy, setBusy] = React.useState(false);
  const ask = (): ReleaseAsk | null => {
    switch (mode) {
      case "version": {
        if (!version.trim()) return null;
        const a = artifactOf(image);
        return { app, environment: env, version: version.trim(), artifacts: a ? { image: a } : undefined };
      }
      case "values":
        return values.trim() ? { app, environment: env, values } : null;
      case "promote":
        return from ? { app, environment: env, promote_from: from } : null;
      case "rollback":
        return to ? { app, environment: env, rollback: { to } } : null;
    }
  };
  const go = async () => {
    const a = ask();
    if (!a) return;
    setBusy(true);
    try {
      const r = await deployApi.create({ ...a, idempotency_key: crypto.randomUUID() });
      toast({ title: `Release ${r.id}: ${describe(r)} → ${env}`, description: STATE_WORDS[r.state], tone: "running", href: releaseHref(r.id) });
      invalidate("deploy/");
      onOpenChange(false);
      navigate(releaseHref(r.id));
    } catch (e) {
      toast({ title: "Not released", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const modes: { value: Mode; label: string }[] = [
    { value: "version", label: "A version" },
    { value: "values", label: "Values" },
    ...(others.length ? [{ value: "promote" as Mode, label: "Promote" }] : []),
    { value: "rollback", label: "Roll back" },
  ];
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent eyebrow={`${app} → ${env}`} title="Release" description="The environment's gates come first: upstream environments, soak, windows, checks, approvals.">
        <div className="rel-dialog">
          <Segmented label="What to release" value={mode} onChange={setMode} options={modes} />
          {mode === "version" && (
            <>
              <TextField label="Version" placeholder="1.4.2" value={version} onChange={(e) => setVersion(e.target.value)} hint="A chart version, or a git package's branch, tag or commit." />
              <TextField label="Image (optional)" placeholder="ghcr.io/acme/web:1.4.2" value={image} onChange={(e) => setImage(e.target.value)} hint="The build it deploys, as the artifact `image`." />
            </>
          )}
          {mode === "values" && (
            <div>
              <label className="field-label" htmlFor="rel-values">
                The environment's values.yaml
              </label>
              <textarea id="rel-values" className="rel-textarea" rows={10} spellCheck={false} value={values} onChange={(e) => setValues(e.target.value)} placeholder={"replicaCount: 3\n"} />
            </div>
          )}
          {mode === "promote" && <SelectField label="What runs in" value={from} onChange={setFrom} options={others.map((o) => ({ value: o, label: o }))} />}
          {mode === "rollback" && (
            <Loadable label="the releases it can go back to">
              <Candidates app={app} env={env} to={to} onPick={setTo} />
            </Loadable>
          )}
          <div className="rel-dialog-actions">
            <Button variant="ghost" onClick={() => onOpenChange(false)}>
              Close
            </Button>
            <Button disabled={busy || !ask()} onClick={() => void go()}>
              <Rocket /> Release
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** One environment of the app: what runs, what is on its way, what its provider says. */
function EnvCard({ app, env, others }: { app: string; env: string; others: string[] }) {
  const d = useSuspenseResource(`deploy/reg/${app}/${env}`, (s) => deployApi.registration(app, env, s), { on: DEPLOY_EVENTS }).data;
  const [open, setOpen] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const reg = d.registration;
  const status = d.provider.status as { sync?: string; health?: string; error?: string } | null;
  const act = async (what: "pause" | "resume" | "enable" | "disable") => {
    setBusy(true);
    try {
      if (what === "pause") await deployApi.pause(app, env);
      else if (what === "resume") await deployApi.resume(app, env);
      else {
        const r = await deployApi.create({ app, environment: env, enabled: what === "enable", idempotency_key: crypto.randomUUID() });
        toast({ title: `Release ${r.id}: ${what} ${app} in ${env}`, tone: "running", href: releaseHref(r.id) });
      }
      invalidate("deploy/");
    } catch (e) {
      toast({ title: `Not ${what}d`, description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Panel
      eyebrow={providerName(d.provider.kind === "argocd" ? `argocd/${d.provider.integration ?? ""}` : "kilncd")}
      title={env}
      actions={
        <>
          <Button asChild size="xs" variant="ghost">
            <Link to={registrationHref(app, env)}>What runs</Link>
          </Button>
          <Button size="xs" onClick={() => setOpen(true)} disabled={!!d.in_flight}>
            <Rocket /> Release
          </Button>
        </>
      }
      className="rel-env"
    >
      <dl className="rel-env-facts">
        <div>
          <dt>Runs</dt>
          <dd>{d.running ? <Link to={releaseHref(d.running.id)}>{describe(d.running)}</Link> : <span className="rel-quiet">nothing yet</span>}</dd>
        </div>
        {d.in_flight && (
          <div>
            <dt>On its way</dt>
            <dd>
              <Link to={releaseHref(d.in_flight.id)} className="rel-flight">
                <StatusDot status={STATE_DOT[d.in_flight.state]} /> {describe(d.in_flight)} · {STATE_WORDS[d.in_flight.state]}
              </Link>
              {d.in_flight.message && <p className="rel-msg">{d.in_flight.message}</p>}
            </dd>
          </div>
        )}
        <div>
          <dt>{d.provider.kind === "argocd" ? "Argo CD" : "KilnCD"}</dt>
          <dd>{status?.error ? <span className="rel-msg">{status.error}</span> : status ? `${status.sync ?? "?"} · ${status.health ?? "?"}` : <span className="rel-quiet">nothing there</span>}</dd>
        </div>
      </dl>
      {/* Nothing released here yet: its first release is all there is to do. */}
      {(d.running || d.in_flight) && (
        <div className="rel-env-actions">
          {reg.paused ? (
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => void act("resume")}>
              <Play /> Resume
            </Button>
          ) : (
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => void act("pause")}>
              <Pause /> Pause
            </Button>
          )}
          {reg.enabled ? (
            <Button size="xs" variant="ghost-danger" disabled={busy || !!d.in_flight} onClick={() => void act("disable")}>
              Disable
            </Button>
          ) : (
            <Button size="xs" variant="ghost" disabled={busy || !!d.in_flight} onClick={() => void act("enable")}>
              Enable
            </Button>
          )}
        </div>
      )}
      <ReleaseDialog app={app} env={env} others={others} open={open} onOpenChange={setOpen} />
    </Panel>
  );
}

/** The environments it isn't registered in yet, one to register it in. */
function RegisterIn({ app, registered }: { app: string; registered: string[] }) {
  const left = useRes(environmentsRes).data.filter((e) => !registered.includes(e.name));
  const [env, setEnv] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  if (!left.length) return null;
  const pick = left.some((e) => e.name === env) ? env : (left[0]?.name ?? "");
  const go = async () => {
    setBusy(true);
    try {
      await deployApi.register(app, pick);
      toast({ title: `${app} registered in ${pick}`, description: "Nothing runs there until you release it.", tone: "succeeded" });
      invalidate("deploy/");
    } catch (e) {
      toast({ title: "Not registered", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="rel-register">
      <SelectField label="Register it in" value={pick} onChange={setEnv} options={left.map((e) => ({ value: e.name, label: `${e.name} · ${providerName(e.provider)}` }))} />
      <Button size="sm" variant="secondary" disabled={busy || !pick} onClick={() => void go()}>
        <Plus /> Register
      </Button>
    </div>
  );
}

/** What an app is, changed here: its project and owners, who to reach, its source repository; it
 * archived (no new releases, what runs keeps running), or deleted. */
function Settings({ a, registered }: { a: App; registered: string[] }) {
  const navigate = useNavigate();
  const [project, setProject] = React.useState(a.project);
  const [owners, setOwners] = React.useState(a.owners.join(", "));
  const [oncall, setOncall] = React.useState(a.contacts?.oncall ?? "");
  const [channels, setChannels] = React.useState((a.contacts?.channels ?? []).join(", "));
  const [source, setSource] = React.useState(a.source ?? "");
  const [busy, setBusy] = React.useState(false);
  const [deleting, setDeleting] = React.useState(false);
  const [keep, setKeep] = React.useState(true);
  const list = (s: string) =>
    s
      .split(",")
      .map((x) => x.trim())
      .filter(Boolean);
  const patch = async (p: AppPatch, done: string) => {
    setBusy(true);
    try {
      await deployApi.patchApp(a.name, p);
      toast({ title: done, tone: "succeeded" });
      invalidate("deploy/");
    } catch (e) {
      toast({ title: "Not changed", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const save = () =>
    patch(
      { project: project.trim() || "default", owners: list(owners), contacts: { ...a.contacts, oncall: oncall.trim() || undefined, channels: list(channels) }, source: source.trim() },
      `${a.name} is saved`,
    );
  const remove = async () => {
    try {
      await deployApi.deleteApp(a.name, { everywhere: true, keep });
      toast({ title: `${a.name} is deleted`, description: a.source ? `Its repository ${a.source} is kept.` : undefined, tone: "succeeded" });
      invalidate("deploy/");
      navigate(boardHref);
    } catch (e) {
      toast({ title: "Not deleted", description: errorMessage(e), tone: "failed" });
    }
  };
  return (
    <Panel
      eyebrow="Settings"
      title="What it is"
      description="Owners, who to reach and its source change here; how it deploys, below."
      actions={
        <>
          <CatalogInfo app={a.name} />
          <AsYaml a={a} />
        </>
      }
    >
      <div className="cd-new-grid">
        <TextField label="Project" value={project} spellCheck={false} onChange={(e) => setProject(e.target.value)} />
        <TextField label="Owner teams" value={owners} placeholder="payments, platform" onChange={(e) => setOwners(e.target.value)} />
        <TextField label="On call" value={oncall} placeholder="ada" onChange={(e) => setOncall(e.target.value)} />
        <TextField
          label="Chat channels"
          value={channels}
          placeholder="slack:#deploys"
          onChange={(e) => setChannels(e.target.value)}
          hint="Where its release messages go: kind:target, a bare #channel being Slack's."
        />
        <TextField label="Source repository" value={source} placeholder="acme/web" spellCheck={false} onChange={(e) => setSource(e.target.value)} hint="One kiln has; empty for none." />
      </div>
      <div className="rel-settings-actions">
        <Button size="sm" disabled={busy} onClick={() => void save()}>
          Save
        </Button>
        <Button size="sm" variant="ghost" disabled={busy} onClick={() => void patch({ archived: !a.archived }, a.archived ? `${a.name} takes releases again` : `${a.name} is archived`)}>
          {a.archived ? "Unarchive" : "Archive"}
        </Button>
        <Button size="sm" variant="ghost-danger" disabled={busy} onClick={() => setDeleting(true)}>
          Delete
        </Button>
      </div>
      <ConfirmDialog
        open={deleting}
        onOpenChange={setDeleting}
        title={`Delete ${a.name}?`}
        description={
          <>
            {registered.length ? `It is deregistered from ${registered.join(", ")} first. ` : ""}
            Its releases stay in the history{a.source ? `, and its repository ${a.source} is kept` : ""}.
            {registered.length > 0 && (
              <label className="rel-keep">
                <input type="checkbox" className="check" checked={keep} onChange={(e) => setKeep(e.target.checked)} /> Leave what it runs in place
              </label>
            )}
          </>
        }
        confirmLabel="Delete the app"
        danger
        onConfirm={remove}
      />
    </Panel>
  );
}

/** The app as Backstage's catalog reads it: its catalog-info.yaml, to copy into its repository. */
function CatalogInfo({ app }: { app: string }) {
  const [open, setOpen] = React.useState(false);
  return (
    <>
      <Button size="sm" variant="ghost" onClick={() => setOpen(true)}>
        catalog-info.yaml
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent eyebrow="Backstage" title={`${app}'s catalog-info.yaml`} description="The app as Backstage's catalog reads it: its owner, system and lifecycle, and who to reach.">
          {open && (
            <Loadable label="its catalog-info.yaml">
              <CatalogYaml app={app} />
            </Loadable>
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}

function CatalogYaml({ app }: { app: string }) {
  const text = useSuspenseResource(`deploy/catalog/${app}`, (s) => deployApi.catalogInfo(app, s), { on: DEPLOY_EVENTS }).data;
  return (
    <div className="cd-manifest">
      <div className="cd-manifest-bar">
        <span className="cd-meta">catalog-info.yaml</span>
        <CopyButton text={text} className="cd-copy" />
      </div>
      <pre className="cd-yaml">{text}</pre>
    </div>
  );
}

/** The whole app as YAML, saved whole: what isn't in it is gone. */
function AsYaml({ a }: { a: App }) {
  const [open, setOpen] = React.useState(false);
  const [text, setText] = React.useState(() => toYaml(a));
  const [busy, setBusy] = React.useState(false);
  const read = yamlMap(text);
  const wrong = read.error ?? (read.value?.name !== a.name ? `Its name stays ${a.name}: another name is another app.` : null);
  const save = async () => {
    if (!read.value) return;
    setBusy(true);
    try {
      await deployApi.putApp(read.value as unknown as App);
      toast({ title: `${a.name} is saved`, description: "Each environment takes a change to how it deploys with its next release.", tone: "succeeded" });
      invalidate("deploy/");
      setOpen(false);
    } catch (e) {
      toast({ title: "Not saved", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <Button size="sm" variant="ghost" onClick={() => setOpen(true)}>
        Edit as YAML
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent eyebrow={a.name} title="The app, as YAML" description="Saved whole, as kiln keeps it: what isn't in it is gone.">
          <div className="rel-dialog">
            <YamlEditor value={text} onChange={setText} label={`${a.name}, as YAML`} />
            {wrong && <p className="rel-msg">{wrong}</p>}
            <div className="rel-dialog-actions">
              <Button variant="ghost" onClick={() => setOpen(false)}>
                Close
              </Button>
              <Button disabled={busy || !!wrong} onClick={() => void save()}>
                Save
              </Button>
            </div>
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}

/** How it deploys: its template, and its package with the values each release sets: a Helm chart,
 * a path in git, or a platform KilnCD deploys to (docs/cd-targets.md §3) with its definition, as
 * its key in `.kiln/deploy.yaml` holds it. A change reaches each environment with its next release
 * there. */
function Deploys({ a, registered }: { a: App; registered: string[] }) {
  const templates = useRes(templatesRes).data;
  const platforms = useRes(providersRes).data.providers.filter((x) => x.built);
  const environments = useRes(environmentsRes).data;
  const p = a.package;
  const [template, setTemplate] = React.useState(a.template ?? "");
  const [kind, setKind] = React.useState<Package["kind"]>(p.kind);
  const [repo, setRepo] = React.useState(p.kind === "platform" ? "" : p.repo);
  const [chart, setChart] = React.useState(p.kind === "helm" ? p.chart : "");
  const [chartVersion, setChartVersion] = React.useState(p.kind === "helm" ? (p.chart_version ?? "") : "");
  const [path, setPath] = React.useState(p.kind === "git" ? p.path : "");
  const [set, setSet] = React.useState(
    Object.entries((p.kind === "platform" ? undefined : p.set) ?? {})
      .map(([k, v]) => `${k}=${v}`)
      .join("\n"),
  );
  const [provider, setProvider] = React.useState(p.kind === "platform" ? p.provider : (platforms[0]?.name ?? ""));
  const [spec, setSpec] = React.useState(p.kind === "platform" ? toYaml(p.spec) : "");
  // Where a platform's own files are: its repository and path, apart from a chart's or git's.
  const [filesRepo, setFilesRepo] = React.useState(p.kind === "platform" ? (p.repo ?? "") : "");
  const [filesPath, setFilesPath] = React.useState(p.kind === "platform" ? (p.path ?? "") : "");
  const [busy, setBusy] = React.useState(false);
  const def = platforms.find((x) => x.name === provider);
  const lines = set
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
  const read: ReturnType<typeof yamlMap> = kind === "platform" ? yamlMap(spec) : {};
  const given = read.value ?? {};
  const missing = kind === "platform" && def && read.value ? def.settings.filter((x) => x.required && !(x.name in given)) : [];
  const unknown = kind === "platform" && def && read.value ? Object.keys(given).filter((k) => !def.settings.some((x) => x.name === k)) : [];
  // The environments it's registered in that can't run it on this platform.
  const lacking =
    kind === "platform"
      ? registered.filter((name) => {
          const e = environments.find((x) => x.name === name);
          return !e || e.provider.startsWith("argocd/") || !e.targets.platforms?.[provider];
        })
      : [];
  const wrong =
    kind === "platform"
      ? !def
        ? "A platform kiln deploys to."
        : read.error
          ? `Its definition doesn't read: ${read.error}`
          : missing.length
            ? `${def.title} needs ${missing.map((x) => x.name).join(", ")}.`
            : null
      : lines.find((l) => !l.includes("="))
        ? "Each value is name=value, one a line."
        : !repo.trim() || (kind === "helm" && !chart.trim())
          ? "Its repository, and a chart's name."
          : null;
  const save = async () => {
    const values = Object.fromEntries(lines.map((l) => [l.slice(0, l.indexOf("=")).trim(), l.slice(l.indexOf("=") + 1).trim()]));
    const pkg: Package =
      kind === "helm"
        ? { kind: "helm", repo: repo.trim(), chart: chart.trim(), chart_version: chartVersion.trim() || undefined, set: values }
        : kind === "git"
          ? { kind: "git", repo: repo.trim(), path: path.trim(), set: values }
          : { kind: "platform", provider, spec: given, repo: filesRepo.trim() || undefined, path: filesPath.trim() || undefined };
    setBusy(true);
    try {
      await deployApi.patchApp(a.name, { template, package: pkg });
      toast({ title: `${a.name} deploys anew`, description: "Each environment takes it with its next release.", tone: "succeeded" });
      invalidate("deploy/");
    } catch (e) {
      toast({ title: "Not changed", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <Panel eyebrow="How it deploys" title={kind === "platform" ? `To ${def?.title ?? provider}` : "Its package"} description="A change reaches each environment with its next release there.">
      <div className="cd-new-grid">
        <SelectField label="Template" value={template} onChange={setTemplate} options={[{ value: "", label: "None" }, ...templates.map((t) => ({ value: t.name, label: t.name }))]} />
        <Segmented
          label="What it deploys"
          value={kind}
          onChange={setKind}
          options={[
            { value: "helm", label: "A Helm chart" },
            { value: "git", label: "A path in git" },
            ...(platforms.length || p.kind === "platform" ? [{ value: "platform" as const, label: "A platform" }] : []),
          ]}
        />
      </div>
      {kind === "platform" ? (
        <>
          <div className="cd-new-grid">
            <SelectField label="Platform" value={provider} onChange={setProvider} options={platforms.map((x) => ({ value: x.name, label: `${x.title} · ${x.name}` }))} />
            <TextField
              label="Its files' repository"
              placeholder="none"
              value={filesRepo}
              spellCheck={false}
              onChange={(e) => setFilesRepo(e.target.value)}
              hint="Where its own files are (a task definition, wrangler.jsonc), if it has any."
            />
            <TextField label="Path" value={filesPath} spellCheck={false} onChange={(e) => setFilesPath(e.target.value)} hint="In that repository, at each release's version." />
          </div>
          <div className="rel-gap">
            <span className="field-label">Its definition, as its key in .kiln/deploy.yaml holds it</span>
            <YamlEditor value={spec} onChange={setSpec} label={`${a.name}'s ${provider} definition`} />
            <p className="field-hint">
              <code>{"${{ release.version }}"}</code>, <code>{"${{ release.artifacts.<name>.digest }}"}</code>, <code>{"${{ env.name }}"}</code> and <code>{"${{ app.name }}"}</code> are each release's.
            </p>
          </div>
          {def && def.settings.length > 0 && <Facts className="rel-gap" items={def.settings.map((x) => ({ label: `${x.name}${x.required ? " · needed" : ""}`, value: x.help, tone: missing.includes(x) ? "fail" : undefined }))} />}
          {unknown.length > 0 && def && (
            <p className="rel-quiet">
              {unknown.join(", ")}: not one of {def.title}'s settings.
            </p>
          )}
          {lacking.length > 0 && (
            <p className="rel-msg">
              {lacking.join(", ")} can't run it yet: a KilnCD environment with a {provider} target (<code>targets.platforms.{provider}</code>, an account and its place) runs it.
            </p>
          )}
        </>
      ) : (
        <div className="cd-new-grid">
          <TextField label={kind === "helm" ? "Chart repository" : "Git repository"} value={repo} spellCheck={false} onChange={(e) => setRepo(e.target.value)} />
          {kind === "helm" ? (
            <>
              <TextField label="Chart" value={chart} spellCheck={false} onChange={(e) => setChart(e.target.value)} />
              <TextField
                label="Chart version"
                placeholder="each release's"
                value={chartVersion}
                spellCheck={false}
                onChange={(e) => setChartVersion(e.target.value)}
                hint="Empty: each release's version."
              />
            </>
          ) : (
            <TextField label="Path" value={path} spellCheck={false} onChange={(e) => setPath(e.target.value)} hint="At each release's version." />
          )}
        </div>
      )}
      {kind !== "platform" && (
        <div className="rel-gap">
          <label className="field-label" htmlFor={`set-${a.name}`}>
            Values a release sets
          </label>
          <textarea
            id={`set-${a.name}`}
            className="rel-textarea"
            rows={4}
            spellCheck={false}
            value={set}
            onChange={(e) => setSet(e.target.value)}
            placeholder={"image.tag=${{ release.artifacts.image.tag }}"}
          />
        </div>
      )}
      {wrong && <p className="rel-msg">{wrong}</p>}
      <div className="rel-settings-actions">
        <Button size="sm" disabled={busy || !!wrong} onClick={() => void save()}>
          Save
        </Button>
      </div>
    </Panel>
  );
}

/** A duration as people say it. */
function took(secs: number | null | undefined) {
  if (secs === null || secs === undefined) return "—";
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
  return `${Math.floor(secs / 86400)}d ${Math.floor((secs % 86400) / 3600)}h`;
}

/** Where its images and chart go: the repositories kiln made for it (docs/deploy-api.md §3.1, step 5). */
function RegistriesPanel({ r }: { r: Registries }) {
  return (
    <Panel eyebrow="Registries" title="Where its images and chart go" description={r.at === "kiln" ? "On kiln's own registry." : `Made through ${r.at}.`}>
      <Facts
        items={[
          { label: "Release images", value: <code>{r.images}</code> },
          { label: "Snapshots", value: <code>{r.snapshots}</code> },
          { label: "Chart", value: <code>{r.chart}</code> },
        ]}
      />
      <p className="muted">A release image's tag never points at another image; only the newest snapshots are kept.</p>
    </Panel>
  );
}

/** How it delivers, over the last 30 days: DORA's four keys, from its releases. */
function Delivery({ app }: { app: string }) {
  const m = useSuspenseResource(`deploy/metrics/app/${app}`, (s) => deployApi.metrics({ app, days: 30 }, s), { on: DEPLOY_EVENTS }).data;
  if (m.changes === 0) return null;
  const rate = m.change_failure_rate;
  return (
    <Panel eyebrow="Delivery" title="The last 30 days" className="rel-delivery">
      <Facts
        items={[
          { label: "Changes landed", value: `${m.landed} (${m.per_day.toFixed(2)} a day)` },
          { label: "Lead time, asked to landed", value: `${took(m.lead_time_secs.p50)} p50 · ${took(m.lead_time_secs.p90)} p90` },
          { label: "Change failure rate", value: rate === null ? "—" : `${Math.round(rate * 100)}% (${m.failed})`, tone: rate !== null && rate > 0.15 ? "fail" : undefined },
          { label: "Time to restore", value: m.time_to_restore_secs.count ? `${took(m.time_to_restore_secs.p50)} p50` : "nothing to restore" },
        ]}
      />
    </Panel>
  );
}

function Latest({ app }: { app: string }) {
  const list = useSuspenseResource(`deploy/releases/app/${app}`, (s) => deployApi.releases({ app, limit: 30 }, s), { on: DEPLOY_EVENTS }).data;
  if (!list.length) return null;
  return (
    <Panel eyebrow="History" title="Its latest releases">
      <ol className="rel-history">
        {list.map((r) => (
          <li key={r.id}>
            <Link to={releaseHref(r.id)}>#{r.id}</Link>
            <span className="rel-env-name">{r.environment}</span>
            <span className="rel-what">{describe(r)}</span>
            <ReleaseState r={r} />
            <span className="rel-who">
              {r.requested_by} · <RelTime ts={r.created_at} />
            </span>
          </li>
        ))}
      </ol>
    </Panel>
  );
}

function AppView({ name }: { name: string }) {
  const d = useSuspenseResource(`deploy/app/${name}`, (s) => deployApi.app(name, s), { on: DEPLOY_EVENTS }).data;
  const envs = d.environments.map((e) => e.registration.environment);
  const a = d.app;
  const pkg = packageWords(a.package);
  return (
    <>
      <header className="cd-head">
        <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, { label: a.name }]} />
        <h1 className="cd-title">{a.name}</h1>
        <p className="cd-note">
          {a.project}
          {a.owners.length > 0 && ` · owned by ${a.owners.join(", ")}`}
          {a.template && ` · from ${a.template}`}
          {a.source && (
            <>
              {" · source "}
              <Link to="/repos" className="text-link">
                {a.source}
              </Link>
            </>
          )}{" "}
          · <code>{pkg}</code>
          {a.archived && " · archived"}
        </p>
      </header>
      {envs.length === 0 ? (
        <p className="rel-quiet">It isn't registered in any environment yet.</p>
      ) : (
        <div className="rel-envs">
          {envs.map((e) => (
            <Loadable key={e} label={e}>
              <EnvCard app={a.name} env={e} others={envs.filter((x) => x !== e)} />
            </Loadable>
          ))}
        </div>
      )}
      <div className="rel-stack">
        <Loadable label="the environments">
          <RegisterIn app={a.name} registered={envs} />
        </Loadable>
        <Loadable label="how it delivers">
          <Delivery app={a.name} />
        </Loadable>
        {a.registries && <RegistriesPanel r={a.registries} />}
        <Loadable label="its releases">
          <Latest app={a.name} />
        </Loadable>
        <Settings key={JSON.stringify(a)} a={a} registered={envs} />
        <Loadable label="how it deploys">
          <Deploys key={JSON.stringify(a)} a={a} registered={envs} />
        </Loadable>
      </div>
    </>
  );
}

export default function ReleaseApp() {
  const name = useParams().app ?? "";
  return (
    <div className="container-console page cd-page">
      <title>{`${name} · Releases · kiln`}</title>
      <Loadable label="the app">
        <AppView name={name} />
      </Loadable>
    </div>
  );
}
