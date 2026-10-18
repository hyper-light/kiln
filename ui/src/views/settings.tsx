import * as React from "react";
import { LogOut } from "lucide-react";
import { ApiError } from "@/api/client";
import { api } from "@/api/client";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, Facts, PageIntro, Panel, TextField } from "@/components/ui/misc";
import { MotionToggle } from "@/components/shell/motion";
import { toast } from "@/components/ui/toast";
import { useAuth } from "@/lib/auth";
import { usePref } from "@/lib/prefs";
import { updatesRes, useRes, warm, type Res } from "@/lib/data";
import { invalidate } from "@/lib/resource";
import { UpdatesHold, UpdatesPanel } from "@/views/settings-updates";

const methodsRes: Res<{ password: boolean; token: boolean }> = { key: "auth/methods", fetch: (sig) => api.authMethods(sig) };

export default function Settings() {
  warm(updatesRes, methodsRes);
  return (
    <div className="container-console page max-w-[880px]">
      <title>Settings · kiln</title>
      <PageIntro index={10} label="Settings" title="Settings" lead="Preferences here are stored in this browser only." />
      {/* The page arrives whole: nothing below a loading panel moves when it lands. */}
      <Loadable label="the settings" fallback={<SettingsPanels updates={<UpdatesHold />} password={<PasswordHold />} />}>
        <SettingsPanels updates={<UpdatesPanel />} password={<PasswordPanel />} />
      </Loadable>
    </div>
  );
}

function SettingsPanels({ updates, password }: { updates: React.ReactNode; password: React.ReactNode }) {
  const [grafana, setGrafana] = usePref("kiln.grafanaUrl", "");
  const [draft, setDraft] = React.useState(grafana);
  const me = useAuth((s) => (s.kind === "authed" ? s.me : null));
  const valid = draft === "" || /^https?:\/\/\S+$/.test(draft.trim());
  return (
    <div className="space-y-4">
      {updates}
      <Panel eyebrow="Integrations" title="Grafana" description="Linked from the Metrics page. kiln ships dashboards as ConfigMaps under deploy/.">
        <form
          className="flex flex-wrap items-end gap-3"
          onSubmit={(e) => {
            e.preventDefault();
            if (!valid) return;
            setGrafana(draft.trim());
            toast({ title: draft.trim() ? "Grafana link saved" : "Grafana link removed", tone: "info" });
          }}
        >
          <TextField
            className="min-w-0 flex-1"
            label="Dashboard URL"
            placeholder="https://grafana.example.com/d/kiln"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            hint={valid ? undefined : "Must start with http:// or https://"}
          />
          <Button type="submit" variant="secondary" disabled={!valid || draft.trim() === grafana}>
            Save
          </Button>
        </form>
      </Panel>
      <Panel eyebrow="Accessibility" title="Motion" description="Ambient prism animation and status pulses. Your operating system's reduced-motion setting always wins.">
        <MotionToggle />
      </Panel>
      {password}
      <Panel
        eyebrow="Session"
        title="Signed in"
        actions={
          <Button variant="ghost" size="sm" onClick={() => void api.logout()}>
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
      </Panel>
    </div>
  );
}

function PasswordHold() {
  return (
    <PasswordFrame description={"\u00a0"}>
      <Hold height={66} />
    </PasswordFrame>
  );
}

function PasswordFrame({ description, actions, children }: { description: React.ReactNode; actions?: React.ReactNode; children: React.ReactNode }) {
  return (
    <Panel eyebrow="Sign-in" title="Password" description={description} actions={actions}>
      {children}
    </Panel>
  );
}

/** The sign-in password: set it once here, then sign in from any device with it. */
function PasswordPanel() {
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
      toast({ title: isSet ? "Password changed" : "Password set", description: "Sign in from any device with it.", tone: "info" });
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
    <PasswordFrame
      description={
        isSet
          ? "On. Any device that can reach this server can sign in with it; the admin token keeps working too."
          : "Off. Choose a password so your phone or another computer can sign in without the admin token."
      }
      actions={
        isSet ? (
          <Button variant="ghost" size="sm" disabled={busy} aria-expanded={removing} onClick={() => setRemoving(!removing)}>
            Remove
          </Button>
        ) : undefined
      }
    >
      {removing && isSet && (
        <Confirm
          className="mb-4"
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
      <form className="grid gap-3 sm:grid-cols-[1fr_1fr_auto] sm:items-end" onSubmit={save} autoComplete="on">
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
        <Button type="submit" variant="secondary" disabled={busy || pw.length < 8 || pw !== again}>
          {isSet ? "Change password" : "Set password"}
        </Button>
      </form>
      {problem && <p className="mt-3 text-[12.5px] text-[#d8c8b0]">{problem}</p>}
    </PasswordFrame>
  );
}
