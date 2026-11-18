/**
 * The Engine tab's Environments (docs/agents.md, "Environments", "In the Engine tab"; decided by the
 * user, 2026-11-18): a person's named sets of variables an agent session runs with, a section that
 * folds as the tab's Sandboxes and Agents do, found by a filter with facets (Base, Uses, Where) and
 * sorted, a card each (engine-tab.tsx, definitions.tsx DefRow): its line (its name, this session's
 * marked, its base and version), then its variables a line each (its name, where it comes from, a
 * value shown or a secret's name, never a secret's value) and the sessions running with it.
 *
 * At the top, a new one, filled in as a card is edited, in place: a `machine` variable picked from
 * what the session's machine says its user has, with where each is set; a `secret`, from the person's
 * own. Then the machine's user's environment, what a session runs with when it has none, and this
 * session's, first and marked. Applied to this session from its card, which says the session starts
 * again on its conversation; deleting one some session runs with is refused, in the server's words.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Check, KeyRound, Pencil, Plus, X } from "lucide-react";
import { agentenvOf, agentenvsApi, agentenvsRes, machineVarsRes, relist, type Agentenv, type AgentenvFrom, type MachineVar } from "@/api/agentenvs";
import type { AgentInfo } from "@/api/harness";
import { mySecretsRes } from "@/api/secrets";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { Loadable } from "@/components/ui/loading";
import { FilterBar, SelectField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { labelOf, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { setData } from "@/lib/resource";
import { Link } from "@/lib/router";
import {
  BASE_WORD,
  blankVar,
  changeOf,
  draftOf,
  ENV_ORDER,
  ENV_SORTS,
  envFacets,
  envWords,
  facetsFor,
  FROM_WORD,
  givesOf,
  mineOf,
  moved,
  problemOf,
  askOf,
  shows,
  SOURCE_WORD,
  type Draft,
  type DraftVar,
  type EnvSort,
} from "./agentenv-logic";
import { GroupHead } from "./definitions";
import { agentLabel } from "./derive";
import { Fold } from "./fold";
import { useAgentList } from "./use-agents";
import "./definition.css";
import "./sandbox.css";
import "./figure/figure.css";
import "./engine-envs.css";

const machineOf = (target: string) => labelOf(target.replace(/^agent:/, ""));

/** The person's environments, found and sorted, a card each: a new one first, then the machine's user's, then this session's, marked. */
export function Environments({ agentId, target }: { agentId: string; target: string }) {
  const envs = useRes(agentenvsRes).data;
  const list = useAgentList();
  const me = list.find((a) => a.id === agentId);
  const mine = mineOf(envs, agentId, agentenvOf(me));
  const facets = React.useMemo(() => envFacets(agentId), [agentId]);
  const [q, setQ] = React.useState("");
  const [chosen, setChosen] = React.useState<Chosen>({});
  const [sort, setSort] = React.useState<EnvSort>("newest");
  const shown = envs.filter((e) => e !== mine && shows(e, envWords(e), q, chosen, facets)).sort(ENV_ORDER[sort]);
  const at = { agentId, target, name: me ? agentLabel(me) : "this session" };
  return (
    <Fold id="engine-environments" title="Environments" count={`${shown.length + (mine ? 1 : 0)} of ${envs.length}`}>
      <FilterBar className="at-bar">
        <FacetBox
          label="Filter environments"
          placeholder="Filter by name, variable, secret…"
          query={q}
          onQuery={setQ}
          facets={facetsFor(envs, envWords, q, chosen, facets)}
          chosen={chosen}
          onChosen={setChosen}
        />
        <SelectField label="Sort environments" hideLabel value={sort} onChange={(v) => setSort(v as EnvSort)} options={ENV_SORTS} />
      </FilterBar>
      <ul className="ms-list" data-list="envs-tab" aria-label="Environments">
        <NewEnv at={at} />
        <MachineLine at={at} mine={mine} />
        {mine && <EnvCard e={mine} at={at} list={list} here />}
        {shown.map((e) => (
          <EnvCard key={e.id} e={e} at={at} list={list} />
        ))}
        {envs.length > 0 && !mine && shown.length === 0 && (
          <li className="ms-node df-body">
            <span className="df-note">No matches.</span>
          </li>
        )}
      </ul>
    </Fold>
  );
}

