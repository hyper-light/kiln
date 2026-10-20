/**
 * A new app, deployed by API (docs/deploy-api.md §3.1): one column that grows as it's answered.
 *
 * 1. Its name, project and owner teams; the name checked as it's left.
 * 2. What it's made from: a template (its package, namespace, checks, and the source repository it
 *    makes), or a package given here.
 * 3. Its source repository: the template's, one kiln has, or none.
 * 4. Where it runs: the environments it's registered in once it's made.
 *
 * Then its page, to release it.
 */
import * as React from "react";
import { Rocket } from "lucide-react";

import { boardHref, checkName, deployApi, packageWords, providerName, releaseAppHref, type AppAsk, type Package } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Panel, SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { reposRes, useRes, warm } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { useNavigate } from "@/lib/router";
import { environmentsRes, templatesRes } from "./index";
import "@/views/deployments/deployments.css";
import "./releases.css";

/** A DNS label, as Kubernetes names things. */
const LABEL = /^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$/;

type Source = "template" | "link" | "none";
type Kind = "helm" | "git";

/** What the name, checked, came to. */
type NameCheck = { name: string; available: boolean; why?: string | null };

export default function NewDeployApp() {
  warm(templatesRes, environmentsRes, reposRes);
  return (
    <div className="container-console page cd-page cd-new">
      <title>New app · Releases · kiln</title>
      <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, { label: "New app" }]} />
      <h1 className="cd-new-title">A new app.</h1>
      <p className="lead">Its name and owners, what it's made from, where its source lives, where it runs. Then release it from its page.</p>
      <Loadable label="the templates and environments" fallback={<Hold height={320} />}>
        <Builder />
      </Loadable>
    </div>
  );
}

