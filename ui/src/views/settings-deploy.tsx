/**
 * Settings → Deployments (docs/cd-targets.md §6, §7.1): where each image kiln publishes lives (the
 * local Docker of the machine that builds it, loaded into the cluster's nodes, or a registry), the
 * registries kiln pulls from and pipelines push to, and the accounts kiln deploys through (AWS,
 * Google Cloud and Azure trust kiln's identity, so nothing is stored for them; the other
 * platforms take a token). A chat account (Slack, Discord, Telegram, Teams) says where its
 * commands and buttons come to kiln, and what that chat needs set (docs/deploy-api.md §12.3).
 */
import * as React from "react";
import { Check, ChevronDown, MessageSquare, Plus, RefreshCw, Trash2 } from "lucide-react";
import { api, type CdImages, type ChatSetup, type ImageHome, type Integration, type IntegrationKind, type Integrations, type OidcIssuer } from "@/api/client";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, TextField } from "@/components/ui/misc";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { toast } from "@/components/ui/toast";
import { relativeTime } from "@/lib/format";
import { upsert, useRes, warm, type Res } from "@/lib/data";
import { invalidate } from "@/lib/resource";
import "./settings-deploy.css";

const imagesRes: Res<CdImages> = {
  key: "cd/images",
  fetch: (sig) => api.cdImages(sig),
};
const integrationsRes: Res<Integrations> = {
  key: "ci/integrations",
  fetch: (sig) => api.integrations(sig),
};
const oidcRes: Res<OidcIssuer> = {
  key: "ci/oidc",
  fetch: (sig) => api.oidc(sig),
};

/** What finds each group in Settings' search, besides its title. */
const IMAGES = {
  about: "Publish images to the build machine's Docker or to a registry, tagged by commit.",
  words: "images image local docker registry repository publish pulled",
};
const REGISTRIES = {
  about: "Credentials for pulling images and pushing pipeline builds. Public images can pull anonymously.",
  words: "registries registry docker hub ghcr quay token username verify add remove",
};
const ACCOUNTS = {
  about: "Accounts kiln deploys through. Clouds trust kiln's identity; other platforms use a stored token.",
  words: "accounts cloud aws gcp google azure oidc token platform verify add remove argo slack discord telegram teams chat commands",
};

/** The kinds a release's messages go to and commands come from. */
const CHAT_KINDS = ["slack", "discord", "telegram", "teams"];

/** The deployment settings: images, registries, then accounts. */
export function DeploySettings() {
  warm(imagesRes, integrationsRes);
  return (
    <Loadable label="the deployment settings" fallback={<DeployHold />}>
      <ImagesGroup />
      <RegistriesGroup />
      <AccountsGroup />
    </Loadable>
  );
}

