/**
 * Above an agent's changes: its change request (docs/agentic.md C1, C4). Open one from its work —
 * the files it changed, committed beside the folder and pushed for review — or, once it's open,
 * where it stands and a new version of it.
 */
import * as React from "react";
import { ArrowUpRight, GitPullRequestArrow } from "lucide-react";
import { Link } from "@/lib/router";
import { changesApi, changesRes, changeWords, type AgentChange } from "@/api/changes";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";

/** Its page: kiln's own review for a change kiln hosts, else the host's. */
export function ChangeLink({ c, children, className }: { c: Pick<AgentChange, "provider" | "change" | "url">; children: React.ReactNode; className?: string }) {
  if (c.provider === "kiln" && c.change)
    return (
      <Link to={`/review/${c.change}`} className={className}>
        {children}
      </Link>
    );
  if (!c.url) return <span className={className}>{children}</span>;
  return (
    <a href={c.url} target="_blank" rel="noreferrer" className={className}>
      {children}
      <ArrowUpRight size={11} aria-hidden="true" className="ml-0.5 inline" />
    </a>
  );
}

/** Nothing at all when what's inside can't load: a row's extra words aren't worth an error. */
export class Quiet extends React.Component<{ children: React.ReactNode }, { failed: boolean }> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  render() {
    return this.state.failed ? null : this.props.children;
  }
}

function useChangeOf(agentId: string): AgentChange | undefined {
  return useRes(changesRes).data.find((x) => x.agent === agentId);
}

/** In a row's narrow meta line: its name, and whether it landed; the rest when pointed at. */
function RowChangeIn({ agentId }: { agentId: string }) {
  const c = useChangeOf(agentId);
  if (!c) return null;
  return (
    <span className="ms-change" data-state={c.state} title={changeWords(c)}>
      <ChangeLink c={c}>
        {c.change ?? "branch pushed"}
        {c.state === "landed" ? " landed" : c.state === "closed" ? " closed" : ""}
      </ChangeLink>
    </span>
  );
}

function RowReviewIn({ agentId }: { agentId: string }) {
  const c = useChangeOf(agentId);
  if (!c || c.state !== "open") return null;
  return (
    <ChangeLink c={c} className="ms-link">
      Review
    </ChangeLink>
  );
}

/** Mission Control's word on an agent's change request (C4): where it stands, linked. */
export function RowChange({ agentId }: { agentId: string }) {
  return (
    <Quiet>
      <React.Suspense fallback={null}>
        <RowChangeIn agentId={agentId} />
      </React.Suspense>
    </Quiet>
  );
}

/** A finished agent's open change request, to review from its row (C4). */
export function RowReview({ agentId }: { agentId: string }) {
  return (
    <Quiet>
      <React.Suspense fallback={null}>
        <RowReviewIn agentId={agentId} />
      </React.Suspense>
    </Quiet>
  );
}

export function ChangeBar({ agentId }: { agentId: string }) {
  const all: AgentChange[] = useRes(changesRes).data;
  const c = all.find((x) => x.agent === agentId);
  const [busy, setBusy] = React.useState(false);
  const [fix, setFix] = React.useState(true);
  const push = async () => {
    setBusy(true);
    try {
      const r = await changesApi.open(agentId, c ? {} : { fix_checks: fix });
      toast({ title: r.change ? `${r.change} version ${r.version} is up for review` : `Pushed ${r.branch}: open its pull request from its link`, tone: "running" });
      invalidate("agents/changes");
    } catch (e) {
      toast.error(c ? "Couldn't push a new version" : "Couldn't open a change request", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  if (!c) {
    return (
      <div className="hx-change-bar">
        <GitPullRequestArrow size={14} aria-hidden="true" className="hx-dim" />
        <span className="hx-change-what">Its work isn&apos;t up for review yet.</span>
        <label className="hx-change-opt">
          <input type="checkbox" className="check" checked={fix} onChange={(e) => setFix(e.target.checked)} />
          Fix failing checks by itself
        </label>
        <Button size="xs" variant="secondary" disabled={busy} onClick={() => void push()}>
          {busy ? "Opening…" : "Open change request"}
        </Button>
      </div>
    );
  }
  const open = c.state === "open";
  return (
    <div className="hx-change-bar" data-state={c.state}>
      <GitPullRequestArrow size={14} aria-hidden="true" className="hx-dim" />
      <span className="hx-change-what">
        <ChangeLink c={c}>{changeWords(c)}</ChangeLink>
        <span className="hx-change-sub">
          version {c.version}
          {c.repo ? ` · ${c.repo}` : ""} · {c.branch}
        </span>
      </span>
      {open && (
        <Button size="xs" variant="ghost" disabled={busy} onClick={() => void push()}>
          {busy ? "Pushing…" : "Push a new version"}
        </Button>
      )}
    </div>
  );
}