function Builder() {
  const templates = useRes(templatesRes).data;
  const environments = useRes(environmentsRes).data;
  const repos = useRes(reposRes).data;
  const navigate = useNavigate();

  const [name, setName] = React.useState("");
  const [checked, setChecked] = React.useState<NameCheck | null>(null);
  const [project, setProject] = React.useState("default");
  const [owners, setOwners] = React.useState("");
  const [templateName, setTemplateName] = React.useState(templates[0]?.name ?? "");
  const [kind, setKind] = React.useState<Kind>("helm");
  const [repo, setRepo] = React.useState("");
  const [chart, setChart] = React.useState("");
  const [path, setPath] = React.useState("");
  const [source, setSource] = React.useState<Source | null>(null);
  const [linked, setLinked] = React.useState(repos[0]?.name ?? "");
  const [envs, setEnvs] = React.useState<string[]>([]);
  const [busy, setBusy] = React.useState(false);

  const t = templates.find((x) => x.name === templateName);
  const makes = t?.repository;
  // Until chosen: the template's repository when it makes one, otherwise none.
  const src: Source = source ?? (makes ? "template" : "none");
  const allowed = environments.filter((e) => !t?.environments?.length || t.environments.includes(e.name));
  const clean = name.trim();
  const nameWrong = clean && !LABEL.test(clean) ? "Lowercase letters, digits and '-', at most 63, as Kubernetes names things." : null;
  const taken = checked && checked.name === clean && !checked.available ? (checked.why ?? `${clean} is taken`) : null;

  const check = async () => {
    if (!clean || nameWrong) return;
    try {
      setChecked(await deployApi.nameFree(clean));
    } catch {
      setChecked(null);
    }
  };

  const pkg = (): Package | undefined => {
    if (t) return undefined;
    if (kind === "helm") return repo.trim() && chart.trim() ? { kind: "helm", repo: repo.trim(), chart: chart.trim() } : undefined;
    return repo.trim() ? { kind: "git", repo: repo.trim(), path: path.trim() } : undefined;
  };

  const ask = (): AppAsk | null => {
    if (!clean || nameWrong || taken) return null;
    const p = pkg();
    if (!t && !p) return null;
    if (src === "link" && !linked) return null;
    return {
      name: clean,
      project: project.trim() || undefined,
      owners: owners
        .split(",")
        .map((o) => o.trim())
        .filter(Boolean),
      template: t?.name,
      package: p,
      // The template makes one unless told otherwise.
      source: src === "link" ? linked : src === "none" && makes ? "" : undefined,
    };
  };

  const make = async () => {
    const a = ask();
    if (!a) return;
    setBusy(true);
    try {
      const app = await deployApi.createApp(a);
      const failed: string[] = [];
      for (const env of envs.filter((e) => allowed.some((x) => x.name === e))) {
        try {
          await deployApi.register(app.name, env);
        } catch (e) {
          failed.push(`${env}: ${errorMessage(e)}`);
        }
      }
      toast({
        title: `${app.name} is made`,
        description: [app.source ? `Its source: ${app.source}.` : "", failed.length ? `Not registered in ${failed.join("; ")}` : ""].filter(Boolean).join(" ") || undefined,
        tone: failed.length ? "info" : "succeeded",
        href: releaseAppHref(app.name),
      });
      invalidate("deploy/");
      invalidate("repos");
      navigate(releaseAppHref(app.name));
    } catch (e) {
      toast({
        title: "Not made",
        description: errorMessage(e),
        tone: "failed",
      });
    } finally {
      setBusy(false);
    }
  };

  const sourceOptions: { value: Source; label: string }[] = [
    ...(makes ? [{ value: "template" as Source, label: "The template's" }] : []),
    { value: "link", label: "One kiln has" },
    { value: "none", label: "None" },
  ];
  const filled = (pattern: string) => pattern.replaceAll("{app}", clean || "<app>").replaceAll("{project}", project.trim() || "default");

  return (
    <div className="cd-new-flow">
      <Panel eyebrow="1" title="Its name" description="Unique in kiln; its namespace and its repository follow it unless its template says otherwise.">
        <div className="cd-new-grid">
          <TextField
            label="Name"
            placeholder="web"
            value={name}
            autoFocus
            spellCheck={false}
            onChange={(e) => {
              setName(e.target.value);
              setChecked(null);
            }}
            onBlur={() => void check()}
            hint={nameWrong ?? taken ?? (checked?.name === clean && checked.available ? `${clean} is free.` : undefined)}
            aria-invalid={!!(nameWrong || taken)}
          />
          <TextField label="Project" placeholder="default" value={project} spellCheck={false} onChange={(e) => setProject(e.target.value)} />
        </div>
        <TextField
          className="rel-gap"
          label="Owner teams"
          placeholder="payments, platform"
          value={owners}
          onChange={(e) => setOwners(e.target.value)}
          hint="The owner first, then a co-owner and an ops owner, separated by commas."
        />
      </Panel>

      <Panel eyebrow="2" title="Made from" description="A template gives its package, namespace and checks, and may make its source repository.">
        <SelectField
          label="Template"
          value={templateName}
          onChange={(v) => {
            setTemplateName(v);
            setSource(null);
          }}
          options={[
            ...templates.map((x) => ({
              value: x.name,
              label: x.description ? `${x.name}: ${x.description}` : x.name,
            })),
            { value: "", label: "None: its package given here" },
          ]}
        />
        {t ? (
          <ul className="rel-template">
            {t.package && (
              <li>
                Deploys <code>{packageWords(t.package)}</code>
                {t.package.kind === "helm" && t.package.chart.includes("{app}") ? " (its chart named after it)" : ""}
              </li>
            )}
            {t.namespace && (
              <li>
                Runs in <code>{filled(t.namespace)}</code>
              </li>
            )}
            {!!t.checks?.length && <li>Every release passes {t.checks.map(checkName).join(", ")}</li>}
            {!!t.environments?.length && <li>May go to {t.environments.join(", ")}</li>}
            {makes && (
              <li>
                Makes <code>{filled(makes.name)}</code>
                {makes.starter ? (
                  <>
                    {" "}
                    from <code>{makes.starter}</code>
                    {makes.starter_ref ? ` at ${makes.starter_ref}` : ""}
                  </>
                ) : (
                  ", empty"
                )}
              </li>
            )}
          </ul>
        ) : (
          <>
            <Segmented
              label="How it deploys"
              value={kind}
              onChange={setKind}
              options={[
                { value: "helm", label: "A Helm chart" },
                { value: "git", label: "A path in git" },
              ]}
            />
            <div className="cd-new-grid">
              <TextField
                label={kind === "helm" ? "Chart repository" : "Git repository"}
                placeholder={kind === "helm" ? "oci://ghcr.io/acme/charts" : "https://github.com/acme/deploy.git"}
                value={repo}
                spellCheck={false}
                onChange={(e) => setRepo(e.target.value)}
              />
              {kind === "helm" ? (
                <TextField label="Chart" placeholder="web" value={chart} spellCheck={false} onChange={(e) => setChart(e.target.value)} hint="Its version is each release's." />
              ) : (
                <TextField label="Path" placeholder="apps/web" value={path} spellCheck={false} onChange={(e) => setPath(e.target.value)} hint="At each release's version: a branch, tag or commit." />
              )}
            </div>
          </>
        )}
      </Panel>

      <Panel eyebrow="3" title="Its source" description="The repository its code lives in, built by kiln's CI.">
        <Segmented label="Its source repository" value={src} onChange={setSource} options={sourceOptions} />
        {src === "template" && makes && (
          <p className="cd-new-note">
            kiln makes <code>{filled(makes.name)}</code>, one of its own repositories
            {makes.starter ? (
              <>
                , its first commit the files of <code>{makes.starter}</code> with the app's name filled in. Its first build starts with it.
              </>
            ) : (
              ", empty: its first push starts it."
            )}
          </p>
        )}
        {src === "link" &&
          (repos.length ? (
            <SelectField className="rel-gap" label="Repository" value={linked} onChange={setLinked} options={repos.map((r) => ({ value: r.name, label: r.name }))} />
          ) : (
            <p className="cd-new-note">kiln has no repositories yet.</p>
          ))}
      </Panel>

      <Panel eyebrow="4" title="Where it runs" description="Registered once it's made; nothing is released until you release it.">
        {allowed.length ? (
          <div className="cd-new-checks">
            {allowed.map((e) => (
              <label key={e.name} className="cd-new-check">
                <input type="checkbox" className="check" checked={envs.includes(e.name)} onChange={(ev) => setEnvs((xs) => (ev.target.checked ? [...xs, e.name] : xs.filter((x) => x !== e.name)))} />
                <span>
                  {e.name} <span className="rel-quiet">· {providerName(e.provider)}</span>
                </span>
              </label>
            ))}
          </div>
        ) : (
          <p className="cd-new-note">{environments.length ? `${t?.name ?? "This template"} allows none of kiln's environments.` : "No environments yet: kiln deploy env add."}</p>
        )}
      </Panel>

      <div className="rel-new-actions">
        <Button disabled={busy || !ask()} onClick={() => void make()}>
          <Rocket /> Make {clean || "the app"}
        </Button>
      </div>
    </div>
  );
}
