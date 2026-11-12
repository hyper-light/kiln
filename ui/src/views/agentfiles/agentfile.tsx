/**
 * One of your Agentfiles (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 31; docs/api.md,
 * "Agentfiles"): its text in the editor, checked by Shards as it's typed, and saved (each new text a
 * version, Shards' refusal said here and nothing kept); renamed; each version read, and any made
 * current again (as a new version: nothing is lost); deleted with its versions, after a question
 * asked in place. Everything in the page's flow. One that isn't there (deleted, another's, a wrong
 * link) says so.
 */
import * as React from "react";
import { agentfileRes, agentfilesApi, agentfileVersionsRes } from "@/api/agentfiles";
import type { Agentfile, AgentfileSaved } from "@/api/wire";
import { Button } from "@/components/ui/button";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, Crumbs, EmptyState, TextField } from "@/components/ui/misc";
import { useAction, useActions } from "@/lib/action";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useNavigate, useParams } from "@/lib/router";
import { keptWords, restoredWords } from "./lint";
import { named } from "./names";
import { AgentfileRead, AgentfileText } from "./text";
import "./agentfiles.css";

export default function AgentfilePage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page af-page">
      <Loadable label="the Agentfile" fallback={<AgentfileHold />}>
        <AgentfileOrNone key={id} id={id} />
      </Loadable>
    </div>
  );
}

/** The way back: Agents, then the Agentfiles. */
function AgentfileCrumbs({ name }: { name?: string }) {
  return <Crumbs index={5} items={[{ label: "Agents", to: "/agents" }, { label: "Agentfiles", to: "/agents/agentfiles" }, ...(name ? [{ label: name }] : [])]} />;
}

function AgentfileHold() {
  return (
    <>
      <title>Agentfile · kiln</title>
      <AgentfileCrumbs />
      <Hold height={420} label="Loading the Agentfile" />
    </>
  );
}

function AgentfileOrNone({ id }: { id: string }) {
  if (useRes(agentfileRes(id)).data === "gone") return <NoAgentfile />;
  return <AgentfileEditor id={id} />;
}

function NoAgentfile() {
  return (
    <>
      <title>No Agentfile · kiln</title>
      <AgentfileCrumbs />
      <EmptyState
        title="No Agentfile here"
        action={
          <Button asChild size="sm" variant="secondary">
            <Link to="/agents/agentfiles">Agentfiles</Link>
          </Button>
        }
      >
        It was deleted, it isn&rsquo;t yours, or the link is wrong.
      </EmptyState>
    </>
  );
}

/** The kept name and text the editor started from: changes are measured against them. */
interface Base {
  version: number;
  name: string;
  text: string;
}

const baseOf = (a: Agentfile): Base => ({ version: a.version, name: a.name, text: a.text });

function AgentfileEditor({ id }: { id: string }) {
  const res = useRes(agentfileRes(id));
  const kept = res.data;
  const [base, setBase] = React.useState<Base | null>(() => (kept === "gone" ? null : baseOf(kept)));
  const [name, setName] = React.useState(base?.name ?? "");
  const [text, setText] = React.useState(base?.text ?? "");
  // The editor's own: a text that comes from elsewhere (a version made current, a newer one) opens anew.
  const [opened, setOpened] = React.useState(0);
  const [said, setSaid] = React.useState<{ text: string; error?: boolean } | null>(null);
  const take = (a: Agentfile) => {
    setBase(baseOf(a));
    setName(a.name);
    setText(a.text);
    setOpened((n) => n + 1);
  };
  const dirty = base !== null && (name !== base.name || text !== base.text);
  // A newer version kept elsewhere comes in, unless there are unsaved changes here.
  if (kept !== "gone" && base && (kept.version !== base.version || kept.name !== base.name) && !dirty) take(kept);

  // Save says it's on its way until kiln has kept it (W21-R15); what's typed meanwhile stays, unsaved
  // against the version kept.
  const [save, saving] = useAction(
    async () => {
      if (!base) return;
      setSaid(null);
      const saved = await agentfilesApi.update(id, { ...(name !== base.name ? { name } : {}), ...(text !== base.text ? { text } : {}) });
      res.setData(() => saved.agentfile);
      setBase(baseOf(saved.agentfile));
      setSaid({ text: keptWords(saved, base.version) });
      invalidate("agentfiles/list");
      invalidate("agentfiles/choices");
      invalidate(`agentfiles/versions/${id}`);
    },
    (e) => setSaid({ text: errorMessage(e), error: true }),
  );
  if (kept === "gone" || !base) return null;
  return (
    <>
      <title>{`${kept.name} · Agentfiles · kiln`}</title>
      <AgentfileCrumbs name={kept.name} />
      <h1 className="af-title">{kept.name}</h1>
      <p className="af-dim">
        v{kept.version} · changed {relativeTime(kept.updated_at)}
      </p>
      <TextField
        label="Its name"
        className="af-name-field"
        value={name}
        onChange={(e) => setName(e.target.value.trim())}
        hint="An image's: lowercase letters and digits, with ., _ or - between them. A new name alone is no new version."
      />
      {name && !named(name) && <p className="af-error">An Agentfile&rsquo;s name is lowercase letters and digits, with ., _ or - between them.</p>}
      <AgentfileText value={text} opened={opened} onChange={setText} label={`${kept.name}'s Agentfile`} />
      <div className="af-actions">
        <Button size="sm" disabled={!dirty || !named(name) || !text.trim()} pending={saving} onClick={save}>
          {saving ? "Saving…" : "Save"}
        </Button>
        {dirty && (
          <Button size="sm" variant="ghost" disabled={saving} onClick={() => take(kept)}>
            Discard changes
          </Button>
        )}
        {said && (
          <p className={said.error ? "af-error" : "af-ok"} role="status">
            {said.text}
          </p>
        )}
      </div>
      <Loadable label="its versions" fallback={<RowsHold rows={2} rowHeight={44} label="Loading its versions" />}>
        <Versions
          a={kept}
          dirty={dirty}
          onRestored={(saved, version) => {
            res.setData(() => saved.agentfile);
            take(saved.agentfile);
            setSaid({ text: restoredWords(saved, version) });
          }}
          onFailed={(e) => setSaid({ text: errorMessage(e), error: true })}
        />
      </Loadable>
      <DeleteAgentfile a={kept} />
    </>
  );
}

