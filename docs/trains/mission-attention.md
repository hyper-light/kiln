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

## UI copy follow-up

At the person's request, routine "Your latest interaction" text is omitted from rows; park,
wake and correction reasons remain. The order options now read Suggested, Last Messaged,
Newest, Oldest, Last active, Name A–Z, Name Z–A, Runner, Harness and Highest cost.
Saved sort values and ordering behavior are unchanged.

Release `e0aed79b0a81a6559ebdb25b1aa37aebf0164722` contains only these UI and documentation
changes on the deployed `a9d8fa71e` base. It was merged and pushed as `528c831da`; later
load-balancer work remains outside this release. The exact release passed UI typecheck,
lint, all 550 tests and build, plus eight affected browser cases in Chromium and WebKit
at desktop and phone sizes.

Production served the ready release at 02:46Z on 2026-11-09. Its executable's SHA-256 is
`058a8ef7af0bf3a419dcff9224f5287738ffc3559faf4fb72047434315bbb379`.
The package checksum and dynamic libraries were checked on production before deployment.
The private database snapshot passed readiness, attention and replay with the new release,
then readiness with the saved previous executable. Runner executables and services were untouched.

The menu asset `/assets/stands-St-WjCQG.js` was verified from the production URL against
the release build, SHA-256
`4adf968c2406c5c272cb2ae7c0666fba271a26ed367cdfdb9a090ce580a91355`.
The rollback executable, snapshot results, asset manifest and independent health-guard
report are kept privately under `~/.cache/kiln-attention-copy-20261109/` on production.
The ten-minute guard completed successfully at 02:56:11Z: all 60 serving checks passed,
the restart counter stayed at 3, there were no OOM events, and nine runners were connected.
All 139 sessions listed before the update remained listed after it.

## Chat-state consistency follow-up

Queued input and admission waits now stay Working in Mission Control and the header. A person's
process pause says Paused; the row's Resume releases that pause. Goals held for the person now
show attention in the header too. The list and header share a pure classifier, and the agents API
agrees for these cases. Ready and Finished describe activity, not a verdict that an objective was met.

Release `6bed36c7380db25cb8dd84064234e57822075ec6` contains these fixes on the deployed `e0aed79b0` base;
unrelated load-balancer changes remain outside the release. Its landing merge is `cc6c642d3`.
The exact release passed UI typecheck, lint, all 556 tests and build, 12 Chromium/WebKit desktop
and phone cases, kiln's all-targets clippy with warnings denied, and all nine `harness::wait`
tests. The main-based tree also passed its 565 UI tests. The full workspace gate was not rerun
for this follow-up.

Production verified the ready release at `2026-11-09T04:57:49Z`. The package checksum and
production libraries passed before deployment. Both the new release and saved previous executable
passed the private database snapshot test. The authenticated Agents page returned 200, and the
served state-classification asset `/assets/mission-rows-nkgf6N_B.js` matched SHA-256
`9e481fe8dfc16890d1ae1ec2ca3dfa44323a895584706e93bfd6b9f7a630ea7b`. The menu asset matched the existing copy release too.

The off-production monitor completed at `2026-11-09T05:08:02Z` with all 60 actual ten-second
samples. Its restart counter stayed at 4, with no startup or subsequent OOM event.
All 140 baseline session IDs remained listed; the final list had 140 sessions.
There were 8 connected runners at the end of the watch. Runner executables and services
were untouched. The new server briefly restored additional conversation-store mirrors at startup;
runner inventory reconciliation returned the list to its exact baseline IDs.

The executable's SHA-256 is
`2ebabb2c962682e687c146d2980a24b27727ebd6d212ffe249a13780e525fd05`.
The rollback executable, private snapshot check, asset manifest, page check and deployment report
are kept under `~/.cache/kiln-attention-states-20261109/` on production. The build and scoped Rust
check logs are under the same cache path on the needs-selected Linux build runner. The monitor
and its report are also kept off production under `~/.cache/kiln-attention/` on the initiating machine.
