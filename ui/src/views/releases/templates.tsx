/**
 * Templates (docs/deploy-api.md §3.2): what apps are made from. Each is the package its apps start
 * with, the namespace they run in, the checks their releases pass, the properties they may have,
 * the environments they may go to, and the source repository and registries kiln makes for each.
 * Here: every template with the apps made from it, a new one, and one changed as YAML or deleted.
 */
import * as React from "react";
import { Link, useNavigate, useParams } from "@/lib/router";
import { Plus } from "lucide-react";

import { boardHref, checkName, deployApi, packageWords, releaseAppHref, templateHref, templatesHref, type App, type Template } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, EmptyState, Facts, Panel } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { appsRes, DEPLOY_EVENTS, templatesRes } from "./index";
import { toYaml, yamlMap, YamlEditor } from "./yaml";
import "@/views/deployments/deployments.css";
import "./releases.css";

/** What a new template starts as. */
const SKELETON = `name: service
description: A web service on kiln-base
package:
  kind: helm
  repo: oci://ghcr.io/acme/charts
  chart: "{app}"
namespace: apps-{app}
checks: [no-snapshots]
environments: [staging, production]
`;

/** A template as people read it, line by line. */
function templateFacts(t: Template) {
  const repo = t.repository;
  return [
    { label: "Package", value: t.package ? <code>{packageWords(t.package)}</code> : "each app's own" },
    { label: "Namespace", value: <code>{t.namespace ?? "{app}"}</code> },
    { label: "Checks", value: t.checks?.length ? t.checks.map(checkName).join(", ") : "none" },
    {
      label: "Properties",
      value: Object.keys(t.properties ?? {}).length
        ? Object.entries(t.properties ?? {})
            .map(([k, vs]) => `${k}: ${vs.join(" | ")}`)
            .join("; ")
        : "none",
    },
    { label: "Environments", value: t.environments?.length ? t.environments.join(", ") : "any" },
    {
      label: "Source repository",
      value: repo ? (
        <>
          <code>{repo.name}</code>
          {repo.host && repo.host !== "kiln" ? ` on ${repo.host}` : " on kiln's git server"}
          {repo.starter ? `, its first commit from ${repo.starter}${repo.starter_ref ? `@${repo.starter_ref}` : ""}` : ""}
        </>
      ) : (
        "none made"
      ),
    },
    { label: "Registries", value: t.registry ? `${t.registry.name} on ${t.registry.at === "kiln" ? "kiln's registry" : t.registry.at}` : "none made" },
  ];
}

/** The apps made from a template, each a link. */
function MadeFrom({ apps }: { apps: App[] }) {
  if (!apps.length) return <span className="rel-quiet">none yet</span>;
  return (
    <>
      {apps.map((a, i) => (
        <React.Fragment key={a.name}>
          {i > 0 && ", "}
          <Link to={releaseAppHref(a.name)}>{a.name}</Link>
        </React.Fragment>
      ))}
    </>
  );
}

