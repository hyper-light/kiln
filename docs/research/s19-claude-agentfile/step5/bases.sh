#!/usr/bin/env bash
# Step 5's supplement on the Linux runner (plan 01a1cf48e8157494ac9a51f7a564f526): the other bases' images as smoke.sh
# left them in WORK's SHARDS_HOME, read again with no pull.
# - Which loader each image holds, read from `shards save` by scan.py paths. smoke.sh's `ls` in the Alpine image
#   with gnu couldn't run: no run of that image starts, since its harness can't launch.
# - The plain Alpine base's own /lib64 and /lib, by `ls`.
# - The harnesses of the two images that work, as their harness.json starts them (`shards read processes`).
#
#   bases.sh SHARDS WORK
set -uo pipefail
shards=${1:?shards}
work=${2:?the work dir of a smoke run}
dir=$(cd "$(dirname "$0")" && pwd)
scan() { python3 -I "$dir/scan.py" "$@"; }
log() { printf '== %s\n' "$*"; }
export SHARDS_HOME="$work/home" DOCKER_CONFIG="$work/docker-config" SHARDS_LOCAL_STORE=none TMPDIR="$work/tmp"
unset DOCKER_AUTH_CONFIG DOCKER_HOST SHARDS_DEFAULT_PLATFORM DOCKER_DEFAULT_PLATFORM
cleanup() {
    "$shards" rm -f claude-b-alpine-musl claude-b-slim-gnu >/dev/null 2>&1
    "$shards" stop daemon >/dev/null 2>&1
}
trap cleanup EXIT
# smoke.sh's reference, a Docker Official Image index by digest.
alpine=alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf
log "shards: $("$shards" --version 2>&1 | head -1); SHARDS_HOME $SHARDS_HOME"
for img in alpine-gnu alpine-musl slim-gnu; do
    log "harness-claude:$img: its filesystem, from shards save"
    if "$shards" save -o "$work/$img.tar" "harness-claude:$img"; then
        scan paths "$work/$img.tar" /harness/claude/bin/claude /lib64/ld-linux-x86-64.so.2 /lib/ld-musl-x86_64.so.1
    else
        echo "save failed"
    fi
done
log "the Alpine base itself, $alpine"
"$shards" run vm --rm --platform linux/amd64 "$alpine" ls -ld /lib64 /lib64/ld-linux-x86-64.so.2 /lib/ld-musl-x86_64.so.1
echo "exit $?"
for pair in alpine-musl:ctx-musl/claude-amd64-musl slim-gnu:ctx/claude-amd64-gnu; do
    img=${pair%%:*} hj="$work/${pair#*:}/harness.json"
    log "harness-claude:$img: its harness as its harness.json starts it"
    "$shards" run vm -d --name "claude-b-$img" --platform linux/amd64 "harness-claude:$img" sleep 60 >/dev/null
    echo "run exit $?"
    # The record is durable: read it again until it holds the harness's exec of Claude Code, 90 s at most.
    deadline=$(($(date +%s) + 90))
    while "$shards" read processes "claude-b-$img" >"$work/processes-b-$img.log" 2>&1; ! grep -q -E 'harness claude +[0-9]+ +exec +/harness/claude/bin/claude' "$work/processes-b-$img.log" && [ "$(date +%s)" -lt "$deadline" ]; do
        sleep 2
    done
    sleep 5
    "$shards" read processes "claude-b-$img" >"$work/processes-b-$img.log" 2>&1
    grep 'harness claude' "$work/processes-b-$img.log" | grep -E ' (exec|exit) ' | cut -c1-220
    scan exec "$work/processes-b-$img.log" "$hj"
    "$shards" rm -f "claude-b-$img" >/dev/null 2>&1
done
log done
