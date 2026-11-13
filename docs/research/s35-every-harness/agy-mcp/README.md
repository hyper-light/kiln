# How Antigravity's CLI takes MCP servers, for an isolated session's own token

kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 35; plan 01a1d2c16eb074f19cf7ae7f1025c0be step 4.

In a microVM, Antigravity's config (`~/.gemini/config/mcp_config.json`) is in the person's home
volume, which all their sessions share (step 18), so one session's own MCP token can't be written
there. The plan names three ways out: a per-session config location agy honours, header expansion,
or a stdio server from the shared config that takes the session's token from agy's environment.

`probe.sh` asks the build kiln's Agentfile pins (agy 1.5.7, `.kiln/images/harness-antigravity/pin.json`,
its tarball and binary checked against the pin's sha256). It starts agy as kiln does
(`--input-format stream-json --output-format stream-json`, `harness/antigravity.rs`) with its stdin
open and empty, so no message is sent and no turn runs, under a throwaway HOME with no credentials.
Its servers point at a local listener (`listen.py`) that records what reaches it.
`probe-1.5.7-linux-x64.log` is its output, verbatim.

| Asked | agy 1.5.7 said | So |
|---|---|---|
| An HTTP server in agy's own config, its `Authorization` header `Bearer ${KILN_PROBE_TOKEN}`, with that variable in agy's environment | the header reached the server as written, `Bearer ${KILN_PROBE_TOKEN}` (log lines 43-44) | no header expansion: a header holds a token only as written into the shared file |
| An HTTP server given in `--settings` ("settings for this session only, never saved") | `Error: --settings: unknown setting "mcpServers"` (line 24) | no per-session MCP server by `--settings` |
| A stdio server in agy's own config | it started at once, before any message, and saw `token=probe-abc123`, agy's own environment (line 41) | a stdio server named in the shared file takes each session's token from that session's environment |

agy connects to its servers as it starts, before any message and before it signs in (lines 43-46).

The way out is the third: kiln names a stdio server in the shared config, the same for every
session and holding no token, which passes each message to the session's own endpoint (step 3's
runner loopback, or the server's `/api/mcp`, at `host.docker.internal:<kiln's port>`) with the token
from the session's environment, which kiln sets for that session alone. What runs it inside the
microVM, and how kiln's isolated launch sets that environment and writes that entry, waits on step
18 (the person's volume layout and the isolated launch's environment) landing on kiln main.

To run it again: `bash probe.sh` from this folder on a Linux x86_64 machine with curl and python3.