/** A new template, written as YAML. */
function NewTemplate() {
  const navigate = useNavigate();
  const [open, setOpen] = React.useState(false);
  const [text, setText] = React.useState(SKELETON);
  const [busy, setBusy] = React.useState(false);
  const read = yamlMap(text);
  const wrong = read.error ?? (typeof read.value?.name === "string" && read.value.name.trim() ? null : "It needs a name.");
  const make = async () => {
    if (!read.value) return;
    setBusy(true);
    try {
      const t = await deployApi.createTemplate(read.value as unknown as Template);
      toast({ title: `Template ${t.name} is made`, tone: "succeeded", href: templateHref(t.name) });
      invalidate("deploy/");
      setOpen(false);
      navigate(templateHref(t.name));
    } catch (e) {
      toast({ title: "Not made", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <Button size="sm" onClick={() => setOpen(true)}>
        <Plus /> New template
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent eyebrow="Templates" title="New template" description="{app} and {project} are each app's own. One that makes source repositories is made by someone who may add them.">
          <div className="rel-dialog">
            <YamlEditor value={text} onChange={setText} label="The template, as YAML" />
            {wrong && <p className="rel-msg">{wrong}</p>}
            <div className="rel-dialog-actions">
              <Button variant="ghost" onClick={() => setOpen(false)}>
                Close
              </Button>
              <Button disabled={busy || !!wrong} onClick={() => void make()}>
                Make it
              </Button>
            </div>
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}

function TemplateList() {
  const templates = useRes(templatesRes).data;
  const apps = useRes(appsRes).data;
  if (!templates.length) {
    return (
      <EmptyState title="No templates yet" action={<NewTemplate />}>
        A template is what apps are made from: the package they start with, where they run, and what kiln makes for each. Make one here or with <code>kiln app template-add &lt;file&gt;</code>.
      </EmptyState>
    );
  }
  return (
    <div className="rel-stack">
      {templates.map((t) => (
        <Panel key={t.name} eyebrow="Template" title={<Link to={templateHref(t.name)}>{t.name}</Link>} description={t.description}>
          <Facts items={[...templateFacts(t), { label: "Apps made from it", value: <MadeFrom apps={apps.filter((a) => a.template === t.name)} /> }]} />
        </Panel>
      ))}
    </div>
  );
}

export default function Templates() {
  return (
    <div className="container-console page cd-page">
      <title>Templates · Releases · kiln</title>
      <header className="cd-head">
        <Crumbs index={2} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, { label: "Templates" }]} />
        <div className="rel-head-row">
          <h1 className="cd-title">Templates</h1>
          <div className="rel-head-actions">
            <NewTemplate />
          </div>
        </div>
        <p className="cd-note">What apps are made from: an app made from one starts with its package, runs in its namespace, passes its checks and goes only where it lets it.</p>
      </header>
      <Loadable label="the templates">
        <TemplateList />
      </Loadable>
    </div>
  );
}

/** A template changed as YAML, saved whole; or deleted, once no app is made from it. */
function Edit({ t, using }: { t: Template; using: App[] }) {
  const navigate = useNavigate();
  const [text, setText] = React.useState(() => toYaml(t));
  const [busy, setBusy] = React.useState(false);
  const [deleting, setDeleting] = React.useState(false);
  const read = yamlMap(text);
  const wrong = read.error ?? (read.value?.name !== t.name ? `Its name stays ${t.name}: another name is another template.` : null);
  const save = async () => {
    if (!read.value) return;
    setBusy(true);
    try {
      await deployApi.putTemplate(read.value as unknown as Template);
      toast({ title: `${t.name} is saved`, tone: "succeeded" });
      invalidate("deploy/");
    } catch (e) {
      toast({ title: "Not saved", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  const remove = async () => {
    try {
      await deployApi.deleteTemplate(t.name);
      toast({ title: `${t.name} is deleted`, tone: "succeeded" });
      invalidate("deploy/");
      navigate(templatesHref);
    } catch (e) {
      toast({ title: "Not deleted", description: errorMessage(e), tone: "failed" });
    }
  };
  return (
    <Panel
      eyebrow="Edit"
      title="The template, as YAML"
      description="Saved whole: what isn't in it is gone. One that makes source repositories is saved by someone who may add them."
      actions={
        <Button size="sm" variant="ghost-danger" disabled={using.length > 0} title={using.length ? "Apps are made from it" : undefined} onClick={() => setDeleting(true)}>
          Delete
        </Button>
      }
    >
      <YamlEditor value={text} onChange={setText} label={`${t.name}, as YAML`} />
      {wrong && <p className="rel-msg">{wrong}</p>}
      {using.length > 0 && <p className="rel-quiet">It can't be deleted while apps are made from it.</p>}
      <div className="rel-settings-actions">
        <Button size="sm" disabled={busy || !!wrong} onClick={() => void save()}>
          Save
        </Button>
      </div>
      <ConfirmDialog
        open={deleting}
        onOpenChange={setDeleting}
        title={`Delete ${t.name}?`}
        description="No app is made from it. It goes for good."
        confirmLabel="Delete the template"
        danger
        onConfirm={remove}
      />
    </Panel>
  );
}

function TemplateView({ name }: { name: string }) {
  const t = useSuspenseResource(`deploy/template/${name}`, (s) => deployApi.template(name, s), { on: DEPLOY_EVENTS }).data;
  const using = useRes(appsRes).data.filter((a) => a.template === t.name);
  return (
    <>
      <header className="cd-head">
        <Crumbs index={2} items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, { label: "Templates", to: templatesHref }, { label: t.name }]} />
        <h1 className="cd-title">{t.name}</h1>
        {t.description && <p className="cd-note">{t.description}</p>}
      </header>
      <div className="rel-stack">
        <Panel eyebrow="Template" title="What its apps start with">
          <Facts items={[...templateFacts(t), { label: "Apps made from it", value: <MadeFrom apps={using} /> }]} />
        </Panel>
        <Edit key={JSON.stringify(t)} t={t} using={using} />
      </div>
    </>
  );
}

export function TemplatePage() {
  const name = useParams().name ?? "";
  return (
    <div className="container-console page cd-page">
      <title>{`${name} · Templates · kiln`}</title>
      <Loadable label="the template">
        <TemplateView name={name} />
      </Loadable>
    </div>
  );
}
