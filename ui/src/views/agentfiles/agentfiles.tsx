/**
 * Agentfiles (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 31; docs/api.md, "Agentfiles"): what
 * agents' microVMs are made from. Yours, each opened on its own page to edit (agentfile.tsx), and a
 * new one written here, in place; and kiln's own, one per harness, read only: read here, or copied
 * to make one yours. An agent starts from one in Mission control (More options → Agentfile).
 */
import * as React from "react";
import { agentfilesApi, agentfilesRes } from "@/api/agentfiles";
import type { AgentfileBrief, AgentfileBuiltin } from "@/api/wire";
import { Button } from "@/components/ui/button";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, TextField } from "@/components/ui/misc";
import { useAction } from "@/lib/action";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useNavigate } from "@/lib/router";
import { copyName, named } from "./names";
import { AgentfileRead, AgentfileText } from "./text";
import "./agentfiles.css";

/** What a new one starts as: an image of an agent's microVM, its harness and grants to be written. */
const STARTER = `# An agent's microVM: a Dockerfile's instructions, and Shards' own (HARNESS, AGENT, SKILL, MCP,
# NETWORK, CONNECT, ATTACH, EXPOSE … FOR). Or copy one of kiln's below, a harness ready to run.
FROM ubuntu:24.04
`;

export default function AgentfilesPage() {
  return (
    <div className="container-console page af-page">
      <title>Agentfiles · kiln</title>
      <Crumbs index={5} items={[{ label: "Agents", to: "/agents" }, { label: "Agentfiles" }]} />
      <h1 className="af-title">Agentfiles</h1>
      <p className="af-dim af-lead">
        What an agent&rsquo;s microVM is made from: a Dockerfile&rsquo;s instructions and Shards&rsquo; own. Yours are kept with every version, checked by Shards as you write them. kiln&rsquo;s are
        read only: copy one to make it yours. An agent starts from one under More options in Mission control.
      </p>
      <Loadable label="Loading Agentfiles" fallback={<RowsHold rows={4} rowHeight={44} label="Loading Agentfiles" />}>
        <Listing />
      </Loadable>
    </div>
  );
}

/** A new one, written here: its name and text, as one of kiln's or empty to start with. */
interface Making {
  name: string;
  text: string;
  /** Made at: each new one opens its own editor. */
  at: number;
}

function Listing() {
  const listing = useRes(agentfilesRes()).data;
  const [making, setMaking] = React.useState<Making | null>(null);
  // A copy's name: free among yours, and not kiln's own, so the two are never taken for each other.
  const names = [...listing.agentfiles.map((a) => a.name), ...listing.builtin.map((b) => b.name)];
  return (
    <>
      <section className="af-section" aria-label="Yours">
        <div className="af-head">
          <h2 className="af-h">Yours</h2>
          {!making && (
            <Button size="xs" variant="secondary" onClick={() => setMaking({ name: "", text: STARTER, at: Date.now() })}>
              New Agentfile
            </Button>
          )}
        </div>
        {making && <NewAgentfile key={making.at} seed={making} onClose={() => setMaking(null)} />}
        {listing.agentfiles.length === 0 ? (
          <p className="af-dim">None yet: write a new one, or copy one of kiln&rsquo;s.</p>
        ) : (
          <ul className="af-rows" aria-label="Your Agentfiles">
            {listing.agentfiles.map((a) => (
              <Yours key={a.id} a={a} />
            ))}
          </ul>
        )}
      </section>
      <section className="af-section" aria-label="kiln's">
        <h2 className="af-h">kiln&rsquo;s</h2>
        <p className="af-dim">One for each harness kiln runs in a microVM, read only.</p>
        {listing.builtin.length === 0 ? (
          <p className="af-dim">This server ships none.</p>
        ) : (
          <ul className="af-rows" aria-label="kiln's Agentfiles">
            {listing.builtin.map((b) => (
              <Builtin key={b.name} b={b} name={copyName(b.name, names)} />
            ))}
          </ul>
        )}
      </section>
    </>
  );
}

function Yours({ a }: { a: AgentfileBrief }) {
  return (
    <li>
      <div className="af-row">
        <Link to={`/agents/agentfiles/${encodeURIComponent(a.id)}`} className="af-name">
          {a.name}
        </Link>
        <span className="af-dim">
          v{a.version} · changed {relativeTime(a.updated_at)}
        </span>
      </div>
    </li>
  );
}

/** One of kiln's: read in place, or copied as yours under `name` (free among yours), and opened. */
function Builtin({ b, name }: { b: AgentfileBuiltin; name: string }) {
  const navigate = useNavigate();
  const [reading, setReading] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  // Copy says it's on its way until kiln has kept the copy, then opens it (W21-R15).
  const [copy, copying] = useAction(
    async () => {
      setError(null);
      const saved = await agentfilesApi.create(name, b.text);
      invalidate("agentfiles/");
      await navigate(`/agents/agentfiles/${encodeURIComponent(saved.agentfile.id)}`);
    },
    (e) => setError(errorMessage(e)),
  );
  return (
    <li>
      <div className="af-row">
        <span className="af-name">{b.name}</span>
        <span className="af-dim">for {b.harness}</span>
        <span className="af-acts">
          <Button size="xs" variant="ghost" aria-expanded={reading} onClick={() => setReading(!reading)}>
            {reading ? "Hide" : "Read"}
          </Button>
          <Button size="xs" variant="secondary" pending={copying} onClick={copy} title={`Keep a copy as yours, named ${name}, to change`}>
            Copy as yours
          </Button>
        </span>
      </div>
      {error && <p className="af-error">{error}</p>}
      {reading && <AgentfileRead text={b.text} label={`${b.name}'s Agentfile`} />}
    </li>
  );
}

/** A new one: its name and text, kept once Shards passes it, then opened on its own page. */
function NewAgentfile({ seed, onClose }: { seed: Making; onClose: () => void }) {
  const navigate = useNavigate();
  const [name, setName] = React.useState(seed.name);
  const [text, setText] = React.useState(seed.text);
  const [error, setError] = React.useState<string | null>(null);
  const ok = named(name) && text.trim() !== "";
  // Keep it says it's on its way until kiln has kept it (Shards' refusal is said here), then opens it (W21-R15).
  const [keep, keeping] = useAction(
    async () => {
      setError(null);
      const saved = await agentfilesApi.create(name, text);
      invalidate("agentfiles/");
      await navigate(`/agents/agentfiles/${encodeURIComponent(saved.agentfile.id)}`);
    },
    (e) => setError(errorMessage(e)),
  );
  return (
    <div className="af-new" role="group" aria-label="A new Agentfile">
      <TextField label="Its name" className="af-name-field" value={name} onChange={(e) => setName(e.target.value.trim())} placeholder="claude-rust" />
      {name && !named(name) && <p className="af-error">An Agentfile&rsquo;s name is lowercase letters and digits, with ., _ or - between them.</p>}
      <AgentfileText value={seed.text} opened={seed.at} onChange={setText} label="The new Agentfile" />
      <div className="af-actions">
        <Button size="sm" disabled={!ok} pending={keeping} onClick={keep}>
          Keep it
        </Button>
        <Button size="sm" variant="ghost" onClick={onClose}>
          Cancel
        </Button>
        {error && <p className="af-error">{error}</p>}
      </div>
    </div>
  );
}
