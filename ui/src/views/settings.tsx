/**
 * Settings: sections that open and close (this device, deployments, integrations, sign-in), each
 * in groups of rows, and a search that narrows the page to what matches. A link to a section or a
 * group (`/settings#deployments`, `/settings#deployments.accounts`) opens it. Updates are on the
 * System page: a line here says so.
 */
import * as React from "react";
import { Check, LoaderCircle, LogOut } from "lucide-react";
import { ApiError, api } from "@/api/client";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, Facts, PageIntro, TextField } from "@/components/ui/misc";
import { SettingRow, SettingsGroup, SettingsSearch, SettingsSection, SettingsSections, matches, openHash, wordsOf } from "@/components/ui/settings";
import { useMotionPreference } from "@/components/shell/motion";
import { Link, useLocation } from "@/lib/router";
import { toast } from "@/components/ui/toast";
import { useAuth } from "@/lib/auth";
import { usePref } from "@/lib/prefs";
import { useRes, warm, type Res } from "@/lib/data";
import { invalidate } from "@/lib/resource";
import { DeploySettings } from "@/views/settings-deploy";
import { NotificationsGroup, NotificationsHold } from "@/views/settings-notifications";
import { DeliveriesGroup, DeliveriesHold, MyNotificationsGroup, MyNotificationsHold, myNotificationsRes } from "@/views/settings-ci-notifications";
import { ChatAccountsGroup, ChatAccountsHold, chatLinksRes } from "@/views/settings-chat";
import { MyPasswordGroup, MyPasswordHold, PasskeysGroup, PasskeysHold, myPasswordRes, passkeysRes } from "@/views/settings-signin";
import { PeopleGroup, PeopleHold, RolesGroup, RolesHold, peopleRes, rolesRes } from "@/views/settings-people";
import { TokensGroup, TokensHold, tokensRes } from "@/views/settings-tokens";
import { CiSecretsGroups, CiSecretsHold } from "@/views/settings-ci";
import { RoutingGroups, RoutingHold } from "@/views/settings-routing";
import { OidcGroups, OidcHold } from "@/views/settings-oidc";
import { ActionsGroup, ActionsHold } from "@/views/settings-actions";
import { pushDevicesRes, pushStateRes, thisDeviceRes } from "@/api/push";

/** What finds the line saying Updates are on the System page, in a search of Settings. */
const ELSEWHERE_WORDS = "updates upgrade upgrades version server runners policy builds binaries platform waits";

const methodsRes: Res<{ password: boolean; token: boolean }> = {
  key: "auth/methods",
  fetch: (sig) => api.authMethods(sig),
};

