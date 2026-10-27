/**
 * A new app (docs/cd-targets.md §8.2), built in place: one column that grows as it's answered.
 *
 * 1. Where: the repository it lives in, the platform, the account and place, the environment.
 * 2. What runs there: what the account holds, found for you; pick one to adopt it as it runs, or
 *    start new.
 * 3. Its definition: the platform's settings, as `.kiln/deploy.yaml` spells them.
 * 4. What it deploys: images, by digest at each commit.
 * 5. How it syncs.
 * 6. Review: the YAML it makes, checked and rendered the way kiln will read it.
 * 7. Save: keep it in kiln, or copy it into the repository.
 */
import * as React from "react";
import { Check, Plus, Search, Trash2 } from "lucide-react";

import { api } from "@/api/client";
import { cdApi } from "@/api/cd";
import type { CdDiscovery, CdProvider, CdValidation } from "@/api/types";
import { Button } from "@/components/ui/button";
import { CopyBlock } from "@/components/ui/copy-block";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Panel, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes, warm, type Res } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useNavigate } from "@/lib/router";
import "./deployments.css";

const providersRes: Res<{ providers: CdProvider[] }> = { key: "cd/providers", fetch: (s) => cdApi.providers(s) };
const reposRes: Res<Awaited<ReturnType<typeof api.repos>>> = { key: "repos", fetch: (s) => api.repos(s) };
const clustersRes: Res<Awaited<ReturnType<typeof cdApi.clusters>>> = { key: "cd/clusters", fetch: (s) => cdApi.clusters(s) };

/** The platform choice for Kubernetes: kiln's first provider, built in rather than in the catalog. */
const KUBERNETES = "kubernetes";

/** Settings whose value is an object (a JSON editor), or a file of the repository by path. */
const OBJECT_SETTINGS = new Set(["task", "service-spec", "config", "url", "set", "spec", "parameters", "vars", "domains"]);

/* ---------------------------- YAML ---------------------------- */

const PLAIN = /^[A-Za-z0-9_./$@~-][A-Za-z0-9_ ./$@~=+-]*$/;
const RESERVED = /^(true|false|null|yes|no|on|off|~|-?\d+(\.\d+)?([eE][-+]?\d+)?)$/i;

function scalar(v: unknown): string {
  if (v === null || v === undefined) return "null";
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  const s = String(v);
  return PLAIN.test(s) && !RESERVED.test(s) && !s.endsWith(" ") ? s : JSON.stringify(s);
}

const complex = (v: unknown): v is object => typeof v === "object" && v !== null;
const empty = (v: object) => (Array.isArray(v) ? v.length === 0 : Object.keys(v).length === 0);

/** YAML in block style: maps and lists indented, strings quoted where YAML would read them otherwise. */
export function toYaml(v: unknown, indent = 0): string {
  const pad = " ".repeat(indent);
  if (Array.isArray(v)) {
    if (!v.length) return `${pad}[]`;
    return v.map((x) => (complex(x) && !empty(x) ? `${pad}- ${toYaml(x, indent + 2).trimStart()}` : `${pad}- ${complex(x) ? (Array.isArray(x) ? "[]" : "{}") : scalar(x)}`)).join("\n");
  }
  if (complex(v)) {
    const entries = Object.entries(v);
    if (!entries.length) return `${pad}{}`;
    return entries
      .map(([k, x]) => (complex(x) && !empty(x) ? `${pad}${scalar(k)}:\n${toYaml(x, indent + 2)}` : `${pad}${scalar(k)}: ${complex(x) ? (Array.isArray(x) ? "[]" : "{}") : scalar(x)}`))
      .join("\n");
  }
  return `${pad}${scalar(v)}`;
}

/* ---------------------------- the draft ---------------------------- */

interface Artifact {
  name: string;
  kind: "image" | "published";
  /** The image (`docker.io/acme/api`), or the published image's name. */
  value: string;
}

interface Draft {
  repo: string;
  platform: string;
  account: string;
  place: Record<string, string>;
  environment: string;
  name: string;
  /** Each setting as typed: text, or JSON for object settings. */
  settings: Record<string, string>;
  artifacts: Artifact[];
  auto: boolean;
  selfHeal: boolean;
  prune: boolean;
  /** Kubernetes: the source's path, the cluster, the namespace. */
  path: string;
  cluster: string;
  namespace: string;
}