/** Which session the section is of: its id, its machine, its name. */
type At = { agentId: string; target: string; name: string };

/* ------------------------------------------------ applying one */

/**
 * Session `at` on environment `e` (null: its machine's user's): shown at once (the session moved to
 * its card), its harness started again on its conversation; a refusal puts the list back and says why.
 */
function apply(at: At, e: Agentenv | null, why: (w: string | null) => void): Promise<unknown> {
  setData<Agentenv[]>(agentenvsRes.key, (l) =>
    l.map((x) => ({ ...x, sessions: x.id === e?.id ? [...x.sessions.filter((s) => s !== at.agentId), at.agentId] : x.sessions.filter((s) => s !== at.agentId) })),
  );
  why(null);
  return agentenvsApi.apply(at.agentId, e?.id ?? null).then(
    (r) => {
      relist();
      toast({
        title: e ? `${at.name} runs with ${e.name}` : `${at.name} runs with its machine's user's environment`,
        description: r.restarted ? "It starts again on its conversation." : undefined,
        tone: "running",
      });
    },
    (err: unknown) => {
      relist();
      why(errorMessage(err));
    },
  );
}

/** Apply it to this session, asked again in place: it starts the session again on its conversation. */
function ApplyAsk({ at, e, onDone, why }: { at: At; e: Agentenv | null; onDone: () => void; why: (w: string | null) => void }) {
  const [busy, setBusy] = React.useState(false);
  const what = e ? `${e.name} v${e.version}` : "its machine's user's environment";
  return (
    <li className="ms-node df-ask ev-ask" data-stand="needs" data-depth={2}>
      <span className="ms-confirm ev-confirm" role="group" aria-label={`Apply ${e ? e.name : "the machine's user's environment"} to this session?`}>
        <span>
          {at.name} starts again on its conversation, with {what}.
        </span>
        <button
          type="button"
          className="ms-go"
          disabled={busy}
          aria-busy={busy || undefined}
          aria-label={`Apply ${e ? e.name : "the machine's user's environment"} to this session`}
          onClick={() => {
            setBusy(true);
            void apply(at, e, why).finally(onDone);
          }}
        >
          <Check size={15} />
        </button>
        <button type="button" className="ms-go" aria-label="Keep it as it is" onClick={onDone}>
          <X size={15} />
        </button>
      </span>
    </li>
  );
}

/* ------------------------------------------------ the machine's user's environment */

/** What a session runs with when it has none: this session's, marked, or the way back to it. */
function MachineLine({ at, mine }: { at: At; mine: Agentenv | undefined }) {
  const [asking, setAsking] = React.useState(false);
  const [why, setWhy] = React.useState<string | null>(null);
  return (
    <>
      <li className="ms-node df-defrow df-head" data-stand="done" data-depth={0} data-env="machine">
        <span className="df-head-main">
          <span className="df-strong df-defname">Machine&apos;s user environment</span>
          {!mine && <span className="df-head-v df-this">this session</span>}
        </span>
        <span className="df-head-end">
          {mine ? (
            <Button size="xs" variant="secondary" onClick={() => setAsking(true)}>
              Go back to it
            </Button>
          ) : (
            <span className="df-head-v">{machineOf(at.target)}</span>
          )}
        </span>
      </li>
      <li className="ms-node df-body" data-depth={2}>
        <span className="df-note">What its user would have in a terminal on {machineOf(at.target)}: a session with no environment of yours runs with it.</span>
        {why && (
          <span className="sb-why" role="alert">
            {why}
          </span>
        )}
      </li>
      {asking && <ApplyAsk at={at} e={null} why={setWhy} onDone={() => setAsking(false)} />}
    </>
  );
}