export default function Settings() {
  // Each section for those who can use it (W10-R02): the owner sees every one, a person what their rights reach.
  const can = useAuth((s) => (s.kind === "authed" ? s.me?.can : undefined));
  // A person (not the owner): their own password beside their passkeys.
  const person = useAuth((s) => s.kind === "authed" && s.me?.kind === "user");
  const may = (c: string, server = true) => !can || can[c] === "server" || (!server && can[c] === "some");
  const [pushes, deployments, server, manages] = [may("agents.run"), may("integrations.manage"), may("settings.manage"), may("users.manage")];
  // The owner and people keep tokens; others reach Settings through one.
  const signedIn = useAuth((s) => s.kind === "authed" && (s.me?.kind === "owner" || s.me?.kind === "user" || !s.me?.kind));
  warm(
    methodsRes,
    thisDeviceRes,
    myNotificationsRes,
    passkeysRes,
    ...(signedIn ? [tokensRes, rolesRes] : []),
    ...(manages ? [peopleRes] : []),
    ...(person ? [myPasswordRes, chatLinksRes] : []),
    ...(pushes ? [pushStateRes, pushDevicesRes] : []),
  );
  // The sections shown, numbered in order.
  let shown = 0;
  const next = () => ++shown;
  const [query, setQuery] = React.useState("");
  // At /settings#<section or group>, arriving or following a link here: that one opens.
  const { hash } = useLocation();
  const arrived = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (el && hash) openHash(hash);
    },
    [hash],
  );
  return (
    <div ref={arrived} className="container-console page st max-w-[880px]">
      <title>Settings · kiln</title>
      <PageIntro index={10} label="Settings" title="Settings" />
      <SettingsSearch value={query} onChange={setQuery} />
      <SettingsSections query={query} onQueryChange={setQuery}>
        <SettingsSection id="device" index={next()} title="This device" about="How kiln behaves in this browser" words="browser phone">
          {pushes && (
            <Loadable label="notifications" fallback={<NotificationsHold />}>
              <NotificationsGroup />
            </Loadable>
          )}
          <MotionGroup />
        </SettingsSection>
        {(!query.trim() || matches(wordsOf(query), ELSEWHERE_WORDS)) && (
          <p className="st-elsewhere">
            Updates and builds are on the System page. <Link to="/system#system.updates">System → Updates</Link>
          </p>
        )}
        {deployments && (
          <SettingsSection id="deployments" index={next()} title="Deployments" about="Where images live, the registries kiln pulls from, the accounts it deploys through" words="cd deploy">
            <DeploySettings />
          </SettingsSection>
        )}
        <SettingsSection id="integrations" index={next()} title="Integrations" about={server ? "Peered kiln clusters, and a Grafana link" : "A Grafana link"} words="peering">
          {server && <ClustersGroup />}
          <GrafanaGroup />
        </SettingsSection>
        <SettingsSection
          id="signin"
          index={next()}
          title="Sign-in"
          about={server ? "The password other devices sign in with, your passkeys, and this session" : "Your password and passkeys, and this session"}
          words="login account"
          status={server ? <PasswordStatus /> : undefined}
        >
          {server && (
            <Loadable label="sign-in" fallback={<PasswordHold />}>
              <PasswordGroup />
            </Loadable>
          )}
          {person && (
            <Loadable label="your password" fallback={<MyPasswordHold />}>
              <MyPasswordGroup />
            </Loadable>
          )}
          <Loadable label="your passkeys" fallback={<PasskeysHold />}>
            <PasskeysGroup />
          </Loadable>
          <SessionGroup />
        </SettingsSection>
        {manages && (
          <SettingsSection id="people" index={next()} title="People" about="Who signs in here, what each may do where, and the roles grants name" words="people users grants roles">
            <Loadable label="the people" fallback={<PeopleHold />}>
              <PeopleGroup />
            </Loadable>
            <Loadable label="the roles" fallback={<RolesHold />}>
              <RolesGroup />
            </Loadable>
          </SettingsSection>
        )}
        {signedIn && (
          <SettingsSection id="tokens" index={next()} title="Tokens" about="Personal access tokens, for the CLI, scripts and tools" words="tokens pat api cli">
            <Loadable label="your tokens" fallback={<TokensHold />}>
              <TokensGroup />
            </Loadable>
          </SettingsSection>
        )}
        {(may("secrets.manage", false) || may("variables.manage", false) || may("routing.manage") || may("settings.manage")) && (
          <SettingsSection
            id="ci"
            index={next()}
            title="CI"
            about="The secrets and variables runs read, where legs run, the actions they may use, and the identity jobs prove"
            words="ci secrets variables routing actions oidc"
          >
            {(may("secrets.manage", false) || may("variables.manage", false)) && (
              <Loadable label="the scopes" fallback={<CiSecretsHold />}>
                <CiSecretsGroups />
              </Loadable>
            )}
            {may("routing.manage") && (
              <Loadable label="the routing rules" fallback={<RoutingHold />}>
                <RoutingGroups />
              </Loadable>
            )}
            {may("settings.manage") && (
              <>
                <Loadable label="the actions policy" fallback={<ActionsHold />}>
                  <ActionsGroup />
                </Loadable>
                <Loadable label="the issuer" fallback={<OidcHold />}>
                  <OidcGroups />
                </Loadable>
                <Loadable label="the deliveries" fallback={<DeliveriesHold />}>
                  <DeliveriesGroup />
                </Loadable>
              </>
            )}
          </SettingsSection>
        )}
        <SettingsSection id="you" index={next()} title="You" about="What kiln tells you of your repositories, and the chat accounts that act as you" words="person account">
          <Loadable label="your notifications" fallback={<MyNotificationsHold />}>
            <MyNotificationsGroup />
          </Loadable>
          {person && (
            <Loadable label="your chat accounts" fallback={<ChatAccountsHold />}>
              <ChatAccountsGroup />
            </Loadable>
          )}
        </SettingsSection>
      </SettingsSections>
    </div>
  );
}

