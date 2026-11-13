# Plan step 20: Codex's built-in Agentfile (the record)

Step 20's record is `step1.md` to `step3.md`. This file holds what later steps add to it.

## Hosts by name

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18, part 4 (plan 01a1d2bd5014705d918ad1f2ce1154f2, step 4).
`.kiln/images/harness-codex/Agentfile` grants these hosts by name (Shards D236), on TCP 443, HTTPS's port (RFC 9110
§4.2.2), and nothing else.

OpenAI's Codex documentation has no page listing the CLI's hosts. Its index (`evidence/hosts/learn-chatgpt-llms.txt`)
has pages on sandboxing, approvals and permission profiles, but none on the hosts themselves. So each host is cited
to the OpenAI page that names it, where one does, and to Codex's own source in every case: codex-rs at the pinned
tag rust-v0.167.2, in `evidence/hosts/codex-rs-excerpts.txt` (numbered lines, each file's URL and whole SHA-256).
Every page is saved as OpenAI publishes it, in `evidence/hosts/` (`SOURCES.tsv`: URL, fetch date 2026-11-13,
SHA-256).

| Host | For | OpenAI's page | Codex's source |
|---|---|---|---|
| `api.openai.com` | the Responses API, signed in with an API key | the API reference, `openai-api-reference-overview.md`: `curl https://api.openai.com/v1/models` | `cli/src/doctor.rs` 3942: `"https://api.openai.com/v1/responses"` |
| `chatgpt.com` | Codex's Responses through ChatGPT's backend, signed in with ChatGPT | `connect-to-a-gateway.md`: "the gateway must proxy `/models` and `/responses` to `https://chatgpt.com/backend-api/codex`"; `config-sample.md`: `chatgpt_base_url = "https://chatgpt.com/backend-api/"` | `cli/src/doctor.rs` 3955: `"https://chatgpt.com/backend-api/codex/responses"` |
| `auth.openai.com` | the ChatGPT sign-in, its token refresh and its revocation | `app-server.md`, the device sign-in: `"verificationUrl": "https://auth.openai.com/codex/device"` | `login/src/server.rs` 78: `DEFAULT_ISSUER: &str = "https://auth.openai.com"`; `login/src/auth/manager.rs` 230-231: `.../oauth/token`, `.../oauth/revoke` |

OpenAI's pages name `auth.openai.com` only as the device sign-in's page. Its token exchange, refresh and revocation
rest on codex-rs. Unchanged from part 2.
