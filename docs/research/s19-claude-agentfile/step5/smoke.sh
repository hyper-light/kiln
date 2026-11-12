#!/usr/bin/env bash
# Step 5 (plan 01a1cf48e8157494ac9a51f7a564f526; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 19): Claude Code's
# built-in Agentfile, its context written by kiln's own fetcher (`kiln agentfiles context claude`), built by a real
# shards for both platforms and run in microVMs. It follows step 23's smoke.sh for Goose (kiln main,
# docs/research/s23-goose-agentfile/step4/smoke.sh) and Shards' harness bring-up: a fresh SHARDS_HOME, an empty
# DOCKER_CONFIG and SHARDS_LOCAL_STORE=none.
#
#   smoke.sh SHARDS KILN KSRC WORK [all|arch]
#
# KSRC is the kiln checkout KILN was built from: its pin.json and harness files are the ones KILN embeds.
#
# Both modes run these on this host:
# - the context for linux/amd64 and linux/arm64 (the Agentfile's own base, gnu: no RUN), held to the pin;
# - one image for both platforms, and its OCI index as `shards save` exports it;
# - the version smoke on this host's architecture: Claude Code's own `--version` as the run's command, its stdout
#   held byte for byte to pin.json's smoke line;
# - the real image run with its own harness.json and HOME a host folder, `shards read processes` held to it.
#
# `all` (Linux with KVM) adds:
# - the probe: a test-only harness.json, `sh -c` probe.sh with the real run.env, given the project and HOMEDIR
#   with -v. It writes what it sees into the project as probe.txt, as no shards command reads a harness's output;
# - the control: the probe without the Agentfile's three network lines;
# - other bases, each by digest: Alpine with CLAUDE_LIBC=musl and debian:trixie-slim with gnu, then Alpine with
#   gnu, which can't start the glibc build;
# - the credentials scan of the exported image (scan.py secrets), and of a copy of the Agentfile given
#   ENV ANTHROPIC_API_KEY.
#
# Needs: a shards built from Shards main at or after 997f4e47e (build-shards.sh), a kiln with the fetcher
# (build-kiln.sh), python3, curl and od.
set -euo pipefail
abs() { (cd "$(dirname "$1")" && printf '%s/%s\n' "$(pwd)" "$(basename "$1")"); }
shards=$(abs "${1:?shards}")
kiln=$(abs "${2:?kiln}")
ksrc=$(cd "${3:?kiln checkout}" && pwd)
work=${4:?work dir, which must not exist}
mode=${5:-all}
dir=$(cd "$(dirname "$0")" && pwd)
pin="$ksrc/.kiln/images/harness-claude/pin.json"
case "$(uname -m)" in x86_64) here=amd64 ;; aarch64 | arm64) here=arm64 ;; *) echo "unknown arch $(uname -m)"; exit 2 ;; esac
mkdir "$work"
work=$(cd "$work" && pwd)
scan() { python3 -I "$dir/scan.py" "$@"; }
log() { printf '== %s\n' "$*"; }
now_ms() { python3 -I -c 'import time; print(int(time.time() * 1000))'; }
if command -v sha256sum >/dev/null; then sum() { sha256sum "$@"; }; else sum() { shasum -a 256 "$@"; }; fi
step() { # NAME COMMAND...: its output in $work/NAME.log; prints its exit and time
    local name=$1 s rc=0
    shift
    s=$(now_ms)
    "$@" >"$work/$name.log" 2>&1 || rc=$?
    printf '%s: exit %s in %s ms\n' "$name" "$rc" $(($(now_ms) - s))
    return $rc
}

# NAME OUT: the bytes in OUT held to pin.json's smoke line and its newline; the verdict also in $work/NAME.exact.
exact() {
    printf '%s STDOUT_HEX %s\n' "$1" "$(od -An -tx1 "$2" | tr -s ' \n' ' ')"
    python3 -I -c 'import json, sys; want = json.load(open(sys.argv[1]))["smoke"]["stdout"] + "\n"; print(open(sys.argv[2], "rb").read() == want.encode())' "$pin" "$2" >"$work/$1.exact"
    printf '%s EXACT %s\n' "$1" "$(cat "$work/$1.exact")"
}

