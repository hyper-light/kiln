/**
 * An app in one environment, as its provider sees it (docs/deploy-api.md §8.1): its objects with
 * their sync and health, the manifests it renders, what a sync would change, and its workloads'
 * logs, from KilnCD or Argo CD alike; and another version previewed: what it would render, and
 * what deploying it would change. Its secrets there (§3.5): set, never shown again.
 */
import * as React from "react";
import { Link, useParams } from "@/lib/router";

import { appHref } from "@/api/cd";
import { boardHref, deployApi, describe, releaseAppHref, releaseHref, type ObjectAction, type RunningObject, type SecretMeta } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { Loadable } from "@/components/ui/loading";
import { CopyButton, Crumbs, SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { HealthPair, KindBadge, objectKey, objectLabel, SyncPair } from "@/views/deployments/parts";
import { FileDiffView } from "@/views/harness/diff";
import { DEPLOY_EVENTS } from "./index";
import "@/views/deployments/deployments.css";
import "./releases.css";

type Tab = "resources" | "manifests" | "diff" | "logs" | "secrets";

/** The kinds whose pods have logs. */
const RUNS_PODS = new Set(["Deployment", "StatefulSet", "DaemonSet", "ReplicaSet", "Job", "Pod"]);

function useObjects(app: string, env: string) {
  return useSuspenseResource(`deploy/reads/${app}/${env}/resources`, (s) => deployApi.resources(app, env, s), { on: DEPLOY_EVENTS, pollMs: 15_000 }).data;
}

/** What it says of an object, when it says anything. */
function says(o: RunningObject) {
  if (o.missing) return "not there yet: a sync creates it";
  if (o.requires_pruning) return "no longer in git: a sync that prunes deletes it";
  return o.message ?? o.why ?? "";
}

function Objects({ app, env }: { app: string; env: string }) {
  const list = useObjects(app, env);
  if (!list.length) return <p className="rel-quiet">Nothing runs here yet.</p>;
  return (
    <ul className="rel-objects">
      {list.map((o) => (
        <li key={objectKey(o)}>
          <span className="rel-object">
            <KindBadge kind={o.kind} group={o.group} />
            <span className="rel-object-name">
              {o.namespace ? `${o.namespace}/` : ""}
              {o.name}
            </span>
          </span>
          <SyncPair sync={o.sync} />
          <HealthPair health={o.health} />
          {says(o) && <span className="rel-msg">{says(o)}</span>}
          <ObjectActions app={app} env={env} o={o} />
        </li>
      ))}
    </ul>
  );
}

/** An action's name, as people say it. */
const ACTION_WORDS: Record<string, string> = { "create-job": "Run it now", delete: "Delete it", restart: "Restart", pause: "Pause its rollout", resume: "Resume", suspend: "Suspend", scale: "Scale" };
const actionWords = (name: string) => ACTION_WORDS[name] ?? name.replace(/-/g, " ").replace(/^./, (c) => c.toUpperCase());

/** What its provider can do to an object now, in a dialog: a restart, a pause, a scale, a pod deleted. */
function ObjectActions({ app, env, o }: { app: string; env: string; o: RunningObject }) {
  const [open, setOpen] = React.useState(false);
  return (
    <>
      <Button size="sm" variant="ghost" onClick={() => setOpen(true)} aria-label={`What can be done to ${o.kind} ${o.name}`}>
        Actions
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent eyebrow="Actions" title={`${o.kind} ${o.name}`} description="What its provider can do to it as it runs. A release changes what runs; a sync may undo what these change.">
          {open && (
            <Loadable label="what it can do">
              <ActionList app={app} env={env} o={o} onDone={() => setOpen(false)} />
            </Loadable>
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}

function ActionList({ app, env, o, onDone }: { app: string; env: string; o: RunningObject; onDone: () => void }) {
  const target = { group: o.group || undefined, kind: o.kind, namespace: o.namespace || undefined, name: o.name };
  const list = useSuspenseResource(`deploy/actions/${app}/${env}/${objectKey(o)}`, (s) => deployApi.actions(app, env, target, s)).data;
  const [params, setParams] = React.useState<Record<string, string>>({});
  const [busy, setBusy] = React.useState(false);
  const [deleting, setDeleting] = React.useState<ObjectAction | null>(null);
  const run = async (a: ObjectAction) => {
    const given = Object.fromEntries((a.params ?? []).map((p) => [p.name, params[`${a.name}.${p.name}`] ?? p.default ?? ""]));
    setBusy(true);
    try {
      const r = await deployApi.runAction(app, env, target, a.name, given);
      toast({ title: actionWords(a.name), description: r.done, tone: "succeeded" });
      invalidate(`deploy/reads/${app}/${env}`);
      onDone();
    } catch (err) {
      toast({ title: `Not done: ${actionWords(a.name).toLowerCase()}`, description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  if (!list.length) return <p className="rel-quiet">Its provider offers nothing to do to a {o.kind}.</p>;
  return (
    <div className="rel-dialog">
      {list.map((a) => (
        <div key={a.name} className="rel-register">
          {(a.params ?? []).map((p) => (
            <TextField
              key={p.name}
              label={p.name}
              value={params[`${a.name}.${p.name}`] ?? p.default ?? ""}
              spellCheck={false}
              onChange={(e) => setParams((x) => ({ ...x, [`${a.name}.${p.name}`]: e.target.value }))}
            />
          ))}
          <Button size="sm" variant={a.name === "delete" ? "ghost-danger" : "secondary"} disabled={a.disabled || busy} onClick={() => (a.name === "delete" ? setDeleting(a) : void run(a))}>
            {actionWords(a.name)}
          </Button>
          {a.disabled && <span className="rel-quiet">not now</span>}
        </div>
      ))}
      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(v) => !v && setDeleting(null)}
        title={`Delete ${o.name}?`}
        description="Its workload makes it again."
        confirmLabel="Delete it"
        danger
        onConfirm={() => (deleting ? run(deleting) : undefined)}
      />
    </div>
  );
}

function Manifests({ app, env, version }: { app: string; env: string; version: string }) {
  const list = useSuspenseResource(`deploy/reads/${app}/${env}/manifests/${version}`, (s) => deployApi.manifests(app, env, version || undefined, s), { on: DEPLOY_EVENTS }).data;
  if (!list.length) return <p className="rel-quiet">{version ? `${version} renders nothing.` : "It renders nothing."}</p>;
  return (
    <div className="rel-manifests">
      {list.map((m) => (
        <details key={objectKey(m.key)} className="rel-manifest">
          <summary>
            <KindBadge kind={m.key.kind} group={m.key.group} /> {objectLabel(m.key)}
          </summary>
          <div className="cd-manifest">
            <div className="cd-manifest-bar">
              <span className="cd-meta">{objectLabel(m.key)}</span>
              <CopyButton text={m.yaml} className="cd-copy" />
            </div>
            <pre className="cd-yaml">{m.yaml}</pre>
          </div>
        </details>
      ))}
    </div>
  );
}

function Diffs({ app, env, version }: { app: string; env: string; version: string }) {
  const list = useSuspenseResource(`deploy/reads/${app}/${env}/diff/${version}`, (s) => deployApi.diff(app, env, version || undefined, s), { on: DEPLOY_EVENTS }).data;
  const changed = list.filter((d) => d.hunks.length > 0 || d.missing || d.requires_pruning);
  if (!changed.length) return <p className="rel-quiet">{version ? `Deploying ${version} changes nothing that runs.` : "It matches git: a sync changes nothing."}</p>;
  return (
    <div className="cd-diffs">
      <p className="cd-meta">{version ? `What deploying ${version} changes` : "What a sync changes"}: − is what runs, + what it becomes. Fields the cluster fills in itself are left out.</p>
      {changed.map((d) => (
        <div key={objectKey(d.key)}>
          {d.missing && <p className="rel-msg">{objectLabel(d.key)}: not there yet, made by a sync.</p>}
          {d.requires_pruning && <p className="rel-msg">{objectLabel(d.key)}: no longer in git, deleted by a sync that prunes.</p>}
          {d.hunks.length > 0 && <FileDiffView diff={{ path: objectLabel(d.key), hunks: d.hunks }} label={objectLabel(d.key)} />}
        </div>
      ))}
    </div>
  );
}

function LogLines({ app, env, w, tail }: { app: string; env: string; w: RunningObject; tail: number }) {
  const log = useSuspenseResource(`deploy/logs/${app}/${env}/${objectKey(w)}/${tail}`, (s) => deployApi.logs(app, env, { kind: w.kind, name: w.name, namespace: w.namespace, tail }, s), {
    pollMs: 10_000,
  }).data;
  const several = new Set(log.lines.map((l) => l.pod)).size > 1;
  return (
    <>
      {!!log.errors?.length && <p className="rel-msg">{log.errors.join("; ")}</p>}
      {log.lines.length ? (
        <pre className="rel-log">{log.lines.map((l) => `${l.ts.slice(11, 19)}  ${several ? `${l.pod}  ` : ""}${l.text}`).join("\n")}</pre>
      ) : (
        <p className="rel-quiet">No lines yet.</p>
      )}
    </>
  );
}

function Logs({ app, env }: { app: string; env: string }) {
  const workloads = useObjects(app, env).filter((o) => RUNS_PODS.has(o.kind) && !o.missing);
  const [pick, setPick] = React.useState("");
  const [tail, setTail] = React.useState("200");
  const w = workloads.find((o) => objectKey(o) === pick) ?? workloads[0];
  if (!w) return <p className="rel-quiet">Nothing here runs pods.</p>;
  return (
    <>
      <div className="rel-register">
        <SelectField label="Workload" value={objectKey(w)} onChange={setPick} options={workloads.map((o) => ({ value: objectKey(o), label: objectLabel(o) }))} />
        <SelectField label="Lines" value={tail} onChange={setTail} options={["100", "200", "1000"].map((n) => ({ value: n, label: `the last ${n}` }))} />
      </div>
      <Loadable label="its logs">
        <LogLines app={app} env={env} w={w} tail={Number(tail)} />
      </Loadable>
    </>
  );
}

/** What the server says happens next (`release 12 delivers it`), as a sentence. */
const sentence = (s: string) => `${s.charAt(0).toUpperCase()}${s.slice(1)}.`;

/** A secret's versions, newest first: who set each and when, and which keep their value. */
function SecretVersions({ app, env, name }: { app: string; env: string; name: string }) {
  const versions = useSuspenseResource(`deploy/secrets/${app}/${env}/${name}/versions`, (s) => deployApi.secretVersions(app, env, name, s)).data;
  return (
    <ul className="rel-secret-versions">
      {versions.map((v) => (
        <li key={v.version}>
          <span className="rel-what">v{v.version}</span> {v.deleted ? "deleted" : "set"} by {v.set_by} {relativeTime(v.set_at)}
          {!v.deleted && v.destroyed && <span className="rel-quiet"> · its value no longer kept</span>}
        </li>
      ))}
    </ul>
  );
}

/** The reads of the secrets for deliveries, newest first. */
function SecretUses({ app, env }: { app: string; env: string }) {
  const reads = useSuspenseResource(`deploy/secrets/${app}/${env}/uses`, (s) => deployApi.secretUses(app, env, s), { on: DEPLOY_EVENTS }).data;
  if (!reads.length) return <p className="rel-quiet">No delivery has read them yet.</p>;
  return (
    <ul className="rel-secret-versions">
      {reads.map((r, i) => (
        <li key={i}>
          {relativeTime(r.at)} · {r.by} for {r.purpose}: <span className="rel-what">{r.versions || "none"}</span>
        </li>
      ))}
    </ul>
  );
}

/** One secret: its version and who set it, a new value for it, its versions, its deletion. */
function SecretRow({ app, env, m, onDelete }: { app: string; env: string; m: SecretMeta; onDelete: () => void }) {
  const [value, setValue] = React.useState("");
  const [setting, setSetting] = React.useState(false);
  const [history, setHistory] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const save = async () => {
    setBusy(true);
    try {
      const next = await deployApi.setSecret(app, env, m.name, value);
      toast({ title: `${m.name} is version ${next.version}`, description: sentence(next.next), tone: "succeeded" });
      setValue("");
      setSetting(false);
      invalidate(`deploy/secrets/${app}/${env}`);
    } catch (err) {
      toast({ title: "Not set", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <li>
      <span className="rel-object">
        <span className="rel-what">{m.name}</span>
        <span className="rel-quiet">
          v{m.version} · {m.size} B · set by {m.set_by} {relativeTime(m.set_at)}
        </span>
      </span>
      <span className="rel-secret-acts">
        <Button size="sm" variant="ghost" onClick={() => setSetting(!setting)}>
          New value
        </Button>
        <Button size="sm" variant="ghost" onClick={() => setHistory(!history)}>
          Versions
        </Button>
        <Button size="sm" variant="ghost-danger" onClick={onDelete}>
          Delete
        </Button>
      </span>
      {setting && (
        <form
          className="rel-register rel-secret-form"
          onSubmit={(e) => {
            e.preventDefault();
            void save();
          }}
        >
          <TextField label={`${m.name}'s new value`} type="password" autoComplete="off" value={value} onChange={(e) => setValue(e.target.value)} />
          <Button size="sm" type="submit" disabled={busy || !value}>
            Set
          </Button>
        </form>
      )}
      {history && (
        <Loadable label="its versions">
          <SecretVersions app={app} env={env} name={m.name} />
        </Loadable>
      )}
    </li>
  );
}

/**
 * The app's secrets here: names, versions, who set each and when. A value is set (a password
 * field, or `kiln app secret set` for one of several lines) and never shown again; on KilnCD, what
 * runs gets them as the Secret `<app>-kiln`.
 */
function Secrets({ app, env, argo }: { app: string; env: string; argo: boolean }) {
  const { secrets } = useSuspenseResource(`deploy/secrets/${app}/${env}`, (s) => deployApi.secrets(app, env, s), { on: DEPLOY_EVENTS }).data;
  const [name, setName] = React.useState("");
  const [value, setValue] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [deleting, setDeleting] = React.useState<string | null>(null);
  const add = async () => {
    setBusy(true);
    try {
      const m = await deployApi.setSecret(app, env, name, value);
      toast({ title: `${m.name} is version ${m.version}`, description: sentence(m.next), tone: "succeeded" });
      setName("");
      setValue("");
      invalidate(`deploy/secrets/${app}/${env}`);
    } catch (err) {
      toast({ title: "Not set", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const remove = async () => {
    if (!deleting) return;
    try {
      const gone = await deployApi.deleteSecret(app, env, deleting);
      toast({ title: `${deleting} is deleted`, description: `${sentence(gone.next)} The record of its versions stays.`, tone: "succeeded" });
      invalidate(`deploy/secrets/${app}/${env}`);
    } catch (err) {
      toast({ title: "Not deleted", description: errorMessage(err), tone: "failed" });
    }
  };
  return (
    <>
      <p className="rel-msg">
        {argo
          ? "Argo CD renders from git alone, so these don't reach it yet: they're kept for when they can."
          : `Each release delivers them, at their versions then, as the Secret ${app}-kiln (kiln-base reads it with kilnSecrets: true); a change is released. Values are never shown again.`}
      </p>
      {secrets.length ? (
        <ul className="rel-objects">
          {secrets.map((m) => (
            <SecretRow key={m.name} app={app} env={env} m={m} onDelete={() => setDeleting(m.name)} />
          ))}
        </ul>
      ) : (
        <p className="rel-quiet">No secrets yet.</p>
      )}
      <form
        className="rel-register"
        onSubmit={(e) => {
          e.preventDefault();
          void add();
        }}
      >
        <TextField label="Name" placeholder="DB_PASSWORD" value={name} spellCheck={false} autoComplete="off" onChange={(e) => setName(e.target.value.toUpperCase())} />
        <TextField
          label="Value"
          type="password"
          autoComplete="off"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          hint="One line; one of several (a key, a certificate) with kiln app secret set --from-file."
        />
        <Button size="sm" type="submit" disabled={busy || !name.trim() || !value}>
          Add
        </Button>
      </form>
      <h3 className="rel-sub">Read for deliveries</h3>
      <Loadable label="its reads">
        <SecretUses app={app} env={env} />
      </Loadable>
      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(o) => !o && setDeleting(null)}
        title={`Delete ${deleting ?? ""}?`}
        description="A release takes it from what runs. The record of its versions stays."
        confirmLabel="Delete the secret"
        danger
        onConfirm={remove}
      />
    </>
  );
}

function RegistrationView({ app, env }: { app: string; env: string }) {
  const d = useSuspenseResource(`deploy/reg/${app}/${env}`, (s) => deployApi.registration(app, env, s), { on: DEPLOY_EVENTS }).data;
  const [tab, setTab] = React.useState<Tab>("resources");
  const [version, setVersion] = React.useState("");
  const [preview, setPreview] = React.useState("");
  const argo = d.provider.kind === "argocd";
  return (
    <>
      <header className="cd-head">
        <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, { label: app, to: releaseAppHref(app) }, { label: env }]} />
        <h1 className="cd-title">
          {app} <span className="rel-title-in">in</span> {env}
        </h1>
        <p className="cd-note">
          {argo ? (
            <>
              Argo CD {d.provider.integration} · Application <code>{d.provider.app}</code>
            </>
          ) : (
            <>
              KilnCD · <Link to={appHref(d.provider.app)}>{d.provider.app}</Link>
            </>
          )}{" "}
          · {d.running ? <Link to={releaseHref(d.running.id)}>runs {describe(d.running)}</Link> : "nothing released yet"}
        </p>
        {d.registration.degraded && <p className="rel-msg rel-alert">Needs a person: {d.registration.degraded}. The next release that lands clears this.</p>}
      </header>
      <Segmented
        className="rel-tabs"
        label="What to show"
        value={tab}
        onChange={setTab}
        options={[
          { value: "resources", label: "Objects" },
          { value: "manifests", label: "Manifests" },
          { value: "diff", label: "Diff" },
          { value: "logs", label: "Logs" },
          { value: "secrets", label: "Secrets" },
        ]}
      />
      {d.running && (tab === "manifests" || tab === "diff") && (
        <div className="rel-register">
          <TextField label="Another version" placeholder="6.11.0" value={version} spellCheck={false} onChange={(e) => setVersion(e.target.value)} />
          <Button size="sm" variant="secondary" disabled={!version.trim()} onClick={() => setPreview(version.trim())}>
            Preview
          </Button>
          {preview && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                setPreview("");
                setVersion("");
              }}
            >
              What runs
            </Button>
          )}
        </div>
      )}
      <div className="rel-reads">
        <Loadable label={tab === "resources" ? "its objects" : tab}>
          {tab === "resources" && <Objects app={app} env={env} />}
          {tab === "manifests" && <Manifests app={app} env={env} version={preview} />}
          {tab === "diff" && <Diffs app={app} env={env} version={preview} />}
          {tab === "logs" && <Logs app={app} env={env} />}
          {tab === "secrets" && <Secrets app={app} env={env} argo={argo} />}
        </Loadable>
      </div>
    </>
  );
}

export default function ReleaseRegistration() {
  const params = useParams();
  const app = params.app ?? "";
  const env = params.env ?? "";
  return (
    <div className="container-console page cd-page">
      <title>{`${app} in ${env} · Releases · kiln`}</title>
      <Loadable label={`${app} in ${env}`}>
        <RegistrationView app={app} env={env} />
      </Loadable>
    </div>
  );
}
