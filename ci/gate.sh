#!/usr/bin/env bash
# kiln's gate, as steps the agent that owns a change composes that change's gate from (AGENTS.md,
# "Your change's gate"; W05-R03): each step alone, natively on the machine it runs on (Linux, macOS,
# Windows), scoped to crates, and held to what its run asked for. A gate is the runs its change
# needs, side by side, each asking kiln for a machine with what it needs (never naming one).
# ci/gates.sh starts them, waits for each, and passes only when every one did:
#
#   ci/gates.sh --needs linux/amd64,cpus=<N>,mem=<M> --since origin/main \
#               --needs macos,cpus=<N>,mem=<M> clippy --since origin/main
#   ci/gate.sh --since origin/main --dry-run    # its scope and why, its steps and caps; runs nothing
#
# Its steps, named first (both, in this order, when none is): clippy, `cargo clippy --all-targets`
# with warnings denied; and test, the libraries' tests built first (kiln's crate compiled twice at
# once, as itself and as its tests, is what runs a build out of memory), then every test. Its scope:
# -p <crate> (again for each crate), and --since <rev>: what changed since rev's merge base with
# HEAD, committed or not, untracked too, as docs/ci.md's affected() has it (W24-R17): each changed
# file's crate; every crate below a changed Cargo.toml, Cargo.lock, .cargo/ or rust-toolchain that's
# in no crate; the crates whose code reads a changed file by path, as a test reads a doc or .kiln/:
# a string literal in Rust, or a path in a script a test runs, naming the file or a folder above it
# (comments, data and prose don't read; a `format!` path counts when it's rooted, `{root}/…`, or
# climbs out of its crate, `../`), or naming a changed crate's manifest outside include_str!, as code
# that builds that crate by path rather than as a Cargo dependency does; and every crate that
# depends on one of those (cargo tree -i). It says which crates, and why. A changed file it can't
# place (in no crate, and read by none) puts the whole workspace in scope, and it says so; so does
# naming no scope at all. Only a change that's empty leaves nothing to check.
#
# It holds itself to what it was asked, the CPUs and the memory: the needs `kiln run --needs` gives
# the command (KILN_NEEDS), or --cpus and --mem. It has no size of its own: asked neither, it says
# how to ask and runs nothing. As a systemd user unit, its memory is capped at what it asked
# (MemoryMax), and it yields to everything else on the machine: CPUWeight 1, nice 19, and I/O
# bandwidth caps on each disk its build is on (--io-read, --io-write; --io off drops them). Its CPUs
# are what it runs at once: a rustc per CPU asked (--jobs), and as many test threads
# (--test-threads). No CPU quota: one changes what the tests see of the machine (the CPUs a runner
# reports, W13), as a runner never does. Without a systemd user manager (macOS, Windows' Git Bash, a
# container) it runs uncapped at nice 19 (Windows' Idle priority), and says so. On Windows with no
# NASM, aws-lc-sys (kiln's cryptography) builds from the objects it ships (AWS_LC_SYS_PREBUILT_NASM=1).
#
# Each step says its peak memory, the measure the next ask follows: in its own unit, its anonymous
# memory (memory.stat's anon: memory.peak counts page cache too, which the kernel takes back as it
# needs); elsewhere its process tree's resident memory (ps: shared pages and mapped files count, so
# it's above what the step needs); on Windows none (Git Bash doesn't see cargo's own children).
# Each is sampled every 2 s (sample_every, below): a shorter spike can be missed, so it's a floor.
#
# While it runs in its unit it watches the services it shares the machine with, their I/O pressure
# ("full" avg10) over 30 (--guard-pressure), or --guard-url answering in over 3 s
# (--guard-url-time), three checks running (--guard-checks), 10 s apart, and it yields: it stops and
# says GATE: YIELDED (exit 75). Those numbers are still to derive (below, by io_read). Which: named
# with --guard-containers (KILN_GATE_GUARD_CONTAINERS), those containers alone (a machine's
# production, say); else kiln's own, the server and runner units kiln's installer makes (system or
# user), each its own; on a machine without kiln's units, system.slice's, with the containers its
# own tests start in Docker left out (their scopes are in system.slice too, and tests/pools.rs' burst
# of them alone took it past 30: fedora and adacachy, 2026-10-28). Stopped or ended, it removes what its tests left in Docker (a test server takes
# its containers and networks with it only when it ends): those mounting its run's folder, and those
# labelled with the hash of a folder in its run's TMPDIR, as kiln's runners label what they make.
#
# Its build is its own: a target dir per tree under ${XDG_CACHE_HOME:-~/.cache}/kiln-gate/<tree>
# (--target-dir names another: cargo's lock queues the builds on it), and a folder per run there,
# runs/<time>-<pid>, with its TMPDIR, git config, logs, verdict and memory samples, and a systemd
# unit named for it: two gates of one tree on one machine share nothing else. git's environment is
# cleared, with an empty global config of its own (writable: tests write it) and no system one, so a
# machine's signing or hooks reach no test. HOME stays the person's: tests start the coding agents
# installed there, signed in there. Each step's log is in its run's logs folder (it says where).
#
# The last line is its verdict, from what its steps wrote once every one had ended, never from an
# exit code alone: GATE: PASS (exit 0); GATE: FAIL and why (exit 1: a step failed, or the run ended
# before its steps said, as a kill or an OOM kill ends it); GATE: YIELDED and why (exit 75,
# EX_TEMPFAIL: its guard stopped it for the machine's sake: run it again later, or elsewhere);
# GATE: STOPPED (128 and the signal: stopped from outside). Exit 2: it couldn't start (what it was
# asked, or its scope, couldn't be worked out), and it says why.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
gate=$here/ci/gate.sh
# How often its memory is sampled: every 2 s, the period at which the kernel flushes memory.stat's
# counters (mm/memcontrol.c, FLUSH_TIME = 2 * HZ, Linux 5.13 on; between flushes a read can lag by
# up to a batch of pages per CPU). The process tree's resident memory, elsewhere, as often, alike.
sample_every=2

