#!/usr/bin/env bash
# Plan step 26, step 4 (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d): Antigravity's image built for
# linux/amd64 and linux/arm64 with a shards from Shards main (build-shards.sh), and run in microVMs.
# The build context is laid out as pin.json pins it (the shared fetcher, `kiln agentfiles context`,
# isn't written yet): each archive held to its SHA-256, SHA-512 and size, its one member to its own
# SHA-256 and size, at antigravity-<arch>/bin/agy 0755, beside the embedded harness.json. Then:
#   build   `shards build image` of both platforms, timed; the index and each platform's size
#   smoke   `shards run vm --rm --platform $PLATFORM IMAGE /harness/antigravity/bin/agy --version`
#   harness `shards run vm -d ... sleep 600` with the project and home volumes, and the kernel's
#           record of the harness's processes (`shards read processes`)
#   probe   the same, with a test-only harness.json whose command is probe.sh, and its control:
#           the probe's image with no network lines
#   bases   the probe on other bases, by digest (BASES: name=ref@sha256:... pairs)
# Every command's output is kept under the run's directory; the record is record.txt there.
#   step4.sh PLATFORM PHASE...      (PLATFORM linux/amd64 or linux/arm64)
set -uo pipefail
here=$(pwd)
platform=$1; shift
W="$HOME/.cache/s26-agy-step4"
S=${SHARDS_BIN:-$W/shards-target/test-release/shards}
R=${RUN_DIR:-$W/run}
mkdir -p "$R/home" "$R/docker-config" "$W/dl" || exit 1
[ -f "$R/docker-config/config.json" ] || printf '{}\n' > "$R/docker-config/config.json"
export SHARDS_HOME="$R/home" DOCKER_CONFIG="$R/docker-config" SHARDS_LOCAL_STORE=none
unset DOCKER_AUTH_CONFIG SHARDS_DEFAULT_PLATFORM DOCKER_DEFAULT_PLATFORM
sha256() { (sha256sum "$1" 2>/dev/null || shasum -a 256 "$1") | cut -d' ' -f1; }
sha512() { (sha512sum "$1" 2>/dev/null || shasum -a 512 "$1") | cut -d' ' -f1; }
size() { wc -c < "$1" | tr -d ' '; }
now_ms() { python3 -I -c 'import time; print(int(time.time() * 1000))'; }
say() { printf '%s\n' "$*" | tee -a "$R/record.txt"; }
# step LOG CMD...: CMD's output in LOG, its exit status and time said.
step() {
  local log=$1; shift
  local t0 t1 rc=0
  t0=$(now_ms)
  "$@" > "$R/$log" 2>&1 || rc=$?
  t1=$(now_ms)
  say "$log: exit $rc in $((t1 - t0)) ms: $*"
  return $rc
}

ctx() {  # DIR [HARNESS_JSON]: the context, every file held to the pin.
  local dir=$1 hj=${2:-$here/harness.json}
  rm -rf "$dir"; mkdir -p "$dir" || return 1
  cp "$here/Agentfile" "$dir/Agentfile" || return 1
  python3 -I -c 'import json, sys
p = json.load(open(sys.argv[1]))
for b in p["builds"]:
    f = b["files"][0]; u = f["unpack"]
    print(b["platform"].split("/")[1], f["url"], f["sha256"], f["sha512"], f["size"], u["member"], u["sha256"], u["size"], f["path"], f["mode"])' "$here/pin.json" |
  while read -r arch url a256 a512 asize member m256 msize path mode; do
    local a="$W/dl/$arch.tar.gz"
    if [ ! -f "$a" ] || [ "$(sha256 "$a")" != "$a256" ]; then
      curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL --retry 3 -o "$a" "$url" || { say "ctx: fetch of $url failed"; return 1; }
    fi
    [ "$(sha256 "$a")" = "$a256" ] && [ "$(sha512 "$a")" = "$a512" ] && [ "$(size "$a")" = "$asize" ] || { say "ctx: $url is not the pin's archive"; return 1; }
    local h="$dir/antigravity-$arch"
    mkdir -p "$h/$(dirname "$path")" "$h/.x" || return 1
    tar -xzf "$a" -C "$h/.x" "$member" || { say "ctx: $url has no $member"; return 1; }
    [ "$(sha256 "$h/.x/$member")" = "$m256" ] && [ "$(size "$h/.x/$member")" = "$msize" ] || { say "ctx: $member of $url is not the pin's"; return 1; }
    mv "$h/.x/$member" "$h/$path" && rmdir "$h/.x" && chmod "$mode" "$h/$path" || return 1
    cp "$hj" "$h/harness.json" || return 1
    say "ctx: $dir/antigravity-$arch: $path $(sha256 "$h/$path") mode $mode, harness.json $(sha256 "$h/harness.json")"
  done
}