/* ------------------------------------------------ a card */

function EnvCard({ e, at, list, here }: { e: Agentenv; at: At; list: readonly AgentInfo[]; here?: boolean }) {
  const [editing, setEditing] = React.useState(false);
  const [asking, setAsking] = React.useState<"apply" | "delete" | null>(null);
  const [why, setWhy] = React.useState<string | null>(null);
  const sessions = e.sessions.map((id) => list.find((a) => a.id === id) ?? { id, name: id, target: "" });
  return (
    <>
      {/* Its line: its name (this session's marked), its base and version, its pencil. */}
      <li className="ms-node df-defrow df-head" data-stand={e.sessions.length > 0 ? "working" : "done"} data-depth={0} data-env={e.id}>
        <span className="df-head-main">
          <span className="df-strong df-defname">{e.name}</span>
          {here && <span className="df-head-v df-this">this session</span>}
        </span>
        <span className="df-head-end">
          <span className="df-head-v">
            {BASE_WORD[e.base]} · v{e.version}
          </span>
        </span>
        <span className="df-head-acts">
          <button type="button" className="hx-icon" aria-label={`Edit ${e.name}`} aria-pressed={editing} title="Edit" onClick={() => setEditing(!editing)}>
            <Pencil size={14} />
          </button>
        </span>
      </li>
      <li className="ms-tree df-groups">
        <ul className="ms-tree-rows" aria-label={`${e.name}: variables and sessions`}>
          {editing ? (
            <EnvForm e={e} at={at} onDone={() => setEditing(false)} />
          ) : (
            <>
              {e.description && (
                <li className="ms-node df-body" data-depth={2}>
                  <span className="df-note">{e.description}</span>
                </li>
              )}
              <GroupHead label="Variables" fixed open onOpen={() => {}} summary={<span className="df-head-v">{e.vars.length}</span>} />
              <li className="ms-node df-body" data-depth={2}>
                <span className="df-facts">
                  <span className="df-fact">
                    <span className="ms-edit-k">base</span>
                    <span className="df-fact-v">{e.base === "user" ? "The machine's user's environment" : "Clean: only what its harness needs to run"}</span>
                  </span>
                </span>
                {e.vars.length === 0 ? (
                  <span className="df-note">No variables: the base as it is.</span>
                ) : (
                  <ul className="sb-rows ev-rows" aria-label={`${e.name}'s variables`}>
                    {e.vars.map((v, i) => (
                      <li key={`${i}:${v.name}`} className="sb-row ev-var" data-from={v.from}>
                        <span className="sb-mono sb-clip ev-name">{v.name}</span>
                        <span className="sb-tags">
                          <i>{FROM_WORD[v.from]}</i>
                        </span>
                        <span className="sb-mono sb-clip ev-gives" title={v.from === "secret" ? `Your secret ${v.secret ?? ""}: its value is never shown` : givesOf(v)}>
                          {v.from === "secret" && <KeyRound size={11} aria-hidden="true" />}
                          {givesOf(v)}
                        </span>
                      </li>
                    ))}
                  </ul>
                )}
              </li>
              <GroupHead label="Sessions" fixed open onOpen={() => {}} summary={<span className="df-head-v">{e.sessions.length}</span>} />
              <li className="ms-node df-body" data-depth={2}>
                {sessions.length === 0 ? (
                  <span className="df-note">No session runs with it.</span>
                ) : (
                  <ul className="sb-rows ev-rows" aria-label={`Sessions running with ${e.name}`}>
                    {sessions.map((s) => (
                      <li key={s.id} className="sb-row ev-session">
                        <Link to={`/agents/${s.id}`} className="ms-link sb-clip">
                          {"kind" in s ? agentLabel(s) : s.name}
                        </Link>
                        <span className="sb-dim sb-clip">{s.id === at.agentId ? "this session" : s.target ? machineOf(s.target) : ""}</span>
                      </li>
                    ))}
                  </ul>
                )}
                <span className="ev-acts">
                  {!here && (
                    <Button size="xs" variant="secondary" onClick={() => setAsking("apply")}>
                      Apply to this session
                    </Button>
                  )}
                  <Button size="xs" variant="ghost-danger" onClick={() => setAsking("delete")}>
                    Delete
                  </Button>
                </span>
                {!here && <span className="df-note">Applying it starts this session again on its conversation.</span>}
                {why && (
                  <span className="sb-why" role="alert">
                    {why}
                  </span>
                )}
              </li>
              {asking === "apply" && <ApplyAsk at={at} e={e} why={setWhy} onDone={() => setAsking(null)} />}
              {asking === "delete" && <DeleteAsk e={e} why={setWhy} onDone={() => setAsking(null)} />}
            </>
          )}
        </ul>
      </li>
    </>
  );
}

