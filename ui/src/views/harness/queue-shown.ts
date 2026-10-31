import type { Delivery } from "../../api/wire/Delivery";

/**
 * What a queue holds that the conversation doesn't show yet (CP-T3.2). A message it does show,
 * queued where it was said (held for a harness that has since ended: its delivery state is there),
 * waits there, with its own Edit and Unsend, and isn't listed again above the composer.
 */
export function notShown<T extends { id: string }>(queue: T[], delivery: Partial<Record<string, Delivery>> | undefined): T[] {
  return delivery && queue.some((q) => delivery[q.id]) ? queue.filter((q) => !delivery[q.id]) : queue;
}
