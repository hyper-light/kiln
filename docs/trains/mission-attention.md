# Train notes: Mission Control attention

Plan `01a199e1b177752e84dad5dc990e6a15`, research
`01a199e1e6eb77c4873daa55fafaac83`.

## What landed

- `6693acb98`: the attention feature.
- `1a15626b9`: its Settings comparison table fitted to a phone.
- `a9d8fa71e`: the maintenance wake and owned Reach waits on the runtime's lock-free primitives.

The order is learned from the person's uses and agent turns, with a Thompson draw per visit.
Only the person's own actions move rows during a visit; requests rise immediately. Pins,
parks, archive and restore are in each row's menu, with bulk actions and Undo in place.
Settings → You → Attention holds automation settings, the model comparison and replay.
Automatic archiving is on by default after seven days parked; automatic parking remains opt-in.

## Deployment

The tested release is `a9d8fa71ec76f91e9f28df9d97204aeacfd650fa`, already on main.
It includes the three attention merges above. Later load-balancer work is outside this release.

Production was still serving `fccfb0b2df9b4b9e9cc157526cafc0a1725842e4` when this
deployment was requested on 2026-11-08. The browser reaches the bare-metal server at
`192.168.1.90:7070`, served by the system unit `kiln-server.service`. Its executable is
`~/.local/share/kiln-server/bin/kiln`, separate from the runner's executable and service.
The old KilnCD Kubernetes app's synced revision is not proof of the bare-metal server's version.

The server took the release at 01:48Z on 2026-11-09, on the person's request to finish
deployment. The existing bare-metal supervisor update path was used, as for the DB query audit
train; the retired Kubernetes app was not synced. `/healthz/server` reports the exact release
commit and `/readyz` is ready. The attention snapshot and replay each return 200. The settings
and Mission Control assets served by production match the release's built files byte for byte,
including the new Attention settings controls. Both attention migrations are recorded.

All 139 sessions listed before deployment were listed after it. Runner policies remained Notify;
runner executables and services were not upgraded. The independent ten-minute health guard
completed at 01:58Z: all 60 readiness checks passed, the new server had no additional restart or
OOM event, and nine runners were connected (the same count as before deployment).

The release executable's SHA-256 is
`a5f19e78289af3f2f34c07265f2f289b937de4752c6e54facf41411bbf31f191`.
Its build, snapshot checks, rollback check and deployment report are kept privately on the
production host under `~/.cache/kiln-attention-ship-20261108/`.

## Validation and rollback

Before deployment, build the exact release with its Linux UI server, and start it against a
private SQLite backup with local agents, discovery, deployments, updates and cluster replication
disabled. Check readiness, the embedded commit, the attention snapshot and replay, and both
attention migrations. Production's database is not used by that test server.

Keep the previous server executable before the atomic replacement. The server's supervisor
takes the changed executable; runner executables and services are untouched. Readiness and the
embedded commit must confirm the release, then the attention API and served UI must confirm the
feature. The rollback executable stays beside the release. The migrations do not raise the
minimum reader epoch, so the previous server can open the database.

Runner archive and restore commands require `CAP_HARNESS_ARCHIVE` (bit 57). Runners from
before this release cannot execute them. Their upgrades remain the person's action under the
repository's runner rule.

The landing was checked with Linux clippy, kiln-ci's 969 library tests, and targeted kiln tests;
the lock-free follow-up passed clippy and all 155 targeted tests. The UI's typecheck, lint and
build passed. Mission Control's six browser suites ran in Chromium and WebKit at desktop and
phone widths; two load-sensitive failures passed when rerun. The known allocation-budget test
and two UI frame-time failures also occurred on the pre-feature main tree.