# NAME IMAGE: Claude Code's own --version as the run's command.
smoke() {
    local name=$1 image=$2 s rc=0
    s=$(now_ms)
    "$shards" run vm --rm --platform "linux/$here" "$image" /harness/claude/bin/claude --version >"$work/$name.out" 2>"$work/$name.err" || rc=$?
    printf '%s: exit %s in %s ms\n' "$name" "$rc" $(($(now_ms) - s))
    printf '%s STDOUT %s\n' "$name" "$(cat "$work/$name.out")"
    exact "$name" "$work/$name.out"
    printf -- '--- %s stderr\n' "$name"
    grep -v -E '^\s*$' "$work/$name.err" | tail -8 || true
    return $rc
}

# SRC DST AGENTFILE HARNESS_JSON: a context like SRC, its binaries hard links to SRC's, its Agentfile and each
# platform's harness.json the ones given.
derive() {
    python3 -I - "$@" <<'PY'
import os, shutil, sys
src, dst, agentfile, hj = sys.argv[1:]
for root, _, files in os.walk(src):
    rel = os.path.relpath(root, src)
    os.makedirs(os.path.join(dst, rel), exist_ok=True)
    for n in files:
        s, d = os.path.join(root, n), os.path.join(dst, rel, n)
        if rel == "." and n == "Agentfile":
            shutil.copyfile(agentfile, d)
        elif n == "harness.json":
            shutil.copyfile(hj, d)
        else:
            os.link(s, d)
PY
}

# SRC OUT COMMAND_TEXT: SRC's harness.json, its command `sh -c COMMAND_TEXT`, its env as it is.
harness_sh() {
    python3 -I - "$@" <<'PY'
import json, sys
src, out, text = sys.argv[1:]
h = json.load(open(src))
h["run"]["command"] = ["sh", "-c", text]
json.dump(h, open(out, "w"), indent=2)
PY
}

# VM OUT: `shards read processes VM` into OUT until it holds the harness's exec of Claude Code, then once more
# after 5 s for its end; 90 s at most. The record is durable, so reading it again misses nothing.
await_exec() {
    local deadline=$(($(date +%s) + 90))
    while :; do
        "$shards" read processes "$1" >"$2" 2>&1 || true
        if grep -q -E 'harness claude +[0-9]+ +exec +/harness/claude/bin/claude' "$2"; then
            sleep 5
            "$shards" read processes "$1" >"$2" 2>&1 || true
            return 0
        fi
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 2
    done
}

export SHARDS_HOME="$work/home" DOCKER_CONFIG="$work/docker-config" SHARDS_LOCAL_STORE=none TMPDIR="$work/tmp"
unset DOCKER_AUTH_CONFIG DOCKER_HOST SHARDS_DEFAULT_PLATFORM DOCKER_DEFAULT_PLATFORM
mkdir -p "$SHARDS_HOME" "$DOCKER_CONFIG" "$TMPDIR"
printf '{}\n' >"$DOCKER_CONFIG/config.json"
names="claude-real claude-vprobe claude-probe claude-control claude-alpine-gnu"
cleanup() {
    for n in $names; do "$shards" rm -f "$n" >/dev/null 2>&1 || true; done
    "$shards" stop daemon >/dev/null 2>&1 || true
}
trap cleanup EXIT

log "shards: $("$shards" --version 2>&1 | head -1), sha256 $(sum "$shards" | cut -d' ' -f1)"
log "kiln: $("$kiln" --version 2>&1 | head -1), built from $(git -C "$ksrc" rev-parse HEAD), sha256 $(sum "$kiln" | cut -d' ' -f1)"
log "host: $(hostname) $(uname -srm); mode $mode; pin.json sha256 $(sum "$pin" | cut -d' ' -f1)"

# --variant gnu: the fetcher takes no default variant, and Claude Code's pin has two per platform (gnu, musl).
log "context: kiln agentfiles context claude, linux/amd64 and linux/arm64, the Agentfile's own base, gnu"
step context "$kiln" agentfiles context claude "$work/ctx" --platform linux/amd64,linux/arm64 --variant gnu || { cat "$work/context.log"; exit 1; }
printf 'recipe %s\n' "$(grep -E '^[0-9a-f]{64}$' "$work/context.log" | tail -1)"
scan context "$work/ctx" "$ksrc" gnu linux/amd64 linux/arm64

