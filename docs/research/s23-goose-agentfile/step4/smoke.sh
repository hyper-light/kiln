#!/usr/bin/env bash
# Step 4 (plan 01a1ce705e3d72af83ef15ae83b62f45): Goose's built-in Agentfile built by a real shards for
# both platforms and run in microVMs. After step 25's smoke.sh for Devin, and Shards' harness bring-up
# (Shards docs/research/measurements/harness-bringup-v1/bringup.sh): a fresh SHARDS_HOME, an empty
# DOCKER_CONFIG, SHARDS_LOCAL_STORE=none.
#
#   smoke.sh SHARDS PIN_JSON AGENTFILE HARNESS_JSON WORK [all|arch]
#
# Both modes build one image for linux/amd64 and linux/arm64, list its platforms, run the smoke on
# this host's architecture (Goose's own `--version`, its stdout held byte for byte to the pin's line),
# and run the real image with its own harness.json, reading which processes started (`shards read
# processes`) and what Goose wrote under GOOSE_PATH_ROOT, a host folder. `all` (Linux with KVM) adds:
# - the probe: a test-only harness.json whose command is `sh -c PROBE`, writing what it sees into the
#   project volume, as no shards command reads a harness's output;
# - the control: the probe without the Agentfile's three network lines;
# - the probe on Alpine and on debian:trixie-slim, each by digest;
# - a secrets scan of the image, and of a copy of the Agentfile given ENV OPENAI_API_KEY.
#
# The build context is assembled as `kiln agentfiles context goose` will assemble it (step 19's
# fetcher, not yet written, through step 25's one unpacker, not yet landed). Each pinned archive is
# downloaded once into a cache named by its digest, and its size and SHA-256 are checked before
# anything reads it. Its one regular member, goose, is streamed out to goose-<arch>/bin/goose, mode
# 0755, its SHA-256 and size checked against the pin's, with harness.json beside it.
#
# Needs: a shards built from Shards main at or after 1da9132b7 (build-shards.sh), python3, curl, od.
set -euo pipefail
shards=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
pin=${2:?pin.json}
agentfile=${3:?Agentfile}
harness=${4:?harness.json}
work=${5:?work dir (must not exist)}
mode=${6:-all}
case "$(uname -m)" in x86_64) here=amd64 ;; aarch64 | arm64) here=arm64 ;; *) echo "unknown arch $(uname -m)"; exit 2 ;; esac
mkdir "$work"
work=$(cd "$work" && pwd)
cache="$HOME/.cache/s23-step4/archives"
mkdir -p "$cache"
log() { printf '== %s\n' "$*"; }
now_ms() { python3 -I -c 'import time; print(int(time.time() * 1000))'; }
step() {
    local name=$1 s e rc=0
    shift
    s=$(now_ms)
    "$@" >"$work/$name.log" 2>&1 || rc=$?
    e=$(now_ms)
    printf '%s: exit %s in %s ms\n' "$name" "$rc" $((e - s))
    return $rc
}

