/**
 * Settings → Deployments (docs/cd-targets.md §6, §7.1): where each image kiln publishes lives (the
 * local Docker of the machine that builds it, loaded into the cluster's nodes, or a registry), the
 * registries kiln pulls from and pipelines push to, and the accounts kiln deploys through (AWS,
 * Google Cloud and Azure trust kiln's identity, so nothing is stored for them; the other
 * platforms take a token).
 */
import * as React from "react";
import { Check, Plus, RefreshCw, Trash2 } from "lucide-react";
import { api, type CdImages, type ImageHome, type Integration, type IntegrationKind, type Integrations, type OidcIssuer } from "@/api/client";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, Panel, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { relativeTime } from "@/lib/format";
import { useRes, warm, type Res } from "@/lib/data";
import { invalidate } from "@/lib/resource";

const imagesRes: Res<CdImages> = { key: "cd/images", fetch: (sig) => api.cdImages(sig) };
const integrationsRes: Res<Integrations> = { key: "ci/integrations", fetch: (sig) => api.integrations(sig) };
const oidcRes: Res<OidcIssuer> = { key: "ci/oidc", fetch: (sig) => api.oidc(sig) };

/** The deployment settings: images, registries, then accounts. */
export function DeploySettings() {
  warm(imagesRes, integrationsRes);
  return (
    <Loadable label="the deployment settings" fallback={<DeployHold />}>
      <div className="space-y-4">
        <ImagesPanel />
        <RegistriesPanel />
        <AccountsPanel />
      </div>
    </Loadable>
  );
}

