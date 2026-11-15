/**
 * Settings → Your secrets (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P4.7; `/api/me/secrets`): a
 * person's own, theirs alone, for their Agentfiles' sources to fetch with. A value is never shown
 * again once set: its row has its size, and when it was set and last used. An Agentfile names one as
 * `LABEL kiln.secret.<ID>=user:<NAME>` (its sources' credential, chosen by name there).
 */
import * as React from "react";
import { mySecretsApi, mySecretsRes } from "@/api/secrets";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Confirm } from "@/components/ui/misc";
import { SettingsGroup } from "@/components/ui/settings";
import { useAction } from "@/lib/action";
import { useRes } from "@/lib/data";
import { formatBytes, relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";

const NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

export function MySecretsHold() {
  return (
    <SettingsGroup id="secrets.mine" title="Your secrets" words="secrets mine user agentfile credentials">
      <Hold height={160} />
    </SettingsGroup>
  );
}

export function MySecretsGroup() {
  const res = useRes(mySecretsRes);
  const list = res.data;
  const [name, setName] = React.useState("");
  const [value, setValue] = React.useState("");
  const [problem, setProblem] = React.useState<string | null>(null);
  const [removing, setRemoving] = React.useState<string | null>(null);
  const upper = name.trim().toUpperCase();
  const ready = NAME.test(name.trim()) && value.length > 0;
  // Its answer empties the form, so the form is held as it is until then (W21-R15).
  const [save, saving] = useAction(
    async () => {
      setProblem(null);
      await mySecretsApi.set(upper, value);
      res.reload();
      setName("");
      setValue("");
    },
    (e) => setProblem(errorMessage(e)),
  );
  // Gone at once (W21-R15), and back, saying why, if the server keeps it.
  const remove = (secret: string) => {
    setProblem(null);
    setRemoving(null);
    res.change((d) => d.filter((x) => x.name !== secret));
    mySecretsApi.remove(secret).catch((e: unknown) => {
      setProblem(errorMessage(e));
      res.reload();
    });
  };
  return (
    <SettingsGroup
      id="secrets.mine"
      title="Your secrets"
      about="Yours alone, for your Agentfiles' sources to fetch with. A value isn't shown again once set."
      words="secrets mine user agentfile credentials"
    >
      {list.length === 0 ? (
        <p className="st-note">No secrets of yours yet.</p>
      ) : (
        <ul className="st-list" aria-label="Your secrets">
          {list.map((x) => (
            <li key={x.name}>
              <div className="st-item">
                <span className="st-item-name font-mono text-[12.5px]">{x.name}</span>
                <span className="st-item-meta">
                  {formatBytes(x.size)} · set {relativeTime(x.updated_at)} · {x.last_used_at ? `last used ${relativeTime(x.last_used_at)}` : "never used"}
                </span>
                <span className="st-item-acts">
                  <Button size="xs" variant="ghost" aria-expanded={removing === x.name} onClick={() => setRemoving(removing === x.name ? null : x.name)}>
                    Remove
                  </Button>
                </span>
              </div>
              {removing === x.name && (
                <Confirm
                  label={`Remove ${x.name}`}
                  tone="danger"
                  className="mt-3"
                  actions={
                    <>
                      <Button size="xs" variant="primary" onClick={() => remove(x.name)}>
                        Remove {x.name}
                      </Button>
                      <Button size="xs" variant="ghost" onClick={() => setRemoving(null)}>
                        Keep it
                      </Button>
                    </>
                  }
                >
                  <p>A build of an Agentfile naming it is refused from then on.</p>
                </Confirm>
              )}
            </li>
          ))}
        </ul>
      )}
      <form
        noValidate
        className="mt-4"
        autoComplete="off"
        onSubmit={(e) => {
          e.preventDefault();
          if (ready) save();
        }}
      >
        <div className="st-fields">
          <div>
            <label htmlFor="st-mine-name" className="field-label">
              Name
            </label>
            <div className="field">
              <input id="st-mine-name" type="text" spellCheck={false} autoCapitalize="characters" placeholder="GH_TOKEN" value={name} disabled={saving} onChange={(e) => setName(e.target.value)} />
            </div>
          </div>
          <div>
            <label htmlFor="st-mine-value" className="field-label">
              Value
            </label>
            <div className="field">
              <input id="st-mine-value" type="password" autoComplete="new-password" value={value} disabled={saving} onChange={(e) => setValue(e.target.value)} />
            </div>
          </div>
        </div>
        <div className="mt-3 flex items-center justify-end gap-3">
          {list.some((x) => x.name === upper) && <span className="st-note mt-0">Saving replaces {upper}&rsquo;s value.</span>}
          <Button type="submit" size="xs" variant="secondary" pending={saving} disabled={!ready}>
            Save the secret
          </Button>
        </div>
      </form>
      {problem && (
        <p className="st-note" data-tone="error" role="status">
          {problem}
        </p>
      )}
    </SettingsGroup>
  );
}
