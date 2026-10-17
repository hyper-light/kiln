import * as React from "react";
import { LogOut } from "lucide-react";
import { ApiError } from "@/api/client";
import { api } from "@/api/client";
import { Button } from "@/components/ui/button";
import { Facts, PageIntro, Panel, TextField } from "@/components/ui/misc";
import { MotionToggle } from "@/components/shell/motion";
import { toast } from "@/components/ui/toast";
import { useDocumentTitle, useLocalStorage, useResource } from "@/lib/hooks";
import { UpdatesPanel } from "@/views/settings-updates";

export default function Settings() {
  useDocumentTitle("Settings");
  const [grafana, setGrafana] = useLocalStorage("kiln.grafanaUrl", "");
  const [draft, setDraft] = React.useState(grafana);
  const me = useResource((s) => api.me(s), []);
  const valid = draft === "" || /^https?:\/\/\S+$/.test(draft.trim());

  return (
    <div className="container-console page max-w-[880px]">
      <PageIntro index={11} label="Settings" title="Settings" lead="Preferences here are stored in this browser only." />
      <div className="space-y-4">
        <UpdatesPanel />
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
        <PasswordPanel />
        <Panel eyebrow="Session" title="Signed in" actions={<Button variant="ghost" size="sm" onClick={() => void api.logout()}><LogOut /> Sign out</Button>}>
          <Facts
            items={[
              { label: "Server", value: me.data?.hostname ?? "—" },
              { label: "Version", value: me.data?.version ?? "—" },
              { label: "Origin", value: location.host },
            ]}
          />
        </Panel>
      </div>
    </div>
  );
}

/** The sign-in password: set it once here, then sign in from any device with it. */
function PasswordPanel() {
  const methods = useResource((sig) => api.authMethods(sig), []);
  const [pw, setPw] = React.useState("");
  const [again, setAgain] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const isSet = methods.data?.password ?? false;
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
      methods.reload();
    } catch (err) {
      setProblem(err instanceof ApiError ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!window.confirm("Remove the sign-in password? Devices will need the admin token or a `kiln login` link to sign in.")) return;
    setBusy(true);
    try {
      await api.clearPassword();
      toast({ title: "Password removed", tone: "info" });
      methods.reload();
    } catch (err) {
      setProblem(err instanceof ApiError ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Panel
      eyebrow="Sign-in"
      title="Password"
      description={
        isSet
          ? "On. Any device that can reach this server can sign in with it; the admin token keeps working too."
          : "Off. Choose a password so your phone or another computer can sign in without the admin token."
      }
      actions={
        isSet ? (
          <Button variant="ghost" size="sm" disabled={busy} onClick={() => void remove()}>
            Remove
          </Button>
        ) : undefined
      }
    >
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
    </Panel>
  );
}
