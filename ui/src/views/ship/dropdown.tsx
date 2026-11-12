/**
 * An opened Ship row (docs/ci.md W26-R29): the repository's commits, and the picked one's sections
 * (Files with the Conversation, Jobs with the picked job's output, Lanes). Apart from the list
 * (index.tsx), which loads it as a row is about to open: its code is most of what the page has.
 */
import * as React from "react";
import type { ShipCommit } from "@/api/ship";
import { Button, buttonVariants } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { Link } from "@/lib/router";
import { ShipCommits, ShipCommitsSlot, type Narrow } from "./commits";
import { shipApi } from "@/api/ship";
import { preload } from "@/lib/resource";
import { OpenSections } from "./sections-load";
import { warmReview } from "./review/warm";

export { warmReview } from "./review/warm";

/**
 * Brings the dropdown's Files into view and focuses it (the Diff button, Changes plan
 * 01a17bd748ff73d98f09996ecdc6873d step 4). The item's diff lives in one place, Review's Files
 * (Ada, 2026-10-27). While its review loads, Files is a placeholder that the loaded section then
 * replaces: it's scrolled to once, and each Files that comes takes the focus, until the loaded one
 * has it (Review marks it with its summary, `.rv-sum`), the reader moves the focus elsewhere, or
 * 15 s pass.
 */
function showFiles(open: HTMLElement): (() => void) | undefined {
  let shown: HTMLElement | null = null;
  /** Whether there's nothing more to do. */
  const show = () => {
    const now = document.activeElement;
    // The reader has moved on (the focus is somewhere, and not in what was shown): leave it there.
    if (shown && now && now !== document.body && !shown.contains(now)) return true;
    const files = open.querySelector<HTMLElement>('section[aria-label="Files"]');
    if (!files) return false;
    if (files !== shown) {
      // Review's Files is focusable (tabIndex -1) since e538638e56; one that isn't is made so.
      if (!files.hasAttribute("tabindex")) files.tabIndex = -1;
      if (!shown) files.scrollIntoView({ block: "start", behavior: document.documentElement.dataset.motion === "paused" ? "auto" : "smooth" });
      files.focus({ preventScroll: true });
      shown = files;
    }
    return files.querySelector(".rv-sum") !== null;
  };
  if (show()) return undefined;
  const watch = new MutationObserver(() => {
    if (show()) stop();
  });
  const timer = window.setTimeout(() => stop(), 15_000);
  const stop = () => {
    watch.disconnect();
    window.clearTimeout(timer);
  };
  watch.observe(open, { childList: true, subtree: true });
  return stop;
}

/**
 * An opened repository's dropdown (docs/ci.md W26-R29): its commits first, then the picked one's
 * sections. The commit picked is the newest when it opened, and stays picked as newer ones arrive,
 * so what you're reading never turns into another commit under you: only a pick changes it, or the
 * row's Review (or Diff), which picks the newest again, opening at its Files.
 */
export function RepoDropdown({ head, narrow, reveal }: { head: ShipCommit; narrow: Narrow; reveal: number }) {
  const [picked, setPicked] = React.useState<ShipCommit>(head);
  const [seen, setSeen] = React.useState(reveal);
  if (seen !== reveal) {
    setSeen(reveal);
    setPicked(head);
  }
  // The newest picked: its latest data, as the row's, kept as it was last once a newer one arrives.
  if (picked.sha === head.sha && picked !== head) setPicked(head);
  const commit = picked.sha === head.sha ? head : picked;
  return (
    <Dropdown
      commit={commit}
      reveal={reveal}
      commits={
        <Loadable label="Loading its commits" fallback={<ShipCommitsSlot repo={head.repo} />}>
          <ShipCommits repo={head.repo} narrow={narrow} picked={commit.sha} onPick={setPicked} />
        </Loadable>
      }
    />
  );
}

/** A commit's sections, in their order: Files (with the Conversation), Jobs (with the picked job's output), Lanes; `commits` before them. */
function Dropdown({ commit, reveal, commits }: { commit: ShipCommit; reveal: number; commits?: React.ReactNode }) {
  // The section reads start together, before its rendering code can suspend (W26-R15).
  warmReview(commit);
  preload(`ship/commit/${commit.repo}/${commit.sha}`, (signal) => shipApi.commit(commit.repo, commit.sha, signal), { pollMs: 15_000, on: ["run", "job"] });
  // A Diff press (reveal counts them) brings Files into view; opening the row any other way doesn't.
  const root = React.useRef<HTMLDivElement | null>(null);
  const files = React.useCallback(
    (el: HTMLDivElement | null) => {
      root.current = el;
      return el && reveal > 0 ? showFiles(el) : undefined;
    },
    [reveal],
  );
  return (
    <div className="ship-open" ref={files}>
      {commits}
      {/* Dropdown boundary agreed with the Ship agent 01a1748c, 2026-10-26 20:28 PDT (NOTES.md):
          Files (with the Conversation) first, owned by Review. One loader for them all: every
          section waits for Files' review, so none is drawn first and then pushed down (2026-10-28).
          A commit picked anew is a new set of sections (W26-R29). */}
      <Loadable key={commit.sha} label="Loading its review">
        <OpenSections commit={commit} />
        {/* f-ship's foot (plan 01a1856fe81375b1bd1d1d86f270a45b, step 11): the commit's runs (on its
            page), its Files (here), its page. */}
        <nav className="ship-foot" aria-label={`${commit.short}'s pages`}>
          <Link className={buttonVariants({ variant: "secondary", size: "xs" })} to={`/ship/${commit.repo}/${commit.sha}#runs`}>
            Runs
          </Link>
          <Button
            variant="secondary"
            size="xs"
            onClick={() => {
              if (root.current) showFiles(root.current);
            }}
          >
            Diff
          </Button>
          <Link className={`${buttonVariants({ variant: "secondary", size: "xs" })} ship-foot__page`} to={`/ship/${commit.repo}/${commit.sha}`}>
            Commit
          </Link>
        </nav>
      </Loadable>
    </div>
  );
}