/** Delete it, asked again in place; one some session runs with is refused, the refusal's words under its card. */
function DeleteAsk({ e, onDone, why }: { e: Agentenv; onDone: () => void; why: (w: string | null) => void }) {
  const [busy, setBusy] = React.useState(false);
  return (
    <li className="ms-node df-ask ev-ask" data-stand="failed" data-depth={2}>
      <span className="ms-confirm ev-confirm" role="group" aria-label={`Delete ${e.name}?`}>
        <span>Delete {e.name}?</span>
        <button
          type="button"
          className="ms-go"
          disabled={busy}
          aria-busy={busy || undefined}
          aria-label={`Delete ${e.name}`}
          onClick={() => {
            setBusy(true);
            why(null);
            agentenvsApi
              .remove(e.id)
              .then(
                () => {
                  setData<Agentenv[]>(agentenvsRes.key, (l) => l.filter((x) => x.id !== e.id));
                  relist();
                  toast({ title: `Deleted ${e.name}`, tone: "info" });
                },
                (err: unknown) => why(errorMessage(err)),
              )
              .finally(onDone);
          }}
        >
          <Check size={15} />
        </button>
        <button type="button" className="ms-go" aria-label="Keep it" onClick={onDone}>
          <X size={15} />
        </button>
      </span>
    </li>
  );
}

/* ------------------------------------------------ a new one */

function NewEnv({ at }: { at: At }) {
  const [making, setMaking] = React.useState(false);
  if (!making)
    return (
      <li className="ms-node df-body ev-new" data-depth={0}>
        <button type="button" className="df-add" onClick={() => setMaking(true)}>
          <Plus size={13} aria-hidden="true" />
          New environment
        </button>
      </li>
    );
  return (
    <li className="ms-tree df-groups">
      <ul className="ms-tree-rows" aria-label="New environment">
        <EnvForm at={at} onDone={() => setMaking(false)} />
      </ul>
    </li>
  );
}

/* ------------------------------------------------ editing one, in place */

const FROMS: { value: AgentenvFrom; label: string }[] = [
  { value: "machine", label: "machine" },
  { value: "value", label: "value" },
  { value: "secret", label: "secret" },
  { value: "unset", label: "unset" },
];

/**
 * A card's form, in place (no dialog): its name, description and base, then its variables a row each
 * (its name, where it comes from, what it takes; moved up or down, or taken out), then ✓ and ×. Saving
 * `e` sends what changed on the version it was opened at: one saved since is refused, in the server's
 * words, and can be started over from.
 */
