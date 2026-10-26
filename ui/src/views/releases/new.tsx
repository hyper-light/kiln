/**
 * A new app, deployed by API (docs/deploy-api.md §3.1): one column that grows as it's answered.
 *
 * 1. Its name, project and owner teams; the name checked as it's left.
 * 2. What it's made from: a template (its package, namespace, checks, and the source repository it
 *    makes), or a package given here.
 * 3. Its source repository: the template's, one kiln has, or none.
 * 4. Where it runs: the environments it's registered in once it's made.
 * 5. Its first releases: a version, released to those of them asked for once it's registered.
 *
 * Then its page, where they're followed.
 */
import * as React from "react";
import { Rocket } from "lucide-react";

import { api } from "@/api/client";
import { boardHref, checkName, deployApi, packageWords, providerName, releaseAppHref, type AppAsk, type Package, type RegistryTemplate } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Panel, SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { reposRes, useRes, warm } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
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
      <p className="lead">Its name and owners, what it's made from, where its source lives, where it runs, and its first releases: then its page follows them.</p>
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
  // The groups the person is in: an app's owner team is one its maker is in (docs/deploy-api.md §3.1).
  const groups = useSuspenseResource("me", (s) => api.me(s)).data.groups ?? [];
  const [templateName, setTemplateName] = React.useState(templates[0]?.name ?? "");
  const [kind, setKind] = React.useState<Kind>("helm");
  const [repo, setRepo] = React.useState("");
  const [chart, setChart] = React.useState("");
  const [path, setPath] = React.useState("");
  const [source, setSource] = React.useState<Source | null>(null);
  const [linked, setLinked] = React.useState(repos[0]?.name ?? "");
  const [envs, setEnvs] = React.useState<string[]>([]);
  const [version, setVersion] = React.useState("");
  // The environments its first release goes to: each it's registered in, unless left out.
  const [held, setHeld] = React.useState<string[]>([]);
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

  // Where it's registered, in the order kiln lists its environments; and where it's first released:
  // straight there, unless a release has to run in another environment first (its `after`), when
  // that environment's landing promotes it there (`promote: auto`) or a person releases it later.
  const registering = allowed.filter((e) => envs.includes(e.name)).map((e) => e.name);
  const gated = (env: string) => (environments.find((e) => e.name === env)?.gates.after ?? []).filter(Boolean);
  const promotes = (env: string) => environments.find((e) => e.name === env)?.gates.promote === "auto";
  const chosen = version.trim() ? registering.filter((e) => !held.includes(e)) : [];
  const releasing = chosen.filter((e) => !gated(e).length);
  const promoted = chosen.filter((e) => gated(e).length && promotes(e) && gated(e).every((a) => releasing.includes(a)));
  const later = chosen.filter((e) => !releasing.includes(e) && !promoted.includes(e));

  const make = async () => {
    const a = ask();
    if (!a) return;
    setBusy(true);
    try {
      const app = await deployApi.createApp(a);
      const failed: string[] = [];
      const registered: string[] = [];
      for (const env of registering) {
        try {
          await deployApi.register(app.name, env);
          registered.push(env);
        } catch (e) {
          failed.push(`${env}: ${errorMessage(e)}`);
        }
      }
      // Its first releases, each through its environment's gates as any release.
      const released: string[] = [];
      for (const env of releasing.filter((e) => registered.includes(e))) {
        try {
          const r = await deployApi.create({ app: app.name, environment: env, version: version.trim(), idempotency_key: `first:${app.name}:${env}` });
          released.push(`${env} (release ${r.id})`);
        } catch (e) {
          failed.push(`${env}: not released (${errorMessage(e)})`);
        }
      }
      const made = [
        app.source ? `Its source: ${app.source}.` : "",
        app.registries ? `Its images: ${app.registries.images}.` : "",
        released.length ? `${version.trim()} released to ${released.join(", ")}.` : "",
        promoted.length ? `${promoted.join(", ")} take${promoted.length === 1 ? "s" : ""} it once it lands in ${[...new Set(promoted.flatMap(gated))].join(", ")}.` : "",
        later.length ? `Release it to ${later.join(", ")} once it runs in ${[...new Set(later.flatMap(gated))].join(", ")}.` : "",
      ];
      toast({
        title: `${app.name} is made`,
        description: [...made, failed.length ? `Not done: ${failed.join("; ")}` : ""].filter(Boolean).join(" ") || undefined,
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
          hint={`The owner first, then a co-owner and an ops owner, separated by commas: kiln's groups (shop) or an identity provider's (github:acme/platform). The owner team is one you're in${groups.length ? `: ${groups.join(", ")}` : ""}.`}
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
            {t.registry && (
              <li>
                Makes its image, snapshot and chart repositories {registryWhere(t.registry)}: <code>{filled(t.registry.name || "{project}/{app}")}</code>
                {t.registry.keep_snapshots ? `, keeping its newest ${t.registry.keep_snapshots} snapshots` : ""}
              </li>
            )}
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

      <Panel eyebrow="4" title="Where it runs" description="Registered once it's made.">
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

      <Panel eyebrow="5" title="Its first releases" description="A version released once it's registered, each through its environment's gates as any release; or none, to release it from its page.">
        <TextField
          label="Version"
          placeholder={t?.package?.kind === "git" || (!t && kind === "git") ? "main" : "1.0.0"}
          value={version}
          spellCheck={false}
          onChange={(e) => setVersion(e.target.value)}
          hint={t?.package?.kind === "git" || (!t && kind === "git") ? "A branch, tag or commit of its package." : "Its chart's version."}
        />
        {version.trim() &&
          (registering.length ? (
            <div className="cd-new-checks rel-gap">
              {registering.map((e) => (
                <label key={e} className="cd-new-check">
                  <input type="checkbox" className="check" checked={!held.includes(e)} onChange={(ev) => setHeld((xs) => (ev.target.checked ? xs.filter((x) => x !== e) : [...xs, e]))} />
                  <span>
                    Release {version.trim()} to {e}
                    {promoted.includes(e) && <span className="rel-quiet"> · once it lands in {gated(e).join(", ")}</span>}
                    {later.includes(e) && <span className="rel-quiet"> · after it runs in {gated(e).join(", ")}: from its page</span>}
                  </span>
                </label>
              ))}
            </div>
          ) : (
            <p className="cd-new-note">Choose where it runs first.</p>
          ))}
      </Panel>

      <div className="rel-new-actions">
        <Button disabled={busy || !ask()} onClick={() => void make()}>
          <Rocket /> {releasing.length ? `Make ${clean || "the app"} and release it` : `Make ${clean || "the app"}`}
        </Button>
      </div>
    </div>
  );
}

/** Where a template's registry is, in words. */
function registryWhere(r: RegistryTemplate): string {
  if (r.at === "kiln") return "on kiln's registry";
  return `through ${r.at}${r.location ? ` in ${r.location}` : ""}`;
}
