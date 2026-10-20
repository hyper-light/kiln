/**
 * Settings → CI → Secrets and variables (docs/ci.md W21-R11, W09): a scope chosen first (the
 * server, an organization, a repository, or one of its workflows or environments written out),
 * then that scope's secrets and variables. A secret's value is never shown again once set: its
 * row has when it was set and last used, or where a brokered one comes from. A narrower scope's
 * win in the legs that see both.
 */
import * as React from "react";
import { ci, type CiSecretMeta, type CiVariable } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, SelectField } from "@/components/ui/misc";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { reposRes, useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { useAuth } from "@/lib/auth";

const NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;
/** A workflow's or an environment's scope, written out. */
const WRITTEN = /^(workflow:[^:\s]+\/[^:\s]+:\S+|env(ironment)?:[^:\s]+\/[^:\s]+:\S+)$/;

export function CiSecretsHold() {
  return (
    <SettingsGroup id="ci.secrets" title="Secrets and variables" words="secrets variables scope">
      <Hold height={160} />
    </SettingsGroup>
  );
}

/** The scope picker, then the scope's secrets and its variables. */
export function CiSecretsGroups() {
  const repos = useRes(reposRes).data;
  const [scope, setScope] = React.useState("server");
  const [written, setWritten] = React.useState("");
  const owners = [...new Set(repos.map((r) => r.name.split("/")[0] ?? "").filter(Boolean))].sort();
  const options = [
    { value: "server", label: "The server: every repository" },
    ...owners.map((o) => ({ value: `org:${o}`, label: `Organization ${o}` })),
    ...repos.map((r) => ({ value: `repo:${r.name}`, label: `Repository ${r.name}` })),
  ];
  const writtenOk = WRITTEN.test(written.trim());
  return (
    <>
      <SettingsGroup
        id="ci.secrets"
        title="Secrets and variables"
        about="What runs read as secrets.NAME and vars.NAME: a scope's own, the narrowest winning in each leg. A secret's value isn't shown again once set."
        words="secrets variables scope organization repository workflow environment"
      >
        <SettingRow label="Scope" help="Choose one, or write a workflow's or an environment's below.">
          <SelectField
            label="Scope"
            hideLabel
            className="st-wide"
            value={options.some((o) => o.value === scope) ? scope : ""}
            onChange={(v) => {
              setScope(v);
              setWritten("");
            }}
            options={options.some((o) => o.value === scope) ? options : [{ value: "", label: scope }, ...options]}
          />
        </SettingRow>
        <form
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            if (writtenOk) setScope(written.trim());
          }}
        >
          <SettingRow label="A workflow or an environment" htmlFor="st-ci-scope" help="workflow:acme/app:.github/workflows/ci.yml, or env:acme/app:production">
            <div className="st-control-line">
              <div className="field st-wide">
                <input id="st-ci-scope" type="text" spellCheck={false} placeholder="env:acme/app:production" value={written} onChange={(e) => setWritten(e.target.value)} />
              </div>
              <Button type="submit" size="xs" variant="secondary" disabled={!writtenOk}>
                Show
              </Button>
            </div>
          </SettingRow>
        </form>
      </SettingsGroup>
      <Loadable label="the secrets" fallback={<Hold height={96} />}>
        <SecretsList key={scope} scope={scope} />
      </Loadable>
      <Loadable label="the variables" fallback={<Hold height={96} />}>
        <VariablesList key={scope} scope={scope} />
      </Loadable>
    </>
  );
}

/**
 * One scope's secrets and variables on its own page (W09-R11): a repository's on the Repositories
 * page, a workflow's in the editor. Each list for those who may manage it; the values never shown.
 */
export function ScopeSecrets({ scope, of }: { scope: string; of: string }) {
  const can = useAuth((s) => (s.kind === "authed" ? s.me?.can : undefined));
  const may = (c: string) => !can || Boolean(can[c]);
  return (
    <div className="st">
      {may("secrets.manage") && <SecretsList scope={scope} title={`${of} secrets`} />}
      {may("variables.manage") && <VariablesList scope={scope} title={`${of} variables`} />}
    </div>
  );
}