function EnvForm({ e, at, onDone }: { e?: Agentenv; at: At; onDone: () => void }) {
  const [d, setD] = React.useState<Draft>(() => draftOf(e));
  const [busy, setBusy] = React.useState(false);
  const [why, setWhy] = React.useState<string | null>(null);
  const problem = problemOf(d);
  const word = e ? "Save" : "Create";
  const setVar = (i: number, change: Partial<DraftVar>) => setD({ ...d, vars: d.vars.map((v, j) => (j === i ? { ...v, ...change } : v)) });
  const save = () => {
    if (problem) return setWhy(problem);
    setBusy(true);
    setWhy(null);
    (e ? agentenvsApi.update(e.id, changeOf(e, d)) : agentenvsApi.create(askOf(d)))
      .then(
        (saved) => {
          setData<Agentenv[]>(agentenvsRes.key, (l) => (l.some((x) => x.id === saved.id) ? l.map((x) => (x.id === saved.id ? saved : x)) : [saved, ...l]));
          relist();
          toast({ title: e ? (saved.version > e.version ? `Saved ${saved.name}, v${saved.version}` : "Nothing changed") : `Made ${saved.name}, v${saved.version}`, tone: "info" });
          onDone();
        },
        (err: unknown) => {
          setWhy(errorMessage(err));
          relist();
        },
      )
      .finally(() => setBusy(false));
  };
  const label = e ? e.name : "the new environment";
  return (
    <li className="sb-form ev-form" aria-label={e ? `Edit ${e.name}` : "New environment"}>
      <span className="sb-form-row">
        <input className="sb-in sb-grow" value={d.name} placeholder="name" aria-label="Environment's name" spellCheck={false} onChange={(x) => setD({ ...d, name: x.target.value })} />
      </span>
      <span className="sb-form-row">
        <input className="sb-in sb-grow" value={d.description} placeholder="what it's for (optional)" aria-label="Description" onChange={(x) => setD({ ...d, description: x.target.value })} />
      </span>
      <span className="sb-form-row">
        <Segmented
          size="xs"
          label="Base"
          value={d.base}
          onChange={(base) => setD({ ...d, base })}
          options={[
            { value: "user", label: "User's", title: "The machine's user's environment, its variables over it" },
            { value: "clean", label: "Clean", title: "Only what its harness needs to run, its variables over that" },
          ]}
        />
        <span className="sb-dim ev-base">{d.base === "user" ? "over the machine's user's environment" : "over only what its harness needs to run"}</span>
      </span>
      <span className="sb-k ev-k">Variables</span>
      {d.vars.map((v, i) => (
        <span key={v.key} className="ev-vrow" role="group" aria-label={`Variable ${i + 1}`}>
          <span className="sb-form-row">
            <input
              className="sb-in ev-vname"
              value={v.name}
              placeholder="NAME"
              aria-label={`Variable ${i + 1}'s name`}
              spellCheck={false}
              autoCapitalize="characters"
              onChange={(x) => setVar(i, { name: x.target.value })}
            />
            <select className="sb-in" value={v.from} aria-label={`Where variable ${i + 1} comes from`} onChange={(x) => setVar(i, { from: x.target.value as AgentenvFrom })}>
              {FROMS.map((f) => (
                <option key={f.value} value={f.value}>
                  {f.label}
                </option>
              ))}
            </select>
            <span className="sb-acts ev-vacts">
              <button type="button" className="ms-go" aria-label={`Move variable ${i + 1} up`} title="Up" disabled={i === 0} onClick={() => setD({ ...d, vars: moved(d.vars, i, -1) })}>
                <ArrowUp size={14} />
              </button>
              <button
                type="button"
                className="ms-go"
                aria-label={`Move variable ${i + 1} down`}
                title="Down"
                disabled={i === d.vars.length - 1}
                onClick={() => setD({ ...d, vars: moved(d.vars, i, 1) })}
              >
                <ArrowDown size={14} />
              </button>
              <button type="button" className="ms-go" aria-label={`Remove variable ${i + 1}`} title="Remove" onClick={() => setD({ ...d, vars: d.vars.filter((_, j) => j !== i) })}>
                <X size={14} />
              </button>
            </span>
          </span>
          <span className="sb-form-row">
            {v.from === "value" && (
              <input className="sb-in sb-grow" value={v.value} placeholder="value" aria-label={`Variable ${i + 1}'s value`} spellCheck={false} onChange={(x) => setVar(i, { value: x.target.value })} />
            )}
            {v.from === "secret" && (
              <Loadable label="Loading your secrets" fallback={<span className="sb-dim">…</span>}>
                <SecretPick v={v} i={i} onPick={(secret) => setVar(i, { secret, name: v.name || secret })} />
              </Loadable>
            )}
            {v.from === "machine" && (
              <Loadable label={`Loading what ${machineOf(at.target)}'s user has`} fallback={<span className="sb-dim">Asking {machineOf(at.target)}…</span>}>
                <MachinePick target={at.target} v={v} i={i} onPick={(pick) => setVar(i, { pick, name: v.name || pick })} />
              </Loadable>
            )}
            {v.from === "unset" && <span className="sb-dim">taken away from what the base gives</span>}
          </span>
        </span>
      ))}
      <span className="sb-form-row">
        <button type="button" className="df-add" onClick={() => setD({ ...d, vars: [...d.vars, blankVar()] })}>
          <Plus size={13} aria-hidden="true" />
          Add a variable
        </button>
      </span>
      {e && d.version !== undefined && e.version !== d.version && (
        <span className="sb-form-row">
          <span className="sb-dim">Saved as v{e.version} since you opened it.</span>
          <Button size="xs" variant="secondary" onClick={() => setD(draftOf(e))}>
            Start over from v{e.version}
          </Button>
        </span>
      )}
      <span className="sb-form-acts">
        <button type="button" className="fig-round" data-size="sm" data-tone="accent" aria-label={`${word} ${label}`} title={word} disabled={busy} aria-busy={busy || undefined} onClick={save}>
          <Check size={15} />
        </button>
        <button type="button" className="fig-round" data-size="sm" data-tone="cancel" aria-label="Cancel" title="Cancel" onClick={onDone}>
          <X size={15} />
        </button>
      </span>
      {why && (
        <span className="sb-why" role="alert">
          {why}
        </span>
      )}
    </li>
  );
}

