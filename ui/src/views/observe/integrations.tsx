/**
 * Integrations: where monitors' messages go (Slack, Discord, Microsoft Teams, PagerDuty, Opsgenie,
 * email through an SMTP relay, or any webhook, signed), each named, each mentioned in a message as
 * `@<kind>-<name>`; the clouds whose
 * metrics kiln reads (`./clouds`), and the Prometheus endpoints it scrapes (`./scrapes`). Secrets
 * are kept by the server and never shown again; a
 * test sends a message at once.
 */
import * as React from "react";
import { Plus, Send, Trash2 } from "lucide-react";
import { observeApi, type Integration, type IntegrationIn, type IntegrationKind, type SmtpSecurity } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { CopyButton, PageIntro, SectionHeading, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Clouds } from "./clouds";
import { LogSources } from "./logsources";
import { Scrapes } from "./scrapes";
import { ObserveNav, integrationsRes } from "./parts";

const KINDS: { value: IntegrationKind; label: string; url: string }[] = [
  { value: "slack", label: "Slack", url: "An incoming webhook's URL (hooks.slack.com/services/…)" },
  { value: "discord", label: "Discord", url: "A channel webhook's URL (discord.com/api/webhooks/…)" },
  { value: "teams", label: "Microsoft Teams", url: "A Workflows webhook's URL, or a connector's" },
  { value: "pagerduty", label: "PagerDuty", url: "Events API v2 (events.pagerduty.com unless set)" },
  { value: "opsgenie", label: "Opsgenie", url: "The Alert API (api.opsgenie.com unless set; EU: https://api.eu.opsgenie.com)" },
  { value: "email", label: "Email", url: "" },
  { value: "webhook", label: "Webhook", url: "Any URL: kiln POSTs each notice as JSON" },
];
const SECURITY: { value: SmtpSecurity; label: string }[] = [
  { value: "starttls", label: "STARTTLS (587)" },
  { value: "tls", label: "TLS (465)" },
  { value: "none", label: "None: a relay on its own network (25)" },
];
const list = (s: string) =>
  s
    .split(/[\s,;]+/)
    .map((x) => x.trim())
    .filter(Boolean);
const address = (a: string) => /^[^\s@<>,;"]+@[^\s@<>,;"]+$/.test(a);

/** Where an integration sends, in words. */
function sendsTo(i: Integration): string {
  if (i.kind === "email") return `${(i.to ?? []).join(", ")} via ${i.host ?? ""}`;
  if (i.kind === "opsgenie") return i.url ?? "api.opsgenie.com";
  return i.url ?? "—";
}
const WORDS = Object.fromEntries(KINDS.map((k) => [k.value, k.label])) as Record<IntegrationKind, string>;

export default function Integrations() {
  const [adding, setAdding] = React.useState(false);
  return (
    <div className="container-console page">
      <title>Integrations · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Integrations"
        lead="Where monitors' messages go; the clouds and Prometheus endpoints whose metrics kiln reads; and the logs its machines collect. Mention an integration in a monitor's message as @kind-name: it is told when a group alerts, warns, stops having data, and recovers."
        actions={
          <Button onClick={() => setAdding(true)}>
            <Plus /> Add integration
          </Button>
        }
      />
      <ObserveNav />
      <SectionHeading index="01" label="Notifications" title="Where monitors' messages go" />
      <Loadable label="the integrations" fallback={<TableHold head={<Head />} rows={3} />}>
        <List onAdd={() => setAdding(true)} />
      </Loadable>
      {adding && <AddDialog onClose={() => setAdding(false)} />}
      <Clouds />
      <Scrapes />
      <LogSources />
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th>Name</th>
      <th>Kind</th>
      <th>Sends to</th>
      <th>Mention</th>
      <th />
    </tr>
  );
}

