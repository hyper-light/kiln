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
import { changeState } from "./change-state";
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
      toast.error(c ? "Couldn't push a new version" : "Couldn't open a pull request", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const s = changeState(c);
  // Its state, what it is, and what to do: one line on a wide card, a line each on a narrow one.
  return (
    <div className="hx-cr" data-state={s.key}>
      <div className="hx-cr-in">
        <span className="hx-cr-head">
          <GitPullRequestArrow size={14} aria-hidden="true" className="hx-dim" />
          <span className="status hx-cr-state" title={c ? changeWords(c) : "Its work isn't up for review yet"}>
            <i className="dot" data-status={s.dot} aria-hidden="true" />
            <span className="status-text" data-status={s.dot}>
              {s.word}
            </span>
          </span>
        </span>
        {c ? (
          <span className="hx-cr-what">
            <ChangeLink c={c}>{c.change ?? c.branch}</ChangeLink>
            <span className="hx-cr-sub">
              v{c.version}
              {c.repo ? ` · ${c.repo}` : ""}
              {c.change ? ` · ${c.branch}` : ""}
            </span>
          </span>
        ) : (
          <span className="hx-cr-fill" />
        )}
        {!c ? (
          <span className="hx-cr-do">
            <label className="hx-cr-opt" title="Failing checks go to the agent to fix">
              <input type="checkbox" className="check" checked={fix} onChange={(e) => setFix(e.target.checked)} />
              Auto-Fix
            </label>
            <Button size="xs" variant="secondary" disabled={busy} onClick={() => void push()} title="Commit its work beside the folder and push it for review">
              {busy ? "Opening…" : "Open PR"}
            </Button>
          </span>
        ) : (
          c.state === "open" && (
            <span className="hx-cr-do">
              <Button size="xs" variant="secondary" disabled={busy} onClick={() => void push()} title="Push its work as a new version">
                {busy ? "Pushing…" : "Push update"}
              </Button>
            </span>
          )
        )}
      </div>
    </div>
  );
}
