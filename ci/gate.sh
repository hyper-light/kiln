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
# and it yields to everything else on the machine: CPUWeight 1, nice 19, and I/O bandwidth caps on
# the disk its build is on (--io-read, --io-write; --io off drops them). Its CPUs are what it runs
# at once: a rustc per 6 GiB asked, at most one per CPU (--jobs), and as many test threads
# (--test-threads). No CPU quota: one changes what the tests see of the machine (the CPUs a runner
# reports, W13), as a runner never does. Without a systemd user manager (macOS, a container) it
# runs uncapped at nice 19, and says so.
#
# While it runs it watches the machine's own services: system.slice's I/O pressure ("full" avg10)
# over 30, or --guard-url answering in over 3 s, three checks running (10 s apart), stops it. The
# containers its own tests start in Docker are left out: their scopes are in system.slice too, and
# tests/pools.rs' burst of them alone took it past 30 (fedora and adacachy, 2026-10-28).
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
dry=false cpus='' mem='' jobs='' threads='' io=on io_read=400M io_write=150M guard_url=${KILN_GATE_GUARD_URL:-} cross=auto
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
if [ "$io" != off ]; then
  # A path: systemd caps the whole disk it's on.
  props+=(-p "IOReadBandwidthMax=$base $io_read" -p "IOWriteBandwidthMax=$base $io_write")
fi

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
echo "guard: system.slice I/O pressure (full avg10) over 30, its own tests' containers left out${guard_url:+, or $guard_url over 3 s}, three checks running, stops it"
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

systemd-run --user --unit "$unit" --collect --wait --pipe --quiet --working-directory="$here" "${props[@]}" "${inner[@]}" &
pid=$!
stop() { systemctl --user stop "$unit" 2> /dev/null || true; }
trap 'stop; exit 143' TERM INT HUP
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
bad=0 why=''
while kill -0 "$pid" 2> /dev/null; do
  sleep 10
  strained=''
  if [ -r /sys/fs/cgroup/system.slice/io.pressure ]; then
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
if [ -n "$why" ]; then
  echo "GATE: FAIL (stopped: the machine was strained three checks running: $why)"
  exit 1
fi
# The steps end saying PASS (0) or FAIL (1); anything else ended them before they could.
if [ $code -gt 1 ]; then
  echo "GATE: FAIL (it ended with exit $code before saying: its unit didn't start, or it was killed)"
fi
exit $code