function DeployHold() {
  return (
    <>
      <SettingsGroup id="deployments.images" title="Images" about={IMAGES.about} words={IMAGES.words}>
        <Hold height={44} />
      </SettingsGroup>
      <SettingsGroup id="deployments.registries" title="Registries" about={REGISTRIES.about} words={REGISTRIES.words}>
        <Hold height={44} />
      </SettingsGroup>
      <SettingsGroup id="deployments.accounts" title="Accounts" about={ACCOUNTS.about} words={ACCOUNTS.words}>
        <Hold height={44} />
      </SettingsGroup>
    </>
  );
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

type Feedback = { text: string; tone: "succeeded" | "error" };

function IntegrationStatus({ integration, pending, feedback, unavailable }: { integration: Integration; pending: "verify" | "remove" | null; feedback: Feedback | null; unavailable?: string }) {
  const text = pending === "verify" ? "Verifying…" : pending === "remove" ? "Removing…" : feedback?.text ?? unavailable ?? integration.last_error ?? (integration.verified_at ? `Verified ${relativeTime(integration.verified_at)}` : "Not verified");
  const tone = pending ? undefined : feedback?.tone ?? (unavailable ? undefined : integration.last_error ? "error" : integration.verified_at ? "succeeded" : undefined);
  return <p className="dep-status" data-tone={tone} role={tone === "error" ? "alert" : "status"}>{text}</p>;
}

function useIntegrationActions(integration: Integration) {
  const { setData } = useRes(integrationsRes);
  const [pending, setPending] = React.useState<"verify" | "remove" | null>(null);
  const [feedback, setFeedback] = React.useState<Feedback | null>(null);
  const active = React.useRef(false);
  const verify = async (as?: { repository?: string; environment?: string }) => {
    if (active.current) return false;
    active.current = true;
    setPending("verify");
    setFeedback(null);
    try {
      const result = await api.verifyIntegration(integration.name, as);
      invalidate("ci/integrations");
      const text = result.ok ? (result.message || "Verified just now") : (result.error || "Verification failed");
      setFeedback({ text, tone: result.ok ? "succeeded" : "error" });
      if (result.ok) toast({ title: `${integration.name} works`, description: result.message, tone: "succeeded" });
      else toast.error(`${integration.name} didn't verify`, text);
      return result.ok;
    } catch (e) {
      const text = `Couldn't verify: ${errorText(e)}`;
      setFeedback({ text, tone: "error" });
      toast.error(`Couldn't verify ${integration.name}`, errorText(e));
      return false;
    } finally {
      active.current = false;
      setPending(null);
    }
  };
  const remove = async () => {
    if (active.current) return;
    active.current = true;
    setPending("remove");
    setFeedback(null);
    try {
      await api.deleteIntegration(integration.name);
      setData((data) => ({ ...data, integrations: data.integrations.filter((item) => item.id !== integration.id) }));
      invalidate("ci/integrations");
      toast({ title: `${integration.name} removed`, tone: "info" });
    } catch (e) {
      setFeedback({ text: `Couldn't remove: ${errorText(e)}`, tone: "error" });
      toast.error(`Couldn't remove ${integration.name}`, errorText(e));
    } finally {
      active.current = false;
      setPending(null);
    }
  };
  return { pending, feedback, verify, remove };
}

/** A field-styled select, its label above it (in a form), or beside it in a row (`bare`). */
function SelectField({
  label,
  value,
  onChange,
  children,
  className,
  bare,
  id: given,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  children: React.ReactNode;
  className?: string;
  bare?: boolean;
  id?: string;
}) {
  const made = React.useId();
  const id = given ?? made;
  const select = (
    <div className={bare ? `select ${className ?? ""}` : "select"}>
      <select id={id} value={value} aria-label={bare ? label : undefined} onChange={(e) => onChange(e.target.value)}>
        {children}
      </select>
      <ChevronDown size={14} aria-hidden="true" />
    </div>
  );
  if (bare) return select;
  return (
    <div className={className}>
      <label htmlFor={id} className="field-label">
        {label}
      </label>
      {select}
    </div>
  );
}

/* ---------------------------- images ---------------------------- */

function ImagesGroup() {
  const data = useRes(imagesRes).data;
  const registries = useRes(integrationsRes).data.integrations.filter((i) => i.kind === "oci");
  const names = [...new Set([...Object.keys(data.used), ...Object.keys(data.images)])].sort();
  return (
    <SettingsGroup id="deployments.images" title="Images" about={IMAGES.about} words={IMAGES.words}>
      {names.length === 0 ? (
        <p className="st-note">
          No app publishes an image yet. An artifact written <code>{"{ published: <name> }"}</code> in an app of <code>.kiln/deploy.yaml</code> shows here.
        </p>
      ) : (
        <ul className="st-list">
          {names.map((n) => (
            <li key={n}>
              <ImageRow name={n} home={data.images[n] ?? { kind: "local" }} apps={data.used[n] ?? []} registries={registries} />
            </li>
          ))}
        </ul>
      )}
    </SettingsGroup>
  );
}

function ImageRow({ name, home, apps, registries }: { name: string; home: ImageHome; apps: string[]; registries: Integration[] }) {
  const { setData } = useRes(imagesRes);
  const [kind, setKind] = React.useState<ImageHome["kind"]>(home.kind);
  const [repository, setRepository] = React.useState(home.kind === "registry" ? home.repository : "");
  const [integration, setIntegration] = React.useState(home.kind === "registry" ? (home.integration ?? "") : "");
  const [saving, setSaving] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const active = React.useRef(false);
  const id = React.useId();
  const draft: ImageHome =
    kind === "local"
      ? { kind: "local" }
      : {
          kind: "registry",
          repository: repository.trim(),
          ...(integration ? { integration } : {}),
        };
  const changed =
    JSON.stringify(draft) !==
    JSON.stringify(
      home.kind === "local"
        ? { kind: "local" }
        : {
            kind: "registry",
            repository: home.repository,
            ...(home.integration ? { integration: home.integration } : {}),
          },
    );
  const valid = kind === "local" || /^[a-z0-9.:-]+(\/[a-z0-9._-]+)+$/.test(repository.trim());
  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!changed || !valid || active.current) return;
    active.current = true;
    setSaving(true);
    setProblem(null);
    try {
      const saved = await api.setCdImage(name, draft);
      setData((data) => ({ ...data, images: { ...data.images, [name]: saved.images[name] ?? draft } }));
      setRepository(repository.trim());
      invalidate("cd/images");
      toast({
        title: kind === "local" ? `${name} lives in the local Docker` : `${name} goes to ${repository.trim()}`,
        tone: "succeeded",
      });
    } catch (e) {
      setProblem(errorText(e));
      toast.error(`Couldn't save where ${name} lives`, errorText(e));
    } finally {
      active.current = false;
      setSaving(false);
    }
  };
  return (
    <form className="dep-image" onSubmit={(e) => void save(e)} aria-label={`Publish ${name}`} aria-busy={saving}>
      <fieldset disabled={saving} className="dep-fieldset">
        <SettingRow label={name} htmlFor={id} help={apps.length ? `Published for ${apps.join(", ")}` : "No app publishes it now"}>
          <SelectField bare id={id} label={`Where ${name} lives`} value={kind} onChange={(v) => setKind(v as ImageHome["kind"])} className="st-wide">
            <option value="local">Local Docker</option>
            <option value="registry">A registry</option>
          </SelectField>
        </SettingRow>
        {kind === "registry" && (
          <div className="st-fields dep-image-fields">
            <div>
              <TextField
                id={`${id}-repository`}
                label="Repository"
                placeholder="docker.io/acme/kiln"
                value={repository}
                onChange={(e) => setRepository(e.target.value)}
                aria-invalid={!valid}
                aria-describedby={`${id}-hint`}
              />
              <p className="st-note" id={`${id}-hint`}>{valid ? "Tagged by commit; leave the tag out" : "Use a repository like docker.io/acme/kiln, without a tag"}</p>
            </div>
            <SelectField label="Pulled with" value={integration} onChange={setIntegration}>
              <option value="">The registry's integration, or none (public)</option>
              {integration && !registries.some((r) => r.name === integration) && <option value={integration}>{integration} (removed)</option>}
              {registries.map((r) => (
                <option key={r.name} value={r.name}>
                  {r.name} · {String(r.config.registry ?? "")}
                </option>
              ))}
            </SelectField>
          </div>
        )}
        {changed && (
          <div className="st-form-acts dep-image-actions">
            <Button type="submit" size="xs" disabled={!valid || saving}>
              <Check /> {saving ? "Saving…" : "Save"}
            </Button>
            <Button
              size="xs"
              variant="ghost"
              onClick={() => {
                setKind(home.kind);
                setRepository(home.kind === "registry" ? home.repository : "");
                setIntegration(home.kind === "registry" ? (home.integration ?? "") : "");
                setProblem(null);
              }}
            >
              Undo
            </Button>
          </div>
        )}
      </fieldset>
      {problem && <p className="dep-status" data-tone="error" role="alert">{problem}</p>}
    </form>
  );
}

