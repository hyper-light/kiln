#!/usr/bin/env bash
# BOLT for kiln's Linux x86-64 release (goal 11, plan 01a1a87b2e56 step 5): the code the server runs in steady
# state laid out on as few pages as it takes, so what it touches of its binary stays resident and the rest doesn't.
#
# Why: a fault on the binary maps the 64 KiB around it that the page cache holds (the kernel's fault_around_bytes;
# madvise(MADV_RANDOM) doesn't change that, a probe on adacachy's 7.2 kernel), and the server's steady state
# touches functions spread over its whole .text: about half its 64 KiB windows stayed resident in the lab's
# realistic profile, 91 of its 174 MiB of code with startup's pages dropped (g11pages). BOLT puts the functions a
# profile says ran together (-reorder-functions=cdsort), moves their blocks that didn't run out of the way
# (-split-functions -split-all-cold -split-eh), orders the blocks that did (-reorder-blocks=ext-tsp), folds
# identical functions (-icf=safe), and leaves what didn't run where the linker put it, out of resident memory.
#
#   bolt.sh tools                               print the directory with llvm-bolt, perf2bolt and merge-fdata
#   bolt.sh check <binary>                      whether BOLT can move <binary>'s functions
#   bolt.sh record <pid> <perf.data> [lbr|samples] [seconds]
#                                               perf records process <pid> (user space), with its branch stacks
#                                               (lbr, the default: Intel's LBR, AMD's LbrExtV2) or its
#                                               instructions alone (samples), until stopped, or for <seconds>
#   bolt.sh profile <binary> <out.fdata|out.yaml> <perf.data>...
#                                               perf2bolt each recording of <binary> (a copy of it, stripped or
#                                               not, with its build id), the profiles merged
#   bolt.sh instrument <binary> <out> <dump> [seconds]
#                                               <binary> with every edge counted: each process of it forks a
#                                               writer that puts the counts since it started in
#                                               <dump>.<writer's pid>.fdata every <seconds> (5)
#   bolt.sh window <first.fdata> <last.fdata> <out.fdata>
#                                               the counts between two dumps of one instrumented process
#   bolt.sh optimize <binary> <profile> <out>   <binary> laid out by <profile> (fdata, or YAML from another
#                                               build: -infer-stale-profile), stripped, into <out>
#   bolt.sh order <binary> <out.order> <perf.data|.fdata>...
#                                               the functions each profile saw run in <binary>, hottest first,
#                                               for the linker to lay out first (KILN_RELEASE_BOLT=<out.order>):
#                                               the layout without BOLT, needing perf alone
#
# The binary is kiln's release built to keep its relocations and symbols (ci/release/build.sh with
# KILN_RELEASE_BOLT; zig-relocs.sh links it). From the environment:
#   KILN_BOLT_BIN    a directory with llvm-bolt, perf2bolt and merge-fdata (a distribution's: Fedora's and Arch's
#                    `llvm-bolt`, Debian's `bolt-<n>`). Otherwise LLVM's release below, fetched once into
#                    KILN_BOLT_HOME (~/.cache/kiln-bolt): 1.2 GB, of which the three tools and BOLT's
#                    instrumentation runtime are kept. KILN_BOLT_URL and KILN_BOLT_SHA256 name another copy.
#   PERF, PERF_LIBS  the perf binary, and a directory of its libraries (as scripts/shard_lab/hook.sh)
#   BOLT_FREQ        perf's sampling frequency (perf's own default otherwise: 4000 a second)
#   BOLT_EVENT       the event perf samples on (cycles:u; on a hybrid CPU, fedora's, that's each kind of core's:
#                    cpu_atom/cycles/u names the E-cores' alone, where the lab's server runs)
#   KILN_BOLT_ICF    optimize's identical code folding: safe (the default), all or none
#   KILN_BOLT_ARGS   more llvm-bolt options for instrument and optimize (-thread-count=4, say)
#   KILN_STRIP       the strip that strips optimize's output (strip)
set -euo pipefail