const blank = (repo: string): Draft => ({
  repo,
  platform: "",
  account: "",
  place: {},
  environment: "production",
  name: "",
  settings: {},
  artifacts: [],
  auto: false,
  selfHeal: false,
  prune: false,
  path: "",
  cluster: "in-cluster",
  namespace: "",
});

/** A setting's value as the deploy file takes it: an object from JSON, else the text (a path, a name). */
function settingValue(name: string, text: string): { value?: unknown; error?: string } {
  const t = text.trim();
  if (!t) return {};
  if (OBJECT_SETTINGS.has(name) && (t.startsWith("{") || t.startsWith("["))) {
    try {
      return { value: JSON.parse(t) as unknown };
    } catch (e) {
      return { error: `${name}: ${errorMessage(e)}` };
    }
  }
  return { value: t };
}

/** The YAML the draft makes: the app's entry in `.kiln/deploy.yaml`, and its environment's target. */
function yamlOf(d: Draft, provider: CdProvider | undefined): { deploy: string; environments: string; errors: string[] } {
  const errors: string[] = [];
  const app: Record<string, unknown> = {};
  if (d.platform === KUBERNETES) {
    app.source = d.path.trim() || ".";
    app.to = { cluster: d.cluster };
    if (d.namespace.trim()) app.namespace = d.namespace.trim();
  } else if (provider) {
    const def: Record<string, unknown> = {};
    for (const s of provider.settings) {
      const { value, error } = settingValue(s.name, d.settings[s.name] ?? "");
      if (error) errors.push(error);
      if (value !== undefined) def[s.name] = value;
      else if (s.required) errors.push(`${s.name} is needed: ${s.help}`);
    }
    app[provider.name] = def;
    app.to = d.environment.trim() || "production";
  }
  if (d.artifacts.length) {
    const arts: Record<string, unknown> = {};
    for (const a of d.artifacts) {
      if (!a.name.trim() || !a.value.trim()) continue;
      arts[a.name.trim()] = a.kind === "published" ? { published: a.value.trim() } : { image: a.value.trim(), tag: "${{ git.sha }}" };
    }
    if (Object.keys(arts).length) app.artifacts = arts;
  }
  if (d.auto || d.selfHeal || d.prune) app.sync = { auto: d.auto, "self-heal": d.selfHeal, prune: d.prune };
  const name = d.name.trim() || "app";
  const deploy = toYaml({ apps: { [name]: app } });
  let environments = "";
  if (provider && d.platform !== KUBERNETES) {
    if (!d.account) errors.push(`an account: the ${provider.integration_title ?? provider.integration} integration it acts through`);
    const target: Record<string, string> = { account: d.account };
    for (const k of provider.place) {
      const v = (d.place[k] ?? "").trim();
      if (!v) errors.push(`the target's ${k}`);
      target[k] = v;
    }
    environments = toYaml({ [d.environment.trim() || "production"]: { targets: { [provider.name]: target } } });
  }
  if (!d.name.trim()) errors.push("a name for the app");
  return { deploy, environments, errors };
}

/* ---------------------------- the page ---------------------------- */

export default function NewApp() {
  warm(providersRes, reposRes, clustersRes);
  return (
    <div className="container-console page cd-page cd-new">
      <title>New app · Deployments · kiln</title>
      <Crumbs index={2} items={[{ label: "Deployments", to: "/deployments" }, { label: "New app" }]} />
      <h1 className="cd-new-title">A new app.</h1>
      <p className="lead">Where it runs, what it is, how it rolls out. Read what kiln makes of it, then keep it.</p>
      <Loadable label="the platforms" fallback={<Hold height={320} />}>
        <Builder />
      </Loadable>
    </div>
  );
}

