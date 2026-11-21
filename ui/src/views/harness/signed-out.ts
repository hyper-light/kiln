/**
 * How each harness says it isn't signed in: the timeline reads every notice by it, so it loads with
 * the conversation; the account card that answers it loads when one shows (account-card.tsx, goal 8).
 */

/** How each harness says it isn't signed in, or that its sign-in ran out. */
const SIGNED_OUT: Record<string, RegExp> = {
  claude: /please run \/login|invalid api key|oauth token has expired|not logged in|authentication_error/i,
  codex: /\b401\b|unauthorized|not logged in|login required|refresh token/i,
};

/** Whether a harness's message says it needs signing in (only for harnesses kiln can sign in; an MCP server's own 401 is not the harness's account). */
export function saysSignedOut(kind: string, text: string): boolean {
  return (SIGNED_OUT[kind]?.test(text) ?? false) && !/\bmcp\b/i.test(text);
}