function DeployHold() {
  return (
    <div className="space-y-4">
      <Panel eyebrow="Deployments" title="Images" description={" "}>
        <Hold height={66} />
      </Panel>
      <Panel eyebrow="Deployments" title="Registries" description={" "}>
        <Hold height={66} />
      </Panel>
      <Panel eyebrow="Deployments" title="Accounts" description={" "}>
        <Hold height={66} />
      </Panel>
    </div>
  );
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** A field-styled select. */
function SelectField({ label, value, onChange, children, className }: { label: string; value: string; onChange: (v: string) => void; children: React.ReactNode; className?: string }) {
  const id = React.useId();
  return (
    <div className={className}>
      <label htmlFor={id} className="field-label">
        {label}
      </label>
      <div className="field">
        <select id={id} value={value} onChange={(e) => onChange(e.target.value)}>
          {children}
        </select>
      </div>
    </div>
  );
}

function ImagesPanel() {
  const data = useRes(imagesRes).data;
  const registries = useRes(integrationsRes).data.integrations.filter((i) => i.kind === "oci");
  const names = [...new Set([...Object.keys(data.used), ...Object.keys(data.images)])].sort();
  return (
    <Panel
      eyebrow="Deployments"
      title="Images"
      description="Where each image kiln publishes lives. Local keeps it in the Docker of the machine that builds it, loaded into the cluster's nodes; a registry gets it pushed by commit."
    >
      {names.length === 0 ? (
        <p className="text-[13px] leading-relaxed text-[#9a9da6]">
          No app publishes an image yet. An artifact written <code className="font-mono text-[12px]">{"{ published: <name> }"}</code> in an app of{" "}
          <code className="font-mono text-[12px]">.kiln/deploy.yaml</code> shows here.
        </p>
      ) : (
        <ul className="divide-y divide-white/[0.06]">
          {names.map((n) => (
            <li key={n} className="py-3 first:pt-0 last:pb-0">
              <ImageRow name={n} home={data.images[n] ?? { kind: "local" }} apps={data.used[n] ?? []} registries={registries} />
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

function ImageRow({ name, home, apps, registries }: { name: string; home: ImageHome; apps: string[]; registries: Integration[] }) {
  const [kind, setKind] = React.useState<ImageHome["kind"]>(home.kind);
  const [repository, setRepository] = React.useState(home.kind === "registry" ? home.repository : "");
  const [integration, setIntegration] = React.useState(home.kind === "registry" ? (home.integration ?? "") : "");
  const [saving, setSaving] = React.useState(false);
  const draft: ImageHome = kind === "local" ? { kind: "local" } : { kind: "registry", repository: repository.trim(), ...(integration ? { integration } : {}) };
  const changed =
    JSON.stringify(draft) !==
    JSON.stringify(home.kind === "local" ? { kind: "local" } : { kind: "registry", repository: home.repository, ...(home.integration ? { integration: home.integration } : {}) });
  const valid = kind === "local" || /^[a-z0-9.:-]+(\/[a-z0-9._-]+)+$/.test(repository.trim());
  const save = async () => {
    setSaving(true);
    try {
      await api.setCdImage(name, draft);
      invalidate("cd/images");
      toast({ title: kind === "local" ? `${name} lives in the local Docker` : `${name} goes to ${repository.trim()}`, tone: "succeeded" });
    } catch (e) {
      toast.error(`Couldn't save where ${name} lives`, errorText(e));
    } finally {
      setSaving(false);
    }
  };
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <span className="font-mono text-[13px] text-[#eceef2]">{name}</span>
        <span className="text-[12px] text-[#8b8f99]">{apps.length ? `published for ${apps.join(", ")}` : "no app publishes it now"}</span>
      </div>
      <Segmented
        label={`Where ${name} lives`}
        value={kind}
        onChange={setKind}
        options={[
          { value: "local", label: "Local Docker" },
          { value: "registry", label: "Registry" },
        ]}
      />
      {kind === "registry" && (
        <div className="flex flex-wrap items-start gap-3">
          <TextField
            className="min-w-[16rem] flex-1"
            label="Repository"
            placeholder="docker.io/acme/kiln"
            value={repository}
            onChange={(e) => setRepository(e.target.value)}
            hint={valid ? "Tagged by commit; no tag here" : "A repository like docker.io/acme/kiln, without a tag"}
          />
          <SelectField className="min-w-[12rem]" label="Pulled with" value={integration} onChange={setIntegration}>
            <option value="">The registry's integration, or none (public)</option>
            {registries.map((r) => (
              <option key={r.name} value={r.name}>
                {r.name} · {String(r.config.registry ?? "")}
              </option>
            ))}
          </SelectField>
        </div>
      )}
      {changed && (
        <div className="flex gap-2">
          <Button size="sm" disabled={!valid || saving} onClick={() => void save()}>
            <Check /> Save
          </Button>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => {
              setKind(home.kind);
              setRepository(home.kind === "registry" ? home.repository : "");
              setIntegration(home.kind === "registry" ? (home.integration ?? "") : "");
            }}
          >
            Undo
          </Button>
        </div>
      )}
    </div>
  );
}

function RegistriesPanel() {
  const registries = useRes(integrationsRes).data.integrations.filter((i) => i.kind === "oci");
  const [adding, setAdding] = React.useState(false);
  return (
    <Panel
      eyebrow="Deployments"
      title="Registries"
      description="Container registries kiln resolves images in, and pulls them from: Docker Hub, GHCR, Quay, a registry of your own. A public image needs none."
      actions={
        !adding && (
          <Button size="sm" variant="secondary" onClick={() => setAdding(true)}>
            <Plus /> Add a registry
          </Button>
        )
      }
    >
      <div className="space-y-4">
        {adding && <AddRegistry onDone={() => setAdding(false)} />}
        {registries.length === 0 && !adding ? (
          <p className="text-[13px] text-[#9a9da6]">None yet: images resolve anonymously.</p>
        ) : (
          <ul className="divide-y divide-white/[0.06]">
            {registries.map((r) => (
              <li key={r.id} className="py-3 first:pt-0 last:pb-0">
                <RegistryRow r={r} />
              </li>
            ))}
          </ul>
        )}
      </div>
    </Panel>
  );
}

function RegistryRow({ r }: { r: Integration }) {
  const [busy, setBusy] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const verify = async () => {
    setBusy(true);
    try {
      const v = await api.verifyIntegration(r.name);
      invalidate("ci/integrations");
      if (v.ok) toast({ title: `${r.name} works`, description: v.message, tone: "succeeded" });
      else toast.error(`${r.name} didn't verify`, v.error);
    } catch (e) {
      toast.error(`Couldn't verify ${r.name}`, errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const remove = async () => {
    try {
      await api.deleteIntegration(r.name);
      invalidate("ci/integrations");
      toast({ title: `${r.name} removed`, tone: "info" });
    } catch (e) {
      toast.error(`Couldn't remove ${r.name}`, errorText(e));
    }
  };
  const user = typeof r.config.username === "string" && r.config.username ? r.config.username : null;
  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <span className="font-mono text-[13px] text-[#eceef2]">{r.name}</span>
        <span className="font-mono text-[12px] text-[#9a9da6]">{String(r.config.registry ?? "")}</span>
        <span className="text-[12px] text-[#8b8f99]">
          {user ? `as ${user}` : "anonymous"}
          {r.has_secret ? " · with a token" : ""}
        </span>
        <span className="ml-auto flex gap-1">
          <Button size="xs" variant="ghost" disabled={busy} onClick={() => void verify()}>
            <RefreshCw /> Verify
          </Button>
          <Button size="xs" variant="ghost-danger" onClick={() => setRemoving(true)}>
            <Trash2 /> Remove
          </Button>
        </span>
      </div>
      <p className="text-[12px] text-[#8b8f99]">
        {r.last_error ? <span className="text-[#d1aaad]">{r.last_error}</span> : r.verified_at ? `Verified ${relativeTime(r.verified_at)}` : "Not verified yet"}
      </p>
      {removing && (
        <Confirm
          label={`Remove ${r.name}`}
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" onClick={() => void remove()}>
                Remove
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setRemoving(false)}>
                Keep it
              </Button>
            </>
          }
        >
          Images that pull with {r.name} pull anonymously after this.
        </Confirm>
      )}
    </div>
  );
}

function AddRegistry({ onDone }: { onDone: () => void }) {
  const [name, setName] = React.useState("");
  const [registry, setRegistry] = React.useState("docker.io");
  const [username, setUsername] = React.useState("");
  const [token, setToken] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const validName = /^[a-z0-9][a-z0-9-]{0,62}$/.test(name.trim());
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!validName || !registry.trim()) return;
    setBusy(true);
    try {
      const config: Record<string, string> = { registry: registry.trim() };
      if (username.trim()) config.username = username.trim();
      await api.createIntegration({ kind: "oci", name: name.trim(), config, ...(token.trim() ? { secret: token.trim() } : {}) });
      invalidate("ci/integrations");
      toast({ title: `${name.trim()} added`, description: "Verify it to check its credentials.", tone: "succeeded" });
      onDone();
    } catch (e2) {
      toast.error("Couldn't add the registry", errorText(e2));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form className="space-y-3 rounded-lg border border-white/10 p-4" onSubmit={(e) => void submit(e)} autoComplete="off">
      <div className="flex flex-wrap gap-3">
        <TextField
          className="min-w-[10rem] flex-1"
          label="Name"
          placeholder="dockerhub"
          value={name}
          onChange={(e) => setName(e.target.value)}
          hint={name && !validName ? "Lower case letters, digits and -" : undefined}
        />
        <TextField className="min-w-[12rem] flex-1" label="Registry" placeholder="docker.io" value={registry} onChange={(e) => setRegistry(e.target.value)} />
      </div>
      <div className="flex flex-wrap gap-3">
        <TextField className="min-w-[10rem] flex-1" label="Username" placeholder="for a private registry (optional)" value={username} onChange={(e) => setUsername(e.target.value)} />
        <TextField className="min-w-[12rem] flex-1" label="Token" type="password" placeholder="an access token (optional)" value={token} onChange={(e) => setToken(e.target.value)} />
      </div>
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={!validName || !registry.trim() || busy}>
          <Plus /> Add
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

/** What an account's row says of its configuration: the fields that name it. */
function accountSummary(i: Integration): string {
  const c = i.config;
  const pick = (k: string) => (typeof c[k] === "string" && c[k] ? String(c[k]) : null);
  const tenant = pick("tenant_id");
  const app = pick("client_id");
  return [pick("role_arn"), pick("provider"), pick("service_account"), tenant && `tenant ${tenant}`, app && `app ${app}`, pick("account_id"), pick("team_id"), pick("org"), pick("address"), pick("organization")]
    .filter(Boolean)
    .join(" · ");
}

/** The accounts kiln deploys through: every integration but registries (docs/cd-targets.md §6). */
function AccountsPanel() {
  const data = useRes(integrationsRes).data;
  const accounts = data.integrations.filter((i) => i.kind !== "oci");
  const kinds = data.kinds.filter((k) => k.kind !== "oci");
  const [adding, setAdding] = React.useState(false);
  return (
    <Panel
      eyebrow="Deployments"
      title="Accounts"
      description="The clouds and platforms kiln deploys to. AWS, Google Cloud and Azure trust kiln's own identity, so no key is stored for them; the other platforms take a token, kept in kiln's secret store."
      actions={
        !adding && (
          <Button size="sm" variant="secondary" onClick={() => setAdding(true)}>
            <Plus /> Add an account
          </Button>
        )
      }
    >
      <div className="space-y-4">
        {adding && <AddAccount kinds={kinds} onDone={() => setAdding(false)} />}
        {accounts.length === 0 && !adding ? (
          <p className="text-[13px] text-[#9a9da6]">None yet. Apps deploy to Kubernetes clusters without one.</p>
        ) : (
          <ul className="divide-y divide-white/[0.06]">
            {accounts.map((a) => (
              <li key={a.id} className="py-3 first:pt-0 last:pb-0">
                <AccountRow a={a} kind={kinds.find((k) => k.kind === a.kind)} />
              </li>
            ))}
          </ul>
        )}
      </div>
    </Panel>
  );
}

function AccountRow({ a, kind }: { a: Integration; kind?: IntegrationKind }) {
  const [busy, setBusy] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const [asking, setAsking] = React.useState(false);
  const [repository, setRepository] = React.useState("");
  const [environment, setEnvironment] = React.useState("");
  const federated = kind?.mode === "oidc";
  const verify = async () => {
    setBusy(true);
    try {
      const v = await api.verifyIntegration(a.name, federated ? { repository: repository.trim() || undefined, environment: environment.trim() || undefined } : undefined);
      invalidate("ci/integrations");
      if (v.ok) toast({ title: `${a.name} works`, description: v.message, tone: "succeeded" });
      else toast.error(`${a.name} didn't verify`, v.error);
    } catch (e) {
      toast.error(`Couldn't verify ${a.name}`, errorText(e));
    } finally {
      setBusy(false);
    }
  };
  const remove = async () => {
    try {
      await api.deleteIntegration(a.name);
      invalidate("ci/integrations");
      toast({ title: `${a.name} removed`, tone: "info" });
    } catch (e) {
      toast.error(`Couldn't remove ${a.name}`, errorText(e));
    }
  };
  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <span className="font-mono text-[13px] text-[#eceef2]">{a.name}</span>
        <span className="text-[12px] text-[#c9ccd3]">{kind?.title ?? a.kind}</span>
        <span className="min-w-0 truncate font-mono text-[12px] text-[#9a9da6]">{accountSummary(a)}</span>
        <span className="ml-auto flex gap-1">
          <Button size="xs" variant="ghost" disabled={busy || !kind?.built} onClick={() => (federated ? setAsking((x) => !x) : void verify())}>
            <RefreshCw /> Verify
          </Button>
          <Button size="xs" variant="ghost-danger" onClick={() => setRemoving(true)}>
            <Trash2 /> Remove
          </Button>
        </span>
      </div>
      <p className="text-[12px] text-[#8b8f99]">
        {!kind?.built ? (
          `Kept for when kiln deploys to ${kind?.title ?? a.kind}: not built yet.`
        ) : a.last_error ? (
          <span className="text-[#d1aaad]">{a.last_error}</span>
        ) : a.verified_at ? (
          `Verified ${relativeTime(a.verified_at)}`
        ) : (
          "Not verified yet"
        )}
      </p>
      {asking && federated && (
        <div className="flex flex-wrap items-start gap-3 rounded-lg border border-white/10 p-3">
          <TextField className="min-w-[12rem] flex-1" label="As the repository" placeholder="kiln's first repository" value={repository} onChange={(e) => setRepository(e.target.value)} />
          <TextField className="min-w-[10rem] flex-1" label="Environment" placeholder="production" value={environment} onChange={(e) => setEnvironment(e.target.value)} hint="The subject the trust policy names" />
          <Button size="sm" className="mt-6" disabled={busy} onClick={() => void verify()}>
            <RefreshCw /> Verify
          </Button>
        </div>
      )}
      {removing && (
        <Confirm
          label={`Remove ${a.name}`}
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" onClick={() => void remove()}>
                Remove
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setRemoving(false)}>
                Keep it
              </Button>
            </>
          }
        >
          Apps deploying through {a.name} stop reaching their targets until another account takes its place.
        </Confirm>
      )}
    </div>
  );
}

/** What the cloud needs to trust kiln: its issuer, the audience, the subject a sync presents. */
function TrustNote({ kind }: { kind: IntegrationKind }) {
  const issuer = useRes(oidcRes).data;
  const audience = kind.kind === "aws" ? "sts.amazonaws.com" : kind.kind === "azure" ? "api://AzureADTokenExchange" : "the provider's default";
  return (
    <div className="space-y-1 rounded-md bg-white/[0.03] p-3 text-[12px] leading-relaxed text-[#9a9da6]">
      <p>
        Trust kiln's issuer <code className="font-mono text-[#c9ccd3]">{issuer.issuer}</code> with the audience <code className="font-mono text-[#c9ccd3]">{audience}</code>, for the subject{" "}
        <code className="font-mono text-[#c9ccd3]">{"repo:<owner>/<repo>:environment:<environment>"}</code>: one role per environment serves its CI deploy jobs and its CD syncs.
      </p>
      <p>{kind.kind === "gcp" ? "Google Cloud can take kiln's keys inline (kiln oidc jwks)." : "The cloud fetches kiln's keys from the issuer: it must be reachable from the internet, or published where it is (kiln oidc publish)."}</p>
    </div>
  );
}

function AddAccount({ kinds, onDone }: { kinds: IntegrationKind[]; onDone: () => void }) {
  const [kindName, setKindName] = React.useState(kinds.find((k) => k.built)?.kind ?? kinds[0]?.kind ?? "");
  const kind = kinds.find((k) => k.kind === kindName);
  const [name, setName] = React.useState("");
  const [config, setConfig] = React.useState<Record<string, string>>({});
  const [secret, setSecret] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const validName = /^[a-z0-9][a-z0-9-]{0,62}$/.test(name.trim());
  const missing = (kind?.fields ?? []).filter((f) => f.required && !(config[f.name] ?? "").trim());
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!kind || !validName || missing.length) return;
    setBusy(true);
    try {
      const c: Record<string, string> = {};
      for (const f of kind.fields) {
        const v = (config[f.name] ?? "").trim();
        if (v) c[f.name] = v;
      }
      await api.createIntegration({ kind: kind.kind, name: name.trim(), config: c, ...(secret.trim() ? { secret: secret.trim() } : {}) });
      invalidate("ci/integrations");
      toast({ title: `${name.trim()} added`, description: kind.built ? "Verify it to check that it works." : `kiln keeps it for when ${kind.title} is built.`, tone: "succeeded" });
      onDone();
    } catch (e2) {
      toast.error("Couldn't add the account", errorText(e2));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form className="space-y-3 rounded-lg border border-white/10 p-4" onSubmit={(e) => void submit(e)} autoComplete="off">
      <div className="flex flex-wrap gap-3">
        <SelectField
          className="min-w-[12rem] flex-1"
          label="Platform"
          value={kindName}
          onChange={(v) => {
            setKindName(v);
            setConfig({});
          }}
        >
          {kinds.map((k) => (
            <option key={k.kind} value={k.kind}>
              {k.title}
              {k.built ? "" : " (kept until built)"}
            </option>
          ))}
        </SelectField>
        <TextField
          className="min-w-[10rem] flex-1"
          label="Name"
          placeholder={`${kindName}-production`}
          value={name}
          onChange={(e) => setName(e.target.value)}
          hint={name && !validName ? "Lower case letters, digits and -" : "How apps and environments name it"}
        />
      </div>
      {kind && kind.fields.length > 0 && (
        <div className="flex flex-wrap gap-3">
          {kind.fields.map((f) => (
            <TextField
              key={f.name}
              className="min-w-[14rem] flex-1"
              label={`${f.name}${f.required ? "" : " (optional)"}`}
              placeholder={f.help}
              value={config[f.name] ?? ""}
              onChange={(e) => setConfig((c) => ({ ...c, [f.name]: e.target.value }))}
            />
          ))}
        </div>
      )}
      {kind?.secret && <TextField label={kind.secret} type="password" placeholder="kept in kiln's secret store, never shown again" value={secret} onChange={(e) => setSecret(e.target.value)} />}
      {kind?.mode === "oidc" && (
        <Loadable label="kiln's issuer" fallback={<Hold height={60} />}>
          <TrustNote kind={kind} />
        </Loadable>
      )}
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={!kind || !validName || missing.length > 0 || busy}>
          <Plus /> Add
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
      </div>
    </form>
  );
}
