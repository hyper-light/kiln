/** W26-R15: optional syntax work starts only for a committed, enabled reader, never during render. */
import { useCallback, useMemo, useSyncExternalStore } from "react";
import type { Language } from "./languages";
import { HighlightClient, highlightResource } from "./async-client";
import type { HighlightLines } from "./worker-protocol";

const client = new HighlightClient(() => (typeof Worker === "undefined" ? null : new Worker(new URL("./worker.ts", import.meta.url), { type: "module" })));
const plain = () => null;

/** Text and result indices always match. Disabled completed readers retain their exact tokens. */
export function useAsyncHighlight(lang: Language | null | undefined, texts: readonly string[] | null, enabled = true): HighlightLines | null {
  const id = lang?.id;
  const resource = useMemo(() => highlightResource(client, id, texts), [id, texts]);
  const subscribe = useCallback(
    (listener: () => void) => {
      if (!enabled || typeof window === "undefined") return () => {};
      // Preserve the synchronous highlighter's first-load priority: optional WASM/query traffic
      // starts after the document's critical assets, with no request left behind on cancellation.
      if (document.readyState === "complete") return resource.subscribe(listener);
      let off: (() => void) | undefined;
      const start = () => {
        off = resource.subscribe(listener);
      };
      window.addEventListener("load", start, { once: true });
      return () => {
        window.removeEventListener("load", start);
        off?.();
      };
    },
    [enabled, resource],
  );
  return useSyncExternalStore(subscribe, resource.read, plain);
}
