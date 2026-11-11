# B24 alongside-instance rollout proposal — not executed

Prepared by implementer `/root/kiln_status_trace` under Shards policy revision
`7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full policy/CLAUDE and current Kiln ROOT readback; root owns execution/promotion.
No Library files, enrollment, binaries, services or running processes were changed.

Review artifact: [b24-alongside-review.py](b24-alongside-review.py).
It is a script to review, not a service already installed. Its default mode only
checks. `--start` and `--rollback` are explicitly mutating future root actions.

## Verified preflight

Runner `b24c5c951260dcdfd0972f2aeaf3cf51`, 2026-11-11 08:13:07 UTC, normal
1 CPU / 1 GiB / 1 GiB, no sync. Safe receipt:
[b24-alongside-preflight-20261111T081307Z.json](b24-alongside-preflight-20261111T081307Z.json).

- `/Users/adalundhe/.local/bin/kiln` is a regular file, not a symlink; Mach-O arm64,
  308,493,200 bytes; SHA-256
  `1ae92b19b6acf6a2f5e91e7efeffe0f249236fa26c30c5c5d6b71d73f2a04ad9`.
- The exact compiled f555 commit bytes occur at offset 154,386,012; the previous
  32f174 commit bytes are absent. `codesign --verify --strict --verbose=2` exits 0:
  valid on disk, satisfies its Designated Requirement. Native `--version` exits 0.
  This is embedded-source/signature evidence, not proof of a newly running instance.
  The signed-on-this-Mac canonical hash differs from the server's uploaded artifact;
  this proposal pins the exact observed canonical bytes and refuses if they change.
- Free space 65,659,174,912 bytes; required 30 GiB plus one canonical-binary size
  is 32,520,747,920 bytes. The script replaces/copies no binary.
- Original service `io.github.hyper-light.kiln-agent`, supervisor PID1516, remains
  loaded. Existing ProgramArguments and two environment entries were read in memory;
  no environment values or complete argument list are in the retained receipt.
- Existing enrollment `agent.pem`, `agent.key`, `ca.pem` files exist in the common
  agent state directory. Only existence was checked; no PKI value was read or printed
  by the preflight or script.

The original connected runner continues to execute
`/Users/adalundhe/.local/share/kiln/agent/versions/4dfd3bb19754/kiln` and holds its
Claude child. Replacing that watched file is forbidden. The installer's launchd
fast path only copied the canonical path, so this avoids calling it again.

## Existing supported seams

Facts checked against immutable f555 source, not guessed native flags:

- `agent.rs::credentials` chooses `instance_credentials` whenever `--instance` is
  present. `instance_credentials` (f555 lines 291–340) reads the already enrolled
  machine proof/key internally and calls `upgrade::fetch_certificate`, then writes
  only the new instance's certificate. The script makes no CSR, certificate, key,
  enrollment HTTP request or custom auth/protocol implementation.
- `upgrade.rs::instance_dir` puts mutable state in `state/instances/<id>`.
  `InstanceRecord {id,created_at}` is the normal installed-instance metadata, and
  `created_at` is advertised to rank the new process. Fresh `--instance` alone
  without this record would advertise zero and is insufficient for newest ranking.
- `upgrade.rs::instance_unit` is `kiln-agent-` plus the first eight alphanumeric
  ID characters. `install.rs::launchd_label` prefixes `io.github.hyper-light.`.
- `entry.rs::AgentArgs` uses **`--token`**, environment `KILN_JOIN_TOKEN`, and
  `--instance`/`KILN_INSTANCE`. Both token spellings and old instance flags are
  removed from the copied child arguments, and the old token/instance/transient
  supervisor environment entries are removed in memory. All other static agent
  arguments and environment values are copied without printing them.
- `supervise --exe <canonical> -- agent ... --instance <new>` is its existing
  native child/supervisor path. The new instance makes the supervisor lock key
  distinct. Its stdout/stderr go to the new instance's own `kiln.log`.
- `handovers.rs::Hub::check_handover` uses normal crowded-instance events to ask
  a capable older process to `MoveTo {local:true,now:false}`. It excludes terminal
  sessions. Thus native sessions may migrate after their turns through the existing
  capability-gated owner handover; no immediate/live migration is claimed here.
- `upgrade.rs::retire_instance` is Linux/systemd-only in f555. This proposal writes
  **no `replaces.json`** and provides no old-service retirement action on the Mac.

## Concrete new identity and future workflow

- New instance: `15429e44-08b4-43b0-a9b8-71652114543e`.
- New unit: `kiln-agent-15429e44`.
- New label: `io.github.hyper-light.kiln-agent-15429e44`.
- New plist: `/Users/adalundhe/Library/LaunchAgents/io.github.hyper-light.kiln-agent-15429e44.plist`.
- New state/log: `/Users/adalundhe/.local/share/kiln/agent/instances/15429e44-08b4-43b0-a9b8-71652114543e/`.

Root first reviews the script/hash and authorizes its execution. Transfer this script
through the normal known Reach bundle, with no start flag. On b24, run its default
read-only check. It pins the canonical hash, strict signature, expected old plist
shape, old loaded domain, existing enrollment and free-space floor. Any changed
input or existing new identity refuses. It does not print raw subprocess output,
private arguments/environment or PKI material.

Only after root's explicit start decision:

```sh
python3 /absolute/reviewed/path/b24-alongside-review.py --start
```

This creates only the fresh 0700 instance directory, ordinary `instance.json`,
private rollout ownership marker and 0600 new plist, then bootstraps that new label
in the original service's user/GUI domain. It uses the existing canonical executable
for the new supervisor and child. It never calls install/join, changes the old plist,
replaces a binary, stops a process or loads/unloads the old label. Bootstrap happens
only after the new metadata files and exact plist hash have been written and fsynced.
Crash durability of the newly created directory entries is not validated. Partial preparation
failure leaves files for diagnosis; retry refuses to overwrite them.

After that one action, root verifies through normal primary API/updates events:
the exact new instance connected on the same machine, target f555 build reported,
newest ownership correct, and old agents/terminals still retained or properly handed
over. `connected` file absence or an empty queue is never proof of no ownership.
Watch for native macOS local-network permission or built-in certificate refusal;
no permission/protocol workaround is included. New-connection/certificate/handover
success remain **unverified until this future execution**.

## Rollback of only the owned new label

If validation fails, root first confirms via normal authoritative ownership metadata
that the **new** instance has no jobs, native agent sessions or terminals. An unknown
count is not zero. If it has already taken work, leave it running and use the existing
owner/handover workflow; do not unload it merely because a check timed out.

After that explicit no-owned-work confirmation, the sole rollback command is:

```sh
python3 /absolute/reviewed/path/b24-alongside-review.py \
  --rollback --server-confirmed-no-owned-work
```

It verifies its fixed instance/label ownership marker and exact new plist hash, then
disables only that new label to prevent automatic loading at the next login and runs
`launchctl bootout <saved-domain>/io.github.hyper-light.kiln-agent-15429e44`.
It preserves the instance state/certificate/logs and plist. It has no old-label,
old-PID, PKI deletion, remount, sudo or generic kill operation. This rollback flag is
an operator assertion required after actual server verification, not a substitute
for authentication or an automatically inferred proof of emptiness.

Actual checks: local AST parse, pure argument-transform fixture, named diff check.
No rollout mode, new service, certificate request or live handover was executed.
No deployment/no-loss/idle-migration success is claimed.