# ---------------------------------------------------------------- the steps, inside the unit
if [ "${1:-}" = --steps ]; then
  jobs=$2 threads=$3 run=$4 unit=$5 todo=$6
  # What's left is the scope, as cargo takes it: --workspace, or -p <crate> for each crate.
  shift 6
  cd "$here"
  export RUST_TEST_THREADS=$threads
  failed=()
  # Its memory, a sample in bytes: in its own unit, the unit's anonymous memory (memory.stat's anon,
  # page cache left out); elsewhere its process tree's resident memory (ps); on Windows, none.
  measure=tree what='resident memory' cg=''
  case $(uname -s) in MINGW* | MSYS* | CYGWIN*) measure=none ;; esac
  if [ "$unit" != - ] && [ -r /proc/self/cgroup ]; then
    cg=$(awk -F: '$1 == "0" { print $3 }' /proc/self/cgroup)
    case $cg in */"$unit.service") if [ -r "/sys/fs/cgroup$cg/memory.stat" ]; then measure=anon what='anonymous memory'; fi ;; esac
  fi
  sample() {
    if [ $measure = anon ]; then
      awk '$1 == "anon" { print $2; exit }' "/sys/fs/cgroup$cg/memory.stat"
    else
      ps -A -o pid= -o ppid= -o rss= | awk -v root=$$ '{ up[$1] = $2; kib[$1] = $3 }
        END { for (p in up) { q = p; while ((q in up) && q != root && q > 1) q = up[q]; if (q == root) n += kib[p] } print n * 1024 }'
    fi
  }
  samples=$run/memory
  sampler=''
  if [ $measure != none ]; then
    (
      while kill -0 $$ 2> /dev/null; do
        b=$(sample 2> /dev/null) || b=''
        [ -z "$b" ] || echo "$(date +%s) $b" >> "$samples"
        sleep "$sample_every"
      done
    ) > /dev/null 2>&1 &
    sampler=$!
  fi
  # The most memory a sample since $1 (in seconds since the epoch) said: ", <what> peak <n> GiB".
  peak() {
    [ -s "$samples" ] || return 0
    awk -v t0="$1" -v what="$what" '$1 >= t0 && $2 > m { m = $2 } END { if (m > 0) printf ", %s peak %.1f GiB", what, m / 1073741824 }' "$samples" || true
  }
  step() {
    local name=$1 t0 code=0
    t0=$(date +%s)
    shift
    echo "::: $name: $*"
    "$@" > "$run/logs/$name.log" 2>&1 || code=$?
    if [ $code -eq 0 ]; then
      echo "    passed in $(($(date +%s) - t0)) s$(peak "$t0")"
      return 0
    fi
    echo "    FAILED (exit $code) in $(($(date +%s) - t0)) s$(peak "$t0"): $run/logs/$name.log"
    # What failed: the errors, and each failing test with how to run it again alone.
    awk '/^ *Running .*\(.*\)$/ { bin = $NF; gsub(/[()]/, "", bin) }
         /^error(\[E[0-9]+\])?:/ && n < 20 { print "    | " $0; n++ }
         /^test .* \.\.\. FAILED$/ && n < 40 { print "    | " $2 " (alone: " bin " --exact " $2 ")"; n++ }' "$run/logs/$name.log" || true
    failed+=("$name")
  }
  case ,$todo, in *,clippy,*) step clippy cargo clippy --locked "$@" --all-targets -j "$jobs" -- -D warnings ;; esac
  case ,$todo, in
    *,test,*)
      # The libraries as their tests first, then the rest (.kiln/ci.yaml): kiln's crate compiled
      # twice at once (as itself and as its tests) is what runs a build out of memory.
      step test-build cargo test --locked "$@" --lib --no-run -j "$jobs"
      step test cargo test --locked "$@" --no-fail-fast -j "$jobs"
      ;;
  esac
  if [ -n "$sampler" ]; then kill "$sampler" 2> /dev/null || true; wait "$sampler" 2> /dev/null || true; fi
  # Its verdict, which ci/gate.sh outside requires: written whole (renamed into place) once every
  # step has ended, so a run that ends any other way (killed, stopped, out of memory) leaves none.
  if [ ${#failed[@]} -eq 0 ]; then echo PASS > "$run/verdict.part"; else echo "FAIL ${failed[*]}" > "$run/verdict.part"; fi
  mv "$run/verdict.part" "$run/verdict"
  if [ ${#failed[@]} -eq 0 ]; then exit 0; fi
  exit 1
fi

# ---------------------------------------------------------------- what it was asked
# Constants still to derive (W05-R03): nothing measured or cited stands behind these yet, so each is
# a number to replace, not to tune, and a setting the person can pass in until then. What each acts
# on, and the data that could derive it:
# - io_read=400M, io_write=150M (--io-read, --io-write): the unit's I/O bandwidth caps on the disk
#   its build is on. From that disk's own bandwidth (fio on it) and what the machine's services need
#   of it while a gate runs: their io.stat and their probes' latency (docs/headroom.md's targets),
#   with a gate running and without.
# - guard_full=30 (--guard-pressure): the I/O pressure ("full" avg10, percent) of system.slice, or of
#   a container named, that counts as strained. From production's PSI record set against when its
#   probes (a sync in its data folder, its server's health) went past their tolerance.
# - guard_runs=3 (--guard-checks): strained checks in a row before it yields. From how long strained
#   stretches lasted in that record: those that passed on their own against those that harmed a probe.
# - guard_url_s=3 (--guard-url-time): how long --guard-url may take to answer before it counts as
#   strained. From that URL's own latency record, calm against strained (kiln's headroom takes
#   Kubernetes' 1 s probe timeout where a target declares none, docs/headroom.md).
# Its checks are 10 s apart (guard_every): avg10's own window (the kernel's
# Documentation/accounting/psi.rst), so each reads pressure the last didn't. How many in a row
# (guard_runs) is what's left to derive of that.
io_read=400M io_write=150M guard_full=30 guard_runs=3 guard_url_s=3 guard_every=10
dry=false cpus='' mem='' jobs='' threads='' io=on guard_url=${KILN_GATE_GUARD_URL:-} guard_containers=${KILN_GATE_GUARD_CONTAINERS:-} since='' target_dir=''
wanted=() crates=()
# An option's value: there, not empty, and not the next option (W05-R03: `-p ""` scoped nothing and passed).
value() {
  if [ $# -lt 2 ]; then echo "ci/gate.sh: $1 takes a value (--help says what it takes)" >&2; exit 2; fi
  case $2 in '' | -*) echo "ci/gate.sh: $1 takes a value, not ${2:-an empty one} (--help says what it takes)" >&2; exit 2 ;; esac
}
whole() { case $2 in *[!0-9]* | 0*) echo "ci/gate.sh: $1 takes a whole number from 1 up, not $2" >&2; exit 2 ;; esac; }
number() { awk -v v="$2" 'BEGIN { exit !(v ~ /^[0-9]+(\.[0-9]+)?$/ && v > 0) }' || { echo "ci/gate.sh: $1 takes a number above 0, not $2" >&2; exit 2; }; }
while [ $# -gt 0 ]; do
  case $1 in
    clippy | test) wanted+=("$1") ;;
    -p | --package)
      value "$@"
      case $2 in *[[:space:]]*) echo "ci/gate.sh: $1 takes a crate, not '$2'" >&2; exit 2 ;; esac
      crates+=("$2"); shift ;;
    --since) value "$@"; since=$2; shift ;;
    --dry-run) dry=true ;;
    --cpus) value "$@"; cpus=$2; shift ;;
    --mem) value "$@"; mem=$2; shift ;;
    --jobs) value "$@"; whole "$@"; jobs=$2; shift ;;
    --test-threads) value "$@"; whole "$@"; threads=$2; shift ;;
    --target-dir) value "$@"; target_dir=$2; shift ;;
    --io) value "$@"; io=$2; shift ;;
    --io-read) value "$@"; io_read=$2; shift ;;
    --io-write) value "$@"; io_write=$2; shift ;;
    --guard-url) value "$@"; guard_url=$2; shift ;;
    --guard-containers) value "$@"; guard_containers=$2; shift ;;
    --guard-pressure) value "$@"; number "$@"; guard_full=$2; shift ;;
    --guard-checks) value "$@"; whole "$@"; guard_runs=$2; shift ;;
    --guard-url-time) value "$@"; number "$@"; guard_url_s=$2; shift ;;
    # Each platform's steps run on that platform: there's no cross-compiling left to turn off.
    --no-cross) ;;
    -h | --help) sed -n '2,/^set -euo pipefail/p' "$gate" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) echo "ci/gate.sh: no such step or option $1 (--help says what it takes)" >&2; exit 2 ;;
  esac
  shift