/* ---------------------------- registries ---------------------------- */

function RegistriesGroup() {
  const registries = useRes(integrationsRes).data.integrations.filter((i) => i.kind === "oci");
  const [adding, setAdding] = React.useState(false);
  return (
    <SettingsGroup
      id="deployments.registries"
      title="Registries"
      about={REGISTRIES.about}
      words={REGISTRIES.words}
      actions={
        !adding && (
          <Button size="xs" variant="secondary" onClick={() => setAdding(true)}>
            <Plus /> Add a registry
          </Button>
        )
      }
    >
      {adding && <AddRegistry onDone={() => setAdding(false)} />}
      {registries.length === 0 && !adding ? (
        <p className="st-note">None yet: images resolve anonymously.</p>
      ) : (
        <ul className="st-list">
          {registries.map((r) => (
            <li key={r.id}>
              <RegistryRow r={r} />
            </li>
          ))}
        </ul>
      )}
    </SettingsGroup>
  );
}

function RegistryRow({ r }: { r: Integration }) {
  const { pending, feedback, verify, remove } = useIntegrationActions(r);
  const [removing, setRemoving] = React.useState(false);
  const user = typeof r.config.username === "string" && r.config.username ? r.config.username : null;
  return (
    <>
      <div className="dep-row" aria-busy={pending !== null}>
        <div className="dep-identity">
          <span className="st-item-name">{r.name}</span>
          <div className="dep-meta-row">
            <span className="dep-meta mono">{String(r.config.registry ?? "")}</span>
            <span className="dep-meta">{user ? `as ${user}` : "Anonymous"}{r.has_secret ? " · token stored" : ""}</span>
          </div>
          <IntegrationStatus integration={r} pending={pending} feedback={feedback} />
        </div>
        <div className="dep-actions">
          <Button size="xs" variant="ghost" disabled={pending !== null || removing} onClick={() => void verify()} aria-label={`Verify ${r.name}`}>
            <RefreshCw /> {pending === "verify" ? "Verifying…" : "Verify"}
          </Button>
          <Button size="xs" variant="ghost-danger" disabled={pending !== null || removing} onClick={() => setRemoving(true)} aria-label={`Remove ${r.name}`}>
            <Trash2 /> Remove
          </Button>
        </div>
      </div>
      {removing && (
        <Confirm
          className="dep-confirm"
          label={`Remove ${r.name}`}
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" disabled={pending !== null} onClick={() => void remove()}>
                {pending === "remove" ? "Removing…" : "Remove registry"}
              </Button>
              <Button size="xs" variant="ghost" disabled={pending !== null} onClick={() => setRemoving(false)}>
                Cancel
              </Button>
            </>
          }
        >
          Images that pull with {r.name} pull anonymously after this.
        </Confirm>
      )}
    </>
  );
}

