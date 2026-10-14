import * as React from "react";
import { LogOut } from "lucide-react";
import { api } from "@/api/client";
import { Button } from "@/components/ui/button";
import { Facts, PageIntro, Panel, TextField } from "@/components/ui/misc";
import { MotionToggle } from "@/components/shell/motion";
import { toast } from "@/components/ui/toast";
import { useDocumentTitle, useLocalStorage, useResource } from "@/lib/hooks";

export default function Settings() {
  useDocumentTitle("Settings");
  const [grafana, setGrafana] = useLocalStorage("kiln.grafanaUrl", "");
  const [draft, setDraft] = React.useState(grafana);
  const me = useResource((s) => api.me(s), []);
  const valid = draft === "" || /^https?:\/\/\S+$/.test(draft.trim());

  return (
    <div className="container-console page max-w-[880px]">
      <PageIntro index={8} label="Settings" title="Settings" lead="Preferences here are stored in this browser only." />
      <div className="space-y-4">
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