for phase in "$@"; do
  case $phase in
    build)
      say "# step 4 on $(hostname) ($(uname -srm)) at $(date -u +%FT%TZ); shards $(sha256 "$S")"
      say "# shards: $(SHARDS_HOME="$R/home" "$S" version 2>&1 | grep -E 'Version|Git commit' | head -2 | tr -s ' ' | tr '\n' ' ')"
      say "# carrier: Agentfile $(sha256 "$here/Agentfile"), harness.json $(sha256 "$here/harness.json"), pin.json $(sha256 "$here/pin.json")"
      ctx "$R/ctx" || exit 1
      step build.log "$S" build image --progress=plain --platform linux/amd64,linux/arm64 -t harness-antigravity:s26 "$R/ctx" || exit 1
      step inspect.log "$S" inspect image harness-antigravity:s26
      step images.log "$S" list image
      ;;
    smoke)
      t0=$(now_ms); rc=0
      "$S" run vm --rm --platform "$platform" harness-antigravity:s26 /harness/antigravity/bin/agy --version > "$R/smoke.out" 2> "$R/smoke.err" || rc=$?
      say "smoke: exit $rc in $(( $(now_ms) - t0 )) ms: shards run vm --rm --platform $platform harness-antigravity:s26 /harness/antigravity/bin/agy --version"
      say "smoke stdout bytes: $(od -An -tx1 < "$R/smoke.out" | tr -s ' \n' ' ')"
      say "smoke stderr: $(head -c 600 "$R/smoke.err" | tr '\n' ' ')"
      ;;
    harness)
      rm -rf "$R/proj" "$R/homevol"; mkdir -p "$R/proj" "$R/homevol"
      step harness-run.log "$S" run vm -d --platform "$platform" --name agy-s26 -v "$R/proj:/workspace" -v "$R/homevol:/home/antigravity" harness-antigravity:s26 sleep 600
      for i in $(seq 1 60); do
        "$S" read processes agy-s26 > "$R/harness-processes.log" 2>&1
        grep -q 'harness antigravity.* exit ' "$R/harness-processes.log" && grep -q '/harness/antigravity/bin/agy' "$R/harness-processes.log" && break
        sleep 2
      done
      say "harness: its execs: $(grep -E 'harness antigravity .* exec +/harness' "$R/harness-processes.log" | sed 's/^[^ ]* *//' | head -3 | tr '\n' ';')"
      say "harness: the home volume after: $(cd "$R/homevol" && find . -maxdepth 3 | sort | head -20 | tr '\n' ' ')"
      step harness-rm.log "$S" rm -f agy-s26
      ;;
    probe|control)
      local_ctx="$R/ctx-$phase"; ctx "$local_ctx" "$here/probe-harness.json" || exit 1
      if [ "$phase" = control ]; then
        grep -v -E '^(NETWORK|EXPOSE|CONNECT) ' "$here/Agentfile" > "$local_ctx/Agentfile"
      fi
      step "$phase-build.log" "$S" build image --progress=plain --platform "$platform" -t "harness-antigravity-$phase:s26" "$local_ctx" || exit 1
      rm -rf "$R/proj-$phase" "$R/homevol-$phase"; mkdir -p "$R/proj-$phase" "$R/homevol-$phase"
      # The name's addresses as this host resolves them, for the probe's own connects by address.
      python3 -I -c 'import socket
