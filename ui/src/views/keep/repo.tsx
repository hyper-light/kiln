/**
 * A repository's copies on runners (docs/reach.md §5.1), under its row in Repositories: the copies
 * of its trees runners keep for the runners that ran commands there, what the rules say of each, and
 * its own rules for them.
 */
import { api } from "@/api/client";
import { useRes } from "@/lib/data";
import { formatBytes } from "@/lib/format";
import { invalidate } from "@/lib/resource";
import { RulesForm } from "./rules";
import { CopyList, keepRes, taking } from "./section";

export function RepoKeep({ repoKey }: { repoKey: string }) {
  const view = useRes(keepRes).data;
  const copies = view.runners.flatMap((r) => r.copies.filter((c) => c.repo === repoKey).map((c) => ({ ...c, runner: r.name })));
  const t = taking(copies);
  return (
    <section className="repo-section" aria-label="Copies other runners keep">
      <h4 className="field-label">Copies other runners keep</h4>
      <p className="kp-summary">
        {copies.length
          ? `${copies.length} ${copies.length === 1 ? "copy" : "copies"} of its trees, kept by the runners commands ran on for other runners: ${formatBytes(t.bytes)}${t.unmeasured ? "+" : ""}.`
          : "No runner keeps a copy of its trees for another runner now."}
      </p>
      {copies.length > 0 && <CopyList copies={copies} empty="" where />}
      <RulesForm
        rules={view.repos[repoKey] ?? {}}
        aboveName="each runner's"
        label={`${repoKey}'s rules for its copies`}
        onSave={async (r) => {
          await api.setRepoKeep(repoKey, r);
          invalidate("reach/keep");
        }}
      />
    </section>
  );
}
