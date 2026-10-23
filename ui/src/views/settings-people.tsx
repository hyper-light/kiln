/**
 * Settings → People (docs/ci.md W10-R01 to R03, W21-R11): the people who sign in here, what
 * they may do where (their grants), a sign-in link for each, and the roles grants name. A person
 * removed signs in no more, and their tokens stop working.
 */
import * as React from "react";
import { people, type Person } from "@/api/people";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, SelectField } from "@/components/ui/misc";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { reposRes, useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";

export const peopleRes: Res<Person[]> = { key: "people", fetch: (s) => people.list(s) };
export const rolesRes: Res<Awaited<ReturnType<typeof people.roles>>> = { key: "people/roles", fetch: (s) => people.roles(s) };

type Note = { tone?: "error"; text: string } | null;

/** A grant's text from a role and where it holds (`server`, `org:acme`, `repo:acme/app`). */
export const grantText = (role: string, where: string) => (where === "server" ? role : `${role}@${where}`);

/** Where a grant may hold: the whole server, each organization, each repository. */
export function useWheres(): { value: string; label: string }[] {
  const repos = useRes(reposRes).data;
  const orgs = [...new Set(repos.map((r) => r.name.split("/")[0]).filter(Boolean))].sort();
  return [{ value: "server", label: "The whole server" }, ...orgs.map((o) => ({ value: `org:${o}`, label: `Organization ${o}` })), ...repos.map((r) => ({ value: `repo:${r.name}`, label: r.name }))];
}

/** A role and where it holds, as two selects. */
export function GrantPicker({ roles, role, where, onRole, onWhere }: { roles: string[]; role: string; where: string; onRole: (r: string) => void; onWhere: (w: string) => void }) {
  const wheres = useWheres();
  return (
    <div className="st-control-line">
      <SelectField label="Role" hideLabel className="min-w-0 flex-1" value={role} onChange={onRole} options={roles.map((r) => ({ value: r, label: r }))} />
      <SelectField label="Where" hideLabel className="min-w-0 flex-1" value={where} onChange={onWhere} options={wheres} />
    </div>
  );
}

export function PeopleHold() {
  return (
    <SettingsGroup id="people.people" title="People" words="people users members grants roles invite link">
      <Hold height={160} />
    </SettingsGroup>
  );
}

export function PeopleGroup() {
  const res = useRes(peopleRes);
  const list = res.data;
  const roles = useRes(rolesRes).data.roles.map((r) => r.name);
  const [open, setOpen] = React.useState<string | null>(null);
  const [removing, setRemoving] = React.useState<string | null>(null);
  const [link, setLink] = React.useState<{ id: string; url: string } | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [note, setNote] = React.useState<Note>(null);
  const [name, setName] = React.useState("");
  const [email, setEmail] = React.useState("");
  const [role, setRole] = React.useState("developer");
  const [where, setWhere] = React.useState("server");
  const act = async (f: () => Promise<unknown>, done: string | null) => {
    setBusy(true);
    setNote(null);
    try {
      await f();
      res.reload();
      if (done) setNote({ text: done });
      return true;
    } catch (e) {
      setNote({ tone: "error", text: errorMessage(e) });
      return false;
    } finally {
      setBusy(false);
    }
  };
  return (
    <SettingsGroup
      id="people.people"
      title="People"
      about="Who signs in here, and what each may do where. Each signs in with a link, a host, a passkey or a password."
      words="people users members grants roles invite link"
    >
      {list.length === 0 ? (
        <p className="st-note">Nobody yet: only the owner signs in.</p>
      ) : (
        <ul className="st-list" aria-label="People">
          {list.map((p) => (
            <li key={p.id}>
              <div className="st-item">
                <span className="st-item-name">{p.name}</span>
                <span className="st-item-meta font-mono text-[11.5px]">{p.grants.length ? p.grants.join(" · ") : "no grants: they may do nothing"}</span>
                <span className="st-item-acts">
                  <Button size="xs" variant="ghost" disabled={busy} onClick={() => void act(async () => setLink({ id: p.id, url: (await people.link(p.id)).url }), null)}>
                    Sign-in link
                  </Button>
                  <Button size="xs" variant="ghost" aria-expanded={open === p.id} onClick={() => setOpen(open === p.id ? null : p.id)}>
                    Grants
                  </Button>
                  <Button size="xs" variant="ghost" disabled={busy} aria-expanded={removing === p.id} onClick={() => setRemoving(removing === p.id ? null : p.id)}>
                    Remove
                  </Button>
                </span>
              </div>
              {link?.id === p.id && <Copyable label={`${p.name}'s sign-in link`} value={link.url} help="It works once, for a day: send it to them however you like." />}
              {open === p.id && (
                <Loadable label={`${p.name}'s grants`} fallback={<Hold height={80} />}>
                  <PersonGrants person={p} roles={roles} onChanged={() => res.reload()} />
                </Loadable>
              )}
              {removing === p.id && (
                <Confirm
                  label={`Remove ${p.name}`}
                  tone="danger"
                  className="mt-3"
                  actions={
                    <>
                      <Button size="xs" variant="primary" disabled={busy} onClick={() => void act(() => people.remove(p.id), `Removed ${p.name}.`).then((ok) => ok && setRemoving(null))}>
                        Remove {p.name}
                      </Button>
                      <Button size="xs" variant="ghost" disabled={busy} onClick={() => setRemoving(null)}>
                        Keep them
                      </Button>
                    </>
                  }
                >
                  <p>They&rsquo;re signed out everywhere, and their tokens stop working. Their name isn&rsquo;t given to anyone else.</p>
                </Confirm>
              )}
            </li>
          ))}
        </ul>
      )}
      <form
        noValidate
        className="mt-4"
        onSubmit={(e) => {
          e.preventDefault();
          const n = name.trim();
          if (n)
            void act(() => people.add(n, [grantText(role, where)], email.trim() || null), `Added ${n}. Send them a sign-in link.`).then((ok) => {
              if (ok) {
                setName("");
                setEmail("");
              }
            });
        }}
      >
        <SettingRow label="Someone new" htmlFor="st-person-name" help="Their name here, and their email if they sign in with a host (it finds them).">
          <div className="st-control-line">
            <div className="field st-wide">
              <input id="st-person-name" type="text" autoComplete="off" spellCheck={false} placeholder="ada" value={name} disabled={busy} onChange={(e) => setName(e.target.value)} />
            </div>
            <div className="field st-wide">
              <input aria-label="Their email" type="email" autoComplete="off" placeholder="ada@example.com" value={email} disabled={busy} onChange={(e) => setEmail(e.target.value)} />
            </div>
          </div>
        </SettingRow>
        <SettingRow label="May" help="One grant to start: more under Grants.">
          <GrantPicker roles={roles} role={role} where={where} onRole={setRole} onWhere={setWhere} />
        </SettingRow>
        <div className="mt-3 flex justify-end">
          <Button type="submit" size="xs" variant="secondary" disabled={busy || !name.trim()}>
            Add them
          </Button>
        </div>
      </form>
      {note && (
        <p className="st-note" data-tone={note.tone} role="status">
          {note.text}
        </p>
      )}
    </SettingsGroup>
  );
}

/** A value to copy, read-only, with a Copy button. */
export function Copyable({ label, value, help }: { label: string; value: string; help: string }) {
  const [copied, setCopied] = React.useState(false);
  return (
    <div className="mt-3">
      <div className="st-control-line">
        <div className="field st-wide">
          <input aria-label={label} type="text" readOnly value={value} className="font-mono text-[12px]" onFocus={(e) => e.target.select()} />
        </div>
        <Button type="button" size="xs" variant="secondary" onClick={() => void navigator.clipboard.writeText(value).then(() => setCopied(true))}>
          {copied ? "Copied" : "Copy"}
        </Button>
      </div>
      <p className="st-note mt-2">{help}</p>
    </div>
  );
}

function PersonGrants({ person, roles, onChanged }: { person: Person; roles: string[]; onChanged: () => void }) {
  const res = useSuspenseResource(`people/grants/${person.id}`, (s) => people.grants(person.id, s));
  const [role, setRole] = React.useState("viewer");
  const [where, setWhere] = React.useState("server");
  const [busy, setBusy] = React.useState(false);
  const [note, setNote] = React.useState<Note>(null);
  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setNote(null);
    try {
      await f();
      res.reload();
      onChanged();
    } catch (e) {
      setNote({ tone: "error", text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="mt-3 border-l border-[#262932] pl-4">
      {res.data.length > 0 && (
        <ul className="st-list" aria-label={`${person.name}'s grants`}>
          {res.data.map((g) => (
            <li key={g.id}>
              <div className="st-item">
                <span className="st-item-name font-mono text-[12px]">
                  {g.deny ? "!" : ""}
                  {g.what}
                  {g.scope === "server" ? "" : `@${g.scope}`}
                </span>
                <span className="st-item-meta">
                  by {g.created_by} · {relativeTime(g.created_at)}
                </span>
                <span className="st-item-acts">
                  <Button size="xs" variant="ghost" disabled={busy} onClick={() => void act(() => people.ungrant(g.id))}>
                    Take it back
                  </Button>
                </span>
              </div>
            </li>
          ))}
        </ul>
      )}
      <form
        noValidate
        className="mt-3"
        onSubmit={(e) => {
          e.preventDefault();
          void act(() => people.grant(person.id, grantText(role, where)));
        }}
      >
        <div className="st-control-line">
          <GrantPicker roles={roles} role={role} where={where} onRole={setRole} onWhere={setWhere} />
          <Button type="submit" size="xs" variant="secondary" disabled={busy}>
            Grant
          </Button>
        </div>
      </form>
      {note && (
        <p className="st-note" data-tone={note.tone} role="status">
          {note.text}
        </p>
      )}
    </div>
  );
}

export function RolesHold() {
  return (
    <SettingsGroup id="people.roles" title="Roles" words="roles capabilities custom">
      <Hold height={120} />
    </SettingsGroup>
  );
}

export function RolesGroup() {
  const res = useRes(rolesRes);
  const { roles, capabilities } = res.data;
  const [name, setName] = React.useState("");
  const [caps, setCaps] = React.useState<string[]>([]);
  const [busy, setBusy] = React.useState(false);
  const [note, setNote] = React.useState<Note>(null);
  const act = async (f: () => Promise<unknown>, done: string) => {
    setBusy(true);
    setNote(null);
    try {
      await f();
      res.reload();
      setNote({ text: done });
      return true;
    } catch (e) {
      setNote({ tone: "error", text: errorMessage(e) });
      return false;
    } finally {
      setBusy(false);
    }
  };
  return (
    <SettingsGroup id="people.roles" title="Roles" about="What each role a grant names may do. The built-in ones are kiln's; yours are any capabilities together." words="roles capabilities custom">
      <ul className="st-list" aria-label="Roles">
        {roles.map((r) => (
          <li key={r.name}>
            <div className="st-item">
              <span className="st-item-name">{r.name}</span>
              <span className="st-item-meta font-mono text-[11.5px]">{r.capabilities.join(" · ")}</span>
              <span className="st-item-acts">
                {r.builtin ? (
                  <span className="st-item-meta">built in</span>
                ) : (
                  <Button size="xs" variant="ghost" disabled={busy} onClick={() => void act(() => people.removeRole(r.name), `Removed ${r.name}.`)}>
                    Remove
                  </Button>
                )}
              </span>
            </div>
          </li>
        ))}
      </ul>
      <form
        noValidate
        className="mt-4"
        onSubmit={(e) => {
          e.preventDefault();
          const n = name.trim();
          if (n && caps.length)
            void act(() => people.putRole(n, caps), `Saved ${n}.`).then((ok) => {
              if (ok) {
                setName("");
                setCaps([]);
              }
            });
        }}
      >
        <SettingRow label="A role of your own" htmlFor="st-role-name" help="Saving one of the same name replaces it.">
          <div className="field st-wide">
            <input id="st-role-name" type="text" spellCheck={false} placeholder="releaser" value={name} disabled={busy} onChange={(e) => setName(e.target.value)} />
          </div>
        </SettingRow>
        <SettingRow label="May">
          <fieldset className="flex flex-wrap gap-x-4 gap-y-2" disabled={busy}>
            <legend className="sr-only">Capabilities</legend>
            {capabilities.map((c) => (
              <label key={c} className="flex items-center gap-2 font-mono text-[12px]">
                <input type="checkbox" className="check" checked={caps.includes(c)} onChange={(e) => setCaps(e.target.checked ? [...caps, c] : caps.filter((x) => x !== c))} />
                {c}
              </label>
            ))}
          </fieldset>
        </SettingRow>
        <div className="mt-3 flex justify-end">
          <Button type="submit" size="xs" variant="secondary" disabled={busy || !name.trim() || caps.length === 0}>
            Save the role
          </Button>
        </div>
      </form>
      {note && (
        <p className="st-note" data-tone={note.tone} role="status">
          {note.text}
        </p>
      )}
    </SettingsGroup>
  );
}
