/**
 * The Engine tab's Environments (docs/agents.md, "Environments", "Scopes, stacks, and applied on its
 * own", "In the Engine tab"; decided by the user, 2026-11-18 and 2026-11-19). At the top, this
 * session's stack, top first: reordered by dragging a row or with its up and down buttons, one taken
 * out with ×, the change saved with ✓ and asked again, as it restarts the session; the machine
 * environment where it has none. Below, the person's environments as the tab's other sections list
 * theirs (a filter with facets, a sort, a card each), and a row of scope chips that finds them, starting
 * from this session's own. A card is its variables a line each (a secret by name only), its scope as
 * chips beside its "Apply on its own" switch, and the sessions running with it; it's changed in place,
 * and its Apply puts it on top of this session's stack.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Check, ChevronRight, GripVertical, Plus, X } from "lucide-react";
import {
  agentenvOf,
  agentenvsApi,
  agentenvsRes,
  machineVarsRes,
  relist,
  sessionEnvRes,
  type Agentenv,
  type AgentenvChip,
  type AgentenvFacts,
  type AgentenvFrom,
  type AgentenvScopeKey,
  type AgentenvSession,
  type AgentenvStackEntry,
  type MachineVar,
} from "@/api/agentenvs";
import { agentdefsRes } from "@/api/agentdefs";
import { harnessApi, type AgentInfo, type KindInfo } from "@/api/harness";
import { mySecretsRes } from "@/api/secrets";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { Loadable } from "@/components/ui/loading";
import { FilterBar, SelectField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { agentsRes, harnessSessionsRes, labelOf, useRes, type Res } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { setData } from "@/lib/resource";
import { Link } from "@/lib/router";
import {
  BASE_WORD,
  blankVar,
  changeOf,
  chipsOf,
  draftOf,
  ENV_ORDER,
  ENV_SORTS,
  envFacets,
  envWords,
  facetsFor,
  filterShows,
  moved,
  movedTo,
  problemOf,
  askOf,
  sameChip,
  SCOPE_KEYS,
  shows,
  SOURCE_WORD,
  suggestionsFor,
  withChip,
  type Draft,
  type DraftVar,
  type EnvSort,
} from "./agentenv-logic";
import { GroupHead } from "./definitions";
import { agentLabel } from "./derive";
import { Fold, useFold } from "./fold";
import { STANDS } from "./list-tools";
import { standOf } from "./mission-rows";
import "./definition.css";
import "./sandbox.css";
import "./figure/figure.css";
import "./engine-envs.css";

/** A session's state in Mission control's words. */
const STAND_WORD = new Map(STANDS.map((x) => [x.id, x.label]));

const machineOf = (target: string) => labelOf(target.replace(/^agent:/, ""));

/** The person's environments, found and sorted, a card each: a new one first, then this session's stack's, marked, then the others its filter shows. */
export function Environments({ agentId, target }: { agentId: string; target: string }) {
  const envs = useRes(agentenvsRes).data;
  const session = useRes(sessionEnvRes(agentId)).data;
  // Every page's list of them, kept live by their events: what its runner holds changes as it starts again.
  const list = useRes(harnessSessionsRes).data;
  const me = list.find((a) => a.id === agentId);
  const facets = React.useMemo(() => envFacets(agentId), [agentId]);
  const [q, setQ] = React.useState("");
  const [chosen, setChosen] = React.useState<Chosen>({});
  const [sort, setSort] = React.useState<EnvSort>("newest");
  // The scope chips that find them: this session's own until changed (none: every one).
  const [filter, setFilter] = React.useState<AgentenvChip[] | null>(null);
  const chips = filter ?? chipsOf(session.facts);
  const mine = session.stack.flatMap((x) => envs.find((e) => e.id === x.id) ?? []);
  const shown = envs.filter((e) => !mine.includes(e) && filterShows(e.scope, chips) && shows(e, envWords(e), q, chosen, facets)).sort(ENV_ORDER[sort]);
  const at: At = { agentId, target, name: me ? agentLabel(me) : "this session", stack: session.stack, facts: session.facts };
  // What its runner holds differs from its stack: it takes the stack as its harness next starts.
  const held = me ? (agentenvOf(me)?.id ?? "") : null;
  return (
    <Fold id="engine-environments" title="Environments" count={`${shown.length + mine.length} of ${envs.length}`}>
      <Stack at={at} pending={held !== null && held !== session.stack.map((x) => x.id).join("+")} />
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
      <div className="ev-filter">
        <Chips chips={chips} label="Scope filter" facts={session.facts} onChips={setFilter}>
          {chips.length > 0 && (
            <button type="button" className="ms-link" onClick={() => setFilter([])}>
              Show all
            </button>
          )}
          {filter !== null && (
            <button type="button" className="ms-link" onClick={() => setFilter(null)}>
              This session
            </button>
          )}
        </Chips>
      </div>
      <ul className="ms-list" data-list="envs-tab" aria-label="Environments">
        <NewEnv at={at} />
        {mine.map((e) => (
          <EnvCard key={e.id} e={e} at={at} list={list} here />
        ))}
        {shown.map((e) => (
          <EnvCard key={e.id} e={e} at={at} list={list} />
        ))}
        {envs.length > 0 && mine.length === 0 && shown.length === 0 && (
          <li className="ms-node df-body">
            <span className="df-note">No matches.</span>
          </li>
        )}
      </ul>
    </Fold>
  );
}