function Builder() {
  const providers = useRes(providersRes).data.providers;
  const repos = useRes(reposRes).data;
  const clusters = useRes(clustersRes).data.clusters;
  const navigate = useNavigate();
  const [d, setD] = React.useState<Draft>(() => blank(repos[0]?.name ?? ""));
  const set = <K extends keyof Draft>(k: K, v: Draft[K]) => setD((x) => ({ ...x, [k]: v }));
  const provider = providers.find((p) => p.name === d.platform);
  const { deploy, environments, errors } = yamlOf(d, provider);

  // What the account holds, and what was adopted.
  const [found, setFound] = React.useState<CdDiscovery | null>(null);
  const [looking, setLooking] = React.useState(false);
  const [adopted, setAdopted] = React.useState<{ name: string; note: string | null } | null>(null);
  const [checked, setChecked] = React.useState<{ deploy: string; result: CdValidation } | null>(null);
  const [busy, setBusy] = React.useState(false);

  const where = (): Record<string, string> => ({ provider: d.platform, account: d.account, environment: d.environment, ...d.place });
  const look = async () => {
    setLooking(true);
    try {
      setFound(await cdApi.discover(where()));
    } catch (e) {
      toast.error("Couldn't look in the account", errorMessage(e));
    } finally {
      setLooking(false);
    }
  };
  const adopt = async (name: string, scope?: string) => {
    setBusy(true);
    try {
      const a = await cdApi.adopt({ ...where(), name, ...(scope ? { scope } : {}) });
      const settings: Record<string, string> = {};
      for (const [k, v] of Object.entries(a.definition)) settings[k] = typeof v === "string" ? v : JSON.stringify(v, null, 2);
      setD((x) => ({ ...x, name: x.name || name, settings }));
      setAdopted({ name, note: a.note });
    } catch (e) {
      toast.error(`Couldn't read ${name}`, errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const check = async () => {
    setBusy(true);
    try {
      setChecked({ deploy, result: await cdApi.validate({ deploy, environments: environments || undefined, repo: d.repo || undefined }) });
    } catch (e) {
      toast.error("Couldn't check the app", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const keep = async () => {
    setBusy(true);
    try {
      const r = await cdApi.keep({ deploy, environments: environments || undefined, repo: d.repo });
      invalidate("cd/");
      toast({ title: r.apps.length === 1 ? `${r.apps[0] ?? "The app"} is kept in kiln` : `${r.apps.length} apps kept in kiln`, description: "It shows in Deployments as soon as its sources read.", tone: "succeeded" });
      navigate("/deployments");
    } catch (e) {
      toast.error("Couldn't keep the app", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const platformOptions = [
    { value: "", label: "Choose a platform…" },
    { value: KUBERNETES, label: "Kubernetes" },
    ...providers.map((p) => ({ value: p.name, label: p.built ? p.title : `${p.title} (coming)` })),
  ];
  const fresh = checked?.deploy === deploy;

  return (
    <div className="cd-new-flow">
      <Panel eyebrow="1" title="Where" description="The repository it lives in, and where it runs.">
        <div className="cd-new-grid">
          <SelectField
            label="Repository"
            value={d.repo}
            onChange={(v) => set("repo", v)}
            options={repos.length ? repos.map((r) => ({ value: r.name, label: r.name })) : [{ value: "", label: "No repositories yet" }]}
          />
          <SelectField
            label="Platform"
            value={d.platform}
            onChange={(v) => {
              setD((x) => ({ ...x, platform: v, account: "", place: {}, settings: {} }));
              setFound(null);
              setAdopted(null);
            }}
            options={platformOptions}
          />
        </div>
        {provider && !provider.built && (
          <p className="cd-new-note">
            {provider.title} is in kiln's plan and not built yet: kiln deploys to it once it is. Its accounts can be added in{" "}
            <Link to="/settings#deployments.accounts">Settings → Deployments</Link> now.
          </p>
        )}
        {provider?.built && (
          <div className="cd-new-grid">
            {provider.accounts.length ? (
              <SelectField
                label="Account"
                value={d.account}
                onChange={(v) => set("account", v)}
                options={[{ value: "", label: "Choose an account…" }, ...provider.accounts.map((a) => ({ value: a.name, label: a.last_error ? `${a.name} (didn't verify)` : a.name }))]}
              />
            ) : (
              <p className="cd-new-note">
                No {provider.integration_title ?? provider.integration} account yet. <Link to="/settings#deployments.accounts">Add one in Settings → Deployments</Link>, then come back.
              </p>
            )}
            {provider.place.map((k) => (
              <TextField key={k} label={k} placeholder={k === "region" ? "us-east-1" : k} value={d.place[k] ?? ""} onChange={(e) => set("place", { ...d.place, [k]: e.target.value })} />
            ))}
            <TextField label="Environment" value={d.environment} onChange={(e) => set("environment", e.target.value)} hint="Its target goes in .kiln/environments.yaml" />
          </div>
        )}
        {d.platform === KUBERNETES && (
          <div className="cd-new-grid">
            <SelectField label="Cluster" value={d.cluster} onChange={(v) => set("cluster", v)} options={clusters.map((c) => ({ value: c.name, label: c.connected ? c.name : `${c.name} (not connected)` }))} />
            <TextField label="Namespace" placeholder="default" value={d.namespace} onChange={(e) => set("namespace", e.target.value)} />
            <TextField label="Source" placeholder="deploy/web" value={d.path} onChange={(e) => set("path", e.target.value)} hint="A directory of manifests, a Helm chart or a kustomization" />
          </div>
        )}
      </Panel>

      {provider?.built && d.account && (
        <Panel eyebrow="2" title="What runs there" description={`Adopt what ${d.account} already runs, as it runs now, or start new.`}>
          <div className="flex flex-wrap items-center gap-2">
            <Button size="sm" variant="secondary" disabled={looking || provider.place.some((k) => !(d.place[k] ?? "").trim())} onClick={() => void look()}>
              <Search /> {found ? "Look again" : "Look in the account"}
            </Button>
            {adopted && <span className="cd-new-note">Adopted {adopted.name}: its definition is below.</span>}
          </div>
          {found && (
            <ul className="cd-new-found">
              {found.resources.length === 0 && <li className="cd-new-note">Nothing here yet: start new below.</li>}
              {found.resources.map((r) => (
                <li key={`${r.scope ?? ""}/${r.name}`}>
                  <span className="cd-new-found-name">{r.scope ? `${r.scope} / ${r.name}` : r.name}</span>
                  <span className="cd-new-found-meta">{r.summary}</span>
                  <Button size="xs" variant="ghost" disabled={busy} onClick={() => void adopt(r.name, r.scope)}>
                    Adopt
                  </Button>
                </li>
              ))}
            </ul>
          )}
          {adopted?.note && <p className="cd-new-note">{adopted.note}</p>}
        </Panel>
      )}

      {(provider?.built || d.platform === KUBERNETES) && (
        <Panel eyebrow="3" title="Its definition" description={provider ? `${provider.title}'s settings, as .kiln/deploy.yaml spells them. Objects take JSON, or a file of the repository by path.` : "kiln renders the source as Argo CD would."}>
          <div className="space-y-3">
            <TextField label="Name" placeholder="api" value={d.name} onChange={(e) => set("name", e.target.value)} hint="Each environment's copy is named after it: api-production" />
            {provider?.settings.map((s) =>
              OBJECT_SETTINGS.has(s.name) ? (
                <div key={s.name}>
                  <label className="field-label" htmlFor={`setting-${s.name}`}>
                    {s.name}
                    {s.required ? "" : " (optional)"}
                  </label>
                  <div className="field">
                    <textarea
                      id={`setting-${s.name}`}
                      className="cd-new-code"
                      rows={Math.min(16, Math.max(3, (d.settings[s.name] ?? "").split("\n").length))}
                      placeholder={s.help}
                      value={d.settings[s.name] ?? ""}
                      onChange={(e) => set("settings", { ...d.settings, [s.name]: e.target.value })}
                      spellCheck={false}
                    />
                  </div>
                </div>
              ) : (
                <TextField
                  key={s.name}
                  label={`${s.name}${s.required ? "" : " (optional)"}`}
                  placeholder={s.help}
                  value={d.settings[s.name] ?? ""}
                  onChange={(e) => set("settings", { ...d.settings, [s.name]: e.target.value })}
                />
              ),
            )}
          </div>
        </Panel>
      )}

      {(provider?.built || d.platform === KUBERNETES) && (
        <Panel eyebrow="4" title="What it deploys" description="Images, resolved to their digest at each commit: read in its definition as ${{ artifacts.<name> }}.">
          <div className="space-y-3">
            {d.artifacts.map((a, i) => (
              <div key={i} className="cd-new-grid cd-new-artifact">
                <TextField label="Name" placeholder="image" value={a.name} onChange={(e) => set("artifacts", d.artifacts.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)))} />
                <SelectField
                  label="From"
                  value={a.kind}
                  onChange={(v) => set("artifacts", d.artifacts.map((x, j) => (j === i ? { ...x, kind: v as Artifact["kind"] } : x)))}
                  options={[
                    { value: "image", label: "A registry, tagged by commit" },
                    { value: "published", label: "An image kiln publishes" },
                  ]}
                />
                <TextField
                  label={a.kind === "image" ? "Image" : "Published as"}
                  placeholder={a.kind === "image" ? "docker.io/acme/api" : "api"}
                  value={a.value}
                  onChange={(e) => set("artifacts", d.artifacts.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))}
                />
                <Button size="xs" variant="ghost-danger" className="mt-6" aria-label="Remove the artifact" onClick={() => set("artifacts", d.artifacts.filter((_, j) => j !== i))}>
                  <Trash2 />
                </Button>
              </div>
            ))}
            <Button size="sm" variant="secondary" onClick={() => set("artifacts", [...d.artifacts, { name: d.artifacts.length ? "" : "image", kind: "image", value: "" }])}>
              <Plus /> Add an artifact
            </Button>
          </div>
        </Panel>
      )}

      {(provider?.built || d.platform === KUBERNETES) && (
        <Panel eyebrow="5" title="How it syncs" description="By hand until you say otherwise. Self-heal is off by default on clouds and platforms: people edit consoles, and kiln shows the drift.">
          <div className="cd-new-checks">
            {(
              [
                ["auto", "Sync when git moves"],
                ["selfHeal", "Put back what changes outside kiln"],
                ["prune", "Delete what git no longer holds"],
              ] as const
            ).map(([k, label]) => (
              <label key={k} className="cd-new-check">
                <input type="checkbox" className="check" checked={d[k]} onChange={(e) => set(k, e.target.checked)} />
                {label}
              </label>
            ))}
          </div>
        </Panel>
      )}

      {(provider?.built || d.platform === KUBERNETES) && (
        <Panel eyebrow="6" title="Review" description="What goes in the repository, checked and rendered the way kiln reads it.">
          <div className="space-y-3">
            <div className="cd-new-yaml">
              <div className="cd-new-yaml-head">
                <code>.kiln/deploy.yaml</code>
                <CopyBlock text={deploy} />
              </div>
              <pre>{deploy}</pre>
            </div>
            {environments && (
              <div className="cd-new-yaml">
                <div className="cd-new-yaml-head">
                  <code>.kiln/environments.yaml</code>
                  <CopyBlock text={environments} />
                </div>
                <pre>{environments}</pre>
              </div>
            )}
            {errors.length > 0 && (
              <ul className="cd-new-errors">
                {errors.map((e) => (
                  <li key={e}>Still needed: {e}</li>
                ))}
              </ul>
            )}
            <Button size="sm" variant="secondary" disabled={busy || errors.length > 0} onClick={() => void check()}>
              <Check /> Check it
            </Button>
            {checked && <Checked result={checked.result} stale={!fresh} />}
          </div>
        </Panel>
      )}

      {(provider?.built || d.platform === KUBERNETES) && (
        <Panel eyebrow="7" title="Keep it" description="In the repository, where kiln reads it at every commit; or in kiln, until it moves to the repository.">
          <div className="flex flex-wrap items-center gap-2">
            <Button size="sm" disabled={busy || errors.length > 0 || !d.repo || !fresh || !checked?.result.ok} onClick={() => void keep()}>
              Keep in kiln
            </Button>
            <span className="cd-new-note">{fresh && checked?.result.ok ? "Or copy the YAML above into the repository's files: kiln reads them at the next commit." : "Check it first."}</span>
          </div>
        </Panel>
      )}
    </div>
  );
}

/** What kiln made of the draft: each app, where it goes, and what it renders. */
function Checked({ result, stale }: { result: CdValidation; stale: boolean }) {
  if (!result.ok) return <p className="cd-notice" data-tone="fail">{result.error}</p>;
  return (
    <div className="cd-new-checked" data-stale={stale || undefined}>
      {stale && <p className="cd-new-note">Changed since: check it again.</p>}
      {result.apps.map((a) => (
        <div key={a.id} className="space-y-1">
          <p className="cd-new-found-name">
            {a.name} <span className="cd-new-found-meta">→ {[a.destination.cluster, a.destination.namespace].filter(Boolean).join(" / ")}</span>
          </p>
          {a.error ? (
            <p className="cd-notice" data-tone="fail">
              {a.error}
            </p>
          ) : a.objects ? (
            <ul className="cd-new-objects">
              {a.objects.map((o, i) => {
                const meta = (o.metadata ?? {}) as { name?: string; namespace?: string };
                return (
                  <li key={i}>
                    <details>
                      <summary>
                        {String(o.kind ?? "")} <code>{[meta.namespace, meta.name].filter(Boolean).join("/")}</code>
                      </summary>
                      <pre>{toYaml(o.spec ?? {})}</pre>
                    </details>
                  </li>
                );
              })}
            </ul>
          ) : (
            <p className="cd-new-note">Rendered when kiln reads it: Kubernetes sources render with their tools.</p>
          )}
        </div>
      ))}
    </div>
  );
}
