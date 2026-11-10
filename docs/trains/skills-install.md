# Train notes: installed skills across harnesses

This train serves W20-R01. The person authorized merging, pushing and deploying it on
2026-11-09. It imports packages from GitHub links, local folders, uploaded archives or Markdown,
previews their contents, and installs selected skills for an agent, the person's agents, or a
repository folder. Setup is explicit, reports its commands and logs, and can be retried.

Claude Code uses its native plugin format. Codex registers installed skill roots before sending
queued turns, and uses native skill input after acknowledgement. Older Codex versions and the ACP
and headless harnesses receive scoped instructions and asset paths through Kiln's prompt writer.
This covers Devin, OpenCode, Gemini, Goose, Amp and Antigravity. Optional hooks, agent definitions
and package MCP configuration retain their harness-specific formats; the UI identifies them.

The additive migration `20261109-1200_skills_install` creates package and draft tables, with
content-addressed files under `skills/files`. It does not raise the schema epoch. An older server
ignores these tables. New postcard variants are appended, and runners advertise the named
`skills` feature. Older runners continue their existing sessions; placing packages and delivering
their instructions requires an upgraded runner. Runner upgrades remain the person's action.

The feature's native skills, prompt delivery, protocol and adapter checks are recorded in the
shipping evidence. Linux, macOS and Windows clippy passed; the expanded Windows Codex suite had
two failures in unchanged Unix-path fixtures. These are not claimed as passing. The main-based
candidate preserves main's Ship syntax worker change and reruns the combined UI checks.
The full Linux gate and deployment have their own recorded outcomes; this note makes no claim
that either has completed.

The gate repairs also serve W05-R25: a completed run retains its request until acknowledgment,
including the agent it owes news to. The existing optional request in its persisted run record is
restored even after completion. An adopter registers the run before telling members it owns it,
so a local member's synchronous completion cannot arrive before its follower exists. The regression
checks both request retention across restore and one start followed by one successful completion
when the adopter is also the runner. No new wire variant or schema change is needed for this fix.

The OIDC identity fixture accounts for the process-wide deployment service enabling the issuer
(W10-R11); account reload and permission retry grant their sessions headroom, so authentication
and permission refusal are the holds under test (W20-R01). The adapter measurement fixture counts completed waits independently of the
engine draining its samples. Conflict-agent fixtures wait until their runner advertises its harness
(W26-R17), and resume-from-copy waits for that session's committed native transcript instead of a
future telemetry timestamp (W27-R08). These preserve the behavior each test is meant to check.
The cached-page fixture waits for both sealed chunks to finish compaction before warming them
(G10-S5); one chunk becoming compact does not mean the second has finished.
All ACP configuration and protocol fixtures own a distinct admitted core, renewing its grant for the next
protocol interaction and releasing their waits afterward. Anonymous protocol cores do not join
runner admission: session startup sets a real session ID before starting its harness, and turn
completion already ignores anonymous cores. This keeps unrelated protocol fixtures from sharing
an empty admission key when another test starts the process-wide controller. The cleanup-timer fixture waits for
the persisted decision as well as the removed directory before checking its trigger (G9).

Production uses the dedicated bare-metal server, as documented in the conversation performance
and Mission Control trains. The retained Kubernetes app is not the serving process. Build the
exact landing commit with the production release profile and its carried Linux UI. Check the
candidate and saved previous executable against a private online SQLite backup in a network
namespace, with the real home masked and agents, discovery, CI/CD and updates disabled. Verify
the migration, readiness, commit and served assets before server-only atomic promotion. Preserve
the rollback executable and observe ten minutes of readiness, restart/OOM counters and runner
reconnections. Never replace a runner executable or restart a runner service for this release.