function AddRegistry({ onDone }: { onDone: () => void }) {
  const { setData } = useRes(integrationsRes);
  const [name, setName] = React.useState("");
  const [registry, setRegistry] = React.useState("docker.io");
  const [username, setUsername] = React.useState("");
  const [token, setToken] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const active = React.useRef(false);
  const validName = /^[a-z0-9][a-z0-9-]{0,62}$/.test(name.trim());
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!validName || !registry.trim() || active.current) return;
    active.current = true;
    setBusy(true);
    setProblem(null);
    try {
      const config: Record<string, string> = { registry: registry.trim() };
      if (username.trim()) config.username = username.trim();
      const added = await api.createIntegration({
        kind: "oci",
        name: name.trim(),
        config,
        ...(token.trim() ? { secret: token.trim() } : {}),
      });
      setData((data) => ({ ...data, integrations: upsert(data.integrations, added, (item) => item.id, false) }));
      invalidate("ci/integrations");
      toast({
        title: `${name.trim()} added`,
        description: "Verify it to check its credentials.",
        tone: "succeeded",
      });
      onDone();
    } catch (e2) {
      setProblem(errorText(e2));
      toast.error("Couldn't add the registry", errorText(e2));
    } finally {
      active.current = false;
      setBusy(false);
    }
  };
  return (
    <form className="st-form dep-form" onSubmit={(e) => void submit(e)} autoComplete="off" aria-label="Add a registry" aria-busy={busy}>
      <fieldset disabled={busy} className="dep-fieldset">
        <legend className="dep-form-title">Add a registry</legend>
        <div className="st-fields">
          <TextField label="Name" placeholder="dockerhub" value={name} onChange={(e) => setName(e.target.value)} required aria-invalid={Boolean(name) && !validName} hint={name && !validName ? "Lower case letters, digits and -" : undefined} autoFocus />
          <TextField label="Registry" placeholder="docker.io" value={registry} onChange={(e) => setRegistry(e.target.value)} required />
          <TextField label="Username" placeholder="for a private registry (optional)" value={username} onChange={(e) => setUsername(e.target.value)} />
          <TextField label="Token" type="password" placeholder="an access token (optional)" value={token} onChange={(e) => setToken(e.target.value)} />
        </div>
        {problem && <p className="dep-status" data-tone="error" role="alert">{problem}</p>}
        <div className="st-form-acts dep-form-actions">
          <Button type="submit" size="xs" disabled={!validName || !registry.trim() || busy}>
            <Plus /> {busy ? "Adding…" : "Add registry"}
          </Button>
          <Button type="button" size="xs" variant="ghost" onClick={onDone}>
            Cancel
          </Button>
        </div>
      </fieldset>
    </form>
  );
}

