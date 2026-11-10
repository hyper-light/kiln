/**
 * The agent's skills, in its Tools tab (docs/agents.md, "Skills kiln installs"): those kiln
 * installed, each package pinned to its commit with its state on each machine it went to, and
 * those its harness found on its own. Skills are added from a link (a GitHub repository, a folder
 * or file in one, any git URL), a folder on the agent's machine, an upload, or written here: kiln
 * looks first and shows what it found (skills, agents, the hooks and MCP servers that run code on
 * their own, off until turned on, setup, the tools it needs), then installs it for this agent,
 * every agent of yours, or this repository's folder. Long lists turn a page at a time.
 */
import * as React from "react";
import { ChevronRight, FileUp, Folder, Link2, PenLine, Plus, Trash2 } from "lucide-react";
import { clientId, type SessionStore, type State } from "@/api/harness";
import { skillsApi, type LookAnswer, type LookFrom, type SkillDraft, type SkillItem, type SkillPackage, type SkillPicks, type SkillScope, type SkillsList } from "@/api/skills";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { SearchField } from "@/components/ui/misc";
import { Pager, PerPicker, type PerPage } from "@/components/ui/pager";
import { sendingProps, useActions } from "@/lib/action";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource, type SuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { Prose } from "./prose";

type From = "link" | "folder" | "upload" | "write";
const FROM: { id: From; label: string; icon: React.ReactNode }[] = [
  { id: "link", label: "Link", icon: <Link2 size={12} /> },
  { id: "folder", label: "Folder", icon: <Folder size={12} /> },
  { id: "upload", label: "Upload", icon: <FileUp size={12} /> },
  { id: "write", label: "Write", icon: <PenLine size={12} /> },
];
const SCOPES: { value: SkillScope; label: string }[] = [
  { value: "agent", label: "This agent" },
  { value: "mine", label: "All my agents" },
  { value: "repo", label: "This repo" },
];
const DOT: Record<string, string> = { ready: "succeeded", "setting-up": "running", fetching: "queued", needs: "timed_out", failed: "failed" };
const WORD: Record<string, string> = { ready: "ready", "setting-up": "setting up", fetching: "fetching", needs: "needs", failed: "failed" };

/** One page of `list`, back to the first whenever `q` (the search) or the page's size changes. */
function usePage<T>(list: T[], q: string, per: PerPage) {
  const [at, setAt] = React.useState({ q, per, page: 0 });
  // A new search or page size starts at the first page, and stays there when either goes back.
  if (at.q !== q || at.per !== per) setAt({ q, per, page: 0 });
  const pages = Math.max(1, Math.ceil(list.length / per));
  const page = at.q === q && at.per === per ? Math.min(at.page, pages - 1) : 0;
  const range = list.length ? `${page * per + 1}–${Math.min(list.length, (page + 1) * per)} of ${list.length}` : "—";
  const go = (n: number) => setAt({ q, per, page: n });
  return { rows: list.slice(page * per, (page + 1) * per), many: list.length > per, range, back: page > 0 ? () => go(page - 1) : null, on: page < pages - 1 ? () => go(page + 1) : null };
}

/** A paged list keeps a page's height, so nothing under it moves on its last page. */
function pageStyle(many: boolean, per: PerPage, descriptions: boolean): React.CSSProperties | undefined {
  return many ? ({ "--sk-per": per, "--sk-row": descriptions ? "55px" : "35px" } as React.CSSProperties) : undefined;
}

function Check({ on, onChange, label, disabled }: { on: boolean; onChange: (v: boolean) => void; label: string; disabled?: boolean }) {
  return <input type="checkbox" className="sk-check" checked={on} disabled={disabled} onChange={(e) => onChange(e.target.checked)} aria-label={label} />;
}

