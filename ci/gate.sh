#!/usr/bin/env bash
# kiln's full gate: clippy for Linux, every test of the workspace, then clippy for Windows and
# macOS where zig is. Ask kiln for a machine that has what it needs and let it choose; never name
# one:
#
#   kiln run --needs cpus=12,mem=40g,linux/amd64 --detach -- ci/gate.sh
#   ci/gate.sh --dry-run            # its steps and caps, running nothing
#
# It holds itself to what it was asked: the needs `kiln run --needs` gives the command (KILN_NEEDS),
# or --cpus and --mem. As a systemd user unit, its memory is capped at what it asked (MemoryMax),
# and it yields to everything else on the machine: CPUWeight 1, nice 19 and IOWeight 1, the least
# of each (--io off drops the weight; --io-read and --io-write cap each disk its build is on, where a
# person wants caps: a cap stretches the drain of its own dirty pages that a sync waits behind, a
# weight gives way only where others want the disk, docs/protect.md "The gate's I/O"). It says
# whether the disks honour a weight. Its CPUs are what it runs at once: a rustc per 6 GiB asked, at
# most one per CPU (--jobs), and as many test threads (--test-threads). No CPU quota: one changes
# what the tests see of the machine (the CPUs a runner reports, W13), as a runner never does.
# Without a systemd user manager (macOS, a container) it runs uncapped at nice 19, and says so.
#
# While it runs, this machine's kiln guards the machine's own services (`kiln protect`,
# docs/protect.md "The guard"; LB05, G11): the kernel wakes the guard once the machine stalls (PSI
# triggers), and it reads then how long each service was stalled, from its cgroup; a kiln server
# named with --guard-url (KILN_GATE_GUARD_URL) says its own strain as it happens. It stops the gate
# where the gate can be the cause, and keeps it from starting beside a service strained already:
# every number its own (a person's deadline, 1 s by default; the kernel's windows; what it learned of
# this machine, in ~/.cache/kiln-gate/guard.json). Kiln's own containers aren't services it protects.
# Named with --guard-containers (KILN_GATE_GUARD_CONTAINERS), it protects those containers alone. A
# kiln from before `kiln protect` can't: the gate then runs its loop of 2026-10-28 (system.slice's
# I/O pressure over 30 three looks running, 10 s apart), until the machine's runner is upgraded.
# Stopped or ended, it removes what its tests left in Docker (a test server takes its containers and
# networks with it only when it ends): those mounting its own dir, and those labelled with the hash
# of a folder in its TMPDIR, as kiln's runners label what they make.
#
# Its build is its own: a target dir and TMPDIR under ${XDG_CACHE_HOME:-~/.cache}/kiln-gate/<tree>,
# and git's environment cleared, with an empty global config of its own (writable: tests write it)
# and no system one, so a machine's signing or hooks reach no test. HOME stays the person's: tests
# start the coding agents installed there, signed in there. Each step's log is in its logs folder
# (it says where); the last line is GATE: PASS, or GATE: FAIL and why.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
gate=$here/ci/gate.sh