/* ---------------------------- accounts ---------------------------- */

/** What an account's row says of its configuration: the fields that name it. */
function accountSummary(i: Integration): string {
  const c = i.config;
  const pick = (k: string) => (typeof c[k] === "string" && c[k] ? String(c[k]) : null);
  const tenant = pick("tenant_id");
  const app = pick("client_id");
  return [
    pick("role_arn"),
    pick("provider"),
    pick("service_account"),
    tenant && `tenant ${tenant}`,
    app && `app ${app}`,
    pick("account_id"),
    pick("team_id"),
    pick("org"),
    pick("address"),
    pick("organization"),
  ]
    .filter(Boolean)
    .join(" · ");
}

/** The accounts kiln deploys through: every integration but registries (docs/cd-targets.md §6). */
function AccountsGroup() {
  const data = useRes(integrationsRes).data;
  const accounts = data.integrations.filter((i) => i.kind !== "oci");
  const kinds = data.kinds.filter((k) => k.kind !== "oci");
  const [adding, setAdding] = React.useState(false);
  return (
    <SettingsGroup
      id="deployments.accounts"
      title="Accounts"
      about={ACCOUNTS.about}
      words={ACCOUNTS.words}
      actions={
        !adding && (
          <Button size="xs" variant="secondary" onClick={() => setAdding(true)}>
            <Plus /> Add an account
          </Button>
        )
      }
    >
      {adding && <AddAccount kinds={kinds} onDone={() => setAdding(false)} />}
      {accounts.length === 0 && !adding ? (
        <p className="st-note">None yet. Apps deploy to Kubernetes clusters without one.</p>
      ) : (
        <ul className="st-list">
          {accounts.map((a) => (
            <li key={a.id}>
              <AccountRow a={a} kind={kinds.find((k) => k.kind === a.kind)} />
            </li>
          ))}
        </ul>
      )}
    </SettingsGroup>
  );
}