done
todo=''
for s in clippy test; do
  case " ${wanted[*]-} " in *" $s "*) todo=${todo:+$todo,}$s ;; esac
done
todo=${todo:-clippy,test}
asked_by="--cpus and --mem"
for w in $(printf '%s' "${KILN_NEEDS:-}" | tr ', ' '\n\n'); do
  case $w in
    cpus=* | cpu=*) [ -n "$cpus" ] || { cpus=${w#*=}; asked_by="KILN_NEEDS=$KILN_NEEDS"; } ;;
    mem=* | memory=*) [ -n "$mem" ] || { mem=${w#*=}; asked_by="KILN_NEEDS=$KILN_NEEDS"; } ;;
  esac
done
# What it wasn't asked it doesn't guess: without both, it runs nothing (a dry run says so).
unasked=''
[ -n "$cpus" ] || unasked="the CPUs (cpus=)"
[ -n "$mem" ] || unasked="${unasked:+$unasked and }the memory (mem=)"
cpu_n='' mem_cap=''
if [ -n "$cpus" ]; then
  # CPUs as kiln writes them (8, 0.5, 500m).
  cpu_n=$(awk -v c="$cpus" 'BEGIN { if (c ~ /m$/) { sub(/m$/, "", c); c /= 1000 } if (c !~ /^[0-9.]+$/ || c <= 0) exit 1; printf "%g", c }') ||
    { echo "ci/gate.sh: $cpus is not a number of CPUs (8, 0.5, 500m)" >&2; exit 2; }
  # A rustc per CPU asked, a whole one at least.
  jobs=${jobs:-$(awk -v c="$cpu_n" 'BEGIN { j = int(c); if (j < c) j++; print j }')}
fi
if [ -n "$mem" ]; then
  # Memory in systemd's binary units (16G, 512M).
  mem_cap=$(printf '%s' "$mem" | tr '[:lower:]' '[:upper:]' | sed -E 's/I?B?$//')
  printf '%s' "$mem_cap" | grep -Eq '^[0-9]+(\.[0-9]+)?[KMGT]?$' || { echo "ci/gate.sh: $mem is not a size (16g, 512m)" >&2; exit 2; }
fi
threads=${threads:-$jobs}
case $(uname -s) in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  MINGW* | MSYS* | CYGWIN*) os=windows ;;
  *) os=$(uname -s) ;;
esac

# ---------------------------------------------------------------- where it builds
real_home=$HOME
key=$(printf '%s' "$here" | { sha256sum 2> /dev/null || shasum -a 256; } | cut -c1-12)
tree_dir=${KILN_GATE_DIR:-${XDG_CACHE_HOME:-$real_home/.cache}/kiln-gate/$key}
target_dir=${target_dir:-$tree_dir/target}
# This run's own folder and unit, by when it started and its PID (no two processes running at once
# share one): two gates of one tree on one machine share the target dir alone (W05-R03).
id=$(date -u +%Y%m%dT%H%M%SZ)-$$
run=$tree_dir/runs/$id
unit=kiln-gate-$key-$id
cargo_home=${CARGO_HOME:-$real_home/.cargo}
rustup_home=${RUSTUP_HOME:-$real_home/.rustup}
path=$cargo_home/bin:$real_home/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH
platform_env=()
if [ "$os" = windows ]; then
  # Windows' own programs (cargo, rustc, the tests) take their temporary folder from TMP and TEMP.
  platform_env+=(TMP="$run/tmp" TEMP="$run/tmp")
  # aws-lc-sys assembles with NASM on x86-64 Windows; with none, from the objects it ships.
  if [ -z "${AWS_LC_SYS_PREBUILT_NASM:-}" ] && ! PATH=$path command -v nasm > /dev/null; then platform_env+=(AWS_LC_SYS_PREBUILT_NASM=1); fi
fi

# ---------------------------------------------------------------- what it checks
# The crates a change since rev $1 affects, as lines "S<tab><crate><tab><why>", "N<tab><note>",
# "C<tab><how many files changed>", and "W<tab><why>" when the whole workspace is in scope; it fails
# (2, saying why) when any of that can't be worked out. As docs/ci.md's affected() (W24-R17): each
# changed file's crate (the package whose folder holds it most closely; vendored ones pass it on),
# every package below a changed Cargo.toml, Cargo.lock, .cargo/ or rust-toolchain that's in no
# package, the crates whose code reads a changed file by path, and every workspace crate that depends
# on one of those (cargo tree -i, its dev and build dependencies and every platform's). A changed
# file in no package that no crate reads can't be placed: the whole workspace.
affected() {
  local rev=$1 mb diffed untracked files count root members pkgs where hits reads unplaced seeds deps rc p d s pats dirs
  pats=() dirs=()
  mb=$(git -C "$here" merge-base "$rev" HEAD) || { echo "ci/gate.sh: --since $rev: git found no merge base of it and HEAD here" >&2; return 2; }
  diffed=$(git -C "$here" -c core.quotepath=off diff --name-only --no-renames "$mb" --) || { echo "ci/gate.sh: --since $rev: git diff failed" >&2; return 2; }
  untracked=$(git -C "$here" -c core.quotepath=off ls-files --others --exclude-standard) || { echo "ci/gate.sh: --since $rev: git ls-files failed" >&2; return 2; }
  files=$(printf '%s\n%s\n' "$diffed" "$untracked" | awk 'NF' | sort -u)
  count=$(printf '%s\n' "$files" | awk 'NF { n++ } END { print n + 0 }')
  printf 'C\t%s\n' "$count"
  printf 'N\t%s %s changed since %s (%.12s), committed or not\n' "$count" "$([ "$count" = 1 ] && echo file || echo files)" "$rev" "$mb"
  [ "$count" != 0 ] || return 0
  root=$(cd "$here" && PATH=$path cargo locate-project --workspace --message-format plain | tr '\\' '/') || { echo "ci/gate.sh: --since: cargo locate-project failed" >&2; return 2; }
  root=${root%/Cargo.toml}
  members=$(cd "$here" && PATH=$path cargo tree --locked --workspace --depth 0 --prefix none --format '{p}' -e normal | awk 'NF { print $1 }') || { echo "ci/gate.sh: --since: cargo tree failed, listing the workspace's crates" >&2; return 2; }
  # Every package of the build in this tree: "<name><tab><version><tab><folder from the root><tab>member|dep".
  pkgs=$(cd "$here" && PATH=$path cargo tree --locked --workspace --target all -e normal,build,dev --prefix none --format '{p}') || { echo "ci/gate.sh: --since: cargo tree failed, listing the build's packages" >&2; return 2; }
  pkgs=$(printf '%s\n' "$pkgs" | tr '\\' '/' | M=$(printf '%s ' $members) awk -v root="$root" '
    BEGIN { r = tolower(root); n = split(ENVIRON["M"], m, " "); for (i = 1; i <= n; i++) member[m[i]] = 1 }
    { l = tolower($0); dir = ""
      if (index(l, "(" r ")")) dir = "."
      else if ((i = index(l, "(" r "/")) > 0) { rest = substr($0, i + length(r) + 2); j = index(rest, ")"); if (!j) next; dir = substr(rest, 1, j - 1) }
      else next
      v = $2; sub(/^v/, "", v)
      if (!seen[$1 "@" v]++) print $1 "\t" v "\t" dir "\t" (($1 in member) ? "member" : "dep") }')
  # Each changed file: "own" (in a package, and its folder), "input" (one per package below a Cargo
  # input), or "none" (in no package, or a Cargo input with no package of this build below it).
  where=$(printf '%s\n' "$files" | P=$pkgs awk '
    BEGIN { n = split(ENVIRON["P"], L, "\n"); for (i = 1; i <= n; i++) { split(L[i], f, "\t"); name[i] = f[1]; spec[i] = f[1] "@" f[2]; dir[i] = f[3]; kind[i] = f[4] } }
    NF { file = $0; best = 0; bl = -1
      for (i = 1; i <= n; i++) { d = dir[i]; k = (d == ".") ? 0 : length(d); if ((d == "." || index(file, d "/") == 1) && k > bl) { best = i; bl = k } }
      if (best) { print "own\t" spec[best] "\t" name[best] "\t" kind[best] "\t" file "\t" dir[best]; next }
      base = file; sub(/.*\//, "", base); folder = file; if (!sub(/\/[^\/]*$/, "", folder)) folder = ""
      cargo = 0
      if (index(file, ".cargo/") == 1) { cargo = 1; folder = "" }
      else if ((p = index(file, "/.cargo/")) > 0) { cargo = 1; folder = substr(file, 1, p - 1) }
      if (cargo || base == "Cargo.toml" || base == "Cargo.lock" || base ~ /^rust-toolchain/) {
        below = 0
        for (i = 1; i <= n; i++) if (folder == "" || dir[i] == folder || index(dir[i], folder "/") == 1) { print "input\t" spec[i] "\t" name[i] "\t" kind[i] "\t" file; below++ }
        if (below) next }
      print "none\t\t\t\t" file }')
  printf '%s\n' "$where" | awk -F'\t' '$1 == "own" && $4 == "member" && !c[$3]++ { first[$3] = $5; own[++n] = $3 }
    $1 == "own" && $4 == "member" { more[$3] = c[$3] - 1 }
    $1 == "input" && $4 == "member" && !seen[$3]++ { input[++m] = $3; by[$3] = $5 }
    END { for (i = 1; i <= n; i++) { x = own[i]; print "S\t" x "\tchanged: " first[x] (more[x] ? " and " more[x] " more" : "") }
      for (i = 1; i <= m; i++) { x = input[i]; print "S\t" x "\t" by[x] " changed, a Cargo input of every crate below it" } }'
  # What the workspace's code reads by path: one search of its crates for the names a path to a
  # changed file has in it (a Cargo input aside), then each line found read as code. A file in no
  # package: its own name and each folder's above it. A file in a package: its package's folder's
  # name, which every path into that package from another has in it. A name with no dot in it can
  # only be part of a path (below, a word with a / or a . in it) beside a /, so it's looked for so.
  while IFS= read -r p; do pats+=(-e "$p"); done < <(printf '%s\n' "$where" | awk -F'\t' '
    function name(b) { sub(/.*\//, "", b); if (seen[b]++) return; if (index(b, ".")) print b; else { print b "/"; print "/" b } }
    $1 == "own" && $6 != "." { name($6); next }
    $1 == "own" || $1 == "none" { f = $5; while (f != "") { name(f); if (!sub(/\/[^\/]*$/, "", f)) f = "" } }')
  while IFS= read -r d; do [ -z "$d" ] || dirs+=("$d"); done < <(printf '%s\n' "$pkgs" | awk -F'\t' '$4 == "member" { print $3 }')
  hits=''
  if [ ${#pats[@]} -gt 0 ] && [ ${#dirs[@]} -gt 0 ]; then
    rc=0
    hits=$(cd "$here" && git -c core.quotepath=off grep --no-color -n -I -F --untracked "${pats[@]}" -- "${dirs[@]}") || rc=$?
    # 1 is git grep finding nothing; more is git failing.
    if [ $rc -gt 1 ]; then echo "ci/gate.sh: --since: git grep failed (exit $rc), looking for the code that reads what changed" >&2; return 2; fi
  fi
  # Each line found, read as code: a string literal in Rust (comments and doc comments skipped), any
  # word in a script (sh, bash, Python, Ruby, Perl; after a # comment's start); nothing in any other
  # file, data and prose (a fixture naming a path reads nothing), nor in a crate's manifest (its paths
  # are Cargo's dependencies, which cargo tree has). A path in it names what it resolves to from the
  # line's file, its crate's folder or the workspace's: a changed file, or a folder above one; or a
  # changed crate's manifest, outside include_str! and include_bytes!, as code that builds that crate
  # does. "R<tab>reader<tab>spec<tab>file<tab>where<tab>what it names".
  reads=$(printf '%s\n' "$hits" | P=$pkgs awk -F'\t' -v q="'" '
    function norm(b, t,    s, parts, n, i, k, st, out) {
      s = (b == "") ? t : b "/" t
      n = split(s, parts, "/"); k = 0
      for (i = 1; i <= n; i++) {
        if (parts[i] == "" || parts[i] == ".") continue
        if (parts[i] == "..") { if (k == 0) return ""; k--; continue }
        st[++k] = parts[i]
      }
      out = ""
      for (i = 1; i <= k; i++) out = (i == 1) ? st[i] : out "/" st[i]
      return out
    }
    function emit(list, r,    m, i, a) {
      m = split(list, a, SUBSEP)
      for (i = 2; i <= m; i++) if (owner[a[i]] != rname && !seen[rname SUBSEP a[i] SUBSEP (r == a[i])]++) print "R\t" rname "\t" rspec "\t" a[i] "\t" at "\t" r
    }
    function hit(r) {
      if (r == "") return
      if (r in named) emit(named[r], r)
      if (!embed && (r in manifest)) emit(manifest[r], r)
    }
    function globre(p,    i, n, c, out) {
      out = "^"; n = length(p)
      for (i = 1; i <= n; i++) { c = substr(p, i, 1); if (c == "*") out = out "[^/]*"; else if (c == "?") out = out "[^/]"; else if (index("\\.[]^$|()+{}", c)) out = out "\\" c; else out = out c }
      return out "$"
    }
    function globhit(r,    re, key) {
      if (r == "") return
      re = globre(r)
      for (key in named) if (key ~ re) emit(named[key], r)
      if (!embed) for (key in manifest) if (key ~ re) emit(manifest[key], r)
    }
    # Rust: the string literals of a line, raw and byte strings too, outside comments.
    function rust(s,    n, i, c, out, lit, h, j, k, rest, t) {
      t = s; sub(/^[ \t]+/, "", t)
      if (t ~ /^(\/\/|\/\*|\*)/) return ""
      n = length(s); i = 1; out = ""
      while (i <= n) {
        c = substr(s, i, 1)
        if (c == "/" && substr(s, i + 1, 1) == "/") break
        if (c == "/" && substr(s, i + 1, 1) == "*") { j = index(substr(s, i + 2), "*/"); if (!j) break; i += j + 3; continue }
        if (c == q) {
          if (substr(s, i + 1, 1) == "\\") { j = index(substr(s, i + 3), q); i += j ? j + 3 : n; continue }
          if (substr(s, i + 2, 1) == q) { i += 3; continue }
          i++; continue
        }
        if (c == "r" && (i == 1 || substr(s, i - 1, 1) !~ /[A-Za-z0-9_]/ || (substr(s, i - 1, 1) == "b" && (i == 2 || substr(s, i - 2, 1) !~ /[A-Za-z0-9_]/)))) {
          j = i + 1; h = ""
          while (substr(s, j, 1) == "#") { h = h "#"; j++ }
          if (substr(s, j, 1) == "\"") {
            rest = substr(s, j + 1); k = index(rest, "\"" h)
            if (!k) { out = out "\n" rest; break }
            out = out "\n" substr(rest, 1, k - 1); i = j + k + length(h) + 1; continue
          }
        }
        if (c == "\"") {
          lit = ""; i++
          while (i <= n) { c = substr(s, i, 1); if (c == "\\") { lit = lit substr(s, i, 2); i += 2; continue } if (c == "\"") break; lit = lit c; i++ }
          out = out "\n" lit; i++; continue
        }
        i++
      }
      return out
    }
    # A line of a language whose comments start with #, without its comment.
    function nohash(s,    n, i, c, inq) {
      n = length(s); inq = ""
      for (i = 1; i <= n; i++) { c = substr(s, i, 1); if (inq != "") { if (c == inq) inq = ""; continue } if (c == "\"" || c == q) { inq = c; continue } if (c == "#") return substr(s, 1, i - 1) }
      return s
    }
    BEGIN {
      delim = "[][ \t\n\"" q "=:,;()<>|&$`]+"
      n = split(ENVIRON["P"], L, "\n")
      for (i = 1; i <= n; i++) { split(L[i], f, "\t"); if (f[4] == "member") { k++; mn[k] = f[1]; ms[k] = f[1] "@" f[2]; md[k] = f[3] } }
    }
    # The change: each file in a package or in none, named by itself and each folder above it (in a
    # package, up to the package folder; its manifest names all of it, save to include_str!).
    FNR == NR {
      if ($1 != "own" && $1 != "none") next
      F = $5; owner[F] = ($1 == "own") ? $3 : ""; top = ($1 == "own" && $6 != ".") ? $6 : ""
      p = F
      while (p != "") { named[p] = named[p] SUBSEP F; if (p == top || !sub(/\/[^\/]*$/, "", p)) p = "" }
      if (top != "") { m = top "/Cargo.toml"; manifest[m] = manifest[m] SUBSEP F }
      next
    }
    # A line found: "<path>:<line>:<text>", read by the member whose folder holds it most closely.
    {
      if (!match($0, /^[^:]+:[0-9]+:/)) next
      at = substr($0, 1, RLENGTH - 1); text = substr($0, RLENGTH + 1); sub(/\r$/, "", text)
      path = at; sub(/:[0-9]+$/, "", path)
      best = 0; bl = -1
      for (i = 1; i <= k; i++) { d = md[i]; w = (d == ".") ? 0 : length(d); if ((d == "." || index(path, d "/") == 1) && w > bl) { best = i; bl = w } }
      if (!best) next
      rname = mn[best]; rspec = ms[best]; mdir = (md[best] == ".") ? "" : md[best]
      dir = path; if (!sub(/\/[^\/]*$/, "", dir)) dir = ""
      base = path; sub(/.*\//, "", base)
      embed = index(text, "include_str!") || index(text, "include_bytes!")
      if (base ~ /\.rs$/) { if (!index(text, "\"")) next; text = rust(text) }
      else if (base ~ /\.(sh|bash|py|rb|pl)$/) text = nohash(text)
      else next
      nw = split(text, words, delim)
      for (j = 1; j <= nw; j++) {
        t = words[j]; rooted = 0
        if (t ~ /^\{[^}]*\}/) { sub(/^\{[^}]*\}/, "", t); rooted = 1 }
        if (t !~ /[\/.]/ || t ~ /^[.\/]*$/) continue
        # A path with a placeholder or a glob in it: a read only when rooted or climbing out of its
        # crate; format!("crates/{name}") builds a name, as tests building trees of their own do.
        if (t ~ /[{*?]/) {
          if (!rooted && t !~ /(^|\/)\.\.(\/|$)/) continue
          gsub(/\{[^}]*\}/, "*", t)
          globhit(norm(dir, t)); globhit(norm(mdir, t)); globhit(norm("", t))
        } else { hit(norm(dir, t)); hit(norm(mdir, t)); hit(norm("", t)) }
      }
    }' <(printf '%s\n' "$where") -) || { echo "ci/gate.sh: --since: reading the paths in its crates' code failed" >&2; return 2; }
  # Each reader once, by what it reads: a line naming a changed file itself before one naming a folder.
  printf '%s\n' "$reads" | awk -F'\t' '$1 == "R" && !c[$2]++ { first[$2] = $4 " (" $5 " names " $6 ")"; order[++k] = $2 }
    $1 == "R" && $6 == $4 && !exact[$2]++ { first[$2] = $4 " (" $5 " names " $6 ")" }
    $1 == "R" && !d[$2 SUBSEP $4]++ { files[$2]++ }
    END { for (i = 1; i <= k; i++) { x = order[i]; print "S\t" x "\treads " first[x] (files[x] > 1 ? " and " files[x] - 1 " more" : "") } }'
  # A changed file it can't place, in no package and read by none: what it affects isn't known.
  unplaced=$(awk -F'\t' 'FNR == NR { if ($1 == "R") read[$4] = 1; next } $1 == "none" && !($5 in read) { printf "%s%s", (n++ ? ", " : ""), $5 }' <(printf '%s\n' "$reads") <(printf '%s\n' "$where")) ||
    { echo "ci/gate.sh: --since: placing the changed files failed" >&2; return 2; }
  if [ -n "$unplaced" ]; then
    printf 'N\tin no crate, and read by none: %s\n' "$unplaced"
    printf 'W\t%s\n' "what changed outside every crate, and is read by none, can't be placed"
  fi
  # What depends on them: each workspace crate in an inverted tree of one, and which it depends on.
  seeds=$( { printf '%s\n' "$where" | awk -F'\t' '$1 == "own" || $1 == "input" { print $2 }'; printf '%s\n' "$reads" | awk -F'\t' '$1 == "R" { print $3 }'; } | awk 'NF' | sort -u)
  [ -n "$seeds" ] || return 0
  set --
  for s in $seeds; do set -- "$@" -i "$s"; done
  deps=$(cd "$here" && PATH=$path cargo tree --locked --workspace --target all -e normal,build,dev --prefix depth --format '{p}' "$@") || { echo "ci/gate.sh: --since: cargo tree -i failed, finding what depends on the change" >&2; return 2; }
  printf '%s\n' "$deps" | P=$pkgs awk '
    BEGIN { n = split(ENVIRON["P"], L, "\n"); for (i = 1; i <= n; i++) { split(L[i], f, "\t"); if (f[4] == "member") member[f[1]] = 1 } }
    match($0, /^[0-9]+/) { d = substr($0, 1, RLENGTH) + 0; split(substr($0, RLENGTH + 1), a, " ")
      if (d == 0) { at = a[1]; next }
      if ((a[1] in member) && a[1] != at) print "S\t" a[1] "\tdepends on " at }'
}
scope=()
reasons='' whole='' changed=''
if [ -n "$since" ]; then
  reasons=$(affected "$since") || { echo "ci/gate.sh: its scope couldn't be worked out (above), so it checks nothing" >&2; exit 2; }
  whole=$(printf '%s\n' "$reasons" | awk -F'\t' '$1 == "W" { print $2; exit }')
  changed=$(printf '%s\n' "$reasons" | awk -F'\t' '$1 == "C" { print $2; exit }')
fi
names=$( { printf '%s\n' ${crates[@]+"${crates[@]}"}; printf '%s\n' "$reasons" | awk -F'\t' '$1 == "S" { print $2 }'; } | awk 'NF' | sort -u)
if [ -n "$whole" ] || { [ -z "$since" ] && [ ${#crates[@]} -eq 0 ]; }; then
  scope=(--workspace)
else
  for c in $names; do scope+=(-p "$c"); done
fi
# Nothing in scope passes only when nothing changed (W05-R03): anything else is a scope it lost.
if [ ${#scope[@]} -eq 0 ] && [ "$changed" != 0 ]; then
  echo "ci/gate.sh: nothing in scope, yet ${changed:-what} changed since ${since:-(no --since)}: it can't tell what that affects" >&2
  exit 2
fi

# ---------------------------------------------------------------- how it holds itself
systemd=false
if command -v systemd-run > /dev/null && systemctl --user show-environment > /dev/null 2>&1; then systemd=true; fi
props=(-p "MemoryMax=${mem_cap:-(the memory asked)}" -p CPUWeight=1 -p Nice=19)
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
  while read -r io_dev; do
    props+=(-p "IOReadBandwidthMax=$io_dev $io_read" -p "IOWriteBandwidthMax=$io_dev $io_write")
  done < <(io_disks "$target_dir")
fi

# What the guard watches: kiln's own services here, as its installer makes them (each unit says it's
# a "self-supervising CI/CD job server"), by their cgroups' I/O pressure; tests' units and transient
# ones (`systemd-run`, this gate's own) aren't. Unit and pressure file, one a line.
watched=''
if command -v systemctl > /dev/null; then
  for manager in --system --user; do
    for u in $(systemctl "$manager" list-units --type=service --state=running --no-legend --plain 'kiln*' 2> /dev/null | awk '{ print $1 }'); do
      case $u in kiln-test-* | kiln-gate-*) continue ;; esac
      systemctl "$manager" show -p Description --value "$u" 2> /dev/null | grep -q 'self-supervising CI/CD job server' || continue
      cg=$(systemctl "$manager" show -p ControlGroup --value "$u" 2> /dev/null || true)
      if [ -n "$cg" ] && [ -r "/sys/fs/cgroup$cg/io.pressure" ]; then watched="$watched$u /sys/fs/cgroup$cg/io.pressure"$'\n'; fi
    done
  done
fi

caps=$(printf '%s ' "${props[@]#-p}" | sed 's/  */ /g; s/^ //; s/ $//')
echo "ci/gate.sh: the gate of $here at $(git -C "$here" rev-parse --short HEAD 2> /dev/null || echo '(not a git tree)'), natively on $os/$(uname -m)"
if [ -n "$unasked" ]; then
  echo "asked: not $unasked: ask what this step needs (cpus= and mem= in kiln run --needs, or --cpus and --mem), the peak it said last time; it runs nothing until then"
else
  echo "asked: $(awk -v c="$cpu_n" 'BEGIN { printf "%g CPU%s", c, (c == 1) ? "" : "s" }'), $mem_cap of memory (from $asked_by)"
fi
if $systemd; then
  echo "caps: $caps (systemd user unit $unit)"
else
  echo "caps: $caps would apply, but this machine has no systemd user manager: it runs uncapped, at nice 19$([ "$os" != windows ] || echo " (Idle priority)")"
fi
echo "cargo: -j ${jobs:-(a rustc per CPU asked)}, ${threads:-as many} test threads"
[ ${#platform_env[@]} -eq 0 ] || echo "env: ${platform_env[*]}"
if $systemd; then
  echo "memory: each step's peak anonymous memory in its unit (memory.stat's anon: page cache left out), sampled every $sample_every s"
elif [ "$os" = windows ]; then
  echo "memory: none measured: from Git Bash, cargo's own children (rustc, the tests) and their memory aren't visible"
else
  echo "memory: each step's peak resident memory of its process tree (ps: shared pages and mapped files count, so it's above what it needs), sampled every $sample_every s"
fi
if ! $systemd; then
  echo "guard: none (no systemd here: no cgroups of its own to watch from)"
else
  guard_words="system.slice's I/O pressure (full avg10), its own tests' containers left out,"
  if [ -n "$guard_containers" ]; then
    guard_words="the I/O pressure (full avg10) of ${guard_containers//,/, }"
  elif [ -n "$watched" ]; then
    guard_words="the I/O pressure (full avg10) of kiln's services here ($(printf '%s' "$watched" | awk '{ print $1 }' | paste -sd, - | sed 's/,/, /g'))"
  fi
  echo "guard: $guard_words over $guard_full${guard_url:+, or $guard_url over $guard_url_s s}, $guard_runs check$([ "$guard_runs" = 1 ] || echo s) running ($guard_every s apart), yields (GATE: YIELDED, exit 75); its numbers are still to derive (ci/gate.sh says from what)"
fi
if [ "${scope[0]-}" = --workspace ]; then
  echo "scope: the workspace, every crate"
elif [ ${#scope[@]} -eq 0 ]; then
  echo "scope: none: nothing changed since $since"
else
  echo "scope: $(printf '%s ' $names | sed 's/ $//')"
fi
{ printf '%s\n' "$reasons"; for c in ${crates[@]+"${crates[@]}"}; do printf 'S\t%s\tasked for (-p)\n' "$c"; done; } |
  awk -F'\t' '$1 == "N" { print "  " $2 } $1 == "W" { print "  the whole workspace: " $2 } $1 == "S" && !seen[$2]++ { print "  " $2 ": " $3 }'
if [ ${#scope[@]} -eq 0 ]; then
  echo "steps: none, with nothing in scope"
else
  echo "steps:"
  n=0
  j=${jobs:-<N>} t=${threads:-<N>}
  case ,$todo, in *,clippy,*) n=$((n + 1)); echo "  $n. clippy: cargo clippy --locked ${scope[*]} --all-targets -j $j -- -D warnings" ;; esac
  case ,$todo, in *,test,*) n=$((n + 1)); echo "  $n. test: cargo test --locked ${scope[*]} --lib --no-run -j $j, then cargo test --locked ${scope[*]} --no-fail-fast -j $j (RUST_TEST_THREADS=$t)" ;; esac
fi
echo "build: $target_dir; this run's TMPDIR, git config and logs in $run"
if $dry; then exit 0; fi
if [ -n "$unasked" ]; then
  echo "ci/gate.sh: not asked $unasked this step needs: it has no size of its own, and runs nothing" >&2
  exit 2
fi
if [ ${#scope[@]} -eq 0 ]; then
  echo "GATE: PASS (nothing changed since $since: nothing to check)"
  exit 0
fi

# ---------------------------------------------------------------- the run
mkdir -p "$target_dir" "$tree_dir/runs"
if ! mkdir "$run" 2> /dev/null; then echo "ci/gate.sh: $run is there already: another run took its name" >&2; exit 2; fi
mkdir "$run/tmp" "$run/logs"
: > "$run/gitconfig"
env_vars=(HOME="$real_home" GIT_CONFIG_GLOBAL="$run/gitconfig" GIT_CONFIG_NOSYSTEM=1 CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" PATH="$path" CARGO_TARGET_DIR="$target_dir"
  TMPDIR="$run/tmp" CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_TERM_COLOR=never ${platform_env[@]+"${platform_env[@]}"})
inner_unit=-
if $systemd; then inner_unit=$unit; fi
inner=(env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE "${env_vars[@]}" bash "$gate" --steps "$jobs" "$threads" "$run" "$inner_unit" "$todo" "${scope[@]}")

# A cgroup's I/O pressure: its "full" avg10.
pressure() { awk '/^full/ { split($2, a, "="); print a[2] }' "$1/io.pressure" 2> /dev/null || true; }
# The Docker scopes of the containers this run's tests made. A test's kiln server labels all it
# makes kiln.runner-work, the start of the sha256 of its runner's work folder
# (runner::docker::owner_label), and those folders are under this run's TMPDIR (kiln's tests take
# theirs from TMPDIR; the target dir's tmp is shared by every run of the tree, so it isn't looked in).
own_scopes() {
  command -v docker > /dev/null || return 0
  local works
  works=$(find "$run/tmp" -maxdepth 3 -type d -name work 2> /dev/null | while read -r d; do printf '%s' "$d" | { sha256sum 2> /dev/null || shasum -a 256; } | cut -c1-16; done || true)
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

# What its tests left in Docker: containers mounting its run's folder, and containers and networks
# labelled kiln.runner-work=<h> or kiln.socket.runner.<h>, h the hash of a folder in its TMPDIR (a
# test server's work dir, a test's own). Its best: with Docker out of reach it removes nothing, and
# the verdict stands.
sweep() {
  command -v docker > /dev/null 2>&1 || return 0
  local hashes containers networks
  hashes=$(mktemp)
  find "$run/tmp" -mindepth 1 -maxdepth 3 -type d 2> /dev/null | while read -r d; do printf '%s' "$d" | { sha256sum 2> /dev/null || shasum -a 256; } | cut -c1-16; done | sort -u > "$hashes" || true
  containers=$(docker ps -aq --no-trunc 2> /dev/null | xargs -r docker inspect -f '{{.Id}} {{range .Mounts}}{{.Source}} {{end}}| {{range $k, $v := .Config.Labels}}{{$k}}={{$v}} {{end}}' 2> /dev/null |
    awk -v b="$run/" -v hf="$hashes" 'BEGIN { while ((getline h < hf) > 0) own[h] = 1 } { mine = index($0, b) > 0; for (i = 2; i <= NF; i++) { if ($i ~ /^kiln\.runner-work=/) { split($i, kv, "="); if (kv[2] in own) mine = 1 } if ($i ~ /^kiln\.socket\.runner\./) { k = $i; sub(/^kiln\.socket\.runner\./, "", k); sub(/=.*/, "", k); if (k in own) mine = 1 } } if (mine) print $1 }') || containers=''
  [ -z "$containers" ] || docker rm -f $containers > /dev/null 2>&1 || true
  networks=$(docker network ls -q 2> /dev/null | xargs -r docker network inspect -f '{{.Id}} {{range $k, $v := .Labels}}{{$k}}={{$v}} {{end}}' 2> /dev/null |
    awk -v hf="$hashes" 'BEGIN { while ((getline h < hf) > 0) own[h] = 1 } { for (i = 2; i <= NF; i++) { if ($i ~ /^kiln\.runner-work=/) { split($i, kv, "="); if (kv[2] in own) { print $1; next } } if ($i ~ /^kiln\.socket\.runner\./) { k = $i; sub(/^kiln\.socket\.runner\./, "", k); sub(/=.*/, "", k); if (k in own) { print $1; next } } } }') || networks=''
  [ -z "$networks" ] || docker network rm $networks > /dev/null 2>&1 || true
  rm -f "$hashes"
  if [ -n "$containers$networks" ]; then
    echo "ci/gate.sh: removed what its tests left in Docker (containers: $(printf '%s\n' $containers | grep -c .), networks: $(printf '%s\n' $networks | grep -c .))"
  fi
}
# The guard, beside the unit: every guard_every seconds, how strained the machine's services are;
# strained guard_runs checks running, it writes why in the run's folder and stops the unit.
guard() {
  local bad=0 strained full who own rest took u f
  # Its sleep holds no output open, so a guard stopped mid-sleep keeps no reader of it waiting.
  while sleep "$guard_every" > /dev/null 2>&1; do
    kill -0 "$pid" 2> /dev/null || return 0
    strained=''
    if [ -n "$guard_containers" ]; then
      read -r full who < <(named_strain)
      if awk -v f="$full" -v t="$guard_full" 'BEGIN { exit !(f > t) }'; then strained="$who's I/O pressure $full"; fi
    elif [ -n "$watched" ]; then
      while read -r u f; do
        [ -n "$u" ] && [ -r "$f" ] || continue
        full=$(awk '/^full/ { split($2, a, "="); print a[2] }' "$f")
        if awk -v x="${full:-0}" -v t="$guard_full" 'BEGIN { exit !(x > t) }'; then strained="${strained:+$strained, }$u's I/O pressure $full"; fi
      done <<< "$watched"
    elif [ -r /sys/fs/cgroup/system.slice/io.pressure ]; then
      full=$(pressure /sys/fs/cgroup/system.slice)
      if awk -v f="$full" -v t="$guard_full" 'BEGIN { exit !(f > t) }'; then
        own=$(own_scopes)
        if [ -n "$own" ]; then
          # The services' strain but for this run's own containers: the worst any other cgroup shows.
          rest=$(for d in /sys/fs/cgroup/system.slice/*/; do n=$(basename "$d"); printf '%s\n' "$own" | grep -qxF -- "$n" || pressure "$d"; done | sort -rn | head -1 || true)
          echo "ci/gate.sh: system.slice's I/O pressure $full is partly its own tests' $(printf '%s\n' "$own" | grep -c .) containers; the worst of the rest is ${rest:-0}"
          full=${rest:-0}
        fi
        if awk -v f="$full" -v t="$guard_full" 'BEGIN { exit !(f > t) }'; then strained="system services' I/O pressure $full"; fi
      fi
    fi
    if [ -n "$guard_url" ]; then
      took=$(curl -s -o /dev/null -w '%{time_total}' --max-time "$guard_every" "$guard_url" || echo "$guard_every")
      if awk -v t="$took" -v m="$guard_url_s" 'BEGIN { exit !(t > m) }'; then strained="${strained:+$strained, }$guard_url in ${took}s"; fi
    fi
    if [ -n "$strained" ]; then
      bad=$((bad + 1))
      echo "ci/gate.sh: the machine is strained: $strained ($bad in a row)"
    else
      bad=0
    fi
    if [ $bad -ge "$guard_runs" ]; then
      echo "$strained" > "$run/yielded"
      systemctl --user stop "$unit" 2> /dev/null || true
      return 0
    fi
  done
}
# The steps' processes, by PID: the inner bash and each process below it, as ps has them.
stop_tree() {
  local below
  below=$(ps -A -o pid= -o ppid= 2> /dev/null | awk -v root="$1" '{ up[$1] = $2 } END { for (p in up) { q = up[p]; while ((q in up) && q != root && q > 1) q = up[q]; if (q == root) print p } }') || below=''
  kill -"$2" "$1" $below 2> /dev/null || true
}
# Stopped from outside: it stops what it started (its unit, or its steps' processes), cleans up
# after its tests and says so, with 128 and the signal.
stopped() {
  trap - TERM INT HUP
  if [ -n "$guard_pid" ]; then kill "$guard_pid" 2> /dev/null || true; wait "$guard_pid" 2> /dev/null || true; fi
  if $systemd; then systemctl --user stop "$unit" 2> /dev/null || true; else stop_tree "$pid" "$1"; fi
  wait "$pid" 2> /dev/null || true
  sweep
  echo "GATE: STOPPED (by SIG$1, before its steps ended; logs in $run/logs)"
  exit "$2"
}

pid='' guard_pid=''
if $systemd; then
  # Its own lines, and the unit's stderr that --pipe gives it, go to a file it shows when the unit
  # has ended: how the unit ended is said there ("Finished with result: …").
  systemd-run --user --unit "$unit" --collect --wait --pipe --working-directory="$here" "${props[@]}" "${inner[@]}" 2> "$run/systemd-run.log" &
  pid=$!
  guard &
  guard_pid=$!
else
  nice -n 19 "${inner[@]}" &
  pid=$!
fi
trap 'stopped TERM 143' TERM
trap 'stopped INT 130' INT
trap 'stopped HUP 129' HUP
code=0
wait "$pid" || code=$?
trap - TERM INT HUP
if [ -n "$guard_pid" ]; then kill "$guard_pid" 2> /dev/null || true; wait "$guard_pid" 2> /dev/null || true; fi
sweep

# ---------------------------------------------------------------- the verdict
# What the steps wrote once every one had ended, held against how the run ended: an exit code alone
# says nothing (a unit stopped from outside, or its bash ended by SIGHUP or SIGPIPE, exits 0).
said=''
if [ -s "$run/verdict" ]; then said=$(head -1 "$run/verdict"); fi
result='' ended=''
if $systemd && [ -s "$run/systemd-run.log" ]; then
  echo "systemd-run, and the unit's stderr:"
  sed 's/^/  /' "$run/systemd-run.log"
  result=$(sed -n 's/^Finished with result: //p' "$run/systemd-run.log" | tail -1)
  ended=$(sed -n 's/^Main processes terminated with: //p' "$run/systemd-run.log" | tail -1)
fi
how="exit $code${result:+, unit result $result}${ended:+ ($ended)}"
case $said in
  PASS)
    if [ $code -eq 0 ] && [ "${result:-success}" = success ]; then
      rm -rf -- "${run:?}/tmp"
      echo "GATE: PASS"
      exit 0
    fi
    echo "GATE: FAIL (its steps passed, but its run ended with $how)"
    exit 1
    ;;
  'FAIL '*)
    echo "GATE: FAIL (${said#FAIL }: logs in $run/logs)"
    exit 1
    ;;
esac
if [ -s "$run/yielded" ]; then
  echo "GATE: YIELDED (the machine was strained $guard_runs check$([ "$guard_runs" = 1 ] || echo s) running: $(head -1 "$run/yielded"); run it again later, or elsewhere)"
  exit 75
fi
case $result in oom-kill) how="$how: out of memory at MemoryMax=$mem_cap, what it asked: ask more" ;; esac
most=''
if [ -s "$run/memory" ]; then most=$(awk '$2 > m { m = $2 } END { if (m > 0) printf "%.1f GiB", m / 1073741824 }' "$run/memory"); fi
echo "GATE: FAIL (it ended before its steps said: $how${most:+; the most memory sampled, $most}; logs in $run/logs)"
exit 1