/** A line of a preview: what, its names, how many, and whether it's picked. */
function Line({ label, names, count, on, onChange }: { label: string; names: string; count: number; on?: boolean; onChange?: (v: boolean) => void }) {
  return (
    <div className="sk-row" data-empty={count === 0 || undefined}>
      <span className="sk-label">{label}</span>
      <span className="sk-names hx-trunc">{count ? names : "none"}</span>
      <span className="sk-end">
        {count > 0 && <span className="hx-meta-num">{count}</span>}
        {onChange && <Check on={Boolean(on) && count > 0} disabled={count === 0} onChange={onChange} label={label} />}
      </span>
    </div>
  );
}

function Preview({ d, agent, onInstalled, onCancel }: { d: SkillDraft; agent: string; onInstalled: () => void; onCancel: () => void }) {
  const [picks, setPicks] = React.useState<SkillPicks>({ skills: true, agents: true, hooks: false, mcp: false, setup: true });
  const [scope, setScope] = React.useState<SkillScope>("agent");
  const [error, setError] = React.useState<string | null>(null);
  const [act, acting] = useActions(
    (_what: string, go: () => Promise<void>) => go(),
    (e) => setError(errorMessage(e)),
  );
  const pick = (k: keyof SkillPicks) => (v: boolean) => setPicks((p) => ({ ...p, [k]: v }));
  const items = (picks.skills ? d.skills.length : 0) + (picks.agents ? d.agents.length : 0);
  const install = () =>
    act("install", async () => {
      setError(null);
      await skillsApi.install(agent, d.draft, scope, picks);
      onInstalled();
    });
  return (
    <div className="sk-preview">
      <div className="sk-row sk-row-head">
        <span className="sk-label">Found</span>
        <span className="sk-names hx-trunc">
          <b>{d.name}</b>
          {d.version && <span className="hx-dim"> {d.version}</span>}
        </span>
        <span className="sk-end">
          <span className="hx-tag">{d.shape}</span>
        </span>
      </div>
      <div className="sk-row">
        <span className="sk-label">At</span>
        <span className="sk-names hx-mono hx-trunc">
          {d.source.label} @ {d.sha.slice(0, 7)}
        </span>
        <span className="sk-end">{d.license && <span className="hx-meta-num">{d.license}</span>}</span>
      </div>
      <Line label="Skills" names={d.skills.map((s) => s.name).join(", ")} count={d.skills.length} on={picks.skills} onChange={pick("skills")} />
      {(d.agents.length > 0 || d.hooks.length > 0 || d.mcp.length > 0) && (
        <p className="sk-empty">Skills and setup work across harnesses. Agent definitions, hooks, and package MCP configuration currently activate in Claude Code.</p>
      )}
      <Line label="Agents" names={d.agents.map((s) => s.name).join(", ")} count={d.agents.length} on={picks.agents} onChange={pick("agents")} />
      <Line label="Hooks" names={d.hooks.join(", ")} count={d.hooks.length} on={picks.hooks} onChange={pick("hooks")} />
      <Line label="MCP" names={d.mcp.join(", ")} count={d.mcp.length} on={picks.mcp} onChange={pick("mcp")} />
      <div className="sk-row" data-empty={d.setup.length === 0 || undefined}>
        <span className="sk-label">Setup</span>
        <span className="sk-names">
          {d.setup.length === 0 ? (
            "none"
          ) : (
            <span className="sk-cmds">
              {d.setup.map((s) => (
                <span key={`${s.skill}:${s.command}`} className="hx-trunc">
                  <span className="hx-dim">{s.skill} · </span>
                  <code>{s.command}</code>
                </span>
              ))}
            </span>
          )}
        </span>
        <span className="sk-end">{d.setup.length > 0 && <Check on={picks.setup} onChange={pick("setup")} label="Setup" />}</span>
      </div>
      <div className="sk-row" data-empty={d.needs.length === 0 || undefined}>
        <span className="sk-label">Needs</span>
        <span className="sk-names hx-trunc">{d.needs.length === 0 ? "nothing" : d.needs.map((n) => <code key={n}>{n} </code>)}</span>
        <span className="sk-end" />
      </div>
      {error && <div className="hx-bad sk-none">{error}</div>}
      <div className="sk-row sk-row-foot">
        <span className="sk-label">For</span>
        <span className="sk-names">
          <select className="hx-select sk-scope" value={scope} onChange={(e) => setScope(e.target.value as SkillScope)} aria-label="For">
            {SCOPES.map((s) => (
              <option key={s.value} value={s.value}>
                {s.label}
              </option>
            ))}
          </select>
        </span>
        <span className="sk-end sk-buttons">
          <Button size="xs" variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
          <Button size="xs" disabled={items === 0} {...sendingProps(acting("install"))} onClick={install}>
            Install
          </Button>
        </span>
      </div>
    </div>
  );
}