llvm_version=23.1.5
llvm_asset=LLVM-$llvm_version-Linux-X64.tar.zst
# GitHub's digest of the asset (api.github.com/repos/llvm/llvm-project/releases, llvmorg-23.1.5).
llvm_sha256=${KILN_BOLT_SHA256:-c3107384835f788768f614db610e87fba7077f125ca950eb02a78bab0a308065}
llvm_url=${KILN_BOLT_URL:-https://github.com/llvm/llvm-project/releases/download/llvmorg-$llvm_version/$llvm_asset}
bolt_home=${KILN_BOLT_HOME:-$HOME/.cache/kiln-bolt}

die() {
  echo "bolt.sh: $*" >&2
  exit 1
}

# Whether directory $1 holds BOLT's three tools, and llvm-bolt runs.
runs() {
  local t
  for t in llvm-bolt perf2bolt merge-fdata; do
    [ -x "$1/$t" ] || return 1
  done
  "$1/llvm-bolt" --version > /dev/null 2>&1
}

# LLVM's release into $bolt_home/<version>: its BOLT tools and runtime only. Its zstd frame takes a 1 GiB window.
fetch() {
  command -v zstd > /dev/null || die "fetching LLVM's release takes zstd"
  mkdir -p "$bolt_home"
  local tmp
  tmp=$(mktemp -d "$bolt_home/fetch.XXXXXX")
  echo "bolt.sh: fetching $llvm_url" >&2
  curl -fL --retry 3 -o "$tmp/$llvm_asset" "$llvm_url" || { rm -rf "$tmp"; die "couldn't fetch $llvm_url"; }
  echo "$llvm_sha256  $tmp/$llvm_asset" | sha256sum -c --quiet - || { rm -rf "$tmp"; die "$llvm_asset isn't the release pinned here"; }
  zstd -dc --long=31 --memory=2048MB "$tmp/$llvm_asset" |
    tar -x -C "$tmp" --wildcards --strip-components=1 '*/bin/llvm-bolt' '*/bin/perf2bolt' '*/bin/merge-fdata' '*/lib/libbolt_rt_instr.a' ||
    { rm -rf "$tmp"; die "$llvm_asset has no llvm-bolt, perf2bolt, merge-fdata or libbolt_rt_instr.a: give KILN_BOLT_BIN a distribution's"; }
  rm -rf "${bolt_home:?}/$llvm_version"
  mkdir -p "$bolt_home/$llvm_version"
  mv "$tmp/bin" "$tmp/lib" "$bolt_home/$llvm_version/"
  rm -rf "$tmp"
}

tools() {
  local dir
  if [ -n "${KILN_BOLT_BIN:-}" ]; then
    dir=$KILN_BOLT_BIN
    runs "$dir" || die "KILN_BOLT_BIN ($dir) has no llvm-bolt, perf2bolt and merge-fdata that run"
  else
    dir=$bolt_home/$llvm_version/bin
    runs "$dir" || fetch
    runs "$dir" || die "LLVM $llvm_version's llvm-bolt doesn't run here: give KILN_BOLT_BIN a distribution's"
  fi
  echo "$dir"
}

# perf, with its own libraries where PERF_LIBS says.
perf_cmd() {
  LD_LIBRARY_PATH=${PERF_LIBS:-${LD_LIBRARY_PATH:-}} "${PERF:-perf}" "$@"
}

check() {
  local b=$1 sections
  [ -f "$b" ] || die "$b: no such file"
  readelf -hW "$b" 2> /dev/null | grep 'Machine:.*X86-64' > /dev/null || die "$b: not an x86-64 ELF file"
  sections=$(readelf -SW "$b")
  grep -q ' \.rela\.text ' <<< "$sections" ||
    die "$b: no .rela.text: linked without --emit-relocs (ci/release/zig-relocs.sh), so BOLT can't move its functions"
  grep -q ' \.symtab ' <<< "$sections" || die "$b: no symbols: stripped before BOLT (KILN_RELEASE_BOLT builds with strip off)"
  if ! readelf -nW "$b" | grep 'Build ID' > /dev/null; then
    echo "bolt.sh: $b has no build id: perf2bolt can match a profile to it by its file name alone" >&2
  fi
  echo "$b: relocations and symbols kept; BOLT can move its functions"
}

record() {
  local pid=$1 out=$2 kind=${3:-lbr} secs=${4:-} cmd
  cmd=(record -e "${BOLT_EVENT:-cycles:u}")
  case $kind in
    lbr) cmd+=(-j any,u) ;;
    samples) ;;
    *) die "record takes lbr or samples, not $kind" ;;
  esac
  if [ -n "${BOLT_FREQ:-}" ]; then cmd+=(-F "$BOLT_FREQ"); fi
  cmd+=(-p "$pid" -o "$out")
  if [ -n "$secs" ]; then cmd+=(-- sleep "$secs"); fi
  # exec: whoever started this stops perf by this process's pid (bolt-hook.sh's SIGINT).
  LD_LIBRARY_PATH=${PERF_LIBS:-${LD_LIBRARY_PATH:-}} exec "${PERF:-perf}" "${cmd[@]}"
}