/* ---------------------------- this device ---------------------------- */

function MotionGroup() {
  const { paused, reduced, toggle } = useMotionPreference();
  return (
    <SettingsGroup id="device.motion" title="Motion" words="motion animation prism pulse reduced pause">
      <SettingRow
        inline
        htmlFor="st-motion"
        label="Pause motion"
        help={reduced ? "Your device asks for reduced motion, so kiln keeps still." : "The ambient prism and status pulses keep still. Your device's reduced-motion setting always wins."}
      >
        <label className="st-check-control" htmlFor="st-motion">
          <input id="st-motion" type="checkbox" className="check" checked={paused || reduced} disabled={reduced} onChange={toggle} />
        </label>
      </SettingRow>
    </SettingsGroup>
  );
}

/* ---------------------------- integrations ---------------------------- */

function ClustersGroup() {
  return (
    <SettingsGroup
      id="integrations.clusters"
      title="Peered clusters"
      about="Other kiln clusters, opened through this one from the picker in the header."
      words="clusters peering peer kiln picker"
      actions={
        <Button asChild size="xs" variant="secondary">
          <Link to="/clusters">Manage</Link>
        </Button>
      }
    />
  );
}

function GrafanaGroup() {
  const [grafana, setGrafana] = usePref("kiln.grafanaUrl", "");
  const [draft, setDraft] = React.useState(grafana);
  const [saved, setSaved] = React.useState(false);
  const trimmed = draft.trim();
  let valid = trimmed === "";
  if (trimmed) {
    try {
      const url = new URL(trimmed);
      valid = ["http:", "https:"].includes(url.protocol) && /^https?:\/\//i.test(trimmed) && !/\s/.test(trimmed);
    } catch {
      valid = false;
    }
  }
  return (
    <SettingsGroup id="integrations.grafana" title="Grafana" words="grafana dashboard metrics url link">
      <form
        noValidate
        onSubmit={(e) => {
          e.preventDefault();
          if (!valid) return;
          setGrafana(trimmed);
          setDraft(trimmed);
          setSaved(true);
          toast({
            title: trimmed ? "Grafana link saved" : "Grafana link removed",
            tone: "info",
          });
        }}
      >
        <SettingRow
          label="Dashboard link"
          htmlFor="st-grafana"
          help="Open your dashboard from Metrics. Saved in this browser."
        >
          <div className="st-control-stack">
            <div className="st-control-line">
              <div className="field st-wide">
                <input
                  id="st-grafana"
                  type="url"
                  inputMode="url"
                  placeholder="https://grafana.example.com/d/kiln"
                  value={draft}
                  aria-invalid={!valid || undefined}
                  aria-describedby={!valid || saved ? "st-grafana-feedback" : undefined}
                  onChange={(e) => { setDraft(e.target.value); setSaved(false); }}
                />
              </div>
              <Button type="submit" size="xs" variant="secondary" disabled={!valid || trimmed === grafana}>
                Save
              </Button>
            </div>
            {(!valid || saved) && (
              <p id="st-grafana-feedback" className="st-note st-feedback" data-tone={!valid ? "error" : "success"} role="status">
                {!valid ? "Enter a valid http:// or https:// URL." : <><Check size={13} aria-hidden="true" /> {trimmed ? "Link saved" : "Link removed"}</>}
              </p>
            )}
          </div>
        </SettingRow>
      </form>
    </SettingsGroup>
  );
}

/* ---------------------------- sign-in ---------------------------- */

const PASSWORD_WORDS = "password sign in login token phone set change remove";

function PasswordStatus() {
  const on = useRes(methodsRes).data.password;
  return <span data-tone={on ? "succeeded" : undefined}>{on ? "Password on" : "Password off"}</span>;
}

function PasswordHold() {
  return (
    <SettingsGroup id="signin.password" title="Password" words={PASSWORD_WORDS}>
      <Hold height={66} />
    </SettingsGroup>
  );
}

