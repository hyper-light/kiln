/**
 * Where a session runs, drawn the same everywhere (Mission's rows, an agent's tabs, chat, Runners):
 * its engine as a glyph (a microVM's box, the host's server), how its microVM started (flame warm,
 * layers from its template, snowflake cold, each in its place along kiln's prism) with how long it
 * took, and a pool's warm hosts as dots.
 */
import { Box, Container, Flame, Layers, Server, Snowflake } from "lucide-react";
import type { Engine, StartKind } from "@/api/microvms";
import "./sandbox-marks.css";

export const ENGINE_ICON: Record<Engine, typeof Box> = { microvm: Box, host: Server, container: Container };
export const ENGINE_WORD: Record<Engine, string> = { microvm: "microVM", host: "host", container: "container" };
/** Where its sessions run, in words: "microVMs", "its host", "containers". */
export const ENGINE_PLACE: Record<Engine, string> = { microvm: "microVMs", host: "its host", container: "containers" };
export const START: Record<StartKind, { icon: typeof Flame; word: string }> = {
  warm: { icon: Flame, word: "Warm start" },
  template: { icon: Layers, word: "From its template" },
  cold: { icon: Snowflake, word: "Cold start" },
};
/** A start's time: 214ms, 1.9s. */
export const msText = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}s` : `${Math.round(n)}ms`);

/** Its engine as a glyph, its word beside it when `word`. */
export function EngineGlyph({ engine, word, title }: { engine: Engine; word?: boolean; title?: string }) {
  const Icon = ENGINE_ICON[engine];
  return (
    <span
      className="sm-engine"
      data-engine={engine}
      title={title ?? (engine === "microvm" ? "In a microVM" : engine === "container" ? "In a container on its workspace" : "On the host, without a microVM")}
    >
      <Icon size={12} aria-hidden="true" />
      {word ? <span>{ENGINE_WORD[engine]}</span> : <span className="sr-only">{ENGINE_WORD[engine]}</span>}
    </span>
  );
}

/** How a microVM started, and how long it took. */
export function StartMark({ kind, ms }: { kind: StartKind; ms: number }) {
  const S = START[kind];
  return (
    <span className="sm-start" data-kind={kind} title={`${S.word} in ${msText(ms)}`}>
      <S.icon size={12} aria-hidden="true" />
      {msText(ms)}
    </span>
  );
}

/** A pool's warm hosts of its target, as dots. */
export function WarmDots({ n, of }: { n: number; of: number }) {
  return (
    <span className="sm-warm" role="img" aria-label={`${n} warm of ${of}`} title={`${n} warm of ${of}`}>
      {Array.from({ length: of }, (_, i) => (
        <i key={i} data-on={i < n || undefined} />
      ))}
    </span>
  );
}
