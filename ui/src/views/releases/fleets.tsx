/**
 * Fleet changes (docs/deploy-api.md §3.3): one change to many apps at once. A version, or values
 * set, released to the apps of an environment that a selection picks, each through the
 * environment's gates as any release, and followed together; or a change in each app's source
 * repository (a kiln-base version, or values set in its chart), proposed as one pull request per
 * repository. On kiln's own git server, where there are no pull requests, its branches are
 * merged from here.
 */
import * as React from "react";
import { Link, useNavigate, useParams } from "@/lib/router";
import { GitMerge, Rocket } from "lucide-react";

import { boardHref, deployApi, describe, fleetHref, releaseAppHref, releaseHref, type Fleet, type FleetAsk, type Proposal } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Panel, SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { RelTime } from "@/views/shared";
import { appsRes, boardRes, DEPLOY_EVENTS, environmentsRes, ReleaseState } from "./index";
import "@/views/deployments/deployments.css";
import "./releases.css";

type What = "release" | "source";
type Pick = "all" | "apps" | "template" | "project" | "owner";

/** `path=value` lines as a map, or the first line that isn't one. */
function pairs(text: string): { set: Record<string, string>; wrong?: string } {
  const set: Record<string, string> = {};
  for (const raw of text.split("\n")) {
    const l = raw.trim();
    if (!l) continue;
    const at = l.indexOf("=");
    if (at <= 0) return { set, wrong: `${l}: each value is path=value, one a line.` };
    set[l.slice(0, at).trim()] = l.slice(at + 1).trim();
  }
  return { set };
}

const unique = (xs: string[]) => [...new Set(xs)].sort();

