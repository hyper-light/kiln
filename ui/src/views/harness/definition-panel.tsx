/**
 * The agent's Tools tab, first: the definition it was started from (kiln plan
 * 01a1cd8e862e7169a074db3d5fa33f3d, P5.2, P7.2). The Tools tab is what it works with (MCP servers,
 * skills, usage), and a definition is where those come from, so it sits there rather than in a tab of
 * its own: the version this session started on beside the current one, its experiment's variant, and
 * the definition's values, edited in place.
 */
import { agentdefsRes } from "@/api/agentdefs";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { DefFields, variantTone } from "./definitions";
import { Fold } from "./fold";
import "./definition.css";

export function DefinitionPanel({ agentId }: { agentId: string }) {
  const defs = useRes(agentdefsRes).data;
  const d = defs.find((x) => x.sessions.some((s) => s.id === agentId));
  if (!d) return null;
  const mine = d.sessions.find((s) => s.id === agentId)!;
  const variant = d.experiment?.variants.find((v) => v.key === mine.variant);
  const vi = d.experiment && variant ? d.experiment.variants.indexOf(variant) : -1;
  return (
    <Fold id="definition" title="Definition" count={`v${mine.version}`}>
      <div className="df-panel">
        <span className="df-line">
          <b>{d.name}</b>
          <span>{d.harness}</span>
          <span>{d.agentfile}</span>
          {mine.version === d.version ? (
            <span>v{d.version}, current</span>
          ) : (
            <>
              <span className="df-old">
                started on v{mine.version}; v{d.version} is current
              </span>
              <Button size="xs" variant="secondary" onClick={() => toast({ title: `Restarting on v${d.version}`, tone: "running" })}>
                Restart on v{d.version}
              </Button>
            </>
          )}
        </span>
        {d.experiment && variant && (
          <span className="df-line" title={`Variant ${variant.key} of ${d.experiment.id}`}>
            <span className="df-key" style={{ background: variantTone(vi, d.experiment.variants.length) }}>
              {variant.key}
            </span>
            <span>
              {variant.agentfile} · {variant.cpus} vCPU · {Math.round(variant.memory / 2 ** 30)} GiB
            </span>
            <span className="meter" role="meter" aria-valuenow={variant.weight} aria-valuemin={0} aria-valuemax={100} aria-label={`Weight ${variant.weight}%`}>
              <span style={{ width: `${variant.weight}%` }} />
            </span>
            <span>
              {variant.weight}% of {d.experiment.id}
            </span>
          </span>
        )}
        <DefFields d={d} />
      </div>
    </Fold>
  );
}

/**
 * In the agent's head, beside its harness and machine: the definition it was started from, the version
 * and its variant; pressed, the Tools tab, where they are. A session started from no definition shows
 * nothing here.
 */
export function DefCrumb({ agentId, onOpen }: { agentId: string; onOpen: () => void }) {
  const defs = useRes(agentdefsRes).data;
  const d = defs.find((x) => x.sessions.some((s) => s.id === agentId));
  if (!d) return null;
  const mine = d.sessions.find((s) => s.id === agentId)!;
  const variant = d.experiment?.variants.find((v) => v.key === mine.variant);
  const vi = d.experiment && variant ? d.experiment.variants.indexOf(variant) : -1;
  return (
    <>
      <span aria-hidden="true">·</span>
      <button type="button" className="df-crumb" onClick={onOpen} title={mine.version === d.version ? `${d.name} v${d.version}` : `Started on v${mine.version}; v${d.version} is current`}>
        {d.name} v{mine.version}
        {mine.version !== d.version && <span className="df-old">→v{d.version}</span>}
        {variant && d.experiment && (
          <span className="df-key" style={{ background: variantTone(vi, d.experiment.variants.length) }} title={`Variant ${variant.key} of ${d.experiment.id}`}>
            {variant.key}
          </span>
        )}
      </button>
    </>
  );
}
