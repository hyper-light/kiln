import { useCallback, useState } from "react";

const same = (a: ReadonlySet<string>, b: ReadonlySet<string>) => a.size === b.size && [...a].every((key) => b.has(key));

/**
 * W21-R02: DOM interaction state, not a second conversation cache. An immutable row window
 * keeps selected, focused and expanded rows mounted until that interaction is released.
 * Callback-ref ownership makes observer/listener cleanup follow the real committed element.
 */
export function useRowPins() {
  const [pins, setPins] = useState<ReadonlySet<string>>(() => new Set());
  const ref = useCallback((root: HTMLElement | null) => {
    if (!root) return;
    let alive = true;
    const read = () => {
      if (!alive) return;
      const next = new Set<string>();
      const add = (node: Element | null) => {
        for (let el = node; el && el !== root && root.contains(el); el = el.parentElement) {
          const key = el.getAttribute("data-ev")?.split(" ")[0];
          if (key) next.add(key);
        }
      };
      add(document.activeElement);
      for (const el of root.querySelectorAll('[aria-expanded="true"]')) add(el);
      const selection = document.getSelection();
      if (selection && !selection.isCollapsed) {
        const rows = root.querySelectorAll("[data-ev]");
        for (let i = 0; i < selection.rangeCount; i++) {
          const range = selection.getRangeAt(i);
          for (const el of rows) {
            try {
              if (range.intersectsNode(el)) add(el);
            } catch {
              // A browser can detach a range during a selectionchange notification.
            }
          }
        }
      }
      setPins((previous) => (same(previous, next) ? previous : next));
    };
    const focused = () => queueMicrotask(read);
    const observer = new MutationObserver(read);
    observer.observe(root, { subtree: true, childList: true, attributes: true, attributeFilter: ["aria-expanded"] });
    document.addEventListener("selectionchange", read);
    root.addEventListener("focusin", focused);
    root.addEventListener("focusout", focused);
    read();
    return () => {
      alive = false;
      observer.disconnect();
      document.removeEventListener("selectionchange", read);
      root.removeEventListener("focusin", focused);
      root.removeEventListener("focusout", focused);
    };
  }, []);
  return { pins, ref };
}