/** Every version, newest first; any read in place, and any but the current made current again. */
export function Versions({ a, dirty, onRestored, onFailed }: { a: Agentfile; dirty: boolean; onRestored: (saved: AgentfileSaved, version: number) => void; onFailed: (e: unknown) => void }) {
  const versions = useRes(agentfileVersionsRes(a.id));
  const [open, setOpen] = React.useState<number | null>(null);
  // Each Make current says it's on its way until kiln has made it so (W21-R15).
  const [restore, restoring] = useActions(async (_key: string, version: number) => {
    const saved = await agentfilesApi.restore(a.id, version);
    onRestored(saved, version);
    versions.reload();
    invalidate("agentfiles/list");
    invalidate("agentfiles/choices");
  }, onFailed);
  return (
    <section className="af-section" aria-label="Versions">
      <h2 className="af-h">Versions</h2>
      <ul className="af-rows">
        {versions.data.map((v) => (
          <li key={v.version}>
            <div className="af-row">
              <span className="af-name">v{v.version}</span>
              <span className="af-dim">
                {relativeTime(v.at)} · {v.by}
                {v.version === a.version ? " · current" : ""}
              </span>
              <span className="af-acts">
                <Button size="xs" variant="ghost" aria-expanded={open === v.version} onClick={() => setOpen(open === v.version ? null : v.version)}>
                  {open === v.version ? "Hide" : "Read"}
                </Button>
                {v.version !== a.version && (
                  <Button
                    size="xs"
                    variant="ghost"
                    disabled={dirty}
                    title={dirty ? "Save or discard your changes first" : `Its text current again, as v${a.version + 1}`}
                    pending={restoring(String(v.version))}
                    onClick={() => restore(String(v.version), v.version)}
                  >
                    Make current
                  </Button>
                )}
              </span>
            </div>
            {open === v.version && <AgentfileRead text={v.text} label={`v${v.version}'s text`} />}
          </li>
        ))}
      </ul>
    </section>
  );
}

function DeleteAgentfile({ a }: { a: Agentfile }) {
  const navigate = useNavigate();
  const [asking, setAsking] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  // Delete says it's on its way until kiln has deleted it, then goes back to the list (W21-R15).
  const [remove, removing] = useAction(
    async () => {
      setError(null);
      await agentfilesApi.remove(a.id);
      invalidate("agentfiles/list");
      invalidate("agentfiles/choices");
      await navigate("/agents/agentfiles", { replace: true });
    },
    (e) => setError(errorMessage(e)),
  );
  return (
    <section className="af-section" aria-label="Delete the Agentfile">
      {!asking ? (
        <Button size="xs" variant="ghost-danger" className="justify-self-start" onClick={() => setAsking(true)}>
          Delete Agentfile
        </Button>
      ) : (
        <Confirm
          label="Confirm deleting the Agentfile"
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" pending={removing} onClick={remove}>
                Delete
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setAsking(false)}>
                Keep it
              </Button>
            </>
          }
        >
          <p>
            Delete {a.name} and its {a.version === 1 ? "one version" : `${a.version} versions`}? Agents can&rsquo;t start from it any more.
          </p>
          {error && <p className="af-error">{error}</p>}
        </Confirm>
      )}
    </section>
  );
}
