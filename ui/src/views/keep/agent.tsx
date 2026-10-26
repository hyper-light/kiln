/**
 * An agent's copies on other runners (docs/reach.md §5.1), in its settings: the copies of its trees
 * runners keep since it ran commands there, what the rules say of each, and its own rules for them.
 */
import { api } from "@/api/client";
import { formatBytes } from "@/lib/format";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { RulesForm } from "./rules";
import { CopyList, taking } from "./section";

export function AgentKeep({ id }: { id: string }) {
  const k = useSuspenseResource(`harness/keep/${id}`, (s) => api.agentKeep(id, s), { pollMs: 15_000, on: ["keep"] }).data;
  const t = taking(k.copies);
  return (
    <section className="hx-ctl-group">
      <div className="hx-eyebrow">Copies on other runners</div>
      <p className="kp-summary">
        {k.copies.length
          ? `Runners keep ${k.copies.length} ${k.copies.length === 1 ? "copy" : "copies"} of its trees, from the commands it ran there: ${formatBytes(t.bytes)}${t.unmeasured ? "+" : ""}.`
          : "No runner keeps a copy of its trees now. Its rules apply to those its commands on other runners make."}
      </p>
      {k.copies.length > 0 && <CopyList copies={k.copies} empty="" where />}
      <RulesForm
        rules={k.rules}
        aboveName="its repository's and runner's"
        label="Its rules for its copies"
        onSave={async (r) => {
          await api.setAgentKeep(id, r);
          invalidate(`harness/keep/${id}`);
          invalidate("reach/keep");
        }}
      />
    </section>
  );
}