# --- the build context, as the fetcher will make it ----------------------------------------------
assemble() { # ctx agentfile harness_json
    python3 -I - "$pin" "$2" "$1" "$3" "$cache" <<'PY'
import hashlib, json, os, shutil, subprocess, sys, tarfile
pin, agentfile, ctx, hj, cache = sys.argv[1:]
p = json.load(open(pin))
os.makedirs(ctx)
shutil.copy(agentfile, os.path.join(ctx, "Agentfile"))
def digest(path):
    h, n = hashlib.sha256(), 0
    with open(path, "rb") as f:
        for c in iter(lambda: f.read(1 << 20), b""):
            h.update(c); n += len(c)
    return h.hexdigest(), n
for b in p["builds"]:
    arch = b["platform"].split("/", 1)[1]
    d = os.path.join(ctx, f"goose-{arch}")
    os.makedirs(os.path.join(d, "bin"))
    for f in b["files"]:
        u = f.get("unpack") or {}
        if (u.get("format"), f.get("path"), f.get("mode")) != ("tar.gz", "bin/goose", "0755"):
            sys.exit(f"{b['platform']}: not the pin's layout: {f}")
        arc = os.path.join(cache, f["sha256"] + ".tar.gz")
        if not os.path.exists(arc):
            subprocess.run(["curl", "-fsSL", "--proto", "=https", "--retry", "3", "-o", arc + ".part", f["url"]], check=True)
            os.rename(arc + ".part", arc)
        got = digest(arc)
        if got != (f["sha256"], f["size"]):
            os.remove(arc)
            sys.exit(f"{f['url']}: sha256 {got[0]} size {got[1]}, pinned {f['sha256']} {f['size']}")
        with tarfile.open(arc, "r:gz") as t:
            entries = t.getmembers()
            regular = [e for e in entries if e.isreg()]
            other = [e.name for e in entries if not (e.isreg() or e.isdir())]
            if other or len(regular) != 1 or regular[0].name.removeprefix("./") != u["member"]:
                sys.exit(f"{f['url']}: not one regular {u['member']}: regular {[e.name for e in regular]}, other {other}")
            out = os.path.join(d, f["path"])
            src = t.extractfile(regular[0])
            h, n = hashlib.sha256(), 0
            with open(out + ".part", "wb") as w:
                for c in iter(lambda: src.read(1 << 20), b""):
                    h.update(c); n += len(c); w.write(c)
        if (h.hexdigest(), n) != (u["sha256"], u["size"]):
            os.remove(out + ".part")
            sys.exit(f"{f['url']}: member {u['member']} sha256 {h.hexdigest()} size {n}, pinned {u['sha256']} {u['size']}")
        os.chmod(out + ".part", int(f["mode"], 8))
        os.rename(out + ".part", out)
        print(f"{b['platform']}: {f['url']} sha256 {f['sha256']} size {f['size']}; member {u['member']} to goose-{arch}/{f['path']}, sha256 {u['sha256']} size {u['size']} mode {f['mode']}")
    shutil.copy(hj, os.path.join(d, "harness.json"))
PY
}

# --- the probe's harness.json: the real one, its command replaced, its env kept -------------------
probe_json() { # out
    python3 -I - "$harness" "$1" <<'PY'
import json, sys
src, out = sys.argv[1:]
h = json.load(open(src))
script = r'''
W=/workspace
id -u > $W/uid 2>&1
id > $W/id 2>&1
env | grep -E '^(GOOSE_[A-Z_]*|HOME|PATH|USER)=' | sort > $W/env
goose --version > $W/version.out 2> $W/version.err; echo "exit $?" > $W/version.exit
goose info -v > $W/info 2>&1; echo "exit $?" >> $W/info
KEY=$(cat $W/placeholder-key 2>/dev/null)
GOOSE_PROVIDER=openai GOOSE_MODEL=gpt-4o-mini OPENAI_API_KEY=$KEY timeout 90 goose run --no-session -t hi > $W/run.out 2>&1; echo "exit $?" > $W/run.exit
if command -v bash > /dev/null 2>&1; then
  timeout 10 bash -c 'exec 3<>/dev/tcp/1.1.1.1/80' > $W/port80 2>&1 && echo "port 80 OPEN" >> $W/port80 || echo "port 80 closed (exit $?)" >> $W/port80
elif command -v nc > /dev/null 2>&1; then
  timeout 10 nc -w 5 1.1.1.1 80 < /dev/null > $W/port80 2>&1 && echo "port 80 OPEN" >> $W/port80 || echo "port 80 closed (exit $?)" >> $W/port80
else
  echo "no bash or nc in this base" > $W/port80
fi
{ getent hosts api.openai.com || nslookup api.openai.com; } > $W/dns 2>&1 || echo "name lookup failed" >> $W/dns
find "$GOOSE_PATH_ROOT" -maxdepth 2 > $W/root-tree 2>&1
echo done > $W/done
'''
h["run"]["command"] = ["sh", "-c", script]
json.dump(h, open(out, "w"), indent=2)
PY
}

# The digest a Docker Hub tag names now, by a manifest HEAD (no pull is counted).
hub_ref() { # repo tag
    local token digest
    token=$(curl -fsS "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/$1:pull" | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["token"])')
    digest=$(curl -fsSI -H "Authorization: Bearer $token" -H 'Accept: application/vnd.oci.image.index.v1+json' \
        -H 'Accept: application/vnd.docker.distribution.manifest.list.v2+json' \
        "https://registry-1.docker.io/v2/library/$1/manifests/$2" | tr -d '\r' | awk 'tolower($1) == "docker-content-digest:" { print $2 }')
    printf '%s:%s@%s\n' "$1" "$2" "$digest"
}

