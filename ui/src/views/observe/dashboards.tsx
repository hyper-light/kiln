/** Dashboards: kiln's own (its hosts, CI and CD), then yours; a new one is a click away. */
import * as React from "react";
import { Plus } from "lucide-react";
import { Link, useNavigate } from "@/lib/router";
import { observeApi } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { ObserveNav, dashboardsRes } from "./parts";

export default function Dashboards() {
  const navigate = useNavigate();
  const [busy, setBusy] = React.useState(false);
  return (
    <div className="container-console page">
      <title>Dashboards · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Dashboards"
        lead="Widgets of metrics, logs, events and monitors over one time range. kiln's own show its hosts, its CI and its CD; copy one to change it, or start your own."
        actions={
          <Button
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              try {
                const d = await observeApi.createDashboard({ title: "New dashboard", description: "", template_variables: [], widgets: [] });
                invalidate("observe/dashboards");
                navigate(`/observe/dashboards/${encodeURIComponent(d.id)}`);
              } catch (e) {
                toast({ title: "Couldn't make a dashboard", description: errorMessage(e), tone: "failed" });
              } finally {
                setBusy(false);
              }
            }}
          >
            <Plus /> New dashboard
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the dashboards" fallback={<TableHold head={<Head />} rows={5} />}>
        <List />
      </Loadable>
    </div>
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
