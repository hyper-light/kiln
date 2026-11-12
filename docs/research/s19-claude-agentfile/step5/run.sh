#!/usr/bin/env bash
# Step 5's runs on a runner (plan 01a1cf48e8157494ac9a51f7a564f526), from the carrier folder kiln syncs there (a
# `git init` folder holding this directory's scripts; kiln's sync of the whole worktree times out):
#
#   run.sh builds          build-shards.sh, then build-kiln.sh
#   run.sh smoke MODE      smoke.sh with what they built: all (Linux with KVM) or arch
#   run.sh check           scan.py's own checks on small inputs made there (selftest.py)
#   run.sh bases RUN       bases.sh on smoke run RUN's home (run-<time>), Linux
#
# It copies the scripts into ~/.cache/s19-step5/inputs-<time>, printing their SHA-256, and runs them from there,
# each one's output and exit in ~/.cache/s19-step5/<name>-<time>.{log,exit}. On Linux the work is a transient
# systemd user unit of its own, so that a runner's restart doesn't end it, and it isn't started while
# amphora-bench runs there. On macOS it runs within this run, which kiln holds as a background run.
set -euo pipefail
what=${1:?builds, smoke MODE, check or bases RUN}
R="$HOME/.cache/s19-step5"
mkdir -p "$R"
t=$(date -u +%Y%m%dT%H%M%SZ)
I="$R/inputs-$t"
mkdir "$I"
cp build-shards.sh build-kiln.sh smoke.sh scan.py probe.sh selftest.py bases.sh "$I/"
if command -v sha256sum >/dev/null; then (cd "$I" && sha256sum ./*); else (cd "$I" && shasum -a 256 ./*); fi
case $what in
builds) job="bash $I/build-shards.sh >$R/build-shards-$t.log 2>&1; echo \$? >$R/build-shards-$t.exit; bash $I/build-kiln.sh >$R/build-kiln-$t.log 2>&1; echo \$? >$R/build-kiln-$t.exit" ;;
smoke) job="bash $I/smoke.sh $R/shards-target/test-release/shards $R/kiln-target/debug/kiln $R/kiln $R/run-$t ${2:?all or arch} >$R/smoke-$t.log 2>&1; echo \$? >$R/smoke-$t.exit" ;;
check) job="python3 -I $I/selftest.py $I/scan.py $R/check-$t >$R/check-$t.log 2>&1; echo \$? >$R/check-$t.exit" ;;
bases) job="bash $I/bases.sh $R/shards-target/test-release/shards $R/${2:?a smoke run, run-<time>} >$R/bases-$t.log 2>&1; echo \$? >$R/bases-$t.exit" ;;
*) echo "builds, smoke MODE, check or bases RUN"; exit 2 ;;
esac
echo "RUN $what $t on $(hostname) $(uname -srm)"
if [ "$(uname -s)" = Linux ]; then
    if pgrep -x amphora-bench >/dev/null; then echo "amphora-bench runs here: not started"; exit 75; fi
    systemd-run --user --collect --unit="s19-step5-$what-$t" \
        --setenv=PATH="$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin" bash -c "$job"
    echo "UNIT s19-step5-$what-$t"
else
    bash -c "$job" || true
    for f in "$R"/*-"$t".exit; do echo "$(basename "$f") $(cat "$f")"; done
    for f in "$R"/*-"$t".log; do echo "--- tail of $(basename "$f")"; tail -6 "$f"; done
fi
