/**
 * A new environment apps are released to (docs/deploy-api.md §4): its name, its provider (KilnCD,
 * or one of kiln's Argo CD integrations), the release repository its releases are committed to,
 * and the clusters it deploys to. Its gates are set on its page once it's made.
 */
import * as React from "react";
import { Plus } from "lucide-react";

import { api } from "@/api/client";
import { cdApi } from "@/api/cd";
import { deployApi, releaseEnvHref } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Loadable } from "@/components/ui/loading";
import { SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { useNavigate } from "@/lib/router";

/** A DNS label, as Kubernetes names things. */
const LABEL = /^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$/;

function Form({ onDone }: { onDone: () => void }) {
  const integrations = useSuspenseResource("deploy/integrations", (s) => api.integrations(s)).data.integrations.filter((i) => i.kind === "argocd");
  const clusters = useSuspenseResource("cd/clusters", (s) => cdApi.clusters(s)).data.clusters;
  const navigate = useNavigate();
  const [name, setName] = React.useState("");
  const [provider, setProvider] = React.useState("kilncd");
  const [repo, setRepo] = React.useState("kiln/releases");
  const [branch, setBranch] = React.useState("main");
  const [root, setRoot] = React.useState("");
  const [picked, setPicked] = React.useState<string[]>([]);
  const [argoClusters, setArgoClusters] = React.useState("in-cluster");
  const [busy, setBusy] = React.useState(false);
  const argo = provider.startsWith("argocd/");
  const clean = name.trim();
  const wrong =
    clean && !LABEL.test(clean) ? "Lowercase letters, digits and '-', as Kubernetes names things." : !/^[^/\s]+\/[^/\s]+$/.test(repo.trim()) ? "The release repository is owner/name." : null;
  const make = async () => {
    const kubernetes = argo
      ? argoClusters
          .split(",")
          .map((c) => c.trim())
          .filter(Boolean)
      : picked;
    setBusy(true);
    try {
      const e = await deployApi.createEnvironment({ name: clean, provider, release_repo: { repo: repo.trim(), branch: branch.trim() || "main", root: root.trim() }, targets: { kubernetes } });
      toast({ title: `${e.name} is made`, description: "Set its gates on its page; register apps in it from theirs.", tone: "succeeded", href: releaseEnvHref(e.name) });
      invalidate("deploy/");
      onDone();
      navigate(releaseEnvHref(e.name));
    } catch (err) {
      toast({ title: "Not made", description: errorMessage(err), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="rel-dialog">
      <TextField label="Name" placeholder="staging" value={name} autoFocus spellCheck={false} onChange={(e) => setName(e.target.value)} />
      <SelectField
        label="Provider"
        value={provider}
        onChange={setProvider}
        options={[{ value: "kilncd", label: "KilnCD (kiln's own)" }, ...integrations.map((i) => ({ value: `argocd/${i.name}`, label: `Argo CD ${i.name}` }))]}
      />
      <div className="cd-new-grid">
        <TextField label="Release repository" value={repo} spellCheck={false} onChange={(e) => setRepo(e.target.value)} hint="One of kiln's own: made if it isn't there." />
        <TextField label="Branch" value={branch} spellCheck={false} onChange={(e) => setBranch(e.target.value)} />
        <TextField label="Folder" placeholder="(the top)" value={root} spellCheck={false} onChange={(e) => setRoot(e.target.value)} />
      </div>
      {argo ? (
        <TextField label="Clusters" value={argoClusters} spellCheck={false} onChange={(e) => setArgoClusters(e.target.value)} hint="As Argo CD names them, separated by commas." />
      ) : clusters.length ? (
        <fieldset className="rel-candidates">
          <legend className="field-label">Clusters (none: the one kiln runs in)</legend>
          {clusters.map((c) => (
            <label key={c.name}>
              <input type="checkbox" className="check" checked={picked.includes(c.name)} onChange={(e) => setPicked((xs) => (e.target.checked ? [...xs, c.name] : xs.filter((x) => x !== c.name)))} />
              <span>
                {c.name} <span className="rel-quiet">{c.connected ? `· ${c.version ?? "connected"}` : "· not connected"}</span>
              </span>
            </label>
          ))}
        </fieldset>
      ) : (
        <p className="rel-quiet">KilnCD deploys to the cluster kiln runs in.</p>
      )}
      {wrong && <p className="rel-msg">{wrong}</p>}
      <div className="rel-dialog-actions">
        <Button variant="ghost" onClick={onDone}>
          Close
        </Button>
        <Button disabled={busy || !clean || !!wrong} onClick={() => void make()}>
          <Plus /> Make {clean || "it"}
        </Button>
      </div>
    </div>
  );
}

/** New environment, as a button and its dialog. */
export function NewEnvironment() {
  const [open, setOpen] = React.useState(false);
  return (
    <>
      <Button size="sm" variant="secondary" onClick={() => setOpen(true)}>
        <Plus /> New environment
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent eyebrow="Releases" title="A new environment" description="Where apps are released to. Its gates are set on its page once it's made.">
          {open && (
            <Loadable label="the providers and clusters">
              <Form onDone={() => setOpen(false)} />
            </Loadable>
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}
