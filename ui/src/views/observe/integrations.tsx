/**
 * Integrations: where monitors' messages go (Slack, Discord, Microsoft Teams, PagerDuty, or any
 * webhook, signed), each named, each mentioned in a message as `@<kind>-<name>`; and the clouds
 * whose metrics kiln reads (`./clouds`). Secrets are kept by the server and never shown again; a
 * test sends a message at once.
 */
import * as React from "react";
import { Plus, Send, Trash2 } from "lucide-react";
import { observeApi, type Integration, type IntegrationIn, type IntegrationKind } from "@/api/observe";
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
import { ObserveNav, integrationsRes } from "./parts";

const KINDS: { value: IntegrationKind; label: string; url: string }[] = [
  { value: "slack", label: "Slack", url: "An incoming webhook's URL (hooks.slack.com/services/…)" },
  { value: "discord", label: "Discord", url: "A channel webhook's URL (discord.com/api/webhooks/…)" },
  { value: "teams", label: "Microsoft Teams", url: "A Workflows webhook's URL, or a connector's" },
  { value: "pagerduty", label: "PagerDuty", url: "Events API v2 (events.pagerduty.com unless set)" },
  { value: "webhook", label: "Webhook", url: "Any URL: kiln POSTs each notice as JSON" },
];
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
        lead="Where monitors' messages go, and the clouds whose metrics kiln reads. Mention one in a monitor's message as @kind-name: it is told when a group alerts, warns, stops having data, and recovers."
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
        to send monitors&apos; messages to Slack, Discord, Teams, PagerDuty or a webhook.
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
                <td className="truncate-cell cell-mono">
                  {i.url ?? "—"}
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
  const [busy, setBusy] = React.useState(false);
  const hint = KINDS.find((k) => k.value === kind)?.url;
  // A mention ends at a trailing '.' (the end of a sentence): a name may not.
  const validName = /^[a-z0-9_.-]*[a-z0-9_-]$/.test(name);
  const validUrl = kind === "pagerduty" ? url === "" || /^https?:\/\/\S+$/.test(url) : /^https?:\/\/\S+$/.test(url);
  const ok = validName && validUrl && (kind !== "pagerduty" || key.trim() !== "");
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title="Add an integration" description="Its secrets stay on the server: they are sent once, here, and never shown again.">
        <form
          className="space-y-4"
          onSubmit={async (e) => {
            e.preventDefault();
            if (!ok) return;
            const body: IntegrationIn =
              kind === "pagerduty" ? { kind, routing_key: key.trim(), ...(url ? { url } : {}) } : kind === "webhook" ? { kind, url, ...(secret ? { secret } : {}) } : { kind, url };
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
          <TextField label={kind === "pagerduty" ? "Events URL (optional)" : "URL"} value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://" hint={hint} autoComplete="off" />
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