function List({ onAdd }: { onAdd: () => void }) {
  const all = useRes(integrationsRes).data;
  const [removing, setRemoving] = React.useState<Integration | null>(null);
  if (all.length === 0)
    return (
      <p className="empty-line py-16 text-center">
        None yet.{" "}
        <button type="button" className="text-link" onClick={onAdd}>
          Add one
        </button>{" "}
        to send monitors&apos; messages to Slack, Discord, Teams, PagerDuty, Opsgenie, email or a webhook.
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
            {all.map((i) => (
              <tr key={i.name}>
                <td className="text-[13px] text-[#e4e5e7]">{i.name}</td>
                <td>{WORDS[i.kind] ?? i.kind}</td>
                <td className="truncate-cell cell-mono" title={sendsTo(i)}>
                  {sendsTo(i)}
                  {i.kind === "webhook" && <span className="ml-2 text-[#8c8f98]">{i.signed ? "signed" : "unsigned"}</span>}
                </td>
                <td className="cell-mono whitespace-nowrap">
                  @{i.kind}-{i.name} <CopyButton text={`@${i.kind}-${i.name}`} label="Copy the mention" />
                </td>
                <td className="w-1 whitespace-nowrap">
                  <Button
                    variant="ghost"
                    size="xs"
                    onClick={async () => {
                      try {
                        await observeApi.testIntegration(i.name);
                        toast({ title: `A test went to ${i.name}`, tone: "info" });
                      } catch (e) {
                        toast({ title: `Couldn't test ${i.name}`, description: errorMessage(e), tone: "failed" });
                      }
                    }}
                  >
                    <Send /> Test
                  </Button>
                  <Button variant="ghost-danger" size="xs" aria-label={`Remove ${i.name}`} onClick={() => setRemoving(i)}>
                    <Trash2 />
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? ""}?`}
        description="Monitors that mention it send nothing there any more."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (!removing) return;
          try {
            await observeApi.deleteIntegration(removing.name);
            invalidate("observe/integrations");
          } catch (e) {
            toast({ title: "Couldn't remove it", description: errorMessage(e), tone: "failed" });
          }
        }}
      />
    </>
  );
}

function AddDialog({ onClose }: { onClose: () => void }) {
  const [kind, setKind] = React.useState<IntegrationKind>("slack");
  const [name, setName] = React.useState("");
  const [url, setUrl] = React.useState("");
  const [secret, setSecret] = React.useState("");
  const [key, setKey] = React.useState("");
  // An email's recipients, sender and relay.
  const [to, setTo] = React.useState("");
  const [from, setFrom] = React.useState("");
  const [host, setHost] = React.useState("");
  const [port, setPort] = React.useState("");
  const [security, setSecurity] = React.useState<SmtpSecurity>("starttls");
  const [user, setUser] = React.useState("");
  const [password, setPassword] = React.useState("");
  const [ca, setCa] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const hint = KINDS.find((k) => k.value === kind)?.url;
  // A mention ends at a trailing '.' (the end of a sentence): a name may not.
  const validName = /^[a-z0-9_.-]*[a-z0-9_-]$/.test(name);
  const optionalUrl = kind === "pagerduty" || kind === "opsgenie";
  const validUrl = kind === "email" || (optionalUrl ? url === "" || /^https?:\/\/\S+$/.test(url) : /^https?:\/\/\S+$/.test(url));
  const recipients = list(to);
  const validEmail =
    kind !== "email" ||
    (recipients.length > 0 &&
      recipients.length <= 50 &&
      recipients.every(address) &&
      address(from.trim()) &&
      /^[^\s/]+$/.test(host.trim()) &&
      (port === "" || (/^\d+$/.test(port) && Number(port) > 0 && Number(port) < 65536)));
  const ok = validName && validUrl && validEmail && (!optionalUrl || key.trim() !== "");
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title="Add an integration" description="Its secrets stay on the server: they are sent once, here, and never shown again.">
        <form
          className="space-y-4"
          onSubmit={async (e) => {
            e.preventDefault();
            if (!ok) return;
            const body: IntegrationIn =
              kind === "pagerduty"
                ? { kind, routing_key: key.trim(), ...(url ? { url } : {}) }
                : kind === "opsgenie"
                  ? { kind, api_key: key.trim(), ...(url ? { url } : {}) }
                  : kind === "email"
                    ? {
                        kind,
                        to: recipients,
                        from: from.trim(),
                        host: host.trim(),
                        security,
                        ...(port ? { port: Number(port) } : {}),
                        ...(user.trim() ? { username: user.trim(), password } : {}),
                        ...(ca.trim() ? { ca: ca.trim() } : {}),
                      }
                    : kind === "webhook"
                      ? { kind, url, ...(secret ? { secret } : {}) }
                      : { kind, url };
            setBusy(true);
            try {
              await observeApi.putIntegration(name, body);
              invalidate("observe/integrations");
              onClose();
            } catch (err) {
              toast({ title: "Couldn't add it", description: errorMessage(err), tone: "failed" });
            } finally {
              setBusy(false);
            }
          }}
        >
          <div className="grid gap-3 sm:grid-cols-2">
            <SelectField label="Kind" value={kind} onChange={(k) => setKind(k as IntegrationKind)} options={KINDS} />
            <TextField
              label="Name"
              value={name}
              onChange={(e) => setName(e.target.value.toLowerCase())}
              placeholder="oncall"
              hint={name && !validName ? "Letters, digits, - _ and . only, not ending in ." : name ? `Mentioned as @${kind}-${name}` : undefined}
            />
          </div>
          {kind === "pagerduty" && <TextField label="Routing key" value={key} onChange={(e) => setKey(e.target.value)} autoComplete="off" />}
          {kind === "opsgenie" && <TextField label="API key" value={key} onChange={(e) => setKey(e.target.value)} hint="An API integration's key." autoComplete="off" />}
          {kind === "email" ? (
            <>
              <TextField
                label="To"
                value={to}
                onChange={(e) => setTo(e.target.value)}
                placeholder="oncall@example.com, sre@example.com"
                hint={to && !recipients.every(address) ? "Addresses as local@domain, separated by commas" : "Up to 50 addresses."}
                autoComplete="off"
                spellCheck={false}
              />
              <div className="grid gap-3 sm:grid-cols-2">
                <TextField label="From" value={from} onChange={(e) => setFrom(e.target.value)} placeholder="kiln@example.com" autoComplete="off" spellCheck={false} />
                <TextField label="SMTP relay" value={host} onChange={(e) => setHost(e.target.value)} placeholder="smtp.example.com" autoComplete="off" spellCheck={false} />
              </div>
              <div className="grid gap-3 sm:grid-cols-2">
                <SelectField label="Security" value={security} onChange={(v) => setSecurity(v as SmtpSecurity)} options={SECURITY} />
                <TextField
                  label="Port (optional)"
                  inputMode="numeric"
                  value={port}
                  onChange={(e) => setPort(e.target.value)}
                  placeholder={security === "starttls" ? "587" : security === "tls" ? "465" : "25"}
                />
              </div>
              <div className="grid gap-3 sm:grid-cols-2">
                <TextField label="Username (optional)" value={user} onChange={(e) => setUser(e.target.value)} autoComplete="off" spellCheck={false} />
                <TextField label="Password" type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" disabled={user.trim() === ""} />
              </div>
              <div>
                <label htmlFor="integration-ca" className="field-label">
                  The relay&apos;s CA (optional)
                </label>
                <div className="field">
                  <textarea id="integration-ca" rows={3} className="font-mono" value={ca} onChange={(e) => setCa(e.target.value)} placeholder="-----BEGIN CERTIFICATE-----" spellCheck={false} />
                </div>
                <div className="field-hint">When its certificate is not a public one.</div>
              </div>
            </>
          ) : (
            <TextField
              label={kind === "pagerduty" ? "Events URL (optional)" : kind === "opsgenie" ? "API URL (optional)" : "URL"}
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              placeholder="https://"
              hint={hint}
              autoComplete="off"
            />
          )}
          {kind === "webhook" && (
            <TextField
              label="Signing secret (optional)"
              value={secret}
              onChange={(e) => setSecret(e.target.value)}
              hint="Each request is signed with it as GitHub signs: X-Kiln-Signature: sha256=<HMAC of the body>."
              autoComplete="off"
            />
          )}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={!ok || busy}>
              Add
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