export SHARDS_HOME="$work/home" DOCKER_CONFIG="$work/docker-config" SHARDS_LOCAL_STORE=none
unset DOCKER_AUTH_CONFIG SHARDS_DEFAULT_PLATFORM DOCKER_DEFAULT_PLATFORM
mkdir -p "$SHARDS_HOME" "$DOCKER_CONFIG"
printf '{}\n' >"$DOCKER_CONFIG/config.json"
names="goose-real goose-probe goose-control goose-alpine goose-slim"
cleanup() {
    for n in $names; do "$shards" rm -f "$n" >/dev/null 2>&1 || true; done
    "$shards" stop daemon >/dev/null 2>&1 || true
}
trap cleanup EXIT

log "shards $("$shards" --version 2>&1 | head -1); host $(uname -srm); mode $mode"
log "context"
assemble "$work/ctx" "$agentfile" "$harness"
log "build: one image for linux/amd64 and linux/arm64"
step build-real "$shards" build image --progress=plain --platform linux/amd64,linux/arm64 -t harness-goose:smoke "$work/ctx" || { tail -30 "$work/build-real.log"; exit 1; }
grep -i -E 'warn|SecretsUsed' "$work/build-real.log" | head -10 || true
step images-tree "$shards" images --tree || true
cat "$work/images-tree.log"
for p in amd64 arm64; do
    printf 'inspect linux/%s: ' "$p"
    "$shards" image inspect --platform "linux/$p" --format '{{.Os}}/{{.Architecture}} {{.Id}} size {{.Size}}' harness-goose:smoke 2>&1 || true
done

log "the smoke: Goose's own --version on linux/$here, stdout held to the pin's line"
s=$(now_ms)
rc=0
"$shards" run vm --rm --platform "linux/$here" harness-goose:smoke /harness/goose/bin/goose --version >"$work/run-version.out" 2>"$work/run-version.err" || rc=$?
printf 'run-version: exit %s in %s ms\n' "$rc" $(($(now_ms) - s))
printf 'SMOKE_STDOUT_HEX %s\n' "$(od -An -tx1 "$work/run-version.out" | tr -s ' \n' ' ')"
printf 'SMOKE_EXACT %s\n' "$(python3 -I -c 'import json, sys; want = json.load(open(sys.argv[1]))["smoke"]["stdout"] + "\n"; print(open(sys.argv[2], "rb").read() == want.encode())' "$pin" "$work/run-version.out")"
grep -v -E '^\s*$' "$work/run-version.err" | tail -5 || true

log "the real image: its harness as harness.json declares, Goose's root a host folder"
mkdir -p "$work/real/project" "$work/real/root"
step run-real "$shards" run vm -d --name goose-real --platform "linux/$here" \
    -v "$work/real/project:/workspace" -v "$work/real/root:/home/goose/.local/share/goose" \
    harness-goose:smoke sleep 120 || tail -20 "$work/run-real.log"
sleep 15
step processes-real "$shards" read processes goose-real || true
cat "$work/processes-real.log"
printf -- '--- what goose acp wrote under GOOSE_PATH_ROOT (the host folder)\n'
(cd "$work/real/root" && find . -maxdepth 3 | sort)
"$shards" rm -f goose-real >/dev/null 2>&1 || true