/** The sign-in password: set it once here, then sign in from any device with it. */
function PasswordGroup() {
  const methods = useRes(methodsRes).data;
  const [pw, setPw] = React.useState("");
  const [again, setAgain] = React.useState("");
  const [busy, setBusy] = React.useState<"save" | "remove" | null>(null);
  const [removing, setRemoving] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const isSet = methods.password;
  const tooShort = pw.length > 0 && pw.length < 8;
  const mismatch = again.length > 0 && pw !== again;

  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    if (busy || removing || pw.length < 8 || pw !== again) return;
    setBusy("save");
    setProblem(null);
    try {
      await api.setPassword(pw);
      setPw("");
      setAgain("");
      toast({
        title: isSet ? "Password changed" : "Password set",
        description: "Sign in from any device with it.",
        tone: "info",
      });
      invalidate("auth/methods");
    } catch (err) {
      setProblem(err instanceof ApiError ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  const remove = async () => {
    if (busy) return;
    setBusy("remove");
    setProblem(null);
    try {
      await api.clearPassword();
      toast({ title: "Password removed", tone: "info" });
      setPw("");
      setAgain("");
      setRemoving(false);
      invalidate("auth/methods");
    } catch (err) {
      setProblem(err instanceof ApiError ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  return (
    <SettingsGroup
      id="signin.password"
      title="Password"
      about={
        isSet ? "On: any device that can reach this server signs in with it, and the admin token keeps working." : "Off: set one so your phone or another computer can sign in without the admin token."
      }
      words={PASSWORD_WORDS}
      actions={
        isSet ? (
          <Button variant="ghost" size="xs" disabled={!!busy} aria-expanded={removing} onClick={() => { setRemoving(!removing); setProblem(null); }}>
            Remove
          </Button>
        ) : undefined
      }
    >
      {removing && isSet && (
        <Confirm
          className="mb-3"
          label="Remove the sign-in password"
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" disabled={!!busy} onClick={() => void remove()}>
                {busy === "remove" && <LoaderCircle className="animate-spin" aria-hidden="true" />}
                {busy === "remove" ? "Removing…" : "Remove password"}
              </Button>
              <Button size="xs" variant="ghost" disabled={!!busy} onClick={() => { setRemoving(false); setProblem(null); }}>
                Cancel
              </Button>
            </>
          }
        >
          <p>Devices will need the admin token or a <code>kiln login</code> link to sign in.</p>
        </Confirm>
      )}
      <form className="st-password" onSubmit={save} autoComplete="on" aria-busy={!!busy}>
        <input type="text" name="username" autoComplete="username" value="admin" readOnly tabIndex={-1} aria-hidden="true" className="sr-only" />
        <TextField
          label={isSet ? "New password" : "Password"}
          type="password"
          name="new-password"
          autoComplete="new-password"
          value={pw}
          disabled={!!busy}
          aria-invalid={tooShort || undefined}
          aria-describedby="st-password-help"
          onChange={(e) => { setPw(e.target.value); setProblem(null); }}
          hint={<span id="st-password-help" data-tone={tooShort ? "error" : undefined}>At least 8 characters</span>}
        />
        <TextField
          label="Confirm password"
          type="password"
          name="new-password-repeat"
          autoComplete="new-password"
          value={again}
          disabled={!!busy}
          aria-invalid={mismatch || undefined}
          aria-describedby={mismatch ? "st-password-match" : undefined}
          onChange={(e) => { setAgain(e.target.value); setProblem(null); }}
          hint={mismatch ? <span id="st-password-match" data-tone="error">The two passwords differ</span> : undefined}
        />
        <div className="st-password-actions">
          <Button type="submit" size="xs" variant="secondary" disabled={!!busy || removing || pw.length < 8 || pw !== again}>
            {busy === "save" && <LoaderCircle className="animate-spin" aria-hidden="true" />}
            {busy === "save" ? "Saving…" : isSet ? "Change password" : "Set password"}
          </Button>
        </div>
      </form>
      {problem && (
        <p className="st-note" data-tone="error" role="alert">
          {problem}
        </p>
      )}
    </SettingsGroup>
  );
}

function SessionGroup() {
  const me = useAuth((s) => (s.kind === "authed" ? s.me : null));
  return (
    <SettingsGroup
      id="signin.session"
      title="This session"
      words="session sign out logout server version origin"
      actions={
        <Button variant="ghost" size="xs" onClick={() => void api.logout()}>
          <LogOut /> Sign out
        </Button>
      }
    >
      <Facts
        items={[
          { label: "Server", value: me?.hostname ?? "—" },
          { label: "Version", value: me?.version ?? "—" },
          { label: "Origin", value: location.host },
        ]}
      />
    </SettingsGroup>
  );
}