function AddSkills({ agent, machine, onInstalled, onDone }: { agent: string; machine: string; onInstalled: () => void; onDone: () => void }) {
  const [from, setFrom] = React.useState<From>("link");
  const [link, setLink] = React.useState("");
  const [path, setPath] = React.useState("");
  const [name, setName] = React.useState("");
  const [desc, setDesc] = React.useState("");
  const [body, setBody] = React.useState("");
  const [plugin, setPlugin] = React.useState("");
  const [found, setFound] = React.useState<LookAnswer | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const [act, acting] = useActions(
    (_what: string, go: () => Promise<void>) => go(),
    (e) => setError(errorMessage(e)),
  );
  const asked = (): LookFrom | null =>
    from === "link" && link.trim()
      ? { from, link: link.trim(), plugin: plugin || undefined }
      : from === "folder" && path.trim()
        ? { from, path: path.trim(), plugin: plugin || undefined }
        : from === "write" && name.trim() && desc.trim() && body.trim()
          ? { from, name: name.trim(), description: desc.trim(), body }
          : null;
  const generation = React.useRef(0);
  const [uploaded, setUploaded] = React.useState<File | null>(null);
  const invalidateLook = () => {
    generation.current += 1;
    setFound(null);
    setError(null);
  };
  const inspect = (go: () => Promise<LookAnswer>) => {
    const current = ++generation.current;
    act("look", async () => {
      setError(null);
      const next = await go();
      if (generation.current === current) setFound(next);
    });
  };
  const look = (e?: React.FormEvent) => {
    e?.preventDefault();
    if (from === "upload" && uploaded) {
      inspect(() => skillsApi.upload(agent, uploaded, plugin || undefined));
      return;
    }
    const a = asked();
    if (a) inspect(() => skillsApi.look(agent, a));
  };
  const upload = (file: File | undefined) => {
    if (!file) return;
    setUploaded(file);
    setPlugin("");
    inspect(() => skillsApi.upload(agent, file));
  };
  const pick = (f: From) => {
    invalidateLook();
    setFrom(f);
    setUploaded(null);
    setPlugin("");
  };
  const choices = found && "choose" in found ? found.choose : null;
  const draft = found && "draft" in found ? found : null;
  const looking = acting("look");
  return (
    <div className="sk-add">
      <form className="sk-source" onSubmit={look}>
        <div className="hx-modes sk-from" role="radiogroup" aria-label="From">
          {FROM.map((f) => (
            <button key={f.id} type="button" role="radio" aria-checked={from === f.id} className="hx-mode" data-mode="queue" data-from={f.id} onClick={() => pick(f.id)}>
              {f.icon}
              {f.label}
            </button>
          ))}
        </div>
        {from === "link" && (
          <div className="sk-inline">
            <input
              className="hx-input hx-mono"
              name="link"
              value={link}
              onChange={(e) => {
                invalidateLook();
                setLink(e.target.value);
              }}
              placeholder="github.com/owner/repo"
              spellCheck={false}
              autoCapitalize="off"
              aria-label="Link"
            />
            <Button type="submit" size="sm" variant="secondary" disabled={!asked()} {...sendingProps(looking)}>
              Look
            </Button>
          </div>
        )}
        {from === "folder" && (
          <div className="sk-inline">
            <span className="sk-machine hx-mono" title="The agent's machine">
              {machine === "local" ? "server" : machine}:
            </span>
            <input
              className="hx-input hx-mono"
              name="path"
              value={path}
              onChange={(e) => {
                invalidateLook();
                setPath(e.target.value);
              }}
              placeholder="~/skills"
              spellCheck={false}
              autoCapitalize="off"
              aria-label="Folder"
            />
            <Button type="submit" size="sm" variant="secondary" disabled={!asked()} {...sendingProps(looking)}>
              Look
            </Button>
          </div>
        )}
        {from === "upload" && (
          <label
            className="sk-drop"
            aria-busy={looking || undefined}
            onDragOver={(e) => e.preventDefault()}
            onDrop={(e) => {
              e.preventDefault();
              upload(e.dataTransfer.files[0]);
            }}
          >
            <FileUp size={14} />
            <span>Drop a .zip, .tar.gz or .md</span>
            <span className="sk-drop-pick">Choose</span>
            <input type="file" name="upload" className="sr-only" accept=".md,.zip,.tar.gz,.tgz" onChange={(e) => upload(e.target.files?.[0])} />
          </label>
        )}
        {from === "write" && (
          <>
            <div className="hx-row2">
              <input
                className="hx-input hx-mono"
                name="name"
                value={name}
                onChange={(e) => {
                  invalidateLook();
                  setName(e.target.value);
                }}
                placeholder="name"
                spellCheck={false}
                autoCapitalize="off"
                aria-label="Name"
              />
              <input
                className="hx-input"
                name="desc"
                value={desc}
                onChange={(e) => {
                  invalidateLook();
                  setDesc(e.target.value);
                }}
                placeholder="When to use it"
                aria-label="Description"
              />
            </div>
            <textarea
              className="hx-input hx-mono sk-body"
              name="body"
              rows={5}
              value={body}
              onChange={(e) => {
                invalidateLook();
                setBody(e.target.value);
              }}
              placeholder="# Steps"
              spellCheck={false}
              aria-label="Body"
            />
            <div className="sk-buttons">
              <Button type="submit" size="sm" variant="secondary" disabled={!asked()} {...sendingProps(looking)}>
                Look
              </Button>
            </div>
          </>
        )}
        {choices && (
          <div className="sk-inline">
            <select className="hx-select" value={plugin} onChange={(e) => setPlugin(e.target.value)} aria-label="Plugin">
              <option value="">Pick a plugin</option>
              {choices.map((c: SkillItem) => (
                <option key={c.name} value={c.name}>
                  {c.name}
                </option>
              ))}
            </select>
            <Button type="submit" size="sm" variant="secondary" disabled={!plugin} {...sendingProps(looking)}>
              Look
            </Button>
          </div>
        )}
      </form>
      {error && <div className="hx-bad sk-none">{error}</div>}
      {draft ? (
        <Preview key={draft.draft} d={draft} agent={agent} onInstalled={onInstalled} onCancel={onDone} />
      ) : (
        <div className="sk-buttons">
          <Button size="xs" variant="ghost" onClick={onDone}>
            Cancel
          </Button>
        </div>
      )}
    </div>
  );
}

