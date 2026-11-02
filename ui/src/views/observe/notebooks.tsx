/**
 * Notebooks (docs/observe.md §11.1), Datadog's: documents of text and graphs over a time, each its
 * type (an investigation, a runbook, a postmortem, documentation, a report); templates apart.
 * Found by a part of the name or the author, by type; a new one made here, a postmortem from an
 * incident's page.
 */
import * as React from "react";
import { Plus } from "lucide-react";
import { observeApi, type NotebookType } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SearchField, SelectField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useNavigate } from "@/lib/router";
import { ObserveNav, useParam, useParams } from "./parts";

export const TYPE_WORDS: Record<NotebookType, string> = { postmortem: "Postmortem", runbook: "Runbook", investigation: "Investigation", documentation: "Documentation", report: "Report" };
const TYPES = Object.keys(TYPE_WORDS) as NotebookType[];

type Listed = Awaited<ReturnType<typeof observeApi.notebooks>>;
const notebooksRes = (type: string, templates: boolean): Res<Listed> => ({
  key: `observe/notebooks/list/${type}/${templates}`,
  fetch: (s) => observeApi.notebooks({ type: (type || undefined) as NotebookType | undefined, is_template: templates, count: 1000 }, s),
  opts: { pollMs: 60_000 },
});

const seconds = (rfc3339: string) => Date.parse(rfc3339) / 1000;

export default function Notebooks() {
  const [type] = useParam("type", "");
  const [show] = useParam("show", "notebooks");
  const set = useParams();
  const [filter, setFilter] = React.useState("");
  const templates = show === "templates";
  const navigate = useNavigate();
  const [busy, setBusy] = React.useState(false);
  const make = async () => {
    setBusy(true);
    try {
      const n = await observeApi.createNotebook({
        name: templates ? "Untitled template" : "Untitled notebook",
        cells: [{ type: "notebook_cells", attributes: { definition: { type: "markdown", text: "" } } }],
        time: { live_span: "1h" },
        metadata: { is_template: templates, type: (type || null) as NotebookType | null },
      });
      invalidate("observe/notebooks");
      navigate(`/observe/notebooks/${n.id}?edit=1`);
    } catch (e) {
      toast({ title: "Couldn't make it", description: errorMessage(e), tone: "failed" });
      setBusy(false);
    }
  };
  return (
    <div className="container-console page">
      <title>Notebooks · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Notebooks"
        lead="Documents of text and graphs over a time: investigations, runbooks, postmortems, reports. A postmortem is written from an incident's page, of a template kept here."
        actions={
          <Button onClick={() => void make()} disabled={busy}>
            <Plus /> {templates ? "New template" : "New notebook"}
          </Button>
        }
      />
      <ObserveNav />
      <div className="mb-4 flex flex-wrap items-end gap-3">
        <Segmented
          label="Show"
          value={templates ? "templates" : "notebooks"}
          onChange={(v) => set({ show: v === "notebooks" ? null : v })}
          options={[
            { value: "notebooks", label: "Notebooks" },
            { value: "templates", label: "Templates" },
          ]}
        />
        <SelectField
          label="Type"
          hideLabel
          value={type}
          onChange={(v) => set({ type: v || null })}
          options={[{ value: "", label: "Any type" }, ...TYPES.map((t) => ({ value: t, label: TYPE_WORDS[t] }))]}
          className="min-w-[10rem]"
        />
        <SearchField label="Find a notebook" value={filter} onChange={setFilter} placeholder="A part of its name or its author" className="w-full sm:ml-auto sm:w-72" />
      </div>
      <Loadable key={`${type}/${templates}`} label="the notebooks" fallback={<TableHold head={<Head />} rows={5} />}>
        <List type={type} templates={templates} filter={filter} />
      </Loadable>
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th>Notebook</th>
      <th className="hidden sm:table-cell">Type</th>
      <th className="hidden md:table-cell">Author</th>
      <th className="hidden text-right sm:table-cell">Changed</th>
    </tr>
  );
}

function List({ type, templates, filter }: { type: string; templates: boolean; filter: string }) {
  const all = useRes(notebooksRes(type, templates)).data.data;
  const f = filter.trim().toLowerCase();
  const shown = f ? all.filter((n) => n.attributes.name.toLowerCase().includes(f) || n.attributes.author.handle.toLowerCase().includes(f)) : all;
  if (all.length === 0)
    return (
      <p className="empty-line py-14 text-center">
        {templates ? "No templates yet: a postmortem is written of kiln's own until one is made here." : type ? "No notebook of that type." : "No notebooks yet."}
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
            {shown.map((n) => {
              const a = n.attributes;
              const kind = a.metadata.type ? TYPE_WORDS[a.metadata.type] : "—";
              return (
                <tr key={n.id}>
                  <td className="truncate-cell">
                    <Link to={`/observe/notebooks/${n.id}`} className="text-[13px] text-[#e4e5e7] hover:text-white">
                      {a.name || "Untitled"}
                    </Link>
                    <p className="truncate text-[11px] text-[#8c8f98] md:hidden">
                      <span className="sm:hidden">{kind} · </span>
                      {a.author.handle}
                      <span className="sm:hidden"> · changed {relativeTime(seconds(a.modified))}</span>
                    </p>
                  </td>
                  <td className="hidden text-[12px] sm:table-cell">{a.metadata.type ? <span className="chip">{kind.toLowerCase()}</span> : <span className="text-[#8c8f98]">—</span>}</td>
                  <td className="hidden text-[12px] md:table-cell">{a.author.handle}</td>
                  <td className="hidden text-right text-[12px] text-[#8c8f98] sm:table-cell">{relativeTime(seconds(a.modified))}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {shown.length === 0 && <p className="empty-line py-10 text-center">No notebook matches.</p>}
    </>
  );
}
