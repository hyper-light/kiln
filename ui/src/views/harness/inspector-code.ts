/** W21-R02: selected inspector code can start with actual route navigation, before Agent mounts. */
import { ready, warm, type Res } from "@/lib/data";

export type InspectorTab = "changes" | "plan" | "tools";
type Tabs = typeof import("./inspector-tabs");
export const inspectorTabsRes: Res<Tabs> = { key: "code/agent/inspector-tabs", fetch: () => import("./inspector-tabs") };

/** Existing intent remains code-only; private review reads start only on navigation. */
export function prepareInspectorTab(tab: InspectorTab) {
  if (tab !== "changes") warm(inspectorTabsRes);
}

export function loadInspectorTab(tab: InspectorTab): Promise<void> {
  return tab === "changes" ? Promise.resolve() : ready(inspectorTabsRes);
}