log "build: one image for linux/amd64 and linux/arm64"
step build-real "$shards" build image --progress=plain --platform linux/amd64,linux/arm64 -t harness-claude:smoke "$work/ctx" || { tail -40 "$work/build-real.log"; exit 1; }
printf 'build-real warns SecretsUsedInArgOrEnv: %s\n' "$(grep -c SecretsUsedInArgOrEnv "$work/build-real.log" || true)"
grep -i -E 'warn' "$work/build-real.log" | head -10 || true
step images-tree "$shards" images --tree || true
cat "$work/images-tree.log"
for p in amd64 arm64; do
    printf 'inspect linux/%s: ' "$p"
    "$shards" image inspect --platform "linux/$p" --format '{{.Os}}/{{.Architecture}} {{.Id}} size {{.Size}}' harness-claude:smoke 2>&1 || true
done

# Docker's `save -o` (served as Docker's): shards' own `save image` adds --convert, which first converts each image
# named that isn't a microVM here yet (daemon/commands.rs), a step this export of the image as built doesn't need.
log "the exported image (shards save): its OCI index"
step save-real "$shards" save -o "$work/real.tar" harness-claude:smoke || tail -20 "$work/save-real.log"
scan index "$work/real.tar" || true

log "the smoke: Claude Code's own --version on linux/$here, the run's command, its stdout held to pin.json's line"
smoke version harness-claude:smoke || true
smoke_source=run-command
if [ "$(cat "$work/version.exact")" != True ]; then
    # D115: a run's command reaches none of its image's harnesses. Should it not exec the harness's binary, the
    # harness itself runs `claude --version` into the project, and that file is the smoke.
    smoke_source=harness-probe
    log "the run's command did not print the pin's line: the harness's own version file is the smoke"
    harness_sh "$work/ctx/claude-$here-gnu/harness.json" "$work/vprobe.json" 'claude --version >/workspace/version.out 2>/workspace/version.err; echo $? >/workspace/version.exit'
    derive "$work/ctx" "$work/ctx-vprobe" "$work/ctx/Agentfile" "$work/vprobe.json"
    mkdir -p "$work/vprobe/project" "$work/vprobe/home"
    if step build-vprobe "$shards" build image --progress=plain --platform "linux/$here" -t harness-claude:vprobe "$work/ctx-vprobe" &&
        step run-vprobe "$shards" run vm -d --name claude-vprobe --platform "linux/$here" -v "$work/vprobe/project:/workspace" -v "$work/vprobe/home:/home/claude" harness-claude:vprobe sleep 120; then
        deadline=$(($(date +%s) + 120))
        while [ ! -e "$work/vprobe/project/version.exit" ] && [ "$(date +%s)" -lt "$deadline" ]; do sleep 2; done
        "$shards" rm -f claude-vprobe >/dev/null 2>&1 || true
        printf 'vprobe exit %s\n' "$(cat "$work/vprobe/project/version.exit" 2>/dev/null || echo missing)"
        exact vprobe "$work/vprobe/project/version.out" || true
    fi
fi
printf 'SMOKE_SOURCE %s\n' "$smoke_source"

log "the real image: its harness started as harness.json declares, HOME a host folder"
mkdir -p "$work/real/project" "$work/real/home"
step run-real "$shards" run vm -d --name claude-real --platform "linux/$here" \
    -v "$work/real/project:/workspace" -v "$work/real/home:/home/claude" harness-claude:smoke sleep 120 || tail -20 "$work/run-real.log"
await_exec claude-real "$work/processes-real.log" || echo "no exec of the harness within 90 s"
cat "$work/processes-real.log"
scan exec "$work/processes-real.log" "$work/ctx/claude-$here-gnu/harness.json" || true
printf -- '--- what Claude Code wrote into its HOME, the host folder bound at /home/claude\n'
scan tree "$work/real/home"
"$shards" rm -f claude-real >/dev/null 2>&1 || true

