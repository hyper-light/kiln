# Plan step 19: Claude Code's built-in Agentfile (the record)

Step 19's research and its runs are in `research.md` and `research-restart.md`. This file holds what later steps
add to the record.

## Hosts by name

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18, part 4 (plan 01a1d2bd5014705d918ad1f2ce1154f2, step 4).
`.kiln/images/harness-claude/Agentfile` grants these hosts by name (Shards D236), on TCP 443, HTTPS's port (RFC 9110
§4.2.2), and nothing else. Each is cited to Anthropic's own documentation, saved as Anthropic publishes it in
`evidence/hosts/` (`SOURCES.tsv`: URL, fetch date 2026-11-13, SHA-256). The page is byte for byte the copy step 18
saved for part 2.

| Host | For | Cited to |
|---|---|---|
| `api.anthropic.com` | Claude Code's API | code.claude.com/docs/en/network-config.md, "Network access requirements": "Claude API requests, including the WebFetch domain safety check, feature flag fetches, and telemetry event logging" |
| `claude.ai` | the sign-in of a claude.ai account | the same table: "claude.ai account authentication" |
| `platform.claude.com` | a Console account's sign-in, and a claude.ai sign-in's token exchange, refresh and revocation | the same table: "Anthropic Console account authentication. OAuth token exchange, refresh, and revocation also go to this host for claude.ai accounts, so both Console and claude.ai sign-ins require it" |

The table's other hosts aren't granted.
- **The person's browser:** `claude.com` and `sso.claude.com`, pages a sign-in opens there.
- **Features kiln doesn't drive:** `mcp-proxy.anthropic.com` (claude.ai's connectors; kiln's own MCP rides the
  agent-control link, Shards D254), `bridge.claudeusercontent.com` (Chrome), `*.frame.claudeusercontent.com`
  (artifacts) and `*-review.googlesource.com` (the Desktop app's Gerrit lookup).
- **Plugins, installs, updates and the changelog:** `downloads.claude.ai`, `storage.googleapis.com`,
  `registry.npmjs.org`, `github.com`, `raw.githubusercontent.com` and `formulae.brew.sh`. Updates are off
  (`DISABLE_UPDATES`).
- **Telemetry:** the two Datadog hosts.
- **Documentation lookups:** `code.claude.com`.

Unchanged from part 2.
