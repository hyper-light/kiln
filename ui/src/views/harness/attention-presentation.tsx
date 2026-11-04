/** Actual list presentations, plan 01a199e1b177752e84dad5dc990e6a15. Ref cleanup closes visits, never ordinary re-renders. */
import * as React from "react";
import { attentionClose, attentionRender, fromAttentionRow, type AttentionSnapshot } from "@/api/attention";
import { clientId } from "@/api/harness";
import { AttentionPresentation } from "@/lib/attention-exposure";

/** The same phone layout as mission.css; pointer type is not a layout. */
export const attentionLayout = () => (typeof window === "undefined" ? "mission/unknown" : window.matchMedia("(max-width: 640px)").matches ? "mission/phone" : "mission/desktop");

function displayed(root: HTMLElement) {
  const rows = [...root.querySelectorAll<HTMLElement>(".ms-row[data-agent]")].filter((row) => row.getClientRects().length > 0);
  return {
    shown: rows.flatMap((row) => (row.dataset.agent ? [row.dataset.agent] : [])),
    eligible: new Set(rows.filter((row) => row.dataset.attentionEligible === "true").flatMap((row) => (row.dataset.agent ? [row.dataset.agent] : []))),
  };
}

export function useAttentionPresentation(input: { snapshot: AttentionSnapshot; controls: string; automatic: boolean; allowed: boolean; rendered: string; departure: (fn: () => void) => () => void }) {
  const [state] = React.useState(() => ({
    root: null as HTMLElement | null,
    presentation: new AttentionPresentation({
      render: (frame) => {
        void attentionRender(frame).catch(() => undefined);
      },
      close: (close) => {
        void attentionClose(close).catch(() => undefined);
      },
      key: clientId,
    }),
  }));
  const { snapshot, controls, automatic, allowed, rendered, departure } = input;
  const lifetime = React.useCallback(
    (node: HTMLSpanElement | null) => {
      if (!node) return;
      const root = node.closest<HTMLElement>(".ms-page");
      state.root = root;
      const close = () => state.presentation.close();
      const detach = departure(close);
      window.addEventListener("pagehide", close);
      return () => {
        detach();
        window.removeEventListener("pagehide", close);
        // Ref setup/cleanup can be replayed by React. Only a removed/hidden presentation ended.
        queueMicrotask(() => {
          if (!node.isConnected || !root?.getClientRects().length) close();
        });
        if (state.root === root) state.root = null;
      };
    },
    [state, departure],
  );

  const capture = React.useCallback(
    (node: HTMLSpanElement | null) => {
      void rendered; // Re-capture when the actual row/eligibility signature commits.
      if (!node || !state.root || document.hidden) return;
      const pair = snapshot.pair;
      if (!pair || pair.key !== snapshot.exposure) {
        state.presentation.close();
        return;
      }
      state.presentation.render({ pair, ...displayed(state.root), controls, context: attentionLayout(), automatic, allowed });
    },
    [state, snapshot.pair, snapshot.exposure, controls, automatic, allowed, rendered],
  );

  const choose = React.useCallback(
    (agent: string) => {
      const selection = state.root ? state.presentation.choose(agent, displayed(state.root).shown) : undefined;
      fromAttentionRow(agent, selection ?? snapshot.exposure ?? undefined);
    },
    [state, snapshot.exposure],
  );
  return { lifetime, capture, choose };
}