function SecretsList({ scope, title }: { scope: string; title?: string }) {
  const res = useSuspenseResource(`ci/secrets/${scope}`, (s) => ci.secrets(scope, s));
  const list: CiSecretMeta[] = res.data.secrets;
  const [name, setName] = React.useState("");
  const [value, setValue] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const [removing, setRemoving] = React.useState<string | null>(null);
  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setProblem(null);
    try {
      await f();
      res.reload();
      return true;
    } catch (e) {
      setProblem(errorMessage(e));
      return false;
    } finally {
      setBusy(false);
    }
  };
  const upper = name.trim().toUpperCase();
  const ready = NAME.test(name.trim()) && value.length > 0;
  return (
    <SettingsGroup id="ci.secrets.list" title={title ?? `Secrets in ${scope}`} words="secret value set remove">
      {list.length === 0 ? (
        <p className="st-note">No secrets here yet.</p>
      ) : (
        <ul className="st-list" aria-label="Secrets">
          {list.map((x) => (
            <li key={x.name}>
              <div className="st-item">
                <span className="st-item-name font-mono text-[12.5px]">{x.name}</span>
                <span className="st-item-meta">
                  {x.from ? `from ${x.from}` : `set ${relativeTime(x.updated_at)}`} · {x.last_used_at ? `last used ${relativeTime(x.last_used_at)}` : "never used"}
                </span>
                <span className="st-item-acts">
                  <Button size="xs" variant="ghost" disabled={busy} aria-expanded={removing === x.name} onClick={() => setRemoving(removing === x.name ? null : x.name)}>
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
                      <Button size="xs" variant="primary" disabled={busy} onClick={() => void act(() => ci.deleteSecret(scope, x.name)).then((ok) => ok && setRemoving(null))}>
                        Remove {x.name}
                      </Button>
                      <Button size="xs" variant="ghost" disabled={busy} onClick={() => setRemoving(null)}>
                        Keep it
                      </Button>
                    </>
                  }
                >
                  <p>Runs that read it get an empty value from then on.</p>
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
        onSubmit={async (e) => {
          e.preventDefault();
          if (ready && (await act(() => ci.setSecret(scope, upper, value)))) {
            setName("");
            setValue("");
          }
        }}
      >
        <div className="st-fields">
          <div>
            <label htmlFor="st-secret-name" className="field-label">
              Name
            </label>
            <div className="field">
              <input id="st-secret-name" type="text" spellCheck={false} autoCapitalize="characters" placeholder="DEPLOY_KEY" value={name} disabled={busy} onChange={(e) => setName(e.target.value)} />
            </div>
          </div>
          <div>
            <label htmlFor="st-secret-value" className="field-label">
              Value
            </label>
            <div className="field">
              <input id="st-secret-value" type="password" autoComplete="new-password" value={value} disabled={busy} onChange={(e) => setValue(e.target.value)} />
            </div>
          </div>
        </div>
        <div className="mt-3 flex items-center justify-end gap-3">
          {list.some((x) => x.name === upper) && <span className="st-note mt-0">Saving replaces {upper}&rsquo;s value.</span>}
          <Button type="submit" size="xs" variant="secondary" disabled={busy || !ready}>
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

function VariablesList({ scope, title }: { scope: string; title?: string }) {
  const res = useSuspenseResource(`ci/variables/${scope}`, (s) => ci.variables(scope, s));
  const list: CiVariable[] = res.data.variables;
  const [name, setName] = React.useState("");
  const [value, setValue] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setProblem(null);
    try {
      await f();
      res.reload();
      return true;
    } catch (e) {
      setProblem(errorMessage(e));
      return false;
    } finally {
      setBusy(false);
    }
  };
  const upper = name.trim().toUpperCase();
  const ready = NAME.test(name.trim());
  return (
    <SettingsGroup id="ci.variables.list" title={title ?? `Variables in ${scope}`} words="variable vars value set edit remove">
      {list.length === 0 ? (
        <p className="st-note">No variables here yet.</p>
      ) : (
        <ul className="st-list" aria-label="Variables">
          {list.map((x) => (
            <li key={x.name}>
              <div className="st-item">
                <span className="st-item-name font-mono text-[12.5px]">{x.name}</span>
                <span className="st-item-meta mono">{x.value}</span>
                <span className="st-item-acts">
                  <Button
                    size="xs"
                    variant="ghost"
                    disabled={busy}
                    onClick={() => {
                      setName(x.name);
                      setValue(x.value);
                    }}
                  >
                    Edit
                  </Button>
                  <Button size="xs" variant="ghost" disabled={busy} onClick={() => void act(() => ci.deleteVariable(scope, x.name))}>
                    Remove
                  </Button>
                </span>
              </div>
            </li>
          ))}
        </ul>
      )}
      <form
        noValidate
        className="mt-4"
        onSubmit={async (e) => {
          e.preventDefault();
          if (ready && (await act(() => ci.setVariable(scope, upper, value)))) {
            setName("");
            setValue("");
          }
        }}
      >
        <div className="st-fields">
          <div>
            <label htmlFor="st-var-name" className="field-label">
              Name
            </label>
            <div className="field">
              <input id="st-var-name" type="text" spellCheck={false} autoCapitalize="characters" placeholder="REGION" value={name} disabled={busy} onChange={(e) => setName(e.target.value)} />
            </div>
          </div>
          <div>
            <label htmlFor="st-var-value" className="field-label">
              Value
            </label>
            <div className="field">
              <input id="st-var-value" type="text" spellCheck={false} value={value} disabled={busy} onChange={(e) => setValue(e.target.value)} />
            </div>
          </div>
        </div>
        <div className="mt-3 flex justify-end">
          <Button type="submit" size="xs" variant="secondary" disabled={busy || !ready}>
            Save the variable
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
