# Plan step 25: Devin's built-in Agentfile (the record)

Step 25's evidence is in `pin/`, `step2/`, `step3/` and `evidence/`. This file holds what later steps add to the
record.

## Hosts by name

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18, part 4 (plan 01a1d2bd5014705d918ad1f2ce1154f2, step 4).
`.kiln/images/harness-devin/Agentfile` grants these hosts by name (Shards D236), on TCP 443, HTTPS's port (RFC 9110
§4.2.2), and nothing else.

Devin's docs list its backend's domains to allowlist, mostly as wildcards. That list is `evidence/hosts/windsurf-common-issues.md`,
"What domains should I allowlist", saved as Cognition publishes it (`SOURCES.tsv`: URL, fetch date 2026-11-13,
SHA-256). A destination is a name, never a pattern (D236). So a host inside a documented wildcard is named from Devin's
own pinned build and cited to both. The build is quoted in `evidence/hosts/devin-build.txt`, with its URL and
SHA-256, matched against pin.json's as it downloaded.

| Host | For | Devin's docs | Devin's build (3000.12.43) |
|---|---|---|---|
| `api.devin.ai` | the API server of a devin.ai account, and the ACP live WebSocket | by name: "`api.devin.ai` (API backend and the ACP live WebSocket)" | its shipped `share/devin/docs/reference/partner-builds.mdx`: "The API key is only sent to `api.devin.ai`" |
| `server.codeium.com` | the API server of a Windsurf account, the build's default | the wildcard `.codeium.com`, under "For all customers, Devin Desktop requires (backend domains)" | `bin/devin`: `https://server.codeium.com` beside `WINDSURF_API_SERVER_URL`, which moves it |
| `unleash.codeium.com` | its feature flags, asked as it starts | the same wildcard `.codeium.com` | `bin/devin`: `https://unleash.codeium.com/api/` |

The list's other domains aren't granted.
- **The desktop app:** `.codeiumdata.com`, `update.windsurf.com` and `.windsurf.com`.
- **The desktop app's Google sign-in:** `.googleapis.com` and `apis.google.com`.
- **Updater manifests:** `static.devin.ai`. This layout has no updater.
- **The person's browser:** `app.devin.ai`, where the sign-in page opens.
- **Also not granted:** error reports (Sentry) and `devin ssh`.

Unchanged from part 2.