/** A skill's words, read through Suspense: inline, kept, asked again when opened again. */
function Words({ id, read }: { id: string; read: () => Promise<string> }) {
  const text = useSuspenseResource(id, read).data;
  return <Prose text={text} className="sk-read" />;
}

/** One skill: its name and words, read in place (loading as it opens, W21-R15), and run with words of
 *  its own. */
function SkillLine({
  name,
  description,
  tag,
  words,
  read,
  run,
  hint,
}: {
  name: string;
  description?: string;
  tag?: string;
  words: string;
  read: () => Promise<string>;
  run?: (args?: string) => void;
  hint?: string;
}) {
  const [reading, setReading] = React.useState(false);
  const [running, setRunning] = React.useState(false);
  const [args, setArgs] = React.useState("");
  return (
    <li className="sk-skill">
      <div className="sk-skill-head">
        <code className="hx-skill-name hx-trunc">/{name}</code>
        {tag ? <span className="hx-tag">{tag}</span> : <span />}
        <span className="hx-mcp-actions">
          <button type="button" className="hx-link" aria-expanded={reading} onClick={() => setReading(!reading)}>
            {reading ? "Close" : "Read"}
          </button>
          <button type="button" className="hx-link" disabled={!run} aria-expanded={running} onClick={() => setRunning(!running)}>
            Run
          </button>
        </span>
      </div>
      {description && <div className="hx-skill-desc">{description}</div>}
      {running && run && (
        <form
          className="flex gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            run(args.trim() || undefined);
            setArgs("");
            setRunning(false);
          }}
        >
          <input className="hx-input" autoFocus value={args} onChange={(e) => setArgs(e.target.value)} placeholder={hint || "Arguments (optional)"} />
          <Button size="xs" type="submit">
            Run
          </Button>
        </form>
      )}
      {reading && (
        <Loadable label="Loading the skill" fallback={<div className="sk-read hx-dim">Reading…</div>}>
          <Words id={words} read={read} />
        </Loadable>
      )}
    </li>
  );
}

