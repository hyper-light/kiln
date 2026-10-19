/**
 * An app in one environment, as its provider sees it (docs/deploy-api.md §8.1): its objects with
 * their sync and health, the manifests it renders, what a sync would change, and its workloads'
 * logs, from KilnCD or Argo CD alike. On Argo CD another version is previewed: what it would
 * render, and what deploying it would change.
 */
import * as React from "react";
import { Link, useParams } from "@/lib/router";

import { appHref } from "@/api/cd";
import { boardHref, deployApi, describe, releaseAppHref, releaseHref, type RunningObject } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { CopyButton, Crumbs, SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { useSuspenseResource } from "@/lib/resource";
import { HealthPair, KindBadge, objectKey, objectLabel, SyncPair } from "@/views/deployments/parts";
import { FileDiffView } from "@/views/harness/diff";
import { DEPLOY_EVENTS } from "./index";
import "@/views/deployments/deployments.css";
import "./releases.css";

type Tab = "resources" | "manifests" | "diff" | "logs";

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
        </li>
      ))}
    </ul>
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

function RegistrationView({ app, env }: { app: string; env: string }) {
  const d = useSuspenseResource(`deploy/reg/${app}/${env}`, (s) => deployApi.registration(app, env, s), { on: DEPLOY_EVENTS }).data;
  const [tab, setTab] = React.useState<Tab>("resources");
  const [version, setVersion] = React.useState("");
  const [preview, setPreview] = React.useState("");
  const argo = d.provider.kind === "argocd";
  // Another version previews on Argo CD only (KilnCD shows what runs, and what git would change).
  const shown = argo ? preview : "";
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
        ]}
      />
      {argo && (tab === "manifests" || tab === "diff") && (
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
          {tab === "manifests" && <Manifests app={app} env={env} version={shown} />}
          {tab === "diff" && <Diffs app={app} env={env} version={shown} />}
          {tab === "logs" && <Logs app={app} env={env} />}
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