for fam, name in ((socket.AF_INET, "ip4"), (socket.AF_INET6, "ip6")):
    a = socket.getaddrinfo("cloudcode-pa.googleapis.com", 443, fam, socket.SOCK_STREAM)[0][4][0]
    open(name, "w").write(a + "\n")' 2>/dev/null; mv ip4 ip6 "$R/proj-$phase/" 2>/dev/null
      say "$phase: the host resolves cloudcode-pa.googleapis.com to $(cat "$R/proj-$phase/ip4" 2>/dev/null) and $(cat "$R/proj-$phase/ip6" 2>/dev/null)"
      step "$phase-run.log" "$S" run vm -d --platform "$platform" --name "agy-$phase" -v "$R/proj-$phase:/workspace" -v "$R/homevol-$phase:/home/antigravity" "harness-antigravity-$phase:s26" sleep 600
      for i in $(seq 1 90); do [ -f "$R/proj-$phase/probe/done" ] && break; sleep 2; done
      "$S" read processes "agy-$phase" > "$R/$phase-processes.log" 2>&1
      for f in version version.rc uid gid home dns dns.rc touch.rc touch.err git.rc git.err mcp.rc mcp.out turn.rc https https.rc https.err http80 http80.rc http80.err https-ip4 https-ip4.rc https-ip4.err https-ip6 https-ip6.rc https-ip6.err ca-bundle agy-tls done; do
        say "$phase $f: $(head -c 400 "$R/proj-$phase/probe/$f" 2>/dev/null | tr '\n' ' ')"
      done
      say "$phase turn.out: $(head -c 600 "$R/proj-$phase/probe/turn.out" 2>/dev/null | tr '\n' ' ')"
      say "$phase home volume: $(cd "$R/homevol-$phase" && find . -maxdepth 4 | sort | head -30 | tr '\n' ' ')"
      say "$phase .gitconfig: $(cat "$R/homevol-$phase/.gitconfig" 2>/dev/null | tr '\n' ' ')"
      say "$phase mcp_config.json: $(cat "$R/homevol-$phase/.gemini/config/mcp_config.json" 2>/dev/null | tr -d '\n' | head -c 400)"
      step "$phase-rm.log" "$S" rm -f "agy-$phase"
      ;;
    index)
      for p in linux/amd64 linux/arm64; do
        step "inspect-${p#linux/}.log" "$S" inspect image --platform "$p" harness-antigravity:s26
        say "index ${p}: $(python3 -I -c 'import json, sys
i = json.load(open(sys.argv[1]))[0]
print(i["Descriptor"]["mediaType"], i["Descriptor"]["digest"], i["Os"] + "/" + i["Architecture"], i["Size"], "bytes,", len(i["RootFS"]["Layers"]), "layers")' "$R/inspect-${p#linux/}.log" 2>&1)"
      done
      ;;
    indexdoc)
      d=$(python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))[0]["Descriptor"]["digest"].split(":")[1])' "$R/inspect.log")
      f=$(find "$R/home" -type f -name "$d*" 2>/dev/null | head -1)
      say "index document $d: $(cat "$f" 2>/dev/null | head -c 2000 | tr '\n' ' ')"
      ;;
    bases)
      for pair in ${BASES:-}; do
        bname=${pair%%=*}; bref=${pair#*=}
        bctx="$R/ctx-base-$bname"; ctx "$bctx" "$here/probe-harness.json" || exit 1
        step "base-$bname-build.log" "$S" build image --progress=plain --platform "$platform" --build-arg "BASE=$bref" -t "harness-antigravity-base-$bname:s26" "$bctx" || continue
        say "base $bname: $(grep -E 'DISK|harness-antigravity-base-'"$bname" <("$S" list image) | tr -s ' ' | tail -1)"
        rm -rf "$R/proj-base-$bname" "$R/homevol-base-$bname"; mkdir -p "$R/proj-base-$bname" "$R/homevol-base-$bname"
        step "base-$bname-run.log" "$S" run vm -d --platform "$platform" --name "agy-base-$bname" -v "$R/proj-base-$bname:/workspace" -v "$R/homevol-base-$bname:/home/antigravity" "harness-antigravity-base-$bname:s26" sleep 600
        for i in $(seq 1 90); do [ -f "$R/proj-base-$bname/probe/done" ] && break; sleep 2; done
        for f in version version.rc uid home dns.rc touch.rc mcp.rc turn.rc https.err ca-bundle agy-tls done; do
          say "base $bname $f: $(head -c 400 "$R/proj-base-$bname/probe/$f" 2>/dev/null | tr '\n' ' ')"
        done
        step "base-$bname-rm.log" "$S" rm -f "agy-base-$bname"
      done
      ;;
    stop)
      step stop.log "$S" stop daemon
      ;;
    *) say "unknown phase $phase"; exit 64 ;;
  esac
done
