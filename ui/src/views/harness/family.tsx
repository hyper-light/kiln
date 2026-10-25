/** The End dialog's handling of related agents. */
import * as React from "react";
import "./family.css";
import { harnessApi } from "@/api/harness";
import { useSuspenseResource } from "@/lib/resource";

/** An agent's live children, as the server knows them (theirs too). */
function useChildren(id: string) {
  return useSuspenseResource(`harness/children/${id}`, () => harnessApi.children(id)).data.children;
}

/** In the End dialog: the agents it started that still run, and whether they end with it. */
function EndsWith({ id, on, onChange }: { id: string; on: boolean; onChange: (on: boolean) => void }) {
  const kids = useChildren(id);
  if (kids.length === 0) return null;
  const names = kids.map((k) => k.name).join(", ");
  return (
    <span className="fam-end">
      <span className="fam-end-what">
        It started {kids.length === 1 ? "an agent that's" : `${kids.length} agents that are`} still running: {names}.
      </span>
      <label className="fam-end-check">
        <input type="checkbox" className="check" checked={on} onChange={(e) => onChange(e.target.checked)} />
        End {kids.length === 1 ? "it" : "them"} too
      </label>
    </span>
  );
}

/** What ending agent `id` ends with it: the line for its End dialog, and the end itself. */
export function useEndWithChildren(id: string) {
  const [withKids, setWithKids] = React.useState(true);
  const note = (
    <React.Suspense fallback={null}>
      <EndsWith id={id} on={withKids} onChange={setWithKids} />
    </React.Suspense>
  );
  return { note, end: () => harnessApi.end(id, withKids) };
}
