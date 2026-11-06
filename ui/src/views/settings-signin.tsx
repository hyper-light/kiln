/**
 * Settings → Sign-in (docs/ci.md W10): one's passkeys, made with this browser's authenticator and
 * removed, and a person's own password (the owner's is the server's, beside these).
 */
import * as React from "react";
import { passkeysHere, signin, type Passkey } from "@/api/signin";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Confirm } from "@/components/ui/misc";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { toast } from "@/components/ui/toast";
import { useAction, useHold } from "@/lib/action";
import { useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";

export const passkeysRes: Res<Passkey[]> = { key: "me/passkeys", fetch: (s) => signin.passkeys(s) };
export const myPasswordRes: Res<{ set: boolean }> = { key: "me/password", fetch: (s) => signin.myPassword(s) };

const PASSKEYS = {
  about: "Sign in with your phone, computer or password manager: nothing to type, and nothing a site that isn't this one can use.",
  words: "passkey passkeys webauthn fingerprint face touch security key",
};
const PASSWORD = { about: "Sign in with your name and it, from any device that reaches this server.", words: "password my own name" };

type Note = { tone?: "error"; text: string } | null;

export function PasskeysHold() {
  return (
    <SettingsGroup id="signin.passkeys" title="Passkeys" about={PASSKEYS.about} words={PASSKEYS.words}>
      <Hold height={88} />
    </SettingsGroup>
  );
}

export function PasskeysGroup() {
  const res = useRes(passkeysRes);
  const list = res.data;
  const [name, setName] = React.useState("");
  const [removing, setRemoving] = React.useState<string | null>(null);
  const [note, setNote] = React.useState<Note>(null);
  // Gone at once (W21-R15), and back, saying why, if the server keeps it; said in a toast, as a
  // passkey being made meanwhile has the line under the form.
  const remove = (k: Passkey) => {
    setRemoving(null);
    res.change((l) => l.filter((x) => x.id !== k.id));
    signin.removePasskey(k.id).then(
      () => toast({ title: `Removed ${k.name}`, tone: "info" }),
      (e: unknown) => {
        toast.error(`Couldn't remove ${k.name}`, errorMessage(e));
        res.reload();
      },
    );
  };
  // Its answer empties the name, so the name is held as it is until then (W21-R15).
  const [make, making] = useAction(
    async () => {
      const called = name.trim() || "A passkey";
      setNote(null);
      await signin.addPasskey(called);
      res.reload();
      setNote({ text: `Made ${called}. Sign in with it from the sign-in page.` });
      setName("");
    },
    (e) => setNote({ tone: "error", text: e instanceof DOMException && e.name === "NotAllowedError" ? "No passkey was made: it was cancelled, or it timed out." : errorMessage(e) }),
  );
  return (
    <SettingsGroup id="signin.passkeys" title="Passkeys" about={PASSKEYS.about} words={PASSKEYS.words}>
      {list.length === 0 ? (
        <p className="st-note">None yet.</p>
      ) : (
        <ul className="st-list" aria-label="Passkeys">
          {list.map((k) => (
            <li key={k.id}>
              <div className="st-item">
                <span className="st-item-name">{k.name}</span>
                <span className="st-item-meta">
                  made {relativeTime(k.created_at)} · {k.last_used_at ? `last used ${relativeTime(k.last_used_at)}` : "never used"}
                  {k.backed_up ? " · synced" : ""} · {k.rp_id}
                </span>
                <span className="st-item-acts">
                  <Button size="xs" variant="ghost" aria-expanded={removing === k.id} onClick={() => setRemoving(removing === k.id ? null : k.id)}>
                    Remove
                  </Button>
                </span>
              </div>
              {removing === k.id && (
                <Confirm
                  label={`Remove ${k.name}`}
                  tone="danger"
                  className="mt-3"
                  actions={
                    <>
                      <Button size="xs" variant="primary" onClick={() => remove(k)}>
                        Remove {k.name}
                      </Button>
                      <Button size="xs" variant="ghost" onClick={() => setRemoving(null)}>
                        Keep it
                      </Button>
                    </>
                  }
                >
                  <p>It signs you in no more. Remove it from its device too.</p>
                </Confirm>
              )}
            </li>
          ))}
        </ul>
      )}
      {passkeysHere() ? (
        <form
          noValidate
          className="mt-4"
          onSubmit={(e) => {
            e.preventDefault();
            make();
          }}
        >
          <SettingRow label="A new one" htmlFor="st-passkey-name" help="Name it for the device or password manager that keeps it.">
            <div className="st-control-line">
              <div className="field st-wide">
                <input id="st-passkey-name" type="text" placeholder="Work laptop" maxLength={80} value={name} disabled={making} onChange={(e) => setName(e.target.value)} />
              </div>
              <Button type="submit" size="xs" variant="secondary" pending={making}>
                Make a passkey
              </Button>
            </div>
          </SettingRow>
        </form>
      ) : (
        <p className="st-note">This browser can&rsquo;t make passkeys here: they need a secure (https) page.</p>
      )}
      {note && (
        <p className="st-note" data-tone={note.tone} role="status">
          {note.text}
        </p>
      )}
    </SettingsGroup>
  );
}

export function MyPasswordHold() {
  return (
    <SettingsGroup id="signin.mine" title="Your password" about={PASSWORD.about} words={PASSWORD.words}>
      <Hold height={88} />
    </SettingsGroup>
  );
}

export function MyPasswordGroup() {
  const res = useRes(myPasswordRes);
  const has = res.data.set;
  const [current, setCurrent] = React.useState("");
  const [next, setNext] = React.useState("");
  const [note, setNote] = React.useState<Note>(null);
  const save = async (password: string, done: string) => {
    setNote(null);
    const r = await signin.setMyPassword(password, current);
    res.setData(() => r);
    setCurrent("");
    setNext("");
    setNote({ text: done });
  };
  const failed = (e: unknown) => setNote({ tone: "error", text: errorMessage(e) });
  const hold = useHold(failed);
  const change = () => hold.change("change", () => save(next, has ? "Changed." : "Set: sign in with your name and it."));
  const remove = () => hold.change("remove", () => save("", "Removed: sign in with links, a host or a passkey."));
  // One change to the password at a time, and either answer empties the fields: they're held as
  // they are while it's on its way (W21-R15).
  const busy = hold.going;
  return (
    <SettingsGroup id="signin.mine" title="Your password" about={PASSWORD.about} words={PASSWORD.words} actions={<span className="st-value">{has ? "Set" : "None"}</span>}>
      <form
        noValidate
        autoComplete="on"
        onSubmit={(e) => {
          e.preventDefault();
          if (next && !hold.held("change")) change();
        }}
      >
        {has && (
          <SettingRow label="Current" htmlFor="st-my-current" help="Changing or removing it takes this.">
            <div className="field st-wide">
              <input id="st-my-current" type="password" autoComplete="current-password" value={current} disabled={busy} onChange={(e) => setCurrent(e.target.value)} />
            </div>
          </SettingRow>
        )}
        <SettingRow label={has ? "New" : "Password"} htmlFor="st-my-new" help="8 characters at least.">
          <div className="field st-wide">
            <input id="st-my-new" type="password" autoComplete="new-password" value={next} disabled={busy} onChange={(e) => setNext(e.target.value)} />
          </div>
        </SettingRow>
        <div className="mt-3 flex justify-end gap-2">
          {has && (
            <Button type="button" size="xs" variant="ghost" pending={hold.doing("remove")} disabled={!current || hold.held("remove")} onClick={remove}>
              Remove it
            </Button>
          )}
          <Button type="submit" size="xs" variant="secondary" pending={hold.doing("change")} disabled={hold.held("change") || next.length < 8 || (has && !current)}>
            {has ? "Change it" : "Set it"}
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
