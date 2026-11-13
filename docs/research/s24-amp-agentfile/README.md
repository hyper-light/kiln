# Plan step 24: Amp's built-in Agentfile (the record)

Step 24's evidence is in `evidence/` and `scripts/`. This file holds what later steps add to the record.

## Hosts by name

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18, part 4 (plan 01a1d2bd5014705d918ad1f2ce1154f2, step 4).
`.kiln/images/harness-amp/Agentfile` grants one host by name (Shards D236), on TCP 443, HTTPS's port (RFC 9110
§4.2.2), and nothing else. Amp's pages are saved as Amp publishes them in `evidence/hosts/` (`SOURCES.tsv`: URL,
fetch date 2026-11-13, SHA-256). Amp's own pinned build is quoted in `evidence/hosts/amp-build.txt`, with the build's
URL and SHA-256, matched against pin.json's as it downloaded.

| Host | For | Amp's docs | Amp's build (0.0.1794456033-gb2ff22) |
|---|---|---|---|
| `ampcode.com` | Amp's server: every request the CLI makes, its sign-in, its settings and its threads | `cli-runners.md` 435: "Every time a thread starts on the runner, the runner asks ampcode.com for the variables"; `cli-settings.md` 242: `amp.url` is the setting for "the server that provides the settings" | `--amp-url <url>` "Amp server URL" (`AMP_URL`); "AMP_URL optionally overrides ampcode.com" |

Amp's docs have no list of hosts. Its proxy section names only proxy variables, and its "Egress IPs" page lists the
addresses Amp's servers connect from, not to. So the server's name rests on the docs' own words above, and on the
build.

Not granted:
- `static.ampcode.com`: downloads and update checks, which are off (`AMP_SKIP_UPDATE_CHECK`);
- the hosts of integrations kiln doesn't drive.

Unchanged from part 2.