/** One of the person's own secrets, by name: its value is never read here. */
function SecretPick({ v, i, onPick }: { v: DraftVar; i: number; onPick: (name: string) => void }) {
  const secrets = useRes(mySecretsRes).data;
  if (secrets.length === 0)
    return (
      <span className="sb-dim">
        You have no secrets yet:{" "}
        <Link to="/settings#secrets.mine" className="ms-link">
          add one in Settings
        </Link>
      </span>
    );
  return (
    <select className="sb-in sb-grow" value={v.secret} aria-label={`Variable ${i + 1}'s secret`} onChange={(x) => onPick(x.target.value)}>
      <option value="">Pick one of your secrets</option>
      {secrets.map((s) => (
        <option key={s.name} value={s.name}>
          {s.name}
        </option>
      ))}
    </select>
  );
}

const sourcesOf = (m: MachineVar | undefined) => (m ? m.sources.map((s) => SOURCE_WORD[s] ?? s).join(" · ") : "");

/** A variable the session's machine says its user has, by name, with where each is set. */
function MachinePick({ target, v, i, onPick }: { target: string; v: DraftVar; i: number; onPick: (name: string) => void }) {
  const vars = useRes(machineVarsRes(target)).data;
  const picked = vars.find((m) => m.name === v.pick);
  return (
    <>
      <select className="sb-in sb-grow" value={v.pick} aria-label={`Variable ${i + 1}'s machine variable`} onChange={(x) => onPick(x.target.value)}>
        <option value="">Pick one of {machineOf(target)}&apos;s</option>
        {v.pick && !picked && <option value={v.pick}>{v.pick} (not on it now)</option>}
        {vars.map((m) => (
          <option key={m.name} value={m.name}>
            {m.name} · {sourcesOf(m)}
          </option>
        ))}
      </select>
      {/* Where a picked one is set, its option says; one the machine hasn't, this line. */}
      {v.pick && !picked && <span className="sb-dim ev-src">{`${machineOf(target)}'s user hasn't it now: it's left unset`}</span>}
    </>
  );
}
