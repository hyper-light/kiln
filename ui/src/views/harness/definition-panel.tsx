/**
 * The crumb in an agent's head (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P5.2, P7.2): the definition
 * it was started from, its version and variant. Its agent's card in the Engine tab (engine-tab.tsx) says
 * the rest: what it started on, its variant, its settings, edited in place.
 */
import { agentdefsRes } from "@/api/agentdefs";
import { useRes } from "@/lib/data";
import { sessionVariant, Swatch } from "./definition-parts";
import { useEngineOf } from "./engine-of";
import "./definition.css";

/**
 * In the agent's head, beside its harness and machine: the definition it was started from, the version,
 * and its variant as its swatch; pressed, the Engine tab, where they are (in a microVM: a session on bare
 * metal has no Engine tab, so its crumb only says). A session started from no definition shows nothing here.
 */
export function DefCrumb({ agentId, isolation, onOpen }: { agentId: string; isolation?: string; onOpen: () => void }) {
  const defs = useRes(agentdefsRes).data;
  const engine = useEngineOf(agentId, isolation);
  const d = defs.find((x) => x.sessions.some((s) => s.id === agentId));
  if (!d) return null;
  const mine = d.sessions.find((s) => s.id === agentId)!;
  const variant = sessionVariant(d, agentId);
  const said = (
    <>
      {d.name} v{mine.version}
      {mine.version !== d.version && <span className="df-old">→v{d.version}</span>}
      {variant && <Swatch tone={variant.tone} />}
    </>
  );
  const title = [mine.version === d.version ? `${d.name} v${d.version}` : `Started on v${mine.version}; v${d.version} is current`, variant?.name].filter(Boolean).join(" · ");
  return (
    <>
      <span aria-hidden="true">·</span>
      {engine === "microvm" ? (
        <button type="button" className="df-crumb" onClick={onOpen} title={title}>
          {said}
        </button>
      ) : (
        <span className="df-crumb" title={title}>
          {said}
        </span>
      )}
    </>
  );
}