function PackageRow({ p, q, per, store, list }: { p: SkillPackage; q: string; per: PerPage; store: SessionStore; list: SuspenseResource<SkillsList> }) {
  const [open, setOpen] = React.useState(false);
  const [confirm, setConfirm] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const [act, acting] = useActions(
    (_what: string, go: () => Promise<void>) => go(),
    (e) => setError(errorMessage(e)),
  );
  const shown = q ? p.skills.filter((s) => `${p.name} ${s.name} ${s.description ?? ""}`.toLowerCase().includes(q)) : p.skills;
  const pg = usePage(shown, q, per);
  if (q && shown.length === 0) return null;
  const expanded = open || Boolean(q);
  const machines = p.machines;
  const state = machines.some((m) => m.state === "failed")
    ? "failed"
    : machines.some((m) => m.state === "setting-up" || m.state === "fetching")
      ? "setting-up"
      : machines.some((m) => m.state === "needs")
        ? "needs"
        : "ready";
  const notes = machines.filter((m) => (m.state === "needs" || m.state === "failed") && m.note).map((m) => `${m.note} on ${m.machine}`);
  const change = (what: string, go: () => Promise<unknown>) =>
    act(what, async () => {
      setError(null);
      await go();
      list.reload();
    });
  return (
    <li className="sk-pkg" data-off={!p.on || undefined}>
      <div className="sk-pkg-head">
        <button type="button" className="sk-pkg-name" aria-expanded={expanded} onClick={() => setOpen(!open)}>
          <ChevronRight size={12} className={cn("hx-chev", expanded && "rotate-90")} />
          <i className="dot" data-status={p.on ? DOT[state] : "cancelled"} aria-hidden="true" />
          <span className="hx-trunc">{p.name}</span>
        </button>
        <span className="sk-pkg-at hx-mono hx-trunc">
          {p.source.label} @ {p.sha.slice(0, 7)}
        </span>
        <button
          type="button"
          role="switch"
          aria-checked={p.on}
          aria-label={`${p.name} on`}
          className="hx-switch"
          {...sendingProps(acting("set"))}
          onClick={() => change("set", () => skillsApi.set(p.id, !p.on))}
        >
          <span />
        </button>
      </div>
      <div className="sk-pkg-sub">
        <span className="hx-meta-num">
          {p.skills.length} skills{p.agents.length ? ` · ${p.agents.length} agents` : ""} · {p.scope}
        </span>
        {p.on && state === "setting-up" && <span className="sk-state">setting up</span>}
        {p.on && notes.length > 0 && <span className={cn("sk-state", state === "failed" ? "hx-bad" : "hx-warn")}>{notes.join("; ")}</span>}
        {p.on && (state === "failed" || state === "needs") && (
          <button type="button" className="hx-link sk-update" {...sendingProps(acting("retry"))} onClick={() => change("retry", () => skillsApi.set(p.id, true))}>
            Retry setup
          </button>
        )}
        {p.update && (
          <button type="button" className="hx-link sk-update" {...sendingProps(acting("update"))} onClick={() => change("update", () => skillsApi.update(p.id))}>
            Update to {p.update.slice(0, 7)}
          </button>
        )}
      </div>
      {error && <div className="hx-bad sk-none sk-indent">{error}</div>}
      {expanded && (
        <div className="sk-pkg-body">
          <div className="sk-machines">
            {machines.map((m) => (
              <span key={m.machine} className="sk-machine-state" title={m.log || undefined}>
                <i className="dot" data-status={DOT[m.state] ?? "queued"} aria-hidden="true" />
                <span>{m.machine}</span>
                <span className="hx-dim">{m.state === "needs" || m.state === "failed" ? m.note || WORD[m.state] : WORD[m.state]}</span>
              </span>
            ))}
          </div>
          {p.setup.length > 0 && (
            <div className="sk-cmds">
              {p.setup.map((s) => (
                <span key={`${s.skill}:${s.command}`} className="hx-trunc">
                  <span className="hx-dim">{s.skill} · </span>
                  <code>{s.command}</code>
                </span>
              ))}
            </div>
          )}
          <ul
            className="sk-skills"
            data-paged={pg.many || undefined}
            style={pageStyle(
              pg.many,
              per,
              shown.some((s) => Boolean(s.description)),
            )}
          >
            {pg.rows.map((s) => (
              <SkillLine
                key={s.name}
                name={`${p.name}:${s.name}`}
                description={s.description}
                words={`skills/read/${p.id}/${p.sha}/${s.name}`}
                read={() => skillsApi.read(p.id, s.name).then((r) => r.content)}
                run={(args) => store.send({ type: "skill_run", name: `${p.name}:${s.name}`, args, id: clientId() })}
              />
            ))}
          </ul>
          {pg.many && <Pager back={pg.back} range={pg.range} on={pg.on} label={`${p.name} skills`} />}
          <div>
            {confirm ? (
              <span className="flex items-center gap-2 text-[12px]">
                Remove {p.name}?
                <Button size="xs" variant="danger" {...sendingProps(acting("remove"))} onClick={() => change("remove", () => skillsApi.remove(p.id))}>
                  Remove
                </Button>
                <Button size="xs" variant="ghost" onClick={() => setConfirm(false)}>
                  Keep
                </Button>
              </span>
            ) : (
              <Button size="xs" variant="ghost" onClick={() => setConfirm(true)}>
                <Trash2 /> Remove
              </Button>
            )}
          </div>
        </div>
      )}
    </li>
  );
}

