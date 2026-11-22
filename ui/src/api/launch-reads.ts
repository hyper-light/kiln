/**
 * Mission control's first reads of the machines, as its composer asks for them (W21-R02). The document asks the same
 * URLs as it's parsed (lib/early-script.ts) and the composer's own reads take those answers, so the two are said once,
 * here: with another URL the app would read them again.
 */

/** Every machine's kinds, each distinct list written once (`?shared=1`, docs/agents.md). */
export const KINDS_READ = "/api/harness/kinds?shared=1";

/** The runners by name and what they're doing, without what only a ranking reads of them (`?lean=1`, docs/reach.md). */
export const REACH_READ = "/api/reach/runners?lean=1";

/** The machines that have joined, each with only what starting an agent on it reads of its facts (`?lean=1`, docs/api.md). */
export const AGENTS_READ = "/api/agents?lean=1";

/**
 * The agents kiln runs, each without what a list of them doesn't read (where it started, and its last words when it has
 * some: api/last-words.ts), written as columns (`?lean=1`, docs/agents.md). Asked on Mission Control.
 */
export const SESSIONS_READ = "/api/harness/sessions?lean=1";

/** Only the agents currently needing shell-level attention cards (approvals, held goals, error stops, usage limits). */
export const WAITING_READ = "/api/harness/waiting";
