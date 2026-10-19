/**
 * One app across its environments, side by side: what runs in each, what is on its way, what its
 * provider says; and from each, a release: a new version, new values, a promotion of what another
 * environment runs, or a rollback to one of the releases before. Below, its latest releases.
 */
import * as React from "react";
import { Link, useNavigate, useParams } from "@/lib/router";
import { Pause, Play, Rocket } from "lucide-react";

import { boardHref, deployApi, describe, providerName, releaseHref, STATE_DOT, STATE_WORDS, type ReleaseAsk } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel, SelectField, TextField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { RelTime } from "@/views/shared";
import { DEPLOY_EVENTS, ReleaseState } from "./index";
import "@/views/deployments/deployments.css";
import "./releases.css";

type Mode = "version" | "values" | "promote" | "rollback";

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
        <Button size="xs" onClick={() => setOpen(true)} disabled={!!d.in_flight}>
          <Rocket /> Release
        </Button>
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
      <ReleaseDialog app={app} env={env} others={others} open={open} onOpenChange={setOpen} />
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
  const pkg = a.package.kind === "helm" ? `${a.package.repo} ${a.package.chart}` : a.package.kind === "git" ? `${a.package.repo} ${a.package.path}` : `${a.package.provider}`;
  return (
    <>
      <header className="cd-head">
        <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, { label: a.name }]} />
        <h1 className="cd-title">{a.name}</h1>
        <p className="cd-note">
          {a.project}
          {a.owners.length > 0 && ` · owned by ${a.owners.join(", ")}`}
          {a.template && ` · from ${a.template}`} · <code>{pkg}</code>
          {a.archived && " · archived"}
        </p>
      </header>
      {envs.length === 0 ? (
        <p className="rel-quiet">It isn't registered in any environment yet: kiln app register {a.name} --env &lt;env&gt;.</p>
      ) : (
        <div className="rel-envs">
          {envs.map((e) => (
            <Loadable key={e} label={e}>
              <EnvCard app={a.name} env={e} others={envs.filter((x) => x !== e)} />
            </Loadable>
          ))}
        </div>
      )}
      <Loadable label="how it delivers">
        <Delivery app={a.name} />
      </Loadable>
      <Loadable label="its releases">
        <Latest app={a.name} />
      </Loadable>
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
