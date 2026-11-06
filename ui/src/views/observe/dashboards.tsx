/** Dashboards: kiln's own (its hosts, CI and CD), then yours; a new one is a click away, and one of
 * Datadog's is imported from its JSON. */
import * as React from "react";
import { Plus, Upload } from "lucide-react";
import { Link, useNavigate } from "@/lib/router";
import { observeApi } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useAction } from "@/lib/action";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { HeldFields, ObserveNav, dashboardsRes, useOpening } from "./parts";

export default function Dashboards() {
  const navigate = useNavigate();
  const [importing, setImporting] = React.useState(false);
  const [make, making] = useAction(async () => {
    const d = await observeApi.createDashboard({ title: "New dashboard", description: "", template_variables: [], widgets: [] });
    invalidate("observe/dashboards");
    await navigate(`/observe/dashboards/${encodeURIComponent(d.id)}`);
  }, "Couldn't make a dashboard");
  return (
    <div className="container-console page">
      <title>Dashboards · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Dashboards"
        lead="Widgets of metrics, logs, events and monitors over one time range. kiln's own show its hosts, its CI and its CD; copy one to change it, start your own, or import one of Datadog's."
        actions={
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="secondary" onClick={() => setImporting(true)}>
              <Upload /> Import from Datadog
            </Button>
            <Button onClick={make} pending={making}>
              <Plus /> New dashboard
            </Button>
          </div>
        }
      />
      {importing && <ImportDialog onClose={() => setImporting(false)} />}
      <ObserveNav />
      <Loadable label="the dashboards" fallback={<TableHold head={<Head />} rows={5} />}>
        <List />
      </Loadable>
    </div>
  );
}

/** A Datadog dashboard's JSON (pasted, or its file), made a dashboard of kiln's and opened; what of
 * its widgets kiln draws otherwise, or keeps as notes, said. */
function ImportDialog({ onClose }: { onClose: () => void }) {
  const navigate = useNavigate();
  const file = React.useRef<HTMLInputElement>(null);
  const [text, setText] = React.useState("");
  const opening = useOpening(text);
  const [error, setError] = React.useState<string | null>(null);
  const [submit, submitting] = useAction(
    async () => {
      let body: unknown;
      try {
        body = JSON.parse(text);
      } catch (e) {
        setError(`Not JSON: ${errorMessage(e)}`);
        return;
      }
      setError(null);
      const { dashboard, report } = await observeApi.importDashboard(body);
      invalidate("observe/dashboards");
      const apart = report.otherwise.length + report.not_drawn.length;
      toast({
        title: `Imported ${dashboard.title}`,
        description: apart
          ? `${report.drawn} of ${report.widgets} widgets drawn as Datadog draws them; ${report.otherwise.length} drawn otherwise, ${report.not_drawn.length} kept as notes or left out.`
          : `All ${report.widgets} widgets drawn as Datadog draws them.`,
        tone: "succeeded",
      });
      if (opening.still(text)) await navigate(`/observe/dashboards/${encodeURIComponent(dashboard.id)}`);
    },
    (e) => setError(errorMessage(e)),
  );
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title="Import a Datadog dashboard" description="Its JSON, as Datadog exports it (a dashboard's settings, Export dashboard JSON) or as its API gives it.">
        <form
          ref={opening.ref}
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <HeldFields held={submitting}>
            <div>
              <label htmlFor="datadog-json" className="field-label">
                Dashboard JSON
              </label>
              <div className="field">
                <textarea
                  id="datadog-json"
                  rows={10}
                  className="font-mono"
                  value={text}
                  onChange={(e) => setText(e.target.value)}
                  placeholder='{ "title": "…", "widgets": [ … ] }'
                  spellCheck={false}
                />
              </div>
              <p className="field-hint">
                Or{" "}
                <button type="button" className="text-link" onClick={() => file.current?.click()}>
                  choose its file
                </button>
                .
              </p>
            </div>
            <input
              ref={file}
              type="file"
              accept="application/json,.json"
              className="sr-only"
              tabIndex={-1}
              aria-hidden="true"
              onChange={(e) => {
                const f = e.target.files?.[0];
                e.target.value = "";
                if (f) void f.text().then(setText);
              }}
            />
            {error && (
              <p role="alert" className="text-[12.5px] break-words text-[#e2c4c6]">
                {error}
              </p>
            )}
          </HeldFields>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={!text.trim()} pending={submitting}>
              Import
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function Head() {
  return (
    <tr>
      <th>Dashboard</th>
      <th className="num">Widgets</th>
      <th>Changed</th>
    </tr>
  );
}

function List() {
  const all = useRes(dashboardsRes).data;
  return (
    <div className="table-wrap">
      <table className="data-table">
        <thead>
          <Head />
        </thead>
        <tbody>
          {all.map((d) => (
            <tr key={d.id}>
              <td>
                <Link to={`/observe/dashboards/${encodeURIComponent(d.id)}`} className="text-[13px] text-[#e4e5e7] hover:text-white">
                  {d.title}
                </Link>
                {d.builtin && <span className="chip ml-2 align-middle">kiln</span>}
                {d.description && <p className="mt-0.5 text-[12px] text-[#8c8f98]">{d.description}</p>}
              </td>
              <td className="num cell-mono">{d.widgets}</td>
              <td className="cell-mono">{d.builtin ? "—" : new Date(d.modified).toLocaleString()}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