profile() {
  local bin=$1 out=$2 dir perf data i=0 parts=() ba format=()
  shift 2
  [ $# -gt 0 ] || die "profile takes <binary> <out.fdata> <perf.data>..."
  dir=$(tools)
  # YAML names functions by their code's hashes too, so another build's binary can take it (-infer-stale-profile).
  case $out in *.yaml) format=(--profile-format=yaml) ;; esac
  perf=$(command -v "${PERF:-perf}") || die "no perf (PERF)"
  for data in "$@"; do
    i=$((i + 1))
    # A recording without branch stacks is perf2bolt's basic aggregation (--ba). (grep reads it all: -q's early exit
    # would fail the pipe.)
    ba=()
    perf_cmd evlist -v -i "$data" 2> /dev/null | grep BRANCH_STACK > /dev/null || ba=(--ba)
    # perf2bolt runs the perf it finds on PATH.
    if ! PATH="$(dirname "$perf"):$PATH" LD_LIBRARY_PATH=${PERF_LIBS:-${LD_LIBRARY_PATH:-}} \
      "$dir/perf2bolt" --perfdata="$data" -o "$out.$i" ${ba[@]+"${ba[@]}"} ${format[@]+"${format[@]}"} "$bin" > "$out.$i.log" 2>&1; then
      tail -n 20 "$out.$i.log" >&2
      die "perf2bolt failed on $data ($out.$i.log)"
    fi
    grep -E 'BOLT-INFO: .*have non-empty execution profile|BOLT-WARNING: BOLT is estimated|PERF2BOLT-(WARNING|ERROR)' "$out.$i.log" >&2 || true
    parts+=("$out.$i")
  done
  if [ ${#parts[@]} -eq 1 ]; then
    mv "${parts[0]}" "$out"
  else
    "$dir/merge-fdata" "${parts[@]}" > "$out"
    rm -f "${parts[@]}"
  fi
  echo "$out: $(wc -l < "$out") lines from $# recording(s)"
}

instrument() {
  local bin=$1 out=$2 dump=$3 secs=${4:-5} dir extra=()
  check "$bin" > /dev/null
  dir=$(tools)
  # KILN_BOLT_ARGS here too: a distribution's llvm-bolt unpacked elsewhere than it was built for finds its runtime
  # by -runtime-instrumentation-lib=<libbolt_rt_instr.a>.
  read -r -a extra <<< "${KILN_BOLT_ARGS:-}"
  "$dir/llvm-bolt" "$bin" -o "$out" -instrument -instrumentation-file="$dump" -instrumentation-file-append-pid \
    -instrumentation-sleep-time="$secs" -instrumentation-no-counters-clear ${extra[@]+"${extra[@]}"} > "$out.log" 2>&1 ||
    { tail -n 20 "$out.log" >&2; die "llvm-bolt -instrument failed ($out.log)"; }
  echo "$out: instrumented; each process writes $dump.<its writer's pid>.fdata every $secs s"
}

# An instrumented process's dumps hold its counts since it started, each record `<from> <to> <mispredictions>
# <count>` in 8 fields; a record of the last dump less the first's is what ran between them. Counts past 2^31
# printed whole (%.0f): an awk's own print of a large number may not be.
window() {
  awk 'NR == FNR { if (NF == 8) { k = $1 " " $2 " " $3 " " $4 " " $5 " " $6; m[k] = $7; c[k] = $8 }; next }
    NF == 8 {
      k = $1 " " $2 " " $3 " " $4 " " $5 " " $6
      d = $8 - ((k in c) ? c[k] : 0); e = $7 - ((k in m) ? m[k] : 0)
      if (d > 0) printf "%s %.0f %.0f\n", k, (e > 0 ? e : 0), d
      next
    }
    { print }' "$1" "$2" > "$3"
  echo "$3: $(wc -l < "$3") records ran between $1 and $2"
}

optimize() {
  local bin=$1 prof=$2 out=$3 dir log stale=() extra=()
  [ -s "$prof" ] || die "$prof: no profile"
  check "$bin" > /dev/null
  dir=$(tools)
  log=$bin.bolt.log
  case $prof in *.yaml) stale=(-infer-stale-profile) ;; esac
  read -r -a extra <<< "${KILN_BOLT_ARGS:-}"
  # -icf=safe folds only functions no code or data takes the address of: the binary's C (SQLite, jemalloc, aws-lc,
  # musl) may tell functions apart by address, which folding them would merge (KILN_BOLT_ICF=all folds every one).
  # Not -use-gnu-stack: it gives the new segment PT_GNU_STACK's header, whose size musl takes as its threads'
  # default stack (zig links 16 MiB there); BOLT's own new program headers strip and run as they are.
  if ! "$dir/llvm-bolt" "$bin" -o "$bin.bolt" -data="$prof" \
    -reorder-blocks=ext-tsp -reorder-functions=cdsort -split-functions -split-all-cold -split-eh \
    -icf="${KILN_BOLT_ICF:-safe}" -dyno-stats ${stale[@]+"${stale[@]}"} ${extra[@]+"${extra[@]}"} > "$log" 2>&1; then
    tail -n 20 "$log" >&2
    die "llvm-bolt failed ($log)"
  fi
  "${KILN_STRIP:-strip}" --strip-all -o "$out" "$bin.bolt"
  grep -E 'BOLT-INFO: (.*have non-empty execution profile|splitting separates|basic block reordering modified|ICF folded)' "$log" >&2 || true
  echo "$out: laid out by $prof ($log); its loaded segments (address, KiB, flags):"
  local type off vaddr paddr filesz memsz rest
  readelf -lW "$out" | while read -r type off vaddr paddr filesz memsz rest; do
    if [ "$type" = LOAD ]; then printf '  %s %8d KiB  %s\n' "$vaddr" $((memsz / 1024)) "${rest% *}"; fi
  done
}

# The functions the profiles saw run in $bin, hottest first, one a line, named as its symbol table names them
# (mangled): what LLD lays out first, in that order (--symbol-ordering-file, zig-relocs.sh's KILN_LINK_ORDER), so the
# code the server runs lies together with no BOLT, whole functions moved and none split. A function's weight is its
# samples and the branch records that landed in it, from perf's recordings (with LBR's records where a recording
# holds them; of $bin or of a stripped copy, perf naming the functions by $bin's symbols, matched by build id), or its
# counts in BOLT's profiles (.fdata: perf2bolt's, or an instrumented window's, every function that ran), all the
# profiles' summed. What no profile saw stays after them, where the linker puts it. LLD orders every function of a
# name (a local one's too), and passes over a name the link doesn't define.
order() {
  local bin=$1 out=$2 id data rec n=0 tmp report
  shift 2
  [ $# -gt 0 ] || die "order takes <binary> <out.order> <perf.data|.fdata>..."
  [ -f "$bin" ] || die "$bin: no such file"
  readelf -SW "$bin" | grep ' \.symtab ' > /dev/null ||
    die "$bin: no symbols: an order names functions as an unstripped build does (KILN_RELEASE_BOLT=relocs keeps them)"
  id=$(readelf -n "$bin" | awk '$1 == "Build" && $2 == "ID:" { print $3; exit }')
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/bolt-order.XXXXXX")
  for data in "$@"; do
    n=$((n + 1))
    [ -s "$data" ] || { rm -rf "$tmp"; die "$data: no profile"; }
    case $data in
      *.fdata)
        # BOLT's records: a branch `1 <from> <offset> 1 <to> <offset> <mispredictions> <count>` (0 and an address for
        # an end in no function), a sample `1 <function> <offset> <count>` (no_lbr's); a local function's name ends
        # in `/<n>`, which the linker's doesn't.
        awk 'NF == 8 { if ($1 == 1) w[$2] += $8; if ($4 == 1) w[$5] += $8; next }
          NF == 4 && $1 == 1 { w[$2] += $4 }
          END { for (f in w) { s = f; sub(/\/.*/, "", s); printf "%.0f [.] %s\n", w[f], s } }' "$data" >> "$tmp/counts"
        ;;
      *)
        # perf names a recording's functions from the file at its path under --symfs, if its build id is theirs.
        rec=$(perf_cmd buildid-list -i "$data" 2> /dev/null | awk -v id="$id" 'id != "" && $1 == id { print $2; exit }')
        [ -n "$rec" ] || { rm -rf "$tmp"; die "$data recorded no binary with $bin's build id (${id:-$bin has none})"; }
        mkdir -p "$tmp/symfs$n$(dirname "$rec")"
        ln -sfn "$(realpath "$bin")" "$tmp/symfs$n$rec"
        report=(report -i "$data" --symfs="$tmp/symfs$n" --dsos "$rec" --stdio --no-children -g none --no-demangle)
        if ! perf_cmd "${report[@]}" --no-branch-stack --sort sym -F sample,sym >> "$tmp/counts" 2> "$tmp/err"; then
          tail -n 20 "$tmp/err" >&2
          rm -rf "$tmp"
          die "perf report failed on $data"
        fi
        # (grep reads it all: -q's early exit would fail the pipe.)
        if perf_cmd evlist -v -i "$data" 2> /dev/null | grep BRANCH_STACK > /dev/null &&
          ! perf_cmd "${report[@]}" --branch-stack --sort symbol_to -F sample,symbol_to >> "$tmp/counts" 2> "$tmp/err"; then
          tail -n 20 "$tmp/err" >&2
          rm -rf "$tmp"
          die "perf report failed on $data's branch records"
        fi
        ;;
    esac
  done
  # A line of perf's: `<count> [.] <function> ...` ([k]: the kernel's; an address: a function perf couldn't name).
  awk '$1 ~ /^[0-9]+$/ && $2 == "[.]" && $3 !~ /^0x/ { w[$3] += $1 } END { for (f in w) printf "%.0f %s\n", w[f], f }' "$tmp/counts" |
    LC_ALL=C sort -k1,1nr -k2,2 | awk '{ print $2 }' > "$out"
  rm -rf "$tmp"
  [ -s "$out" ] || die "$out: none of $bin's functions ran in these profiles"
  echo "$out: $(wc -l < "$out") functions, hottest first, from $n profile(s)"
}

cmd=${1:-}
[ $# -gt 0 ] && shift
case $cmd in
  tools) tools ;;
  check) [ $# -eq 1 ] || die "check takes <binary>"; check "$1" ;;
  record) [ $# -ge 2 ] || die "record takes <pid> <perf.data> [lbr|samples] [seconds]"; record "$@" ;;
  profile) profile "$@" ;;
  instrument) [ $# -ge 3 ] || die "instrument takes <binary> <out> <dump> [seconds]"; instrument "$@" ;;
  window) [ $# -eq 3 ] || die "window takes <first.fdata> <last.fdata> <out.fdata>"; window "$@" ;;
  optimize) [ $# -eq 3 ] || die "optimize takes <binary> <profile> <out>"; optimize "$@" ;;
  order) [ $# -ge 3 ] || die "order takes <binary> <out.order> <perf.data|.fdata>..."; order "$@" ;;
  *) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