/** Which session the section is of: its id, its machine, its name, its stack and what it is as scopes see it. */
type At = { agentId: string; target: string; name: string; stack: AgentenvStackEntry[]; facts: AgentenvFacts };

/* ------------------------------------------------ this session's stack */

/**
 * What a stack sends (`agentenvs.apply`, top first): its rows, bar those at its foot that joined on
 * their own, which join again on their own (one moved above one given by name is given by name).
 */
const namedOf = (rows: readonly AgentenvStackEntry[]) => {
  let end = rows.length;
  while (end > 0 && rows[end - 1]!.auto) end -= 1;
  return rows.slice(0, end).map((x) => x.id);
};

/** Session `at` on stack `ids`, top first: the session restarted, its stack as the server answers; a refusal says why. */
function apply(at: At, ids: string[], why: (w: string | null) => void): Promise<unknown> {
  why(null);
  return agentenvsApi.apply(at.agentId, ids).then(
    (done) => {
      setData<AgentenvSession>(sessionEnvRes(at.agentId).key, (s) => ({ ...s, stack: done.stack }));
      relist();
    },
    (err: unknown) => {
      relist();
      why(errorMessage(err));
    },
  );
}

const idsOf = (rows: readonly AgentenvStackEntry[]) => rows.map((x) => x.id).join("+");

/**
 * This session's stack, top first: a row each, its name and "auto" where it joined on its own, dragged
 * by its handle or moved with up and down, taken out with ×. Changes wait for ✓ (× puts them back),
 * then are asked again, as they restart the session.
 */
