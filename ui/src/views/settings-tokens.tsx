/**
 * Settings → Tokens (docs/ci.md W10-R03, W21-R11): one's personal access tokens (everyone's, for
 * those who manage tokens), each doing at most what its grant and its person both may; a new one
 * shown once; any revoked.
 */
import * as React from "react";
import { people, type Minted, type Token } from "@/api/people";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Confirm, SelectField } from "@/components/ui/misc";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { Copyable, GrantPicker, grantText, rolesRes } from "@/views/settings-people";

export const tokensRes: Res<Token[]> = { key: "people/tokens", fetch: (s) => people.tokens(s) };

const LIVES = [7, 30, 90, 366].map((d) => ({ value: String(d), label: `${d} days` }));

type Note = { tone?: "error"; text: string } | null;

/** A token's state, in words. */
function lives(t: Token): string {
  const now = Date.now() / 1000;
  if (t.revoked_at) return `revoked ${relativeTime(t.revoked_at)}`;
  if (t.expires_at && t.expires_at <= now) return `expired ${relativeTime(t.expires_at)}`;
  return t.expires_at ? `ends ${relativeTime(t.expires_at)}` : "doesn't end";
}

export function TokensHold() {
  return (
    <SettingsGroup id="tokens.mine" title="Personal access tokens" words="tokens pat api cli access">
      <Hold height={120} />
    </SettingsGroup>
  );
}

export function TokensGroup() {
  const res = useRes(tokensRes);
  const roles = useRes(rolesRes).data.roles.map((r) => r.name);
  const [note, setNote] = React.useState<Note>(null);
  const [made, setMade] = React.useState<Minted | null>(null);
  const [removing, setRemoving] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [what, setWhat] = React.useState("");
  const [role, setRole] = React.useState("viewer");
  const [where, setWhere] = React.useState("server");
  const [days, setDays] = React.useState("90");
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
      id="tokens.mine"
      title="Personal access tokens"
      about="For the CLI, scripts and other tools. Each does at most what its grant and its person both may, until it ends or is revoked."
      words="tokens pat api cli access revoke"
    >
      {res.data.length === 0 ? (
        <p className="st-note">None yet.</p>
      ) : (
        <ul className="st-list" aria-label="Tokens">
          {res.data.map((t) => (
            <li key={t.id}>
              <div className="st-item">
                <span className="st-item-name font-mono text-[12px]">{t.note || t.masked}</span>
                <span className="st-item-meta">
                  {t.kind === "agent" ? `an agent's, for ${t.on_behalf_of ?? "?"}` : t.principal} · {t.grants.join(" · ")} · {lives(t)} ·{" "}
                  {t.last_used_at ? `used ${relativeTime(t.last_used_at)}` : "never used"}
                </span>
                {!t.revoked_at && (
                  <span className="st-item-acts">
                    <Button size="xs" variant="ghost" disabled={busy} aria-expanded={removing === t.id} onClick={() => setRemoving(removing === t.id ? null : t.id)}>
                      Revoke
                    </Button>
                  </span>
                )}
              </div>
              {removing === t.id && (
                <Confirm
                  label={`Revoke ${t.note || t.masked}`}
                  tone="danger"
                  className="mt-3"
                  actions={
                    <>
                      <Button size="xs" variant="primary" disabled={busy} onClick={() => void act(() => people.revoke(t.id), "Revoked.").then((ok) => ok && setRemoving(null))}>
                        Revoke it
                      </Button>
                      <Button size="xs" variant="ghost" disabled={busy} onClick={() => setRemoving(null)}>
                        Keep it
                      </Button>
                    </>
                  }
                >
                  <p>Whatever uses it is refused from now on.</p>
                </Confirm>
              )}
            </li>
          ))}
        </ul>
      )}
      {made && <Copyable label="The new token" value={made.token} help="Copy it now: it isn't shown again." />}
      <form
        noValidate
        className="mt-4"
        onSubmit={(e) => {
          e.preventDefault();
          void act(async () => setMade(await people.mint(what.trim(), [grantText(role, where)], Number(days))), null).then((ok) => {
            if (ok) setWhat("");
          });
        }}
      >
        <SettingRow label="A new one" htmlFor="st-token-what" help="What it's for, so you know it later.">
          <div className="field st-wide">
            <input id="st-token-what" type="text" placeholder="Release script" maxLength={120} value={what} disabled={busy} onChange={(e) => setWhat(e.target.value)} />
          </div>
        </SettingRow>
        <SettingRow label="May" help="owner: everything you may.">
          <GrantPicker roles={roles} role={role} where={where} onRole={setRole} onWhere={setWhere} />
        </SettingRow>
        <SettingRow label="Lasts">
          <SelectField label="Lasts" hideLabel className="st-wide" value={days} onChange={setDays} options={LIVES} />
        </SettingRow>
        <div className="mt-3 flex justify-end">
          <Button type="submit" size="xs" variant="secondary" disabled={busy}>
            Make the token
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
