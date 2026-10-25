/**
 * Above an agent's changes, when it works in a workspace of its own (docs/agentic.md W3, W6): a
 * Slates volume over its folder, where what it changes stays its own. Writing it back to the
 * folder shows the plan first — what it would create, replace and delete, and any file changed on
 * disk since, which it won't overwrite — and writes exactly that, on your go-ahead.
 */
import * as React from "react";
import { Layers } from "lucide-react";
import { harnessApi, type LandingPlan } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { tildify } from "./derive";

const WORD: Record<string, [string, string]> = {
  create: ["new file", "new files"],
  replace: ["changed file", "changed files"],
  delete: ["deleted file", "deleted files"],
  mkdir: ["new folder", "new folders"],
  rmdir: ["removed folder", "removed folders"],
  rename: ["move", "moves"],
  symlink: ["link", "links"],
  chmod: ["mode change", "mode changes"],
};

/** `2 changed files · 1 new file`. */
function summaryOf(p: LandingPlan): string {
  const by = p.plan.summary?.by_action ?? [];
  const parts = by.filter((a) => a.count > 0).map((a) => `${a.count} ${(WORD[a.action] ?? [a.action, a.action])[a.count === 1 ? 0 : 1]}`);
  return parts.length ? parts.join(" · ") : "nothing to write";
}

/** `over`: the folder its volume is laid over. */
export function WorkspaceBar({ agentId, over: folder }: { agentId: string; over: string }) {
  const [plan, setPlan] = React.useState<LandingPlan | null>(null);
  const [busy, setBusy] = React.useState(false);
  const over = folder ? tildify(folder) : "its folder";
  const look = async () => {
    setBusy(true);
    try {
      setPlan(await harnessApi.planLanding(agentId));
    } catch (e) {
      toast.error("Couldn't plan writing it to disk", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const write = async () => {
    const manifest = plan?.plan.manifest;
    if (!manifest) return;
    setBusy(true);
    try {
      const r = await harnessApi.land(agentId, manifest);
      if (r.changed && r.plan) {
        // Something changed since you looked: the new plan, to look at again.
        setPlan({ plan: r.plan, excludes: r.excludes ?? [], can_write: r.can_write ?? true, why_not: r.why_not });
        toast({ title: "It changed since you looked: here's what it would write now", tone: "info" });
        return;
      }
      toast({ title: `Written to ${over}`, tone: "succeeded" });
      setPlan(null);
    } catch (e) {
      toast.error("Couldn't write it to disk", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const conflicts = plan?.plan.conflicts ?? [];
  return (
    <div className="hx-change-bar hx-ws-bar">
      <div className="hx-ws-head">
        <Layers size={14} aria-hidden="true" className="hx-dim" />
        <span className="hx-change-what">
          Its own volume
          <span className="hx-change-sub">over {over} · what it changes stays here until it&apos;s up for review or written to disk</span>
        </span>
        {!plan && (
          <Button size="xs" variant="ghost" disabled={busy} onClick={() => void look()}>
            {busy ? "Looking…" : "Write to disk…"}
          </Button>
        )}
      </div>
      {plan && (
        <div className="hx-ws-plan">
          <p>
            {plan.plan.grant_required ? summaryOf(plan) : "Nothing waits to be written."}
            {plan.plan.summary && plan.plan.summary.filtered_out > 0 && <span className="hx-dim"> · git&apos;s own files stay out</span>}
          </p>
          {conflicts.length > 0 && (
            <>
              <p className="hx-dim">Changed in {over} since the agent changed them, so nothing is written until they agree:</p>
              <ul className="hx-ws-conflicts">
                {conflicts.map((c) => (
                  <li key={c}>{c.replace(/ \(\w+\)$/, "")}</li>
                ))}
              </ul>
            </>
          )}
          {!plan.can_write && plan.why_not && <p className="hx-dim">{plan.why_not}</p>}
          <div className="hx-ws-actions">
            {plan.can_write && plan.plan.grant_required && conflicts.length === 0 && (
              <Button size="xs" variant="secondary" disabled={busy} onClick={() => void write()}>
                {busy ? "Writing…" : `Write to ${over}`}
              </Button>
            )}
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => setPlan(null)}>
              {plan.can_write ? "Not now" : "Close"}
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