function Stack({ at, pending }: { at: At; pending: boolean }) {
  const [want, setWant] = React.useState<AgentenvStackEntry[] | null>(null);
  const [asking, setAsking] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [why, setWhy] = React.useState<string | null>(null);
  const [drag, setDrag] = React.useState<{ from: number; to: number } | null>(null);
  const rowsRef = React.useRef<HTMLOListElement>(null);
  const staged = want ?? at.stack;
  const rows = drag ? movedTo(staged, drag.from, drag.to) : staged;
  const changed = want !== null && idsOf(want) !== idsOf(at.stack);
  const stage = (next: AgentenvStackEntry[]) => {
    setWant(idsOf(next) === idsOf(at.stack) ? null : next);
    setAsking(false);
  };
  // The row under the pointer, by the rows' middles: where the one dragged goes.
  const indexAt = (y: number) => {
    const items = [...(rowsRef.current?.children ?? [])];
    const i = items.findIndex((r) => y < r.getBoundingClientRect().top + r.getBoundingClientRect().height / 2);
    return i < 0 ? items.length - 1 : i;
  };
  return (
    <div className="ev-stack">
      <ol ref={rowsRef} className="ev-srows" aria-label="This session's environments">
        {rows.length === 0 && (
          <li className="ev-srow" data-default="">
            Machine environment <span className="sb-dim">(default)</span>
          </li>
        )}
        {rows.map((r, i) => (
          <li key={r.id} className="ev-srow" data-dragging={drag?.to === i ? "" : undefined}>
            <span
              className="ev-grip"
              aria-hidden="true"
              onPointerDown={(p) => {
                p.currentTarget.setPointerCapture(p.pointerId);
                setDrag({ from: i, to: i });
              }}
              onPointerMove={(p) => drag && setDrag({ ...drag, to: indexAt(p.clientY) })}
              onPointerUp={() => {
                if (drag) stage(movedTo(staged, drag.from, drag.to));
                setDrag(null);
              }}
              onPointerCancel={() => setDrag(null)}
            >
              <GripVertical size={14} />
            </span>
            <span className="ev-sname">{r.name}</span>
            {r.auto && <span className="ev-badge">auto</span>}
            <span className="ev-sacts">
              <button type="button" className="ms-go" aria-label={`Move ${r.name} up`} title="Up" disabled={i === 0} onClick={() => stage(moved(staged, i, -1))}>
                <ArrowUp size={14} />
              </button>
              <button type="button" className="ms-go" aria-label={`Move ${r.name} down`} title="Down" disabled={i === rows.length - 1} onClick={() => stage(moved(staged, i, 1))}>
                <ArrowDown size={14} />
              </button>
              <button
                type="button"
                className="ms-go"
                aria-label={`Take ${r.name} out`}
                title={r.auto ? "Joins on its own" : "Take out"}
                disabled={r.auto}
                onClick={() => stage(staged.filter((x) => x.id !== r.id))}
              >
                <X size={14} />
              </button>
            </span>
          </li>
        ))}
      </ol>
      {changed && !asking && (
        <span className="sb-form-acts">
          <button type="button" className="fig-round" data-size="sm" data-tone="accent" aria-label="Save this session's environments" title="Save" onClick={() => setAsking(true)}>
            <Check size={15} />
          </button>
          <button type="button" className="fig-round" data-size="sm" data-tone="cancel" aria-label="Discard changes to this session's environments" title="Discard" onClick={() => stage(at.stack)}>
            <X size={15} />
          </button>
        </span>
      )}
      {changed && asking && (
        <Ask
          label="Restart this session?"
          words="Restart this session with these?"
          yes="Restart with these"
          busy={busy}
          onYes={() => {
            setBusy(true);
            void apply(at, namedOf(staged), setWhy).finally(() => {
              setBusy(false);
              setWant(null);
              setAsking(false);
            });
          }}
          onNo={() => setAsking(false)}
        />
      )}
      {pending && !changed && <span className="sb-dim ev-pending">Takes these at its next start</span>}
      {why && (
        <span className="sb-why" role="alert">
          {why}
        </span>
      )}
    </div>
  );
}

/** Asked again in place: its words, ✓ and ×. */
function Ask({ label, words, yes, busy, onYes, onNo }: { label: string; words: string; yes: string; busy: boolean; onYes: () => void; onNo: () => void }) {
  return (
    <span className="ms-confirm ev-confirm" role="group" aria-label={label}>
      <span>{words}</span>
      <button type="button" className="ms-go" disabled={busy} aria-busy={busy || undefined} aria-label={yes} onClick={onYes}>
        <Check size={15} />
      </button>
      <button type="button" className="ms-go" aria-label="Cancel" onClick={onNo}>
        <X size={15} />
      </button>
    </span>
  );
}

/* ------------------------------------------------ scope chips */

/** Each harness the machines run, once, with what it offers (its models, modes and efforts), for a chip's suggestions. */
const kindsRes: Res<KindInfo[]> = {
  key: "agentenvs/kinds",
  fetch: () =>
    harnessApi.kindsAll().then(
      (byTarget) => [
        ...new Map(
          Object.values(byTarget)
            .flat()
            .map((k) => [k.id, k]),
        ).values(),
      ],
      (): KindInfo[] => [],
    ),
};

/** A chip in words: `os: linux`, a machine by its label. */
const chipWord = (c: AgentenvChip) => `${c.key}: ${c.key === "machine" ? labelOf(c.value) : c.value}`;