/** The skills its harness found on its own: what kiln installed isn't listed twice. */
function Found({ state, store, q, per, installed }: { state: State | null; store: SessionStore; q: string; per: PerPage; installed: string[] }) {
  const known = state?.inventory?.skills;
  const skills = (state?.skills ?? []).filter((k) => !installed.some((p) => k.name.startsWith(`${p}:`)));
  const shown = skills.filter((k) => !q || `${k.name} ${k.description} ${k.source}`.toLowerCase().includes(q));
  const pg = usePage(shown, q, per);
  // Its harness's plugins, but those kiln installed (listed above).
  const plugins = (state?.plugins ?? []).filter((p) => !installed.includes(p.name));
  const none =
    known === "unsupported" || (state?.skills === undefined && !known)
      ? "This agent's harness doesn't tell kiln about its skills."
      : known === "loading" && !skills.length
        ? "Asking the agent for its skills…"
        : skills.length === 0
          ? "None."
          : shown.length === 0
            ? "No skills match."
            : null;
  return (
    <section>
      <div className="sk-eyebrow">
        <span className="hx-eyebrow">Found by its harness</span>
      </div>
      {none ? (
        <p className="sk-empty">{none}</p>
      ) : (
        <>
          <ul
            className="sk-skills sk-found"
            data-paged={pg.many || undefined}
            style={pageStyle(
              pg.many,
              per,
              shown.some((s) => Boolean(s.description)),
            )}
          >
            {pg.rows.map((k) => (
              <SkillLine
                key={`${k.source}:${k.name}`}
                name={k.name}
                description={k.description}
                tag={k.source}
                hint={k.args_hint}
                words={`harness/skill/${store.id}/${k.source}:${k.name}`}
                read={() => store.request((client) => ({ type: "skill_view", name: k.name, client }), 8000).then((v) => (v as { content?: string } | null)?.content ?? "Couldn't read this skill.")}
                run={k.user_invocable ? (args) => store.send({ type: "skill_run", name: k.name, args, id: clientId() }) : undefined}
              />
            ))}
          </ul>
          {pg.many && <Pager back={pg.back} range={pg.range} on={pg.on} label="Found skills" />}
        </>
      )}
      {plugins.length > 0 && (
        <>
          <div className="sk-eyebrow">
            <span className="hx-eyebrow">Its plugins</span>
          </div>
          <ul className="sk-skills sk-found">
            {plugins.map((p) => (
              <li key={p.name} className="sk-skill">
                <div className="sk-skill-head">
                  <span className="hx-skill-name hx-trunc">{p.name}</span>
                  {p.source ? <span className="hx-tag">{p.source}</span> : <span />}
                  <span className={cn("text-[11px]", p.enabled ? "hx-good" : "hx-dim")}>{p.enabled ? "on" : "off"}</span>
                </div>
                {(p.skills?.length || p.commands?.length) && <div className="hx-skill-desc">{[...(p.skills ?? []).map((s) => `/${s}`), ...(p.commands ?? []).map((c) => `/${c}`)].join("  ")}</div>}
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}

function Panel({ state, store, agentId }: { state: State | null; store: SessionStore; agentId: string }) {
  const list = useSuspenseResource(`skills/${agentId}`, () => skillsApi.list(agentId), { pollMs: 15_000 });
  const [q, setQ] = React.useState("");
  const [adding, setAdding] = React.useState(false);
  const [per, setPer] = React.useState<PerPage>(10);
  const { packages, machine, installs, native_components = true } = list.data;
  const ql = q.trim().toLowerCase();
  const names = packages.map((p) => p.name);
  const found = (state?.skills ?? []).filter((k) => !names.some((p) => k.name.startsWith(`${p}:`))).length;
  const total = packages.reduce((n, p) => n + p.skills.length, 0) + found;
  return (
    <div className="hx-stack sk-panel">
      <div className="sk-head">
        <SearchField label="Search skills" placeholder={`Search ${total} skills`} value={q} onChange={setQ} />
        <Button size="sm" variant="secondary" disabled={adding || !installs} onClick={() => setAdding(true)}>
          <Plus /> Add
        </Button>
      </div>
      {!installs && <p className="sk-empty">Upgrade kiln on {machine} to install skills.</p>}
      {!native_components && packages.some((p) => p.agents.length || p.hooks.length || p.mcp.length) && (
        <p className="sk-empty">Skills and setup work in this harness. Claude Code agent definitions, hooks, and package MCP configuration activate only in Claude Code.</p>
      )}
      {adding && (
        <AddSkills
          agent={agentId}
          machine={machine}
          onInstalled={() => {
            setAdding(false);
            list.reload();
          }}
          onDone={() => setAdding(false)}
        />
      )}
      <section>
        <div className="sk-eyebrow">
          <span className="hx-eyebrow">Installed by kiln</span>
          <PerPicker per={per} onPer={setPer} label="Skills a page" />
        </div>
        {packages.length === 0 ? (
          <p className="sk-empty">None yet.</p>
        ) : (
          <ul className="sk-pkgs">
            {packages.map((p) => (
              <PackageRow key={p.id} p={p} q={ql} per={per} store={store} list={list} />
            ))}
          </ul>
        )}
      </section>
      <Found state={state} store={store} q={ql} per={per} installed={names} />
    </div>
  );
}

export function SkillsPanel({ state, store, agentId }: { state: State | null; store: SessionStore; agentId?: string }) {
  if (!agentId) return <Found state={state} store={store} q="" per={10} installed={[]} />;
  return (
    <Loadable label="its skills">
      <Panel state={state} store={store} agentId={agentId} />
    </Loadable>
  );
}