function AccountRow({ a, kind }: { a: Integration; kind?: IntegrationKind }) {
  const { pending, feedback, verify, remove } = useIntegrationActions(a);
  const [removing, setRemoving] = React.useState(false);
  const [asking, setAsking] = React.useState(false);
  const [repository, setRepository] = React.useState("");
  const [environment, setEnvironment] = React.useState("");
  const [commands, setCommands] = React.useState(false);
  const verifyId = React.useId();
  const commandsId = React.useId();
  const federated = kind?.mode === "oidc";
  const chat = CHAT_KINDS.includes(a.kind);
  const summary = [accountSummary(a), a.has_signing_secret ? "signing secret stored" : ""].filter(Boolean).join(" · ");
  return (
    <>
      <div className="dep-row" aria-busy={pending !== null}>
        <div className="dep-identity">
          <div className="dep-title"><span className="st-item-name">{a.name}</span><span className="dep-meta">{kind?.title ?? a.kind}</span></div>
          {summary && <p className="dep-meta mono">{summary}</p>}
          <IntegrationStatus integration={a} pending={pending} feedback={feedback} unavailable={!kind?.built ? `${kind?.title ?? a.kind} deployments aren't available yet` : undefined} />
        </div>
        <div className="dep-actions">
          {chat && (
            <Button size="xs" variant="ghost" aria-label={`Commands from ${a.name}`} aria-expanded={commands} aria-controls={commandsId} onClick={() => setCommands((x) => !x)}>
              <MessageSquare /> Commands
            </Button>
          )}
          <Button size="xs" variant="ghost" disabled={pending !== null || !kind?.built || removing} aria-label={`Verify ${a.name}`} aria-expanded={federated ? asking : undefined} aria-controls={federated ? verifyId : undefined} onClick={() => (federated ? setAsking((x) => !x) : void verify())}>
            <RefreshCw /> {pending === "verify" ? "Verifying…" : "Verify"}
          </Button>
          <Button size="xs" variant="ghost-danger" disabled={pending !== null || removing} aria-label={`Remove ${a.name}`} onClick={() => { setRemoving(true); setAsking(false); }}>
            <Trash2 /> Remove
          </Button>
        </div>
      </div>
      {commands && chat && <ChatCommands a={a} id={commandsId} />}
      {asking && federated && (
        <form
          id={verifyId}
          className="st-form dep-form dep-verify"
          aria-label={`Verify ${a.name}`}
          aria-busy={pending !== null}
          onSubmit={(e) => {
            e.preventDefault();
            void verify({ repository: repository.trim() || undefined, environment: environment.trim() || undefined });
          }}
        >
          <fieldset disabled={pending !== null} className="dep-fieldset">
            <legend className="dep-form-title">Verify {a.name}</legend>
            <div className="st-fields">
              <TextField label="As the repository" placeholder="kiln's first repository" value={repository} onChange={(e) => setRepository(e.target.value)} autoFocus />
              <TextField label="Environment" placeholder="production" value={environment} onChange={(e) => setEnvironment(e.target.value)} hint="The subject the trust policy names" />
            </div>
            <div className="st-form-acts dep-form-actions">
              <Button type="submit" size="xs" disabled={pending !== null}>
                <RefreshCw /> {pending === "verify" ? "Verifying…" : "Verify account"}
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setAsking(false)}>Cancel</Button>
            </div>
          </fieldset>
        </form>
      )}
      {removing && (
        <Confirm
          className="dep-confirm"
          label={`Remove ${a.name}`}
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" disabled={pending !== null} onClick={() => void remove()}>
                {pending === "remove" ? "Removing…" : "Remove account"}
              </Button>
              <Button size="xs" variant="ghost" disabled={pending !== null} onClick={() => setRemoving(false)}>
                Cancel
              </Button>
            </>
          }
        >
          Apps deploying through {a.name} stop reaching their targets until another account takes its place.
        </Confirm>
      )}
    </>
  );
}

/** What's still missing before a chat's commands come to kiln, if anything. */
function chatMissing(kind: string, setup: ChatSetup): string | null {
  if (kind === "discord") return setup.public_key ? null : "Add the application's public key (public_key) to check its interactions with.";
  if (kind === "telegram") return setup.commands ? null : "Point the bot's webhook at kiln to take its chats' commands and buttons.";
  return setup.signing_secret ? null : kind === "teams" ? "Store the outgoing webhook's security token as this account's signing secret." : "Store the Slack app's signing secret on this account: commands and buttons are taken only then.";
}

/** Where a chat's commands and buttons come to kiln, what that chat needs set, and for a Telegram
 * bot, its webhook pointed here (docs/deploy-api.md §12.3). */
