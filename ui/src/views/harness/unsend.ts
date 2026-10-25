import type { Command } from "../../api/harness";

/**
 * Unsending a message the conversation shows queued, and undoing that (CP-T3.2, the twentieth
 * adversarial check's). Such a message is held by its harness or the harness's adapter (Claude Code's
 * own queue, an ACP or Codex adapter's waiting list), and `queue_remove` takes back only what kiln's
 * own queue holds: unsent, it still goes, once. So Undo only shows it again. Sent again, it would be
 * another message under an id of its own: a second prompt.
 */
export function unsending(id: string | undefined): { unsend?: Command; undo: Command[] } {
  return { unsend: id ? { type: "queue_remove", id } : undefined, undo: [] };
}