/** Chips, each taken away with its ×, and one added with +: its key from a menu, then its value, suggested from what kiln knows. What goes after them follows in their line. */
function Chips({
  chips,
  label,
  facts,
  onChips,
  empty,
  children,
}: {
  chips: AgentenvChip[];
  label: string;
  facts: AgentenvFacts;
  onChips: (c: AgentenvChip[]) => void;
  empty?: string;
  children?: React.ReactNode;
}) {
  return (
    <span className="ev-tags" role="list" aria-label={label}>
      {chips.length === 0 && empty && <span className="ev-empty">{empty}</span>}
      {chips.map((c) => (
        <span role="listitem" key={`${c.key}:${c.value}`} className="ev-tag">
          {chipWord(c)}
          <button type="button" className="ev-tag-x" aria-label={`Remove ${chipWord(c)}`} onClick={() => onChips(chips.filter((x) => !sameChip(x, c)))}>
            <X size={10} />
          </button>
        </span>
      ))}
      <AddChip label={label} facts={facts} onAdd={(c) => onChips(withChip(chips, c))} />
      {children}
    </span>
  );
}

/** + opens a key's menu and a value's field in place; Enter or leaving it adds the chip, Escape doesn't. */
function AddChip({ label, facts, onAdd }: { label: string; facts: AgentenvFacts; onAdd: (c: AgentenvChip) => void }) {
  const [chip, setChip] = React.useState<AgentenvChip | null>(null);
  const valueRef = React.useRef<HTMLInputElement>(null);
  // The key's menu is focused as it opens, not again as the value's typed.
  const opened = React.useCallback((el: HTMLSelectElement | null) => el?.focus(), []);
  const list = React.useId();
  if (chip === null)
    return (
      <button type="button" className="ev-tag ev-tag-add" aria-label={`Add to ${label}`} onClick={() => setChip({ key: "os", value: "" })}>
        <Plus size={10} />
        add
      </button>
    );
  const done = (save: boolean) => {
    const value = chip.value.trim();
    setChip(null);
    if (save && value) onAdd({ key: chip.key, value });
  };
  return (
    <span
      className="ev-chipin"
      onBlur={(b) => {
        if (!b.currentTarget.contains(b.relatedTarget)) done(true);
      }}
      onKeyDown={(k) => {
        if (k.key === "Enter") done(true);
        if (k.key === "Escape") done(false);
      }}
    >
      <select
        ref={opened}
        className="ev-tag ev-chipkey"
        style={{ "--n": chip.key.length } as React.CSSProperties}
        value={chip.key}
        aria-label={`${label}: key`}
        onChange={(x) => {
          setChip({ key: x.target.value as AgentenvScopeKey, value: "" });
          valueRef.current?.focus();
        }}
      >
        {SCOPE_KEYS.map((k) => (
          <option key={k} value={k}>
            {k}
          </option>
        ))}
      </select>
      <input
        ref={valueRef}
        className="ev-tag ev-tag-in"
        value={chip.value}
        placeholder="value"
        aria-label={`${label}: ${chip.key}`}
        list={list}
        spellCheck={false}
        onChange={(x) => setChip({ ...chip, value: x.target.value })}
      />
      <Loadable fallback={null}>
        <Suggestions id={list} k={chip.key} facts={facts} />
      </Loadable>
    </span>
  );
}

/** What key `k`'s value can be: the machines, harnesses and agent definitions kiln knows, and this session's own. */
function Suggestions({ id, k, facts }: { id: string; k: AgentenvScopeKey; facts: AgentenvFacts }) {
  const known = { machines: useRes(agentsRes).data, harnesses: useRes(kindsRes).data, agents: useRes(agentdefsRes).data };
  const values = [...new Set([...(facts[k] ? [facts[k]] : []), ...suggestionsFor(k, known)])];
  return (
    <datalist id={id}>
      {values.map((v) => (
        <option key={v} value={v}>
          {k === "machine" ? labelOf(v) : v}
        </option>
      ))}
    </datalist>
  );
}

/* ------------------------------------------------ a card */