function NewFleetForm() {
  const navigate = useNavigate();
  const environments = useRes(environmentsRes).data;
  const apps = useRes(appsRes).data;
  const board = useRes(boardRes).data;
  const [what, setWhat] = React.useState<What>("release");
  const [env, setEnv] = React.useState(environments[0]?.name ?? "");
  const [byVersion, setByVersion] = React.useState(true);
  const [version, setVersion] = React.useState("");
  const [values, setValues] = React.useState("");
  const [chart, setChart] = React.useState("");
  const [valuesFile, setValuesFile] = React.useState("");
  const [pick, setPick] = React.useState<Pick>("all");
  const [chosen, setChosen] = React.useState<string[]>([]);
  const [by, setBy] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  // A release goes to the apps registered in the environment; a change in sources to any app, or
  // those registered there when one is named.
  const there = new Set(board.cells.filter((c) => c.registration.environment === env).map((c) => c.registration.app));
  const candidates = apps.filter((a) => (what === "source" && !env) || there.has(a.name));
  const choices: Record<Exclude<Pick, "all" | "apps">, string[]> = {
    template: unique(candidates.flatMap((a) => (a.template ? [a.template] : []))),
    project: unique(candidates.map((a) => a.project)),
    owner: unique(candidates.flatMap((a) => a.owners)),
  };
  const ask = (): { ask?: FleetAsk; wrong?: string } => {
    const f: FleetAsk = { environment: env };
    if (pick === "apps") {
      const picked = chosen.filter((n) => candidates.some((a) => a.name === n));
      if (!picked.length) return { wrong: "Choose an app at least." };
      f.apps = picked;
    } else if (pick !== "all") {
      const v = choices[pick].includes(by) ? by : (choices[pick][0] ?? "");
      if (!v) return { wrong: pick === "template" ? "None of these apps is made from a template." : "No app here to choose from." };
      f[pick] = v;
    }
    if (what === "release") {
      if (!env) return { wrong: "The environment they're released in." };
      if (byVersion) return version.trim() ? { ask: { ...f, version: version.trim() } } : { wrong: "The version each deploys." };
      const p = pairs(values);
      if (p.wrong) return { wrong: p.wrong };
      return Object.keys(p.set).length ? { ask: { ...f, set: p.set } } : { wrong: "A value to set: path=value." };
    }
    const where = { chart: chart.trim() || undefined, values: valuesFile.trim() || undefined };
    if (byVersion) return version.trim() ? { ask: { ...f, source: { ...where, base: version.trim() } } } : { wrong: "The kiln-base version each chart depends on." };
    const p = pairs(values);
    if (p.wrong) return { wrong: p.wrong };
    return Object.keys(p.set).length ? { ask: { ...f, source: { ...where, set: p.set } } } : { wrong: "A value to set: path=value." };
  };
  const made = ask();
  const go = async () => {
    if (!made.ask) return;
    setBusy(true);
    try {
      const r = await deployApi.createFleet({ ...made.ask, idempotency_key: crypto.randomUUID() });
      toast({
        title: `Fleet change ${r.fleet}`,
        description: what === "release" ? "Each app's release goes through the environment's gates." : "Each repository has its proposal.",
        tone: "running",
        href: fleetHref(r.fleet),
      });
      invalidate("deploy/");
      navigate(fleetHref(r.fleet));
    } catch (e) {
      toast({ title: "No fleet change", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const envOptions = [...(what === "source" ? [{ value: "", label: "Any app" }] : []), ...environments.map((e) => ({ value: e.name, label: e.name }))];
  return (
    <div className="rel-stack">
      <Panel eyebrow="1" title="What changes">
        <Segmented
          label="What changes"
          value={what}
          onChange={(v) => {
            setWhat(v);
            if (v === "release" && !env) setEnv(environments[0]?.name ?? "");
          }}
          options={[
            { value: "release", label: "Release them" },
            { value: "source", label: "Change their sources" },
          ]}
        />
        <div className="cd-new-grid rel-gap">
          <Segmented
            label={what === "release" ? "Each release" : "In each source"}
            value={byVersion ? "version" : "values"}
            onChange={(v) => setByVersion(v === "version")}
            options={[
              { value: "version", label: what === "release" ? "A version" : "A kiln-base version" },
              { value: "values", label: "Values set" },
            ]}
          />
          {byVersion ? (
            <TextField
              label={what === "release" ? "Version" : "kiln-base's version"}
              placeholder={what === "release" ? "1.4.2" : "0.1.4"}
              value={version}
              spellCheck={false}
              onChange={(e) => setVersion(e.target.value)}
              hint={what === "release" ? "Each app deploys it, as its own release would." : "Each app's chart depends on it, a vendored one moved too."}
            />
          ) : (
            <div>
              <label className="field-label" htmlFor="fleet-values">
                Values set, by path
              </label>
              <textarea
                id="fleet-values"
                className="rel-textarea"
                rows={4}
                spellCheck={false}
                value={values}
                onChange={(e) => setValues(e.target.value)}
                placeholder={"replicaCount=2\nimage.pullPolicy=IfNotPresent"}
              />
            </div>
          )}
        </div>
        {what === "source" && (
          <div className="cd-new-grid rel-gap">
            <TextField
              label="Each chart"
              placeholder="charts/{app}"
              value={chart}
              spellCheck={false}
              onChange={(e) => setChart(e.target.value)}
              hint="Where it is in the app's repository, {app} its name."
            />
            <TextField label="Its values file" placeholder="<chart>/values.yaml" value={valuesFile} spellCheck={false} onChange={(e) => setValuesFile(e.target.value)} />
          </div>
        )}
      </Panel>
      <Panel eyebrow="2" title="Which apps" description={what === "release" ? "Those registered in the environment the selection picks." : "Any app's, or those registered in an environment."}>
        <div className="cd-new-grid">
          <SelectField label="Environment" value={env} onChange={setEnv} options={envOptions} />
          <SelectField
            label="Selected"
            value={pick}
            onChange={(v) => {
              setPick(v as Pick);
              setBy("");
            }}
            options={[
              { value: "all", label: "Every one" },
              { value: "apps", label: "These apps" },
              { value: "template", label: "Made from a template" },
              { value: "project", label: "In a project" },
              { value: "owner", label: "Owned by a team" },
            ]}
          />
          {pick !== "all" && pick !== "apps" && (
            <SelectField label={pick === "owner" ? "Team" : pick === "project" ? "Project" : "Template"} value={by} onChange={setBy} options={choices[pick].map((c) => ({ value: c, label: c }))} />
          )}
        </div>
        {pick === "apps" &&
          (candidates.length ? (
            <fieldset className="rel-candidates">
              <legend className="field-label">Apps</legend>
              {candidates.map((a) => (
                <label key={a.name}>
                  <input type="checkbox" className="check" checked={chosen.includes(a.name)} onChange={(e) => setChosen(e.target.checked ? [...chosen, a.name] : chosen.filter((n) => n !== a.name))} />
                  <span>
                    {a.name} <span className="rel-quiet">· {a.project}</span>
                  </span>
                </label>
              ))}
            </fieldset>
          ) : (
            <p className="rel-quiet">No app is registered there.</p>
          ))}
        {pick === "all" && <p className="rel-quiet">{candidates.length ? `${candidates.length} apps: ${candidates.map((a) => a.name).join(", ")}.` : "No app is registered there."}</p>}
      </Panel>
      {made.wrong && <p className="rel-msg">{made.wrong}</p>}
      <div className="rel-settings-actions">
        <Button disabled={busy || !made.ask} onClick={() => void go()}>
          <Rocket /> {what === "release" ? "Release them" : "Propose it"}
        </Button>
      </div>
    </div>
  );
}

export default function NewFleet() {
  return (
    <div className="container-console page cd-page">
      <title>Fleet change · Releases · kiln</title>
      <header className="cd-head">
        <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, { label: "Fleet change" }]} />
        <h1 className="cd-title">A fleet change</h1>
        <p className="cd-note">One change to many apps: released to each, through its environment's gates; or proposed in each app's source repository, a pull request each.</p>
      </header>
      <Loadable label="the apps">
        <NewFleetForm />
      </Loadable>
    </div>
  );
}

const PROPOSAL_WORDS: Record<Proposal["state"], string> = { open: "open", merged: "merged", closed: "closed", unchanged: "unchanged: it holds the change already", failed: "failed" };

/** One repository's proposal: where it is, what it changes, and its merge on kiln's own git server. */
function ProposalRow({ id, p }: { id: string; p: Proposal }) {
  const [busy, setBusy] = React.useState(false);
  const merge = async () => {
    setBusy(true);
    try {
      await deployApi.mergeFleet(id, p.repo);
      toast({ title: `${p.repo}: merged into ${p.into}`, tone: "succeeded" });
      invalidate(`deploy/fleet/${id}`);
    } catch (e) {
      toast({ title: "Not merged", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <li>
      <span className="rel-object">
        <span className="rel-what">{p.repo}</span>
        <span className="rel-quiet">
          {p.branch} → {p.into} · {PROPOSAL_WORDS[p.state]}
        </span>
      </span>
      <span className="rel-quiet">
        {p.apps.join(", ")}: {p.files.join(", ")}
      </span>
      {p.url && (
        <a href={p.url} target="_blank" rel="noreferrer">
          {p.host === "gitlab" ? "merge request" : "pull request"}
          {p.number ? ` #${p.number}` : ""}
        </a>
      )}
      {p.host === "kiln" && p.state === "open" && (
        <Button size="sm" variant="secondary" disabled={busy} onClick={() => void merge()}>
          <GitMerge /> Merge
        </Button>
      )}
      {p.note && <p className="rel-msg">{p.note}</p>}
      {p.diff && (
        <details className="rel-manifest">
          <summary>Its diff</summary>
          <pre className="rel-log">{p.diff}</pre>
        </details>
      )}
    </li>
  );
}

function FleetView({ id }: { id: string }) {
  const f: Fleet = useSuspenseResource(`deploy/fleet/${id}`, (s) => deployApi.fleet(id, s), { on: DEPLOY_EVENTS, pollMs: 10_000 }).data;
  const [withdrawing, setWithdrawing] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const open = (f.proposals ?? []).filter((p) => p.state === "open");
  const mergeAll = async () => {
    setBusy(true);
    try {
      await deployApi.mergeFleet(id);
      toast({ title: "Each open proposal on kiln's git server is merged", tone: "succeeded" });
      invalidate(`deploy/fleet/${id}`);
    } catch (e) {
      toast({ title: "Not merged", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const withdraw = async () => {
    try {
      await deployApi.withdrawFleet(id);
      toast({ title: `Fleet change ${id} is withdrawn`, description: "Each pull request is closed, and each branch on kiln's git server deleted.", tone: "info" });
      invalidate(`deploy/fleet/${id}`);
    } catch (e) {
      toast({ title: "Not withdrawn", description: errorMessage(e), tone: "failed" });
    }
  };
  const states = Object.entries(f.states)
    .map(([s, n]) => `${n} ${s.replace(/_/g, " ")}`)
    .join(", ");
  return (
    <>
      <header className="cd-head">
        <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, { label: `Fleet change ${id}` }]} />
        <h1 className="cd-title">Fleet change {id}</h1>
        <p className="cd-note">
          {f.source ? "In apps' sources" : "Released"} · {states || "nothing yet"} · {f.done ? "all done" : "under way"}
        </p>
      </header>
      {f.source ? (
        <Panel
          eyebrow="Proposals"
          title="A pull request in each repository"
          description="On kiln's own git server, a branch each, merged here; on another host, merged there."
          actions={
            open.length > 0 ? (
              <>
                {open.some((p) => p.host === "kiln") && (
                  <Button size="sm" variant="secondary" disabled={busy} onClick={() => void mergeAll()}>
                    <GitMerge /> Merge each
                  </Button>
                )}
                <Button size="sm" variant="ghost-danger" onClick={() => setWithdrawing(true)}>
                  Withdraw
                </Button>
              </>
            ) : undefined
          }
        >
          <ul className="rel-objects">
            {(f.proposals ?? []).map((p) => (
              <ProposalRow key={p.repo} id={id} p={p} />
            ))}
          </ul>
        </Panel>
      ) : (
        <Panel eyebrow="Releases" title="Each app's release">
          <ol className="rel-history">
            {(f.releases ?? []).map((r) => (
              <li key={r.id}>
                <Link to={releaseHref(r.id)}>#{r.id}</Link>
                <Link to={releaseAppHref(r.app)} className="rel-env-name">
                  {r.app}
                </Link>
                <span className="rel-what">
                  {describe(r)} → {r.environment}
                </span>
                <ReleaseState r={r} />
                <span className="rel-who">
                  <RelTime ts={r.created_at} />
                </span>
              </li>
            ))}
          </ol>
        </Panel>
      )}
      <ConfirmDialog
        open={withdrawing}
        onOpenChange={setWithdrawing}
        title={`Withdraw fleet change ${id}?`}
        description="Each open pull request is closed, saying so, and each branch on kiln's git server deleted. Merged ones stay merged."
        confirmLabel="Withdraw it"
        danger
        onConfirm={withdraw}
      />
    </>
  );
}

export function FleetPage() {
  const id = useParams().id ?? "";
  return (
    <div className="container-console page cd-page">
      <title>{`Fleet change ${id} · kiln`}</title>
      <Loadable label="the fleet change">
        <FleetView id={id} />
      </Loadable>
    </div>
  );
}