probe_run() { # name ctx [build args...]
    local name=$1 ctx=$2
    shift 2
    mkdir -p "$work/$name/project" "$work/$name/root"
    printf 'placeholder-not-a-key\n' >"$work/$name/project/placeholder-key"
    step "build-$name" "$shards" build image --progress=plain --platform "linux/$here" -t "harness-goose:$name" "$@" "$ctx" || { tail -20 "$work/build-$name.log"; return 1; }
    step "run-$name" "$shards" run vm -d --name "goose-$name" --platform "linux/$here" \
        -v "$work/$name/project:/workspace" -v "$work/$name/root:/home/goose/.local/share/goose" \
        "harness-goose:$name" sleep 300 || { tail -20 "$work/run-$name.log"; return 1; }
    local deadline=$(($(date +%s) + 200))
    while [ ! -e "$work/$name/project/done" ] && [ "$(date +%s)" -lt "$deadline" ]; do sleep 2; done
    step "processes-$name" "$shards" read processes "goose-$name" || true
    "$shards" rm -f "goose-$name" >/dev/null 2>&1 || true
    log "$name: what the harness wrote"
    for f in uid id env version.out version.exit info run.exit port80 dns root-tree done; do
        printf -- '--- %s\n' "$f"
        cat "$work/$name/project/$f" 2>/dev/null || echo "(missing)"
    done
    printf -- '--- run.out (its last 12 lines)\n'
    tail -12 "$work/$name/project/run.out" 2>/dev/null || echo "(missing)"
    printf -- '--- host: Goose root volume\n'
    (cd "$work/$name/root" && find . -maxdepth 2 | sort)
}

if [ "$mode" = all ]; then
    log "probe: the real Agentfile with a test-only harness.json"
    probe_json "$work/probe-harness.json"
    assemble "$work/ctx-probe" "$agentfile" "$work/probe-harness.json"
    probe_run probe "$work/ctx-probe" || true

    log "control: the probe without its three network lines"
    grep -v -E '^(NETWORK|EXPOSE|CONNECT) ' "$agentfile" >"$work/Agentfile.control"
    assemble "$work/ctx-control" "$work/Agentfile.control" "$work/probe-harness.json"
    probe_run control "$work/ctx-control" || true

    alpine=$(hub_ref alpine 3.24)
    slim=$(hub_ref debian trixie-slim)
    log "probe on Alpine, $alpine"
    probe_run alpine "$work/ctx-probe" --build-arg "BASE=$alpine" || true
    log "probe on debian:trixie-slim, $slim"
    probe_run slim "$work/ctx-probe" --build-arg "BASE=$slim" || true

    log "secrets: the image's env and the files it adds, then a copy of the Agentfile with ENV OPENAI_API_KEY"
    scan() { # image ctx
        local found=0
        "$shards" image inspect --platform "linux/$here" --format '{{json .Config.Env}}' "$1" >"$work/env-$1.json" 2>&1 || true
        printf 'env of %s: %s\n' "$1" "$(cat "$work/env-$1.json")"
        python3 -I -c 'import json, re, sys; env = json.load(open(sys.argv[1])) or []; bad = [e.split("=", 1)[0] for e in env if re.search("KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL", e.split("=", 1)[0])]; print("env names like a credential:", bad); sys.exit(1 if bad else 0)' "$work/env-$1.json" || found=1
        if grep -r -I -l -E '(KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL)[A-Z_]*=|sk-[A-Za-z0-9_-]{20,}' "$2" >"$work/files-$1.txt" 2>/dev/null; then
            printf 'files holding one: %s\n' "$(tr '\n' ' ' <"$work/files-$1.txt")"
            found=1
        fi
        printf 'SCAN %s %s\n' "$1" "$([ $found = 0 ] && echo clean || echo FOUND)"
    }
    scan harness-goose:smoke "$work/ctx"
    { cat "$agentfile"; printf 'ENV OPENAI_API_KEY=placeholder-not-a-key\n'; } >"$work/Agentfile.secret"
    assemble "$work/ctx-secret" "$work/Agentfile.secret" "$harness"
    step build-secret "$shards" build image --progress=plain --platform "linux/$here" -t harness-goose:secret "$work/ctx-secret" || true
    printf 'build warns SecretsUsedInArgOrEnv: %s\n' "$(grep -c 'SecretsUsedInArgOrEnv' "$work/build-secret.log" || true)"
    grep -m2 'SecretsUsedInArgOrEnv' "$work/build-secret.log" || true
    scan harness-goose:secret "$work/ctx-secret"
fi

log "sizes and times (single observations)"
du -sh "$work/ctx" "$work/ctx/goose-"* 2>/dev/null
"$shards" images --format '{{.Repository}}:{{.Tag}} {{.Size}}' 2>&1 | grep harness-goose || true
log "done $work"
