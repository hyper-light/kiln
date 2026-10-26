/**
 * Settings → CI → GitHub (docs/ci.md W14-R10): the GitHub App kiln acts through on GitHub, for its
 * repositories' CI, pull requests and release repositories there. None yet: kiln makes one by
 * GitHub's App manifest flow (it fills in the name, permissions, events and URLs; you click Create
 * on GitHub, in your account or an organization's; GitHub hands it back to kiln). Made: where to
 * install it on repositories.
 */
import * as React from "react";
import { deployApi } from "@/api/deploy";
import { request } from "@/api/client";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Facts } from "@/components/ui/misc";
import { SettingRow, SettingsGroup } from "@/components/ui/settings";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";

export function GithubHold() {
  return (
    <SettingsGroup id="ci.github" title="GitHub" words="github app manifest install">
      <Hold height={100} />
    </SettingsGroup>
  );
}

/** Posts the manifest to GitHub as its form: GitHub shows the App to create. */
function postToGithub(action: string, manifest: string) {
  const form = document.createElement("form");
  form.method = "post";
  form.action = action;
  const field = document.createElement("input");
  field.type = "hidden";
  field.name = "manifest";
  field.value = manifest;
  form.appendChild(field);
  document.body.appendChild(form);
  form.submit();
}

export function GithubGroup() {
  const app = useSuspenseResource("ci/github/app", (s) => deployApi.githubApp(s)).data;
  const [org, setOrg] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const created = typeof window !== "undefined" && new URLSearchParams(window.location.search).get("github") === "created";
  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await request<{ action: string; manifest: string }>("POST", "/api/ci/github/app/manifest", org.trim() ? { org: org.trim() } : {});
      postToGithub(r.action, r.manifest);
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };
  return (
    <SettingsGroup
      id="ci.github"
      title="GitHub"
      about="The GitHub App kiln acts through: its repositories' CI and pull requests there, and release repositories."
      words="github app manifest install create"
    >
      {app.configured ? (
        <>
          <Facts
            items={[
              { label: "App", value: app.html_url ? <a href={app.html_url}>{app.slug ?? app.html_url}</a> : (app.slug ?? "—") },
              { label: "GitHub", value: app.web_url },
            ]}
          />
          {created && <p className="st-note">kiln has its App. Install it on the repositories it's for.</p>}
          {app.install_url && (
            <SettingRow label="Repositories" help="Where it's installed is what kiln reaches: an account's or organization's, all or some of their repositories.">
              <Button asChild size="xs" variant="secondary">
                <a href={app.install_url}>Install on GitHub</a>
              </Button>
            </SettingRow>
          )}
        </>
      ) : (
        <form
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            void create();
          }}
        >
          <SettingRow label="Organization" htmlFor="st-gh-org" help="Where GitHub makes the App: an organization you own, or none for your own account.">
            <div className="st-control-line">
              <div className="field st-wide">
                <input id="st-gh-org" type="text" spellCheck={false} placeholder="Your account" value={org} onChange={(e) => setOrg(e.target.value)} />
              </div>
              <Button type="submit" size="xs" disabled={busy}>
                Create kiln's GitHub App
              </Button>
            </div>
          </SettingRow>
          {error && (
            <p className="st-note" data-tone="error">
              {error}
            </p>
          )}
        </form>
      )}
    </SettingsGroup>
  );
}
