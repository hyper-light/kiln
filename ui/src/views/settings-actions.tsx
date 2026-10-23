/**
 * Settings → CI → Actions (docs/ci.md W21-R11, W07-R04): which actions runs may use. The server's
 * policy is its configuration's (`[ci.actions]`), shown as it is; a repository's is kiln's setting
 * for it, edited here, beside what its trunk's `.kiln/config.yaml` says (which a diff changes).
 * A use either refuses is refused. Mirroring the prefetched actions is a click away.
 */
import * as React from "react";
import { ci, type CiActionsPolicy } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Facts, SelectField } from "@/components/ui/misc";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { reposRes, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";

const lines = (s: string) =>
  s
    .split("\n")
    .map((x) => x.trim())
    .filter(Boolean);

function said(p: CiActionsPolicy | null | undefined): string {
  if (!p) return "none";
  const parts = [p.allow.length ? `allow ${p.allow.join(", ")}` : "any action", p.deny.length ? `but ${p.deny.join(", ")}` : "", p.require_sha_pins ? "pinned by SHA" : ""];
  return parts.filter(Boolean).join(" · ");
}

export function ActionsHold() {
  return (
    <SettingsGroup id="ci.actions" title="Actions" words="actions policy allow deny pins">
      <Hold height={120} />
    </SettingsGroup>
  );
}

export function ActionsGroup() {
  const server = useSuspenseResource("ci/actions/policy", (s) => ci.actionsPolicy(null, s)).data.server;
  const repos = useRes(reposRes).data;
  const [repo, setRepo] = React.useState(repos[0]?.name ?? "");
  const [busy, setBusy] = React.useState(false);
  const [synced, setSynced] = React.useState<string | null>(null);
  return (
    <SettingsGroup
      id="ci.actions"
      title="Actions"
      about="Which actions runs may use: the server's policy and a repository's both apply, and a use either refuses is refused."
      words="actions policy allow deny pins sha prefetch mirror"
      actions={
        <Button
          size="xs"
          variant="ghost"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            try {
              const r = await ci.syncActions();
              const failed = r.synced.filter((x) => x.error);
              setSynced(
                r.synced.length === 0
                  ? "Nothing to mirror: [ci.actions] prefetch is empty."
                  : `Mirrored ${r.synced.length - failed.length} of ${r.synced.length}${failed.length ? `; ${failed.map((x) => `${x.uses}: ${x.error}`).join("; ")}` : ""}.`,
              );
            } catch (e) {
              setSynced(errorMessage(e));
            } finally {
              setBusy(false);
            }
          }}
        >
          Mirror prefetched actions
        </Button>
      }
    >
      <SettingRow label="The server's" help="[ci.actions] in its configuration.">
        <span className="st-value">{said(server)}</span>
      </SettingRow>
      {synced && (
        <p className="st-note" role="status">
          {synced}
        </p>
      )}
      {repos.length > 0 && (
        <>
          <SettingRow label="A repository's">
            <SelectField label="Repository" hideLabel className="st-wide" value={repo} onChange={setRepo} options={repos.map((r) => ({ value: r.name, label: r.name }))} />
          </SettingRow>
          <Loadable label="its policy" fallback={<Hold height={120} />}>
            <RepoPolicy key={repo} repo={repo} />
          </Loadable>
        </>
      )}
    </SettingsGroup>
  );
}

function RepoPolicy({ repo }: { repo: string }) {
  const res = useSuspenseResource(`ci/actions/policy/${repo}`, (s) => ci.actionsPolicy(repo, s));
  const d = res.data;
  const own = d.repository ?? null;
  const [allow, setAllow] = React.useState((own?.allow ?? []).join("\n"));
  const [deny, setDeny] = React.useState((own?.deny ?? []).join("\n"));
  const [pins, setPins] = React.useState(own?.require_sha_pins ?? false);
  const [busy, setBusy] = React.useState(false);
  const [note, setNote] = React.useState<{ tone?: "error"; text: string } | null>(null);
  const act = async (f: () => Promise<unknown>, done: string) => {
    setBusy(true);
    setNote(null);
    try {
      await f();
      res.reload();
      setNote({ text: done });
    } catch (e) {
      setNote({ tone: "error", text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="mt-2">
      <Facts
        items={[
          { label: "Its .kiln/config.yaml", value: <span className="st-value">{said(d.config_yaml)}</span> },
          { label: "Set in kiln", value: <span className="st-value">{said(own)}</span> },
        ]}
      />
      <form
        noValidate
        className="mt-4"
        onSubmit={(e) => {
          e.preventDefault();
          void act(() => ci.setActionsPolicy(repo, { allow: lines(allow), deny: lines(deny), require_sha_pins: pins }), "Saved.");
        }}
      >
        <div className="st-fields">
          <div>
            <label className="field-label" htmlFor="st-actions-allow">
              Allow (one a line; any when empty)
            </label>
            <div className="field">
              <textarea
                id="st-actions-allow"
                rows={3}
                spellCheck={false}
                className="font-mono text-[12px]"
                placeholder={"actions/*\ndocker/*"}
                value={allow}
                disabled={busy}
                onChange={(e) => setAllow(e.target.value)}
              />
            </div>
          </div>
          <div>
            <label className="field-label" htmlFor="st-actions-deny">
              Deny (one a line)
            </label>
            <div className="field">
              <textarea
                id="st-actions-deny"
                rows={3}
                spellCheck={false}
                className="font-mono text-[12px]"
                placeholder="untrusted-org/*"
                value={deny}
                disabled={busy}
                onChange={(e) => setDeny(e.target.value)}
              />
            </div>
          </div>
        </div>
        <SettingRow inline htmlFor="st-actions-pins" label="Pinned by SHA" help="Refuse tags and branches: every remote action at a full commit SHA (KCI4021).">
          <input id="st-actions-pins" type="checkbox" className="check" checked={pins} disabled={busy} onChange={(e) => setPins(e.target.checked)} />
        </SettingRow>
        <div className="mt-3 flex justify-end gap-2">
          {own && (
            <Button type="button" size="xs" variant="ghost" disabled={busy} onClick={() => void act(() => ci.clearActionsPolicy(repo), "Removed: its .kiln/config.yaml's and the server's apply.")}>
              Remove kiln&rsquo;s setting
            </Button>
          )}
          <Button type="submit" size="xs" variant="secondary" disabled={busy}>
            Save the policy
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
