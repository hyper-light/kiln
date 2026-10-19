/**
 * Settings: sections that open and close (this device, updates, deployments, integrations,
 * sign-in), each in groups of rows, and a search that narrows the page to what matches. A link to a
 * section or a group (`/settings#updates`, `/settings#updates.waits`) opens it.
 */
import * as React from "react";
import { LogOut } from "lucide-react";
import { ApiError, api } from "@/api/client";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, Facts, PageIntro, TextField } from "@/components/ui/misc";
import { SettingRow, SettingsGroup, SettingsSearch, SettingsSection, SettingsSections, openHash } from "@/components/ui/settings";
import { useMotionPreference } from "@/components/shell/motion";
import { Link, useLocation } from "@/lib/router";
import { toast } from "@/components/ui/toast";
import { useAuth } from "@/lib/auth";
import { usePref } from "@/lib/prefs";
import { updatesRes, useRes, warm, type Res } from "@/lib/data";
import { invalidate } from "@/lib/resource";
import { UpdatesHold, UpdatesPanel, UpdatesStatus } from "@/views/settings-updates";
import { DeploySettings } from "@/views/settings-deploy";
import { NotificationsGroup, NotificationsHold } from "@/views/settings-notifications";
import { pushDevicesRes, pushStateRes, thisDeviceRes } from "@/api/push";

const methodsRes: Res<{ password: boolean; token: boolean }> = {
  key: "auth/methods",
  fetch: (sig) => api.authMethods(sig),
};

export default function Settings() {
  warm(updatesRes, methodsRes, pushStateRes, pushDevicesRes, thisDeviceRes);
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
      <SettingsSections query={query}>
        <SettingsSection id="device" index={1} title="This device" about="How kiln behaves in this browser" words="browser phone">
          <Loadable label="notifications" fallback={<NotificationsHold />}>
            <NotificationsGroup />
          </Loadable>
          <MotionGroup />
        </SettingsSection>
        <SettingsSection
          id="updates"
          index={2}
          title="Updates"
          about="Upgrade kiln and its runners; a runner keeps what it runs while it's upgraded"
          words="upgrade version"
          status={<UpdatesStatus />}
        >
          <Loadable label="the updates" fallback={<UpdatesHold />}>
            <UpdatesPanel />
          </Loadable>
        </SettingsSection>
        <SettingsSection id="deployments" index={3} title="Deployments" about="Where images live, the registries kiln pulls from, the accounts it deploys through" words="cd deploy">
          <DeploySettings />
        </SettingsSection>
        <SettingsSection id="integrations" index={4} title="Integrations" about="Peered kiln clusters, and a Grafana link" words="peering">
          <ClustersGroup />
          <GrafanaGroup />
        </SettingsSection>
        <SettingsSection id="signin" index={5} title="Sign-in" about="The password other devices sign in with, and this session" words="login account" status={<PasswordStatus />}>
          <Loadable label="sign-in" fallback={<PasswordHold />}>
            <PasswordGroup />
          </Loadable>
          <SessionGroup />
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
        <input id="st-motion" type="checkbox" className="check" checked={paused || reduced} disabled={reduced} onChange={toggle} />
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
  const valid = draft === "" || /^https?:\/\/\S+$/.test(draft.trim());
  return (
    <SettingsGroup id="integrations.grafana" title="Grafana" words="grafana dashboard metrics url link">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (!valid) return;
          setGrafana(draft.trim());
          toast({
            title: draft.trim() ? "Grafana link saved" : "Grafana link removed",
            tone: "info",
          });
        }}
      >
        <SettingRow
          label="Dashboard link"
          htmlFor="st-grafana"
          help={valid ? "Opened from the Metrics page; kiln ships its dashboards as ConfigMaps under deploy/. Kept in this browser." : "It must start with http:// or https://"}
        >
          <div className="field st-wide">
            <input id="st-grafana" type="url" inputMode="url" placeholder="https://grafana.example.com/d/kiln" value={draft} onChange={(e) => setDraft(e.target.value)} />
          </div>
          <Button type="submit" size="xs" variant="secondary" disabled={!valid || draft.trim() === grafana}>
            Save
          </Button>
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
  const [busy, setBusy] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const isSet = methods.password;
  const tooShort = pw.length > 0 && pw.length < 8;
  const mismatch = again.length > 0 && pw !== again;

  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    if (pw.length < 8 || pw !== again) return;
    setBusy(true);
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
      setBusy(false);
    }
  };

  const remove = async () => {
    setBusy(true);
    try {
      await api.clearPassword();
      toast({ title: "Password removed", tone: "info" });
      setRemoving(false);
      invalidate("auth/methods");
    } catch (err) {
      setProblem(err instanceof ApiError ? err.message : String(err));
    } finally {
      setBusy(false);
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
          <Button variant="ghost" size="xs" disabled={busy} aria-expanded={removing} onClick={() => setRemoving(!removing)}>
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
              <Button size="xs" variant="danger" disabled={busy} onClick={() => void remove()}>
                Remove
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setRemoving(false)}>
                Cancel
              </Button>
            </>
          }
        >
          <p>Remove the sign-in password? Devices will need the admin token or a `kiln login` link to sign in.</p>
        </Confirm>
      )}
      <form className="st-password" onSubmit={save} autoComplete="on">
        <input type="text" name="username" autoComplete="username" value="admin" readOnly tabIndex={-1} aria-hidden="true" className="sr-only" />
        <TextField
          label={isSet ? "New password" : "Password"}
          type="password"
          name="new-password"
          autoComplete="new-password"
          value={pw}
          onChange={(e) => setPw(e.target.value)}
          hint={tooShort ? "At least 8 characters" : undefined}
        />
        <TextField
          label="Repeat it"
          type="password"
          name="new-password-repeat"
          autoComplete="new-password"
          value={again}
          onChange={(e) => setAgain(e.target.value)}
          hint={mismatch ? "The two passwords differ" : undefined}
        />
        <Button type="submit" size="xs" variant="secondary" className="st-password-go" disabled={busy || pw.length < 8 || pw !== again}>
          {isSet ? "Change password" : "Set password"}
        </Button>
      </form>
      {problem && (
        <p className="st-note" data-tone="error">
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