if [ "$mode" = all ]; then
    # NAME CTX [BUILD ARGS...]: builds CTX for this host's platform, runs it with the project and HOMEDIR given,
    # waits for the probe's `done`, then reads what it wrote.
    probe_run() {
        local name=$1 ctx=$2
        shift 2
        mkdir -p "$work/$name/project" "$work/$name/home"
        step "build-$name" "$shards" build image --progress=plain --platform "linux/$here" -t "harness-claude:$name" "$@" "$ctx" || { tail -20 "$work/build-$name.log"; return 1; }
        step "run-$name" "$shards" run vm -d --name "claude-$name" --platform "linux/$here" \
            -v "$work/$name/project:/workspace" -v "$work/$name/home:/home/claude" "harness-claude:$name" sleep 300 || { tail -20 "$work/run-$name.log"; return 1; }
        local deadline=$(($(date +%s) + 240))
        while [ ! -e "$work/$name/project/done" ] && [ "$(date +%s)" -lt "$deadline" ]; do sleep 2; done
        step "processes-$name" "$shards" read processes "claude-$name" || true
        "$shards" rm -f "claude-$name" >/dev/null 2>&1 || true
        grep -E ' (exec|exit) ' "$work/processes-$name.log" | grep 'harness claude' | cut -c1-200 || true
        log "$name: probe.txt, in PROJECT"
        cat "$work/$name/project/probe.txt" 2>/dev/null || echo "(missing)"
        log "$name: HOMEDIR, on the host"
        scan homedir "$work/$name/home" || true
    }

    log "this host's own reach, outside any microVM, for the probe's port 80 to be told against"
    curl -sS -o /dev/null -w 'host: https://api.anthropic.com/ http_code=%{http_code} remote=%{remote_ip}:%{remote_port}\n' --max-time 20 https://api.anthropic.com/ || echo "host: https failed ($?)"
    curl -sS -o /dev/null -w 'host: http://api.anthropic.com/ http_code=%{http_code} remote=%{remote_ip}:%{remote_port}\n' --max-time 20 http://api.anthropic.com/ || echo "host: http failed ($?)"

    log "probe: the real Agentfile, a test-only harness.json (sh -c probe.sh, the real run.env)"
    harness_sh "$work/ctx/claude-$here-gnu/harness.json" "$work/probe.json" "$(cat "$dir/probe.sh")"
    python3 -I -c 'import json, sys; a, b = (json.load(open(p)) for p in sys.argv[1:]); print("probe run.env is the real one:", a["run"]["env"] == b["run"]["env"], a["run"]["env"])' "$work/ctx/claude-$here-gnu/harness.json" "$work/probe.json"
    derive "$work/ctx" "$work/ctx-probe" "$work/ctx/Agentfile" "$work/probe.json"
    probe_run probe "$work/ctx-probe" || true

    log "control: the probe without the Agentfile's three network lines"
    grep -v -E '^(NETWORK|EXPOSE|CONNECT) ' "$work/ctx/Agentfile" >"$work/Agentfile.control" || true
    printf 'lines taken out: %s\n' "$(($(wc -l <"$work/ctx/Agentfile") - $(wc -l <"$work/Agentfile.control")))"
    diff "$work/ctx/Agentfile" "$work/Agentfile.control" || true
    derive "$work/ctx" "$work/ctx-control" "$work/Agentfile.control" "$work/probe.json"
    probe_run control "$work/ctx-control" || true

    # Step 23's references, each a Docker Official Image index by digest (kiln main,
    # docs/research/s23-goose-agentfile/step4/evidence/e_smoke_linux.txt).
    alpine=alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf
    slim=debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2

    log "other bases: Alpine with CLAUDE_LIBC=musl, $alpine"
    if step context-musl "$kiln" agentfiles context claude "$work/ctx-musl" --platform "linux/$here" --variant musl --base "$alpine"; then
        printf 'recipe %s\n' "$(grep -E '^[0-9a-f]{64}$' "$work/context-musl.log" | tail -1)"
        scan context "$work/ctx-musl" "$ksrc" musl "linux/$here" || true
        step build-alpine-musl "$shards" build image --progress=plain --platform "linux/$here" -t harness-claude:alpine-musl \
            --build-arg "BASE=$alpine" --build-arg CLAUDE_LIBC=musl "$work/ctx-musl" || tail -30 "$work/build-alpine-musl.log"
        grep -E 'apk add|OK: [0-9]' "$work/build-alpine-musl.log" | head -6 || true
        smoke alpine-musl harness-claude:alpine-musl || true
    else
        cat "$work/context-musl.log"
    fi

    log "other bases: debian:trixie-slim with gnu, $slim"
    step build-slim-gnu "$shards" build image --progress=plain --platform "linux/$here" -t harness-claude:slim-gnu --build-arg "BASE=$slim" "$work/ctx" || tail -30 "$work/build-slim-gnu.log"
    smoke slim-gnu harness-claude:slim-gnu || true

    log "other bases: Alpine with gnu, which must fail to start: the glibc build's loader isn't there"
    step build-alpine-gnu "$shards" build image --progress=plain --platform "linux/$here" -t harness-claude:alpine-gnu --build-arg "BASE=$alpine" "$work/ctx" || tail -30 "$work/build-alpine-gnu.log"
    smoke alpine-gnu harness-claude:alpine-gnu || true
    interp=$(scan interp "$work/ctx/claude-$here-gnu/bin/claude" || true)
    printf "the glibc build's loader (its PT_INTERP): %s\n" "$interp"
    step alpine-gnu-loader "$shards" run vm --rm --platform "linux/$here" harness-claude:alpine-gnu ls -l "$interp" || true
    cat "$work/alpine-gnu-loader.log"
    if [ -f "$work/ctx-musl/claude-$here-musl/bin/claude" ]; then
        interp_musl=$(scan interp "$work/ctx-musl/claude-$here-musl/bin/claude" || true)
        printf "the musl build's loader (its PT_INTERP): %s\n" "$interp_musl"
        step alpine-musl-loader "$shards" run vm --rm --platform "linux/$here" harness-claude:alpine-musl ls -l "$interp_musl" || true
        cat "$work/alpine-musl-loader.log"
    fi
    log "Alpine with gnu: the harness as harness.json starts it"
    step run-alpine-gnu "$shards" run vm -d --name claude-alpine-gnu --platform "linux/$here" harness-claude:alpine-gnu sleep 60 || tail -20 "$work/run-alpine-gnu.log"
    sleep 15
    step processes-alpine-gnu "$shards" read processes claude-alpine-gnu || true
    cat "$work/processes-alpine-gnu.log"
    "$shards" rm -f claude-alpine-gnu >/dev/null 2>&1 || true

    log "credentials: every file of every layer of the exported image, and each config, held to step 2's patterns"
    real_scan=clean
    scan secrets "$work/real.tar" "$pin" || real_scan=FOUND
    printf 'SCAN_RESULT harness-claude:smoke %s\n' "$real_scan"
    log "the mutation: a copy of the Agentfile given ENV ANTHROPIC_API_KEY"
    { cat "$work/ctx/Agentfile"; printf 'ENV ANTHROPIC_API_KEY=placeholder-not-a-key\n'; } >"$work/Agentfile.secret"
    derive "$work/ctx" "$work/ctx-secret" "$work/Agentfile.secret" "$work/ctx/claude-$here-gnu/harness.json"
    step build-secret "$shards" build image --progress=plain --platform "linux/$here" -t harness-claude:secret "$work/ctx-secret" || tail -20 "$work/build-secret.log"
    printf 'build-secret warns SecretsUsedInArgOrEnv: %s\n' "$(grep -c SecretsUsedInArgOrEnv "$work/build-secret.log" || true)"
    grep -m2 SecretsUsedInArgOrEnv "$work/build-secret.log" || true
    step save-secret "$shards" save -o "$work/secret.tar" harness-claude:secret || tail -20 "$work/save-secret.log"
    secret_scan=clean
    scan secrets "$work/secret.tar" "$pin" || secret_scan=FOUND
    printf 'SCAN_RESULT harness-claude:secret %s\n' "$secret_scan"
fi

log "sizes and times (single observations)"
du -sh "$work/ctx" "$work/ctx/claude-"* 2>/dev/null || true
"$shards" images --format '{{.Repository}}:{{.Tag}} {{.ID}} {{.Size}}' 2>&1 | grep harness-claude || true
ls -l "$work"/*.tar 2>/dev/null || true
log "done $work"
