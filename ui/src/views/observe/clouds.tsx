/**
 * Clouds: AWS CloudWatch, Google Cloud Monitoring and Azure Monitor, whose metrics kiln reads
 * (docs/observe.md §10.1). Each is named and pulled by the server on its interval, with credentials
 * given here (sealed on the server, never shown again) or the server's own where it runs.
 */
import * as React from "react";
import { Pencil, Plus, RefreshCw, Trash2 } from "lucide-react";
import { observeApi, type Cloud, type CloudIn, type CloudKind, type CloudTest } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { SectionHeading, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { cloudsRes } from "./parts";

const KINDS: { value: CloudKind; label: string }[] = [
  { value: "aws", label: "AWS CloudWatch" },
  { value: "gcp", label: "Google Cloud Monitoring" },
  { value: "azure", label: "Azure Monitor" },
];
const WORDS = Object.fromEntries(KINDS.map((k) => [k.value, k.label])) as Record<CloudKind, string>;
const TONE = { ok: "succeeded", failing: "failed", starting: "pending", paused: "skipped" } as const;
const STATE = { ok: "Pulling", failing: "Failing", starting: "Starting", paused: "Paused" } as const;
/** What a secret's field says when one is kept. */
const KEPT = "Kept. Type a new one to replace it.";
/** What each cloud's credentials must be allowed, and where the server's own are found. */
const NEED: Record<CloudKind, string> = {
  aws: "They need cloudwatch:ListMetrics and cloudwatch:GetMetricData (and sts:AssumeRole, for a role).",
  gcp: "It needs the Monitoring Viewer role.",
  azure: "It needs the Monitoring Reader role on each subscription.",
};
const WHERE: Record<CloudKind, string> = {
  aws: "As AWS's SDKs find them: the environment, the shared credentials file, EKS's web identity, the container's, or the EC2 instance's role.",
  gcp: "GOOGLE_APPLICATION_CREDENTIALS, gcloud's application default credentials, or the metadata server (Compute Engine, GKE, Cloud Run).",
  azure: "The environment's app, AKS's workload identity, App Service's managed identity, or the VM's.",
};

const list = (s: string) =>
  s
    .split(/[\s,]+/)
    .map((x) => x.trim())
    .filter(Boolean);
const count = (n: number) => n.toLocaleString();

export function Clouds() {
  const [editing, setEditing] = React.useState<Cloud | "new" | null>(null);
  return (
    <section className="mt-12">
      <SectionHeading
        index="02"
        label="Clouds"
        title="Metrics kiln reads"
        aside={
          <Button variant="ghost" onClick={() => setEditing("new")}>
            <Plus /> Add cloud
          </Button>
        }
      />
      <Loadable label="the clouds" fallback={<TableHold head={<Head />} rows={2} />}>
        <List onAdd={() => setEditing("new")} onEdit={setEditing} />
      </Loadable>
      {editing && <CloudDialog cloud={editing === "new" ? null : editing} onClose={() => setEditing(null)} />}
    </section>
  );
}

/**
 * As monitors' list: the state first, then the name, what went wrong beneath it (and, on a phone,
 * its cloud); what it reads, how and when, on wider screens.
 */
function Head() {
  return (
    <tr>
      <th>State</th>
      <th>Name</th>
      <th className="hidden md:table-cell">Cloud</th>
      <th className="hidden md:table-cell">Reads</th>
      <th className="hidden md:table-cell">Credentials</th>
      <th className="hidden sm:table-cell">Last pull</th>
      <th />
    </tr>
  );
}

/** What an integration reads, in words. */
function reads(c: Cloud): string {
  const some = (xs: string[] | undefined, all: string) => (xs && xs.length > 0 ? xs.join(", ") : all);
  switch (c.kind) {
    case "aws":
      return `${some(c.regions, "the server's region")} · ${some(c.namespaces, "every namespace")}`;
    case "gcp":
      return `${some(c.projects, "the credentials' project")} · ${some(c.services, "every service")}`;
    case "azure":
      return `${some(c.subscriptions, "every subscription")} · ${some(c.resource_types, "every type")}`;
  }
}

function credentials(c: Cloud): string {
  switch (c.kind) {
    case "aws":
      return `${c.access_key_id ? `keys ${c.access_key_id}` : "the server's own"}${c.role_arn ? `, as ${c.role_arn.split("/").pop()}` : ""}`;
    case "gcp":
      return c.service_account ? "a service account's key" : "the server's own";
    case "azure":
      return c.client_secret ? `the app ${c.client_id ?? ""}` : "the server's own";
  }
}

function List({ onAdd, onEdit }: { onAdd: () => void; onEdit: (c: Cloud) => void }) {
  const all = useRes(cloudsRes).data;
  const [removing, setRemoving] = React.useState<Cloud | null>(null);
  if (all.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        None yet.{" "}
        <button type="button" className="text-link" onClick={onAdd}>
          Add one
        </button>{" "}
        to read AWS CloudWatch, Google Cloud Monitoring or Azure Monitor into kiln&apos;s metrics.
      </p>
    );
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {all.map((c) => {
              const s = c.status;
              return (
                <tr key={c.name}>
                  <td className="whitespace-nowrap">
                    <span className="status">
                      <i className="dot" data-status={TONE[s.state]} aria-hidden="true" />
                      <span className="status-text" data-status={TONE[s.state]}>
                        {STATE[s.state]}
                      </span>
                    </span>
                  </td>
                  <td className="truncate-cell">
                    <div className="truncate text-[13px] text-[#e4e5e7]">{c.name}</div>
                    <div className="truncate text-[12px] text-[#8c8f98] md:hidden">{WORDS[c.kind] ?? c.kind}</div>
                    {s.error ? (
                      <div className="truncate text-[12px] text-[#e2c4c6]" title={s.error}>
                        {s.error}
                      </div>
                    ) : (
                      s.warnings &&
                      s.warnings.length > 0 && (
                        <div className="truncate text-[12px] text-[#8c8f98]" title={s.warnings.join("\n")}>
                          {s.warnings[0]}
                        </div>
                      )
                    )}
                  </td>
                  <td className="hidden whitespace-nowrap md:table-cell">{WORDS[c.kind] ?? c.kind}</td>
                  <td className="hidden md:table-cell">
                    <div className="max-w-[18rem] truncate" title={reads(c)}>
                      {reads(c)}
                    </div>
                  </td>
                  <td className="hidden whitespace-nowrap md:table-cell">{credentials(c)}</td>
                  <td className="hidden whitespace-nowrap sm:table-cell">
                    {s.last_pull ? (
                      <>
                        {relativeTime(s.last_pull / 1000)}
                        <div className="text-[12px] text-[#8c8f98]">
                          {count(s.series)} series{s.over > 0 ? `, ${count(s.over)} past the limit` : ""}
                        </div>
                      </>
                    ) : (
                      "—"
                    )}
                  </td>
                  <td className="w-1 whitespace-nowrap">
                    <Button
                      variant="ghost"
                      size="xs"
                      aria-label={`Pull ${c.name} now`}
                      onClick={async () => {
                        try {
                          await observeApi.pullCloud(c.name);
                          toast({ title: `Pulling ${c.name}`, tone: "info" });
                          window.setTimeout(() => invalidate("observe/clouds"), 3000);
                        } catch (e) {
                          toast({ title: `Couldn't pull ${c.name}`, description: errorMessage(e), tone: "failed" });
                        }
                      }}
                    >
                      <RefreshCw /> <span className="hidden sm:inline">Pull now</span>
                    </Button>
                    <Button variant="ghost" size="xs" aria-label={`Edit ${c.name}`} onClick={() => onEdit(c)}>
                      <Pencil />
                    </Button>
                    <Button variant="ghost-danger" size="xs" aria-label={`Remove ${c.name}`} onClick={() => setRemoving(c)}>
                      <Trash2 />
                    </Button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? ""}?`}
        description="Its metrics are read no more; those read stay, until their retention."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (!removing) return;
          try {
            await observeApi.deleteCloud(removing.name);
            invalidate("observe/clouds");
          } catch (e) {
            toast({ title: "Couldn't remove it", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

/** The form's fields, as text. */
interface Form {
  kind: CloudKind;
  name: string;
  interval: string;
  tags: string;
  /** "given" (here) or "server" (the server's own). */
  creds: "given" | "server";
  // AWS
  regions: string;
  namespaces: string;
  stats: string;
  keyId: string;
  secret: string;
  role: string;
  externalId: string;
  // Google
  projects: string;
  services: string;
  key: string;
  // Azure
  tenant: string;
  client: string;
  subscriptions: string;
  types: string;
  aggregations: string;
}

function formOf(c: Cloud | null): Form {
  const f: Form = {
    kind: c?.kind ?? "aws",
    name: c?.name ?? "",
    interval: String(c?.interval_s ?? 300),
    tags: (c?.tags ?? []).join(", "),
    creds: "given",
    regions: "",
    namespaces: "",
    stats: "Sum, SampleCount",
    keyId: "",
    secret: "",
    role: "",
    externalId: "",
    projects: "",
    services: "",
    key: "",
    tenant: "",
    client: "",
    subscriptions: "",
    types: "",
    aggregations: "",
  };
  if (!c) return f;
  switch (c.kind) {
    case "aws":
      return {
        ...f,
        creds: c.access_key_id ? "given" : "server",
        regions: (c.regions ?? []).join(", "),
        namespaces: (c.namespaces ?? []).join(", "),
        stats: (c.stats ?? []).join(", "),
        keyId: c.access_key_id ?? "",
        role: c.role_arn ?? "",
        externalId: c.external_id ?? "",
      };
    case "gcp":
      return { ...f, creds: c.service_account ? "given" : "server", projects: (c.projects ?? []).join(", "), services: (c.services ?? []).join(", ") };
    case "azure":
      return {
        ...f,
        creds: c.client_secret ? "given" : "server",
        tenant: c.tenant_id ?? "",
        client: c.client_id ?? "",
        subscriptions: (c.subscriptions ?? []).join(", "),
        types: (c.resource_types ?? []).join(", "),
        aggregations: (c.aggregations ?? []).join(", "),
      };
  }
}

/**
 * The integration as it is sent. A secret left empty is the kept one (left out); credentials changed
 * to the server's own remove the kept ones (sent empty). Settings only the API says (endpoints,
 * periods, look-backs) are kept as they were.
 */
function bodyOf(f: Form, was: Cloud | null): CloudIn {
  const common = { interval_s: Number(f.interval) || 300, tags: list(f.tags), lookback_s: was?.lookback_s, max_series: was?.max_series, paused: was?.paused };
  const given = f.creds === "given";
  const secret = (s: string) => (given ? s.trim() || undefined : "");
  switch (f.kind) {
    case "aws": {
      const prior = was?.kind === "aws" ? was : null;
      return {
        ...common,
        kind: "aws",
        regions: list(f.regions),
        namespaces: list(f.namespaces),
        stats: list(f.stats),
        access_key_id: given ? f.keyId.trim() : undefined,
        secret_access_key: secret(f.secret),
        role_arn: f.role.trim() || undefined,
        external_id: f.externalId.trim() || undefined,
        period_s: prior?.period_s,
        endpoint: prior?.endpoint,
        sts_endpoint: prior?.sts_endpoint,
      };
    }
    case "gcp": {
      const prior = was?.kind === "gcp" ? was : null;
      return { ...common, kind: "gcp", projects: list(f.projects), services: list(f.services), service_account: secret(f.key), period_s: prior?.period_s, endpoint: prior?.endpoint };
    }
    case "azure": {
      const prior = was?.kind === "azure" ? was : null;
      return {
        ...common,
        kind: "azure",
        tenant_id: given ? f.tenant.trim() : undefined,
        client_id: f.client.trim() || undefined,
        client_secret: secret(f.key),
        subscriptions: list(f.subscriptions),
        resource_types: list(f.types),
        aggregations: list(f.aggregations),
        period_s: prior?.period_s,
        authority: prior?.authority,
        endpoint: prior?.endpoint,
      };
    }
  }
}

function CloudDialog({ cloud, onClose }: { cloud: Cloud | null; onClose: () => void }) {
  const [f, setF] = React.useState<Form>(() => formOf(cloud));
  const [busy, setBusy] = React.useState(false);
  const [tested, setTested] = React.useState<CloudTest | null>(null);
  const set = (patch: Partial<Form>) => {
    setF((old) => ({ ...old, ...patch }));
    setTested(null);
  };
  // A secret kept is one this integration had, of this cloud, given here.
  const kept = cloud !== null && cloud.kind === f.kind && formOf(cloud).creds === "given";
  const given = f.creds === "given";
  const validName = /^[A-Za-z0-9_.-]{1,64}$/.test(f.name);
  const gcpKey =
    f.key.trim() === "" ||
    (() => {
      try {
        return JSON.parse(f.key)?.type !== undefined;
      } catch {
        return false;
      }
    })();
  const credsOk =
    !given ||
    (f.kind === "aws"
      ? f.keyId.trim() !== "" && (f.secret.trim() !== "" || kept)
      : f.kind === "gcp"
        ? gcpKey && (f.key.trim() !== "" || kept)
        : f.tenant.trim() !== "" && f.client.trim() !== "" && (f.key.trim() !== "" || kept));
  const ok = validName && Number(f.interval) >= 10 && credsOk && (f.kind !== "aws" || list(f.stats).length > 0);
  const save = async () => {
    setBusy(true);
    try {
      await observeApi.putCloud(f.name, bodyOf(f, cloud));
      invalidate("observe/clouds");
      onClose();
    } catch (err) {
      toast({ title: "Couldn't save it", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const test = async () => {
    setBusy(true);
    try {
      setTested(await observeApi.testCloud(f.name || "new", bodyOf(f, cloud)));
    } catch (err) {
      setTested({ ok: false, error: errorMessage(err) });
    } finally {
      setBusy(false);
    }
  };
  const credsOptions = [
    { value: "given", label: f.kind === "aws" ? "Keys given here" : f.kind === "gcp" ? "A service account's key" : "An app's secret" },
    { value: "server", label: "The server's own, where it runs" },
  ];
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={cloud ? `Edit ${cloud.name}` : "Add a cloud"} description="Its secrets stay on the server, sealed: they are sent once, here, and never shown again.">
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (ok) void save();
          }}
        >
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            {cloud ? (
              <TextField label="Cloud" value={WORDS[f.kind]} readOnly />
            ) : (
              <SelectField label="Cloud" value={f.kind} onChange={(k) => set({ ...formOf(null), name: f.name, kind: k as CloudKind })} options={KINDS} />
            )}
            <TextField
              label="Name"
              value={f.name}
              readOnly={cloud !== null}
              onChange={(e) => set({ name: e.target.value })}
              placeholder="prod"
              hint={f.name && !validName ? "Letters, digits, - _ and . only" : undefined}
            />
          </div>
          <div>
            <SelectField label="Credentials" value={f.creds} onChange={(v) => set({ creds: v as Form["creds"] })} options={credsOptions} />
            <p className="field-hint">{given ? NEED[f.kind] : `${WHERE[f.kind]} ${NEED[f.kind]}`}</p>
          </div>
          {f.kind === "aws" && (
            <>
              {given && (
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                  <TextField label="Access key ID" value={f.keyId} onChange={(e) => set({ keyId: e.target.value })} autoComplete="off" spellCheck={false} />
                  <TextField label="Secret access key" type="password" value={f.secret} onChange={(e) => set({ secret: e.target.value })} autoComplete="new-password" hint={kept ? KEPT : undefined} />
                </div>
              )}
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <TextField label="Role to assume (optional)" value={f.role} onChange={(e) => set({ role: e.target.value })} placeholder="arn:aws:iam::123456789012:role/kiln" spellCheck={false} />
                <TextField label="External ID (optional)" value={f.externalId} onChange={(e) => set({ externalId: e.target.value })} spellCheck={false} />
              </div>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <TextField
                  label="Regions"
                  value={f.regions}
                  onChange={(e) => set({ regions: e.target.value })}
                  placeholder="us-east-1, eu-west-1"
                  hint="None: the server's own region."
                  spellCheck={false}
                />
                <TextField
                  label="Namespaces"
                  value={f.namespaces}
                  onChange={(e) => set({ namespaces: e.target.value })}
                  placeholder="AWS/EC2, AWS/Lambda"
                  hint="None: every namespace with metrics."
                  spellCheck={false}
                />
              </div>
              <TextField
                label="Statistics"
                value={f.stats}
                onChange={(e) => set({ stats: e.target.value })}
                hint="With Sum and SampleCount, each metric's own name is its average. Each statistic is a query CloudWatch bills."
                spellCheck={false}
              />
            </>
          )}
          {f.kind === "gcp" && (
            <>
              {given ? (
                <div>
                  <label htmlFor="cloud-key" className="field-label">
                    Service account key (JSON)
                  </label>
                  <div className="field">
                    <textarea
                      id="cloud-key"
                      rows={5}
                      className="font-mono"
                      value={f.key}
                      onChange={(e) => set({ key: e.target.value })}
                      placeholder='{"type": "service_account", ...}'
                      spellCheck={false}
                      autoComplete="off"
                    />
                  </div>
                  {(!gcpKey || kept) && <div className="field-hint">{!gcpKey ? <span className="text-[#e2c4c6]">Not a key file&apos;s JSON.</span> : KEPT}</div>}
                </div>
              ) : null}
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <TextField label="Projects" value={f.projects} onChange={(e) => set({ projects: e.target.value })} placeholder="my-project" hint="None: the credentials' project." spellCheck={false} />
                <TextField
                  label="Services"
                  value={f.services}
                  onChange={(e) => set({ services: e.target.value })}
                  placeholder="compute.googleapis.com, kubernetes.io"
                  hint="Metric type prefixes. None: every type."
                  spellCheck={false}
                />
              </div>
            </>
          )}
          {f.kind === "azure" && (
            <>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                {given && <TextField label="Tenant ID" value={f.tenant} onChange={(e) => set({ tenant: e.target.value })} autoComplete="off" spellCheck={false} />}
                <TextField
                  label={given ? "Client ID" : "Client ID of a user-assigned identity (optional)"}
                  value={f.client}
                  onChange={(e) => set({ client: e.target.value })}
                  autoComplete="off"
                  spellCheck={false}
                />
                {given && <TextField label="Client secret" type="password" value={f.key} onChange={(e) => set({ key: e.target.value })} autoComplete="new-password" hint={kept ? KEPT : undefined} />}
              </div>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <TextField label="Subscriptions" value={f.subscriptions} onChange={(e) => set({ subscriptions: e.target.value })} hint="None: every one the credentials see." spellCheck={false} />
                <TextField
                  label="Resource types"
                  value={f.types}
                  onChange={(e) => set({ types: e.target.value })}
                  placeholder="Microsoft.Compute/virtualMachines"
                  hint="None: every type with metrics."
                  spellCheck={false}
                />
              </div>
              <TextField
                label="Aggregations besides each metric's own"
                value={f.aggregations}
                onChange={(e) => set({ aggregations: e.target.value })}
                placeholder="Maximum, Minimum"
                spellCheck={false}
              />
            </>
          )}
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField
              label="Every (seconds)"
              inputMode="numeric"
              value={f.interval}
              onChange={(e) => set({ interval: e.target.value })}
              hint={Number(f.interval) < 10 ? "10 at the least" : undefined}
            />
            <TextField label="Tags" value={f.tags} onChange={(e) => set({ tags: e.target.value })} placeholder="env:prod, team:infra" hint="Given to every series." spellCheck={false} />
          </div>
          {tested && (
            <div className="field-hint" role="status">
              {tested.ok ? (
                <span>
                  Reads {count(tested.series ?? 0)} series of {count(tested.metrics ?? 0)} metrics, {count(tested.points ?? 0)} points, in {count(tested.calls ?? 0)} calls
                  {tested.over ? `; ${count(tested.over)} more past the limit` : ""}.
                  {tested.sample && tested.sample.length > 0 && <span className="font-mono"> {tested.sample.slice(0, 4).join(", ")}…</span>}
                </span>
              ) : (
                <span className="text-[#e2c4c6]">{tested.error}</span>
              )}
            </div>
          )}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="ghost" disabled={!ok || busy} onClick={() => void test()}>
              Test
            </Button>
            <Button type="submit" disabled={!ok || busy}>
              {cloud ? "Save" : "Add"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
