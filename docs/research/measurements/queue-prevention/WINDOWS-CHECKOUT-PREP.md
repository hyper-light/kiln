# Preserving the completed native Windows checkout

POLICY_READ implementer: Shards origin/main06ae067c0beda71c98a8c6f9633df9feb71fc11b;
rules SHA256df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Scope: this helper/request/evidence only. No Rust/UI/build/service actions or children.

Root's actual native parser run14871 passed for workflow46610bbd/step340dbc0c,
bound four arguments and closed its owned terminal (after[]). The read-only preflight
reported228 status lines. Root's follow-up13136 with core.filemode=false found exactly
two deleted tracked Helm symlinks and two modified state-view evidence documents.
This is source preparation evidence, not a completed checkout/build or established
Windows Reach startup cause.

Git documents core.fileMode=false when crossing environments that lose executable
bits, and core.symlinks=false as checking out link text in plain files while retaining
the recorded link type. The helper changes only these two local settings in the exact
owned clone; it records effective/local values of those keys and core.autocrlf first,
and leaves autocrlf unchanged. [Git config2.56.0](https://git-scm.com/docs/git-config/2.56.0)

The helper requires exact f555 HEAD, unstaged status codes and the exact four known
paths. It rejects other tracked/untracked/staged changes and any now-existing link
path. In a new GUID-owned cache directory it saves both documents as exact raw bytes,
hashes them, captures a raw native Git binary diff for all four paths, and records the
missing links and prior stash. Only after preservation/recheck does it change the two
platform settings and run named path-scoped `git stash push`. Git specifies that this
form records and rolls back only matching paths, leaving other paths intact.
[Git stash](https://git-scm.com/docs/git-stash#Documentation/git-stash.txt-pathspec)

The script verifies the new stash's name, parent and exact changed path set, and that
its binary diff matches the preserved diff. It then requires unchanged f555 HEAD,
empty complete status, unchanged backup hashes and unchanged autocrlf. The raw backups
retain exact document bytes independently of any Git clean-filter/line-ending behavior.
Failure retains backups and any stash; there is no reset, clean, forced checkout,
stash drop, crypto/signing change, build, install or service action. This preparation
does not check out the final signed commit; the separately reviewed normal CI job does.

Root executes the prepared parser request first, then may execute the separate apply
request through the existing owned terminal. Both payloads decompress, check exact
length/SHA and parse before any invocation. Parser mode never executes the helper.
Apply mode runs the helper with its explicit `-Apply` switch. The script itself defaults
to a read-only preflight without that switch. No request has been executed by this agent.

Local checks: generator syntax and gzip/base64/hash round trips plus scoped diff check.
Native PowerShell parsing and apply behavior of this new helper remain unrun until
Root executes the requests. No completed preservation/stash/clean-checkout claim.

## Actual native warning failure and recovery inspection

Root's parser for helper558d4f87 passed. Its actual apply run68093 stopped in
`named-stash`: PowerShell raised RemoteException on Git's normal LF-to-CRLF warning.
The two local platform settings were already corrected; backups and binary diff
remain in `checkout-preservation-7cd1f5ee3aa84f89b2494a21d1de7697`. The recorded stash
was null, which does not establish that Git failed to create one before PowerShell
interrupted capture. No unchanged/clean/stash-success claim follows from that failure.

The corrected Git-Lines uses ProcessStartInfo with concurrent stdout/stderr drains
and the process's actual ExitCode. Stderr alone cannot turn a successful Git command
into a refusal. Nonzero exits remain errors (apart from explicitly expected missing
config/ref exit1); ErrorActionPreference stays Stop and no signing setting changes.

The separate `windows-recovery-checkout-inspect.ps1` is read-only. It reports exact
HEAD/status/count+first20, the three named effective/local Git settings, current
stash identity/subject/parent/exact path set, and hashes of the known before receipt,
two backups and binary diff. It reads only that exact existing preservation directory;
no document contents, environment, credentials or keys enter output. It does not
apply/drop a stash, change config, reset, checkout or build. Root inspects these facts
before deciding whether any preparation retry is needed. New parser/inspection and
corrected apply payloads are prepared but not executed by this agent.

Root's independent read-only run45345 then positively established exact f555 HEAD,
status_count0, filemode=false, symlinks=[false,false] (global and local), autocrlf=true.
Both document backups (784B/22960B, SHA prefixes f3438e/ffccb) matched before.json;
the saved patch SHA was98030f40fdc7f3e9247dddbc2d67c3f87327fc7fd18f5bdc470b8a3cb07e341e.
Named stash00b2f5ee6cf95957e38c1fd1aaab33010e7903cb had exact f555 parent,
the expected name and exactly two deletions plus two document modifications.
Link paths now exist as plain Archive files. Root closed the owned terminal, after[].
This proves the original stash completed before the PowerShell warning exception;
68093 remains an actual exit1 red. No v3 apply/retry is needed or authorized here.
Its final flag-count check would reject two equal false effective entries, so it is
an unused candidate, not successful cleanup evidence.

The final `windows-recovery-stash-proof.ps1` request only reads. It hashes the exact
four-path Git binary diff from f555 to that pinned stash using raw stdout into a
MemoryStream bounded by the pinned saved patch length plus one detection byte.
It requires the saved patch SHA above, unchanged HEAD/clean status and both original
backup hashes, then prints only small digests/identities. Root owns execution;
this final comparison remains unrun until its actual receipt is recorded.
