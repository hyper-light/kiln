/**
 * The agent's own definition (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P5.2, P7.2), first in its
 * Agents tab: the version this session started on beside the current one, its experiment's variant
 * (its swatch and name, never a letter), where it runs and how fast it starts, and the definition's
 * values, edited in place. In the head, the crumb that opens it.
 */
import { agentdefsRes } from "@/api/agentdefs";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { DefFields, RunsMarks, sessionVariant, Swatch } from "./definitions";
import { Fold } from "./fold";
import "./definition.css";

export function DefinitionPanel({ agentId }: { agentId: string }) {
  const defs = useRes(agentdefsRes).data;
  const d = defs.find((x) => x.sessions.some((s) => s.id === agentId));
  if (!d) return null;
  const mine = d.sessions.find((s) => s.id === agentId)!;
  const variant = sessionVariant(d, agentId);
  const weight = d.experiment?.variants.find((v) => v.id === mine.variant)?.weight;
  return (
    <Fold id="definition" title="This agent" count={`${d.name} v${mine.version}`}>
      <div className="df-panel">
        <span className="df-line">
          <b>{d.name}</b>
          <span>{d.harness}</span>
          <span>{d.agentfile}</span>
          <RunsMarks d={d} />
        </span>
        <span className="df-line">
          {mine.version === d.version ? (
            <span>v{d.version} · current</span>
          ) : (
            <>
              <span className="df-old">
                v{mine.version} → v{d.version}
              </span>
              <Button size="xs" variant="secondary" onClick={() => toast({ title: `Restarting on v${d.version}`, tone: "running" })}>
                Restart on v{d.version}
              </Button>
            </>
          )}
        </span>
        {variant && weight !== undefined && (
          <span className="df-line" title={`${variant.name}: ${weight}% of ${d.experiment!.id}'s new sessions`}>
            <Swatch tone={variant.tone} />
            <span>{variant.name}</span>
            <span className="meter" role="meter" aria-valuenow={weight} aria-valuemin={0} aria-valuemax={100} aria-label={`${weight}% of new sessions`}>
              <span style={{ width: `${weight}%` }} />
            </span>
            <span>{weight}%</span>
          </span>
        )}
        <DefFields d={d} />
      </div>
    </Fold>
  );
}

/**
 * In the agent's head, beside its harness and machine: the definition it was started from, the version,
 * and its variant as its swatch; pressed, the Agents tab, where they are. A session started from no
 * definition shows nothing here.
 */
export function DefCrumb({ agentId, onOpen }: { agentId: string; onOpen: () => void }) {
  const defs = useRes(agentdefsRes).data;
  const d = defs.find((x) => x.sessions.some((s) => s.id === agentId));
  if (!d) return null;
  const mine = d.sessions.find((s) => s.id === agentId)!;
  const variant = sessionVariant(d, agentId);
  return (
    <>
      <span aria-hidden="true">·</span>
      <button
        type="button"
        className="df-crumb"
        onClick={onOpen}
        title={[mine.version === d.version ? `${d.name} v${d.version}` : `Started on v${mine.version}; v${d.version} is current`, variant?.name].filter(Boolean).join(" · ")}
      >
        {d.name} v{mine.version}
        {mine.version !== d.version && <span className="df-old">→v{d.version}</span>}
        {variant && <Swatch tone={variant.tone} />}
      </button>
    </>
  );
}