function EnvCard({ e, at, list, here }: { e: Agentenv; at: At; list: readonly AgentInfo[]; here?: boolean }) {
  // Each folds to its head: closed as it starts, then as the person last left it (its own key: what a
  // build that opened them all at first kept doesn't open it).
  const [open, setOpen] = useFold(`engine-env-open.${e.id}`, false);
  const [asking, setAsking] = React.useState<"delete" | "apply" | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [why, setWhy] = React.useState<string | null>(null);
  const sessions = e.sessions.map((id) => list.find((a) => a.id === id) ?? { id, name: id, target: "" });
  return (
    <>
      {/* Its line: its fold, its name (this session's marked), its base and version. */}
      <li className="ms-node df-defrow df-head" data-stand={e.sessions.length > 0 ? "working" : "done"} data-depth={0} data-env={e.id}>
        <span className="df-head-main">
          <button type="button" className="ev-chev" aria-expanded={open} aria-label={`${open ? "Collapse" : "Expand"} ${e.name}`} onClick={() => setOpen(!open)}>
            <ChevronRight size={12} className={open ? "hx-chev rotate-90" : "hx-chev"} aria-hidden="true" />
          </button>
          <Inline value={e.name} action={`Rename ${e.name}`} label={`${e.name}'s name`} className="df-strong df-defname" onSave={(name) => change(e, { name }, setWhy)} />
          {here && <Here />}
        </span>
        <span className="df-head-end">
          <span className="df-head-v">
            {BASE_WORD[e.base]} base · v{e.version}
          </span>
        </span>
      </li>
      {open && (
        <li className="ms-tree df-groups">
          <ul className="ms-tree-rows" aria-label={`${e.name}: variables and sessions`}>
            <>
              <li className="ms-node df-body ev-tagrow" data-depth={2}>
                <span className="ev-tags" role="list" aria-label={`${e.name}'s tags`}>
                  {e.tags.map((t) => (
                    <span role="listitem" key={t} className="ev-tag">
                      {t}
                      <button type="button" className="ev-tag-x" aria-label={`Remove tag ${t}`} onClick={() => change(e, { tags: e.tags.filter((x) => x !== t) }, setWhy)}>
                        <X size={10} />
                      </button>
                    </span>
                  ))}
                  <AddTag name={e.name} onAdd={(t) => change(e, { tags: [...e.tags, t] }, setWhy)} />
                </span>
              </li>
              <li className="ms-node df-body ev-desc" data-depth={2}>
                <Inline
                  value={e.description ?? ""}
                  action={`Describe ${e.name}`}
                  label={`${e.name}'s description`}
                  placeholder="Add a description"
                  className="df-note"
                  onSave={(d) => change(e, { description: d || null }, setWhy)}
                />
              </li>
              <GroupHead label="Scope" fixed open onOpen={() => {}} summary={<span className="df-head-v">{e.scope.length}</span>} />
              <li className="ms-node df-body ev-chiprow" data-depth={2}>
                <Chips chips={e.scope} label={`${e.name}'s scope`} facts={at.facts} empty="Everywhere" onChips={(scope) => change(e, { scope }, setWhy)} />
                <label className="ev-auto">
                  Apply on its own
                  <button type="button" role="switch" aria-checked={e.auto} aria-label={`Apply ${e.name} on its own`} className="hx-switch" onClick={() => change(e, { auto: !e.auto }, setWhy)}>
                    <span />
                  </button>
                </label>
              </li>
              <GroupHead label="Variables" fixed open onOpen={() => {}} summary={<span className="df-head-v">{e.vars.length}</span>} />
              <EnvForm key={`${e.id}:${e.version}`} e={e} at={at} />
              <GroupHead label="Sessions" fixed open onOpen={() => {}} summary={<span className="df-head-v">{e.sessions.length}</span>} />
              <li className="ms-node df-body" data-depth={2}>
                {sessions.length === 0 ? (
                  <span className="df-note">No sessions.</span>
                ) : (
                  <table className="ev-table" aria-label={`Sessions running with ${e.name}`}>
                    <thead>
                      <tr>
                        <th scope="col">Session</th>
                        <th scope="col">Machine</th>
                        <th scope="col">State</th>
                      </tr>
                    </thead>
                    <tbody>
                      {sessions.map((s) => (
                        <tr key={s.id}>
                          <td>
                            <Link to={`/agents/${s.id}`} className="ms-link">
                              {"kind" in s ? agentLabel(s) : s.name}
                            </Link>
                            {s.id === at.agentId && <Here />}
                          </td>
                          <td className="sb-dim">{s.target ? machineOf(s.target) : "—"}</td>
                          <td className="sb-dim">{"kind" in s ? (STAND_WORD.get(standOf(s)) ?? standOf(s)) : "—"}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                )}
                <span className="ev-acts">
                  {at.stack[0]?.id !== e.id && (
                    <Button size="xs" variant="secondary" aria-label={`Apply ${e.name} to this session`} onClick={() => setAsking("apply")}>
                      Apply
                    </Button>
                  )}
                  <Button size="xs" variant="danger" onClick={() => setAsking("delete")}>
                    Delete
                  </Button>
                </span>
                {why && (
                  <span className="sb-why" role="alert">
                    {why}
                  </span>
                )}
              </li>
              {asking === "delete" && <DeleteAsk e={e} why={setWhy} onDone={() => setAsking(null)} />}
              {asking === "apply" && (
                <li className="ms-node df-ask ev-ask" data-depth={2}>
                  <Ask
                    label={`Apply ${e.name}?`}
                    words={`Restart this session with ${e.name} on top?`}
                    yes={`Restart with ${e.name} on top`}
                    busy={busy}
                    onYes={() => {
                      setBusy(true);
                      void apply(at, [e.id, ...namedOf(at.stack).filter((x) => x !== e.id)], setWhy).finally(() => {
                        setBusy(false);
                        setAsking(null);
                      });
                    }}
                    onNo={() => setAsking(null)}
                  />
                </li>
              )}
            </>
          </ul>
        </li>
      )}
    </>
  );
}

/** This session runs with it: a dot in the colour kiln marks this session with, its words its label. */
const Here = () => <span className="ev-here" role="img" aria-label="This session runs with it" title="This session runs with it" />;

/** `e` changed as `patch` says, shown at once and saved over the version shown; a refusal puts it back and says why. */
function change(e: Agentenv, patch: Partial<Pick<Agentenv, "name" | "tags" | "scope" | "auto">> & { description?: string | null }, why: (w: string | null) => void) {
  setData<Agentenv[]>(agentenvsRes.key, (l) => l.map((x) => (x.id === e.id ? { ...x, ...patch, description: patch.description === null ? undefined : (patch.description ?? x.description) } : x)));
  why(null);
  agentenvsApi.update(e.id, { version: e.version, ...patch }).then(
    (saved) => {
      setData<Agentenv[]>(agentenvsRes.key, (l) => l.map((x) => (x.id === saved.id ? saved : x)));
      if (patch.scope || patch.auto !== undefined) relist();
    },
    (err: unknown) => {
      relist();
      why(errorMessage(err));
    },
  );
}

/** Text edited where it's shown: pressed, a field in its place; Enter or leaving it saves, Escape doesn't. */
function Inline({ value, action, label, placeholder, className, onSave }: { value: string; action: string; label: string; placeholder?: string; className?: string; onSave: (v: string) => void }) {
  const [text, setText] = React.useState<string | null>(null);
  if (text === null)
    return (
      <button type="button" className={`ev-inline ${className ?? ""}`} aria-label={action} title={action} onClick={() => setText(value)}>
        {value || <span className="ev-placeholder">{placeholder}</span>}
      </button>
    );
  const done = (save: boolean) => {
    const t = text.trim();
    setText(null);
    if (save && t !== value) onSave(t);
  };
  return (
    <input
      ref={(el) => el?.focus()}
      className={`sb-in ev-inline-in ${className ?? ""}`}
      value={text}
      placeholder={placeholder}
      aria-label={label}
      spellCheck={false}
      onChange={(x) => setText(x.target.value)}
      onKeyDown={(k) => {
        if (k.key === "Enter") done(true);
        if (k.key === "Escape") done(false);
      }}
      onBlur={() => done(true)}
    />
  );
}

/** A tag added where the tags are: + opens a field; Enter or leaving it adds what's typed. */
function AddTag({ name, onAdd }: { name: string; onAdd: (t: string) => void }) {
  const [text, setText] = React.useState<string | null>(null);
  if (text === null)
    return (
      <button type="button" className="ev-tag ev-tag-add" aria-label={`Add a tag to ${name}`} onClick={() => setText("")}>
        <Plus size={10} />
        tag
      </button>
    );
  const done = (save: boolean) => {
    const t = text.trim();
    setText(null);
    if (save && t) onAdd(t);
  };
  return (
    <input
      ref={(el) => el?.focus()}
      className="ev-tag ev-tag-in"
      value={text}
      placeholder="tag"
      aria-label={`New tag for ${name}`}
      spellCheck={false}
      onChange={(x) => setText(x.target.value)}
      onKeyDown={(k) => {
        if (k.key === "Enter") done(true);
        if (k.key === "Escape") done(false);
      }}
      onBlur={() => done(true)}
    />
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
                },
                (err: unknown) => why(errorMessage(err)),
              )
              .finally(onDone);
          }}
        >
          <Check size={15} />
        </button>
        <button type="button" className="ms-go" aria-label="Cancel" onClick={onDone}>
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
        <Button size="xs" variant="secondary" aria-label="New environment" onClick={() => setMaking(true)}>
          <Plus aria-hidden="true" />
          New
        </Button>
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
function EnvForm({ e, at, onDone }: { e?: Agentenv; at: At; onDone?: () => void }) {
  const [d, setD] = React.useState<Draft>(() => draftOf(e));
  const [busy, setBusy] = React.useState(false);
  const [why, setWhy] = React.useState<string | null>(null);
  const problem = problemOf(d);
  // An existing one's fields differ from what's kept: ✓ and × act.
  const changed = !e || Object.keys(changeOf(e, d)).some((k) => k !== "version");
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
          onDone?.();
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
    <li className="sb-form ev-form" aria-label={e ? `${e.name}'s variables` : "New environment"}>
      {/* An existing one's name and description are edited in its header. */}
      {!e && (
        <>
          <span className="sb-form-row">
            <input className="sb-in sb-grow" value={d.name} placeholder="name" aria-label="Environment's name" spellCheck={false} onChange={(x) => setD({ ...d, name: x.target.value })} />
          </span>
          <span className="sb-form-row">
            <input className="sb-in sb-grow" value={d.description} placeholder="Description (optional)" aria-label="Description" onChange={(x) => setD({ ...d, description: x.target.value })} />
          </span>
        </>
      )}
      {/* Its base, and a variable added beside it. */}
      <span className="sb-form-row ev-baserow">
        <Segmented
          size="xs"
          label="Base"
          value={d.base}
          onChange={(base) => setD({ ...d, base })}
          options={[
            { value: "user", label: "Machine", title: "Start from the machine environment" },
            { value: "clean", label: "Clean", title: "Start empty: only what the harness needs" },
          ]}
        />
        <button type="button" className="fig-round ev-add" aria-label="Add a variable" title="Add a variable" onClick={() => setD({ ...d, vars: [...d.vars, blankVar()] })}>
          <Plus size={12} aria-hidden="true" />
        </button>
      </span>
      {!e && <span className="sb-k ev-k">Variables</span>}
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
            {v.from === "unset" && <span className="sb-dim">Removed</span>}
          </span>
        </span>
      ))}
      {e && d.version !== undefined && e.version !== d.version && (
        <span className="sb-form-row">
          <span className="sb-dim">Changed elsewhere (now v{e.version}).</span>
          <Button size="xs" variant="secondary" onClick={() => setD(draftOf(e))}>
            Start over from v{e.version}
          </Button>
        </span>
      )}
      <span className="sb-form-acts">
        <button
          type="button"
          className="fig-round"
          data-size="sm"
          data-tone="accent"
          aria-label={`${word} ${label}`}
          title={word}
          disabled={busy || !changed}
          aria-busy={busy || undefined}
          onClick={save}
        >
          <Check size={15} />
        </button>
        <button
          type="button"
          className="fig-round"
          data-size="sm"
          data-tone="cancel"
          aria-label={e ? `Discard changes to ${e.name}` : "Cancel"}
          title={e ? "Discard changes" : "Cancel"}
          disabled={!changed}
          onClick={() => {
            setWhy(null);
            if (e) setD(draftOf(e));
            else onDone?.();
          }}
        >
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
      {v.pick && !picked && <span className="sb-dim ev-src">{`Not set on ${machineOf(target)}`}</span>}
    </>
  );
}