# ---------------------------------------------------------------- the steps, inside the unit
if [ "${1:-}" = --steps ]; then
  jobs=$2 threads=$3 logs=$4 cross=$5
  cd "$here"
  export RUST_TEST_THREADS=$threads
  failed=()
  step() {
    local name=$1 t0=$SECONDS code=0
    shift
    echo "::: $name: $*"
    "$@" > "$logs/$name.log" 2>&1 || code=$?
    if [ $code -eq 0 ]; then
      echo "    passed in $((SECONDS - t0)) s"
      return
    fi
    echo "    FAILED (exit $code) in $((SECONDS - t0)) s: $logs/$name.log"
    # What failed: the errors, and each failing test with how to run it again alone.
    awk '/^ *Running .*\(.*\)$/ { bin = $NF; gsub(/[()]/, "", bin) }
         /^error(\[E[0-9]+\])?:/ && n < 20 { print "    | " $0; n++ }
         /^test .* \.\.\. FAILED$/ && n < 40 { print "    | " $2 " (alone: " bin " --exact " $2 ")"; n++ }' "$logs/$name.log" || true
    failed+=("$name")
  }
  step clippy-linux cargo clippy --locked --workspace --all-targets -j "$jobs" -- -D warnings
  # The libraries as their tests first, then the rest (.kiln/ci.yaml): kiln's crate compiled twice
  # at once (as itself and as its tests) is what runs a build out of memory.
  step test-build cargo test --locked --workspace --lib --no-run -j "$jobs"
  step test cargo test --locked --workspace --no-fail-fast -j "$jobs"
  if [ "$cross" = on ]; then
    for t in x86_64-pc-windows-gnu aarch64-apple-darwin; do
      step "clippy-$t" env CARGO_TARGET_DIR="$CARGO_TARGET_DIR-$t" cargo-zigbuild clippy --locked --workspace --target "$t" -j "$jobs" -- -D warnings
    done
  fi
  if [ ${#failed[@]} -eq 0 ]; then
    echo "GATE: PASS"
  else
    echo "GATE: FAIL (${failed[*]})"
    exit 1
  fi
  exit 0
fi

# ---------------------------------------------------------------- what it was asked
dry=false cpus='' mem='' jobs='' threads='' io=on io_read='' io_write='' guard_url=${KILN_GATE_GUARD_URL:-} guard_containers=${KILN_GATE_GUARD_CONTAINERS:-} cross=auto
while [ $# -gt 0 ]; do
  case $1 in
    --dry-run) dry=true ;;
    --cpus) cpus=$2; shift ;;
    --mem) mem=$2; shift ;;
    --jobs) jobs=$2; shift ;;
    --test-threads) threads=$2; shift ;;
    --io) io=$2; shift ;;
    --io-read) io_read=$2; shift ;;
    --io-write) io_write=$2; shift ;;
    --guard-url) guard_url=$2; shift ;;
    --guard-containers) guard_containers=$2; shift ;;
    --no-cross) cross=off ;;
    -h | --help) sed -n '2,/^set -euo pipefail/p' "$gate" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) echo "ci/gate.sh: no such option $1 (--help says what it takes)" >&2; exit 2 ;;
  esac
  shift