function ChatCommands({ a, id }: { a: Integration; id: string }) {
  const { setData } = useRes(integrationsRes);
  const [setup, setSetup] = React.useState<ChatSetup | null>(null);
  const [problem, setProblem] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [draft, setDraft] = React.useState("");
  React.useEffect(() => {
    const ctl = new AbortController();
    api.integrationChat(a.name, ctl.signal).then(setSetup, (e) => {
      if (!ctl.signal.aborted) setProblem(errorText(e));
    });
    return () => ctl.abort();
  }, [a.name]);
  const connect = async () => {
    setBusy(true);
    setProblem(null);
    try {
      setSetup(await api.connectIntegrationChat(a.name));
      invalidate("ci/integrations");
      toast({ title: `${a.name}'s bot reports to kiln`, description: "Commands and buttons in its chats come here now.", tone: "succeeded" });
    } catch (e) {
      setProblem(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  // What Slack and Teams sign with, or Discord's public key, kept now.
  const keep = async (value: string) => {
    setBusy(true);
    setProblem(null);
    try {
      const changed = await api.updateIntegration(a.name, a.kind === "discord" ? { config: { ...a.config, public_key: value } } : { signing_secret: value });
      setData((data) => ({ ...data, integrations: upsert(data.integrations, changed, (item) => item.id, false) }));
      invalidate("ci/integrations");
      setSetup(await api.integrationChat(a.name));
      setDraft("");
    } catch (e) {
      setProblem(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  if (!setup) return problem ? <p className="dep-status" data-tone="error" role="alert">{problem}</p> : <Hold height={60} />;
  const missing = chatMissing(a.kind, setup);
  const asks = a.kind === "discord" ? (setup.public_key ? null : "The application's public key") : a.kind === "telegram" ? null : setup.signing_secret ? null : a.kind === "teams" ? "The outgoing webhook's security token" : "The Slack app's signing secret";
  return (
    <div id={id} className="st-trust" aria-label={`Commands from ${a.name}`}>
      <p>
        Its address: <code className="break-all">{setup.address}</code>
      </p>
      <p>{setup.how}</p>
      {setup.scopes && (
        <p>
          The bot&rsquo;s scopes <code>{setup.scopes.join(", ")}</code>, its events <code>{(setup.events ?? []).join(", ")}</code>.
        </p>
      )}
      <p className="dep-status" data-tone={missing ? undefined : "succeeded"} role="status">
        {missing ?? "kiln takes its commands: deploy, approve, reject, rollback, status. Each runs as the person who said it, once they've linked their account (Settings → You → Chat accounts)."}
      </p>
      {asks && (
        <form
          className="st-form-acts dep-form-actions"
          onSubmit={(e) => {
            e.preventDefault();
            if (draft.trim()) void keep(draft.trim());
          }}
        >
          <TextField label={asks} type={a.kind === "discord" ? "text" : "password"} placeholder={a.kind === "discord" ? "hex, from the application's General Information" : "kept in kiln's secret store, never shown again"} value={draft} onChange={(e) => setDraft(e.target.value)} />
          <Button type="submit" size="xs" variant="secondary" disabled={busy || !draft.trim()}>
            {busy ? "Keeping…" : "Keep it"}
          </Button>
        </form>
      )}
      {problem && <p className="dep-status" data-tone="error" role="alert">{problem}</p>}
      {a.kind === "telegram" && (
        <div className="st-form-acts dep-form-actions">
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => void connect()}>
            {busy ? "Pointing…" : setup.commands ? "Point its webhook here again" : "Point its webhook at kiln"}
          </Button>
        </div>
      )}
    </div>
  );
}

/** What the cloud needs to trust kiln: its issuer, the audience, the subject a sync presents. */
function TrustNote({ kind }: { kind: IntegrationKind }) {
  const issuer = useRes(oidcRes).data;
  const audience = kind.kind === "aws" ? "sts.amazonaws.com" : kind.kind === "azure" ? "api://AzureADTokenExchange" : "the provider's default";
  return (
    <div className="st-trust">
      <p>
        Trust kiln&rsquo;s issuer <code>{issuer.issuer}</code> with the audience <code>{audience}</code>, for the subject <code>{"repo:<owner>/<repo>:environment:<environment>"}</code>: one role per
        environment serves its CI deploy jobs and its CD syncs.
      </p>
      <p>
        {kind.kind === "gcp"
          ? "Google Cloud can take kiln's keys inline (kiln oidc jwks)."
          : "The cloud fetches kiln's keys from the issuer: it must be reachable from the internet, or published where it is (kiln oidc publish)."}
      </p>
    </div>
  );
}

function AddAccount({ kinds, onDone }: { kinds: IntegrationKind[]; onDone: () => void }) {
  const { setData } = useRes(integrationsRes);
  const [kindName, setKindName] = React.useState(kinds.find((k) => k.built)?.kind ?? kinds[0]?.kind ?? "");
  const kind = kinds.find((k) => k.kind === kindName);
  const [name, setName] = React.useState("");
  const [config, setConfig] = React.useState<Record<string, string>>({});
  const [secret, setSecret] = React.useState("");
  const [signing, setSigning] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const active = React.useRef(false);
  const validName = /^[a-z0-9][a-z0-9-]{0,62}$/.test(name.trim());
  const missing = (kind?.fields ?? []).filter((f) => f.required && !(config[f.name] ?? "").trim());
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!kind || !validName || missing.length || active.current) return;
    active.current = true;
    setBusy(true);
    setProblem(null);
    try {
      const c: Record<string, string> = {};
      for (const f of kind.fields) {
        const v = (config[f.name] ?? "").trim();
        if (v) c[f.name] = v;
      }
      const added = await api.createIntegration({
        kind: kind.kind,
        name: name.trim(),
        config: c,
        ...(secret.trim() ? { secret: secret.trim() } : {}),
        ...(kind.signing && signing.trim() ? { signing_secret: signing.trim() } : {}),
      });
      setData((data) => ({ ...data, integrations: upsert(data.integrations, added, (item) => item.id, false) }));
      invalidate("ci/integrations");
      toast({
        title: `${name.trim()} added`,
        description: kind.built ? "Verify it to check that it works." : `kiln keeps it for when ${kind.title} is built.`,
        tone: "succeeded",
      });
      onDone();
    } catch (e2) {
      setProblem(errorText(e2));
      toast.error("Couldn't add the account", errorText(e2));
    } finally {
      active.current = false;
      setBusy(false);
    }
  };
  return (
    <form className="st-form dep-form" onSubmit={(e) => void submit(e)} autoComplete="off" aria-label="Add an account" aria-busy={busy}>
      <fieldset disabled={busy} className="dep-fieldset">
        <legend className="dep-form-title">Add an account</legend>
        <div className="st-fields">
          <SelectField
            label="Platform"
            value={kindName}
            onChange={(v) => {
              setKindName(v);
              setConfig({});
              setSecret("");
              setSigning("");
              setProblem(null);
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
            label="Name"
            placeholder={`${kindName}-production`}
            value={name}
            onChange={(e) => setName(e.target.value)}
            required
            aria-invalid={Boolean(name) && !validName}
            hint={name && !validName ? "Lower case letters, digits and -" : "How apps and environments name it"}
          />
          {kind?.fields.map((f) => (
            <TextField
              key={f.name}
              label={`${accountFieldLabel(f.name)}${f.required ? "" : " (optional)"}`}
              placeholder={f.help}
              value={config[f.name] ?? ""}
              onChange={(e) => setConfig((c) => ({ ...c, [f.name]: e.target.value }))}
              required={f.required}
            />
          ))}
          {kind?.secret && <TextField label={kind.secret} type="password" placeholder="kept in kiln's secret store, never shown again" value={secret} onChange={(e) => setSecret(e.target.value)} />}
          {kind?.signing && <TextField label={`Signing secret (optional): ${kind.signing}`} type="password" placeholder="kept in kiln's secret store, never shown again" value={signing} onChange={(e) => setSigning(e.target.value)} />}
        </div>
        {kind?.mode === "oidc" && (
          <Loadable label="kiln's issuer" fallback={<Hold height={60} />}>
            <TrustNote kind={kind} />
          </Loadable>
        )}
        {problem && <p className="dep-status" data-tone="error" role="alert">{problem}</p>}
        <div className="st-form-acts dep-form-actions">
          <Button type="submit" size="xs" disabled={!kind || !validName || missing.length > 0 || busy}>
            <Plus /> {busy ? "Adding…" : "Add account"}
          </Button>
          <Button type="button" size="xs" variant="ghost" onClick={onDone}>
            Cancel
          </Button>
        </div>
      </fieldset>
    </form>
  );
}

function accountFieldLabel(name: string): string {
  const labels: Record<string, string> = {
    role_arn: "Role ARN",
    provider: "Identity provider",
    service_account: "Service account",
    tenant_id: "Tenant ID",
    client_id: "Client ID",
    account_id: "Account ID",
    team_id: "Team ID",
    org: "Organization",
  };
  const readable = name.replace(/_/g, " ");
  return labels[name] ?? readable.charAt(0).toUpperCase() + readable.slice(1);
}
