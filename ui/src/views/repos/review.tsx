/**
 * A repository's Review section (docs/ci.md W26-R18): the switch that lets agents fix code from
 * agents' feedback, off until someone who can land its changes turns it on; what it does, in
 * words; who set it and when. Someone who can't change it sees it disabled, saying why. Turning it
 * moves nothing: the same rows, whatever they say.
 */
import * as React from "react";
import { agentFixesRes, fixesSetBy, reviewApi } from "@/api/review";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";

/** Why someone who can't change the switch can't. */
export const CANT_CHANGE = "Only someone who can land its changes (diffs.land) turns it on or off.";

/** Brings the section into view as it's drawn, when Ship's agent fixes linked to it. */
const revealHere = (el: HTMLElement | null) => el?.scrollIntoView({ block: "center" });

/** `reveal`: the URL asked for this section, so it's brought into view as it's drawn. */
export function RepoReview({ repo, reveal }: { repo: string; reveal?: boolean }) {
  const res = useRes(agentFixesRes(repo));
  const v = res.data;
  // What it's being turned to, until the server says it is.
  const [turning, setTurning] = React.useState<boolean | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const says = React.useId();
  const turn = async () => {
    if (turning !== null) return;
    const to = !v.agents_fix;
    setTurning(to);
    setError(null);
    try {
      const now = await reviewApi.setAgentFixes(repo, to);
      res.setData(() => now);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setTurning(null);
    }
  };
  return (
    <section className="repo-section" aria-label="Review" ref={reveal ? revealHere : undefined}>
      <h4 className="field-label">Review</h4>
      <button
        type="button"
        role="switch"
        className="repo-switch"
        aria-checked={turning ?? v.agents_fix}
        aria-busy={turning !== null}
        aria-describedby={says}
        disabled={!v.may_change}
        title={v.may_change ? undefined : CANT_CHANGE}
        onClick={() => void turn()}
      >
        <i aria-hidden="true" />
        <span>Agents fix from agents' feedback</span>
      </button>
      <p className="repo-switch-words" id={says}>
        {v.says}
      </p>
      <p className="repo-switch-words" data-error={error ? "" : undefined} role={error ? "alert" : undefined}>
        {error ?? fixesSetBy(v)}
      </p>
      {!v.may_change && <p className="repo-switch-words">{CANT_CHANGE}</p>}
    </section>
  );
}
