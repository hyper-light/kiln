/** "Start agents here": from a terminal tab, start agents of any harness inside that shell. */
import * as React from "react";
import { useNavigate } from "@/lib/router";
import { Bot } from "lucide-react";
import { Tip } from "@/components/ui/tooltip";
import { SpawnDialog } from "./spawn";

export function SpawnHereButton({ target, shell }: { target: string; shell: string | null }) {
  const [open, setOpen] = React.useState(false);
  const navigate = useNavigate();
  return (
    <>
      <Tip content="Start agents in this shell">
        <button className="icon-btn" data-variant="bare" data-size="sm" aria-label="Start agents in this shell" disabled={!shell} onClick={() => setOpen(true)}>
          <Bot size={14} />
        </button>
      </Tip>
      {open && (
        <SpawnDialog
          open={open}
          onOpenChange={setOpen}
          defaults={{ target, shell: shell ?? undefined }}
          onStarted={(started) => {
            setOpen(false);
            navigate(started.length === 1 && started[0] ? `/agents/${started[0].id}` : "/agents");
          }}
        />
      )}
    </>
  );
}