done
asked_by="--cpus and --mem"
for w in $(printf '%s' "${KILN_NEEDS:-}" | tr ', ' '\n\n'); do
  case $w in
    cpus=* | cpu=*) [ -n "$cpus" ] || { cpus=${w#*=}; asked_by="KILN_NEEDS=$KILN_NEEDS"; } ;;
    mem=* | memory=*) [ -n "$mem" ] || { mem=${w#*=}; asked_by="KILN_NEEDS=$KILN_NEEDS"; } ;;
  esac
done
if [ -z "$cpus" ] && [ -z "$mem" ]; then asked_by="nothing: what a full gate needs"; fi
cpus=${cpus:-6} mem=${mem:-40g}
# CPUs as kiln writes them (8, 0.5, 500m); memory in systemd's binary units (40G, 512M).
cpu_n=$(awk -v c="$cpus" 'BEGIN { if (c ~ /m$/) { sub(/m$/, "", c); c /= 1000 } if (c !~ /^[0-9.]+$/ || c <= 0) exit 1; printf "%g", c }') ||
  { echo "ci/gate.sh: $cpus is not a number of CPUs (8, 0.5, 500m)" >&2; exit 2; }
mem_cap=$(printf '%s' "$mem" | tr '[:lower:]' '[:upper:]' | sed -E 's/I?B?$//')
printf '%s' "$mem_cap" | grep -Eq '^[0-9]+(\.[0-9]+)?[KMGT]?$' || { echo "ci/gate.sh: $mem is not a size (40g, 512m)" >&2; exit 2; }
mem_gib=$(awk -v m="$mem_cap" 'BEGIN { u = substr(m, length(m)); n = m + 0; k = (u == "T") ? 1024 : (u == "G") ? 1 : (u == "M") ? 1 / 1024 : (u == "K") ? 1 / 1048576 : 1 / 1073741824; print n * k }')
jobs=${jobs:-$(awk -v c="$cpu_n" -v g="$mem_gib" 'BEGIN { j = int(c); if (j < c) j++; m = int(g / 6); if (m < j) j = m; if (j < 1) j = 1; print j }')}
threads=${threads:-$jobs}

# ---------------------------------------------------------------- where it builds
real_home=$HOME
key=$(printf '%s' "$here" | { sha256sum 2>/dev/null || shasum -a 256; } | cut -c1-12)
base=${KILN_GATE_DIR:-${XDG_CACHE_HOME:-$real_home/.cache}/kiln-gate/$key}
target_dir=$base/target
logs=$base/logs/$(date -u +%Y%m%dT%H%M%SZ)
gitconfig=$base/gitconfig
cargo_home=${CARGO_HOME:-$real_home/.cargo}
rustup_home=${RUSTUP_HOME:-$real_home/.rustup}
path=$cargo_home/bin:$real_home/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH
if [ "$cross" = auto ]; then
  if PATH=$path command -v cargo-zigbuild > /dev/null && PATH=$path command -v zig > /dev/null; then cross=on; else cross=off; fi
fi
unit=kiln-gate-$key-$(date +%s)
systemd=false
if command -v systemd-run > /dev/null && systemctl --user show-environment > /dev/null 2>&1; then systemd=true; fi
props=(-p "MemoryMax=$mem_cap" -p CPUWeight=1 -p Nice=19)
# The disks its build is on, one a line, as systemd's I/O caps want them: a path on btrfs (a
# subvolume, as adacachy's /home is) has an anonymous st_dev that systemd maps to no disk ("Failed
# to set 'io.max' … to '0:0 …': No such device", and no cap at all), so they're the disks under the
# mount's block device (lsblk's inverse tree: a partition's disk, a RAID's or a mapped device's
# disks), and under each device of a btrfs, which can spread over several disks and put its chunks
# on any of them (adacachy's /home, over four; findmnt names one); the path where none can be found
# (no findmnt or lsblk).
io_disks() {
  local p=$1 src='' fs='' uuid='' d devs=() disks=''
  while [ ! -e "$p" ] && [ "$p" != / ]; do p=$(dirname "$p"); done
  read -r src fs uuid < <(findmnt -no SOURCE,FSTYPE,UUID -T "$p" 2> /dev/null || true) || true
  if [ "$fs" = btrfs ] && [ -n "$uuid" ]; then
    for d in "/sys/fs/btrfs/$uuid/devices"/*; do
      if [ -b "/dev/${d##*/}" ]; then devs+=("/dev/${d##*/}"); fi
    done
  fi
  src=${src%%\[*}
  if [ ${#devs[@]} -eq 0 ] && [ -b "$src" ]; then devs=("$src"); fi
  if [ ${#devs[@]} -gt 0 ]; then
    disks=$(lsblk -snro KNAME,TYPE "${devs[@]}" 2> /dev/null | awk '$2 == "disk" { print "/dev/" $1 }' | sort -u || true)
  fi
  echo "${disks:-$1}"
}
if [ "$io" != off ]; then
  props+=(-p IOWeight=1)
  if [ -n "$io_read$io_write" ]; then
    while read -r io_dev; do
      [ -z "$io_read" ] || props+=(-p "IOReadBandwidthMax=$io_dev $io_read")
      [ -z "$io_write" ] || props+=(-p "IOWriteBandwidthMax=$io_dev $io_write")
    done < <(io_disks "$base")
  fi
fi
# Whether a disk honours an I/O weight: a weight is enforced only by the io.cost controller (the
# root's io.cost.qos, enable=1) or by BFQ as its scheduler, and only in a cgroup given the io
# controller (systemd's user@.service delegates pids, memory and cpu alone). Asked is not enforced:
# it says which (docs/protect.md "The gate's I/O").
weighed() {
  local dev=${1##*/} majmin sched mgr
  [ -r "/sys/block/$dev/dev" ] || { echo "$1: not a disk this can read"; return; }
  majmin=$(cat "/sys/block/$dev/dev")
  sched=$(sed -n 's/.*\[\(.*\)\].*/\1/p' "/sys/block/$dev/queue/scheduler" 2> /dev/null || true)
  mgr=/sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/cgroup.controllers
  if ! grep -qw io "$mgr" 2> /dev/null; then
    echo "$1: not enforced, your user manager has no io controller (root's, once: Delegate=pids memory cpu io for user@.service)"
  elif [ "$sched" = bfq ] || grep -q "^$majmin .*enable=1" /sys/fs/cgroup/io.cost.qos 2> /dev/null; then
    echo "$1: enforced (${sched:-?}$(grep -q "^$majmin .*enable=1" /sys/fs/cgroup/io.cost.qos 2> /dev/null && echo ', io.cost on'))"
  else
    echo "$1: not enforced, its scheduler is ${sched:-unknown} and io.cost is off (root's, once: BFQ, or io.cost.qos enable=1)"
  fi
}
# The guard: this machine's kiln, if it has `kiln protect`.
kiln_bin=$(PATH=$path command -v kiln || true)
guard=legacy
if [ -n "$kiln_bin" ] && "$kiln_bin" protect --help > /dev/null 2>&1; then guard=protect; fi

steps=("clippy-linux: cargo clippy --locked --workspace --all-targets -j $jobs -- -D warnings"
  "test-build: cargo test --locked --workspace --lib --no-run -j $jobs"
  "test: cargo test --locked --workspace --no-fail-fast -j $jobs (RUST_TEST_THREADS=$threads)")
if [ "$cross" = on ]; then
  for t in x86_64-pc-windows-gnu aarch64-apple-darwin; do steps+=("clippy-$t: cargo-zigbuild clippy --locked --workspace --target $t -j $jobs -- -D warnings"); done
fi
caps=$(printf '%s ' "${props[@]#-p}" | sed 's/  */ /g; s/^ //; s/ $//')
echo "ci/gate.sh: the gate of $here at $(git -C "$here" rev-parse --short HEAD 2> /dev/null || echo '(not a git tree)')"
echo "asked: $(awk -v c="$cpu_n" 'BEGIN { printf "%g CPU%s", c, (c == 1) ? "" : "s" }'), $mem_cap of memory (from $asked_by)"
if $systemd; then
  echo "caps: $caps (systemd user unit $unit)"
else
  echo "caps: $caps would apply, but this machine has no systemd user manager: it runs uncapped, at nice 19"
fi
echo "cargo: -j $jobs, $threads test threads"
if [ "$io" != off ] && $systemd; then
  while read -r io_dev; do echo "io weight: $(weighed "$io_dev")"; done < <(io_disks "$base")
fi
if [ "$guard" = protect ]; then
  echo "guard: kiln protect ($kiln_bin): ${guard_containers:+the containers ${guard_containers//,/, }, }the machine's services' stalls by event${guard_url:+, and what $guard_url says of itself}; it stops the gate where the gate can be the cause (docs/protect.md)"
elif [ -n "$guard_containers" ]; then
  echo "guard: this machine's kiln has no kiln protect, so the loop of 2026-10-28: I/O pressure (full avg10) over 30 in ${guard_containers//,/, }${guard_url:+, or $guard_url over 3 s}, three checks running, stops it (upgrading its runner is the person's)"
else
  echo "guard: this machine's kiln has no kiln protect, so the loop of 2026-10-28: system.slice I/O pressure (full avg10) over 30, its own tests' containers left out${guard_url:+, or $guard_url over 3 s}, three checks running, stops it (upgrading its runner is the person's)"
fi
echo "steps:"
n=0
for s in "${steps[@]}"; do n=$((n + 1)); echo "  $n. $s"; done
[ "$cross" = on ] || echo "  (no clippy for Windows and macOS: no zig and cargo-zigbuild here)"
echo "build: $target_dir, TMPDIR $base/tmp, git config $gitconfig, logs $logs"
if $dry; then exit 0; fi

# ---------------------------------------------------------------- the run
mkdir -p "$target_dir" "$base/tmp" "$logs"
: > "$gitconfig"
env_vars=(HOME="$real_home" GIT_CONFIG_GLOBAL="$gitconfig" GIT_CONFIG_NOSYSTEM=1 CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" PATH="$path" CARGO_TARGET_DIR="$target_dir"
  TMPDIR="$base/tmp" CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_TERM_COLOR=never)
inner=(env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE "${env_vars[@]}" bash "$gate" --steps "$jobs" "$threads" "$logs" "$cross")
if ! $systemd; then
  exec nice -n 19 "${inner[@]}"
fi

stop() { systemctl --user stop "$unit" 2> /dev/null || true; }
# A cgroup's I/O pressure: its "full" avg10.
pressure() { awk '/^full/ { split($2, a, "="); print a[2] }' "$1/io.pressure" 2> /dev/null || true; }
# The Docker scopes of the containers this gate's tests made. A test's kiln server labels all it
# makes kiln.runner-work, the start of the sha256 of its runner's work folder
# (runner::docker::owner_label), and those folders are under this gate's TMPDIR or its target's tmp.
own_scopes() {
  command -v docker > /dev/null || return 0
  local works
  works=$(find "$base/tmp" "$target_dir/tmp" -maxdepth 3 -type d -name work 2> /dev/null | while read -r d; do printf '%s' "$d" | { sha256sum 2> /dev/null || shasum -a 256; } | cut -c1-16; done || true)
  [ -n "$works" ] || return 0
  docker ps --no-trunc --filter label=kiln.runner-work --format '{{.ID}} {{.Label "kiln.runner-work"}}' 2> /dev/null | while read -r id h; do
    if printf '%s\n' "$works" | grep -qxF -- "$h"; then echo "docker-$id.scope"; fi
  done || true
  return 0
}
# With --guard-containers: the most strained of the containers named, its "full" avg10 and its name.
# None of them running, there's nothing of theirs to keep from: 0.
named_strain() {
  local worst=-1 who='' id n v
  while read -r id n; do
    v=$(pressure "/sys/fs/cgroup/system.slice/docker-$id.scope")
    [ -n "$v" ] || continue
    if awk -v v="$v" -v w="$worst" 'BEGIN { exit !(v > w) }'; then worst=$v who=$n; fi
  done < <(docker ps -q --no-trunc 2> /dev/null | xargs -r docker inspect -f '{{.Id}} {{.Name}}' 2> /dev/null | awk -v want=",$guard_containers," '{ n = substr($2, 2); if (index(want, "," n ",")) print $1, n }')
  if [ -z "$who" ]; then worst=0 who="$guard_containers (none running)"; fi
  echo "$worst $who"
}

# What its tests left in Docker: containers mounting its dir, and containers and networks labelled
# kiln.runner-work=<h> or kiln.socket.runner.<h>, h the hash of a folder in its TMPDIR (a test
# server's work dir, a test's own).
sweep() {
  command -v docker > /dev/null 2>&1 || return 0
  local hashes containers networks
  hashes=$(mktemp)
  find "$base/tmp" -mindepth 1 -maxdepth 3 -type d 2> /dev/null | while read -r d; do printf '%s' "$d" | { sha256sum 2> /dev/null || shasum -a 256; } | cut -c1-16; done | sort -u > "$hashes"
  containers=$(docker ps -aq --no-trunc 2> /dev/null | xargs -r docker inspect -f '{{.Id}} {{range .Mounts}}{{.Source}} {{end}}| {{range $k, $v := .Config.Labels}}{{$k}}={{$v}} {{end}}' 2> /dev/null |
    awk -v b="$base/" -v hf="$hashes" 'BEGIN { while ((getline h < hf) > 0) own[h] = 1 } { mine = index($0, b) > 0; for (i = 2; i <= NF; i++) { if ($i ~ /^kiln\.runner-work=/) { split($i, kv, "="); if (kv[2] in own) mine = 1 } if ($i ~ /^kiln\.socket\.runner\./) { k = $i; sub(/^kiln\.socket\.runner\./, "", k); sub(/=.*/, "", k); if (k in own) mine = 1 } } if (mine) print $1 }')
  [ -z "$containers" ] || docker rm -f $containers > /dev/null 2>&1 || true
  networks=$(docker network ls -q 2> /dev/null | xargs -r docker network inspect -f '{{.Id}} {{range $k, $v := .Labels}}{{$k}}={{$v}} {{end}}' 2> /dev/null |
    awk -v hf="$hashes" 'BEGIN { while ((getline h < hf) > 0) own[h] = 1 } { for (i = 2; i <= NF; i++) { if ($i ~ /^kiln\.runner-work=/) { split($i, kv, "="); if (kv[2] in own) { print $1; next } } if ($i ~ /^kiln\.socket\.runner\./) { k = $i; sub(/^kiln\.socket\.runner\./, "", k); sub(/=.*/, "", k); if (k in own) { print $1; next } } } }')
  [ -z "$networks" ] || docker network rm $networks > /dev/null 2>&1 || true
  rm -f "$hashes"
  if [ -n "$containers$networks" ]; then
    echo "ci/gate.sh: removed what its tests left in Docker (containers: $(printf '%s\n' $containers | grep -c .), networks: $(printf '%s\n' $networks | grep -c .))"
  fi
}
trap 'stop; sweep; exit 143' TERM INT HUP
if [ "$guard" = protect ]; then
  why_file=$base/guard.why
  rm -f "$why_file"
  args=(protect --unit "$unit" --state "${XDG_CACHE_HOME:-$real_home/.cache}/kiln-gate/guard.json" --log "$logs/guard.ndjson" --why "$why_file")
  [ -z "$guard_url" ] || args+=(--server "$guard_url")
  [ -z "$guard_containers" ] || args+=(--containers "$guard_containers")
  code=0
  "$kiln_bin" "${args[@]}" -- systemd-run --user --unit "$unit" --collect --wait --pipe --quiet --working-directory="$here" "${props[@]}" "${inner[@]}" || code=$?
  trap - TERM INT HUP
  sweep
  if [ $code -eq 75 ] && [ -s "$why_file" ]; then
    said=$(head -c 4000 "$why_file")
    echo "GATE: FAIL (${said%%: *}: the machine was strained: ${said#*: }; its guard's log: $logs/guard.ndjson)"
    exit 1
  fi
  if [ $code -gt 1 ]; then
    echo "GATE: FAIL (it ended with exit $code before saying: its unit didn't start, or it was killed)"
  fi
  exit $code
fi
# A kiln without `kiln protect`: the loop of 2026-10-28, its numbers G11's, until this machine's runner
# is upgraded (the person's; docs/protect.md "The gate's guard before kiln protect").
systemd-run --user --unit "$unit" --collect --wait --pipe --quiet --working-directory="$here" "${props[@]}" "${inner[@]}" &
pid=$!
bad=0 why=''
while kill -0 "$pid" 2> /dev/null; do
  sleep 10
  strained=''
  if [ -n "$guard_containers" ]; then
    read -r full who < <(named_strain)
    if awk -v f="$full" 'BEGIN { exit !(f > 30) }'; then strained="$who's I/O pressure $full"; fi
  elif [ -r /sys/fs/cgroup/system.slice/io.pressure ]; then
    full=$(pressure /sys/fs/cgroup/system.slice)
    if awk -v f="$full" 'BEGIN { exit !(f > 30) }'; then
      own=$(own_scopes)
      if [ -n "$own" ]; then
        # The services' strain but for this gate's own containers: the worst any other cgroup shows.
        rest=$(for d in /sys/fs/cgroup/system.slice/*/; do n=$(basename "$d"); printf '%s\n' "$own" | grep -qxF -- "$n" || pressure "$d"; done | sort -rn | head -1 || true)
        echo "ci/gate.sh: system.slice's I/O pressure $full is partly its own tests' $(printf '%s\n' "$own" | grep -c .) containers; the worst of the rest is ${rest:-0}"
        full=${rest:-0}
      fi
      if awk -v f="$full" 'BEGIN { exit !(f > 30) }'; then strained="system services' I/O pressure $full"; fi
    fi
  fi
  if [ -n "$guard_url" ]; then
    took=$(curl -s -o /dev/null -w '%{time_total}' --max-time 10 "$guard_url" || echo 10)
    if awk -v t="$took" 'BEGIN { exit !(t > 3) }'; then strained="${strained:+$strained, }$guard_url in ${took}s"; fi
  fi
  if [ -n "$strained" ]; then
    bad=$((bad + 1))
    echo "ci/gate.sh: the machine is strained: $strained ($bad in a row)"
  else
    bad=0
  fi
  if [ $bad -ge 3 ]; then
    why=$strained
    stop
    break
  fi
done
code=0
wait "$pid" || code=$?
trap - TERM INT HUP
sweep
if [ -n "$why" ]; then
  echo "GATE: FAIL (stopped: the machine was strained three checks running: $why)"
  exit 1
fi
# The steps end saying PASS (0) or FAIL (1); anything else ended them before they could.
if [ $code -gt 1 ]; then
  echo "GATE: FAIL (it ended with exit $code before saying: its unit didn't start, or it was killed)"
fi
exit $code
