#!/bin/bash
# kiln-lab's hook (KILN_LAB_HOOK) for goal 11, plan 01a1a87b2e56 step 5: the binary's pages and its code layout.
#
# A fault on the binary maps the 64 KiB of it around the fault, so what of the binary stays resident is every
# 64 KiB window the server touched (crates/kiln/src/resident.rs). BOLT (ci/release/bolt.sh) lays out together the
# code a profile says the server ran; this hook takes that profile from a round's window, and says what of the
# binary was resident as the window ended, mapping by mapping, so a BOLTed build's new hot text (a mapping of its
# own) is told from the code it left in place and from the read-only data.
#
#   bolt-hook.sh <event> <server pid> <round>.hook      (the lab runs it; NEXT_HOOK: a hook to run after it, with the
#                                                       same arguments)
#
# Every round, as its window ends: pages.txt, each of the server's mappings of its executable (/proc/<pid>/smaps:
# address range, permissions, file offset, size and resident KiB), their sum, and the process's RssFile; and
# sections.txt, the same by section (sections.py: each section's resident KiB, the file's own pages and the process's
# copies, and its 64 KiB windows touched), which says which code and data the server touched at all: a BOLTed
# build's hot .text against the .bolt.org.text its profile missed, .rodata, .eh_frame and .gcc_except_table (read
# only as a panic unwinds), .data; and kinds.txt, the whole process's resident memory by kind (kinds.py: the
# binary's, threads' stacks, turso's huge-page-advised arenas, the allocator's mappings, the C heap's), which
# splits the anonymous memory jemalloc's own figures overstate (goal 11 step 5, part 6). For a profiling round of a
# jemalloc-prof build, also: HEAP_DUMP=1, its heap profile dumped as the window ends (heap-dump.log says how it went);
# PURGE_PROBE=1, purge.txt, its Rss and Anonymous before and after jemalloc purges its dirty pages (GDB, gdb's path).
# BOLT_RECORD says what else, from the event BOLT_FROM says to the window's end (the lab waits for the hook, so
# what lasts runs in the background):
#   none          nothing more (the default: an A/B's rounds)
#   lbr           perf records the server with its branch stacks (perf.data; bolt.sh record lbr)
#   samples       the same without them (a machine with no LBR)
#   instrumented  the server is a `bolt.sh instrument` build, whose writer (a child of the server) puts its counts
#                 since it started in <BOLT_DUMP>.<writer's pid>.fdata every few seconds: the latest at the start and
#                 at the window's end (instr-first.fdata, instr-last.fdata), and their difference, what ran between
#                 them alone (instr-window.fdata, bolt.sh window)
# BOLT_FROM: `server` (the default: from the server's start), `fill` or `window`. What stays resident at the window's
# end is all the server touched since startup's pages left (resident.rs), early in its start: the runners'
# connecting and the fill too. A function the profile leaves out costs a 64 KiB window wherever the linker put
# it, one it takes in that only starting ran costs its own bytes among the hot ones, so the profile starts early.
# PERF, PERF_LIBS, BOLT_FREQ and BOLT_EVENT as bolt.sh takes them; BOLT_SH, bolt.sh (this checkout's ci/release/bolt.sh).
event=$1 pid=$2 dir=$3
here=$(cd "$(dirname "$0")" && pwd)
bolt=${BOLT_SH:-$here/../../ci/release/bolt.sh}
mkdir -p "$dir"

# The server's mappings of its executable, as the window ends.
pages() {
  local exe
  exe=$(readlink "/proc/$pid/exe") || return
  {
    awk -v exe="$exe" '
      /^[0-9a-f]+-[0-9a-f]+ / {
        path = $0; sub(/^[^ ]+ +[^ ]+ +[^ ]+ +[^ ]+ +[^ ]+ */, "", path)
        mine = (path == exe); range = $1; perms = $2; off = $3; next
      }
      mine && /^Size:/ { size = $2 }
      mine && /^Rss:/ { printf "%s %s offset=%s size_kb=%d rss_kb=%d\n", range, perms, off, size, $2; rss += $2 }
      END { printf "binary rss_kb=%d\n", rss }' "/proc/$pid/smaps"
    awk '/^RssFile:/ { print "process rss_file_kb=" $2 }' "/proc/$pid/status"
  } > "$dir/pages.txt" 2> /dev/null
  python3 -I "$here/sections.py" "$pid" > "$dir/sections.txt" 2>> "$dir/hook.log" || true
  python3 -I "$here/kinds.py" "$pid" > "$dir/kinds.txt" 2>> "$dir/hook.log" || true
}

# The instrumented server's dump: its writer's file, the writer a child of the server's.
dump_of() {
  local c
  for c in $(cat "/proc/$pid/task/"*/children 2> /dev/null); do
    if [ -f "${BOLT_DUMP:?BOLT_RECORD=instrumented needs BOLT_DUMP}.$c.fdata" ]; then
      echo "$BOLT_DUMP.$c.fdata"
      return
    fi
  done
  return 1
}

# A copy of dump $1 no write was under way in: the same twice, a second apart (a dump is one write of a few MB,
# well within it). A dump lands between the two copies once in its interval's seconds (bolt.sh instrument's 5), so
# ten tries all straddling one is (1/5)^10.
stable_copy() {
  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    cp -f "$1" "$2.a" 2> /dev/null || return 1
    sleep 1
    cp -f "$1" "$2.b" 2> /dev/null || return 1
    if cmp -s "$2.a" "$2.b"; then
      mv -f "$2.a" "$2"
      rm -f "$2.b"
      return 0
    fi
  done
  rm -f "$2.a" "$2.b"
  return 1
}

case ${BOLT_FROM:-server} in
  server) from=server-up ;;
  fill) from=fill-start ;;
  window) from=window-start ;;
  *) echo "bolt-hook: BOLT_FROM is server, fill or window, not $BOLT_FROM" >> "$dir/hook.log"; from=server-up ;;
esac
case "$event" in
  "$from")
    case ${BOLT_RECORD:-none} in
      lbr | samples)
        nohup "$bolt" record "$pid" "$dir/perf.data" "$BOLT_RECORD" > "$dir/perf.log" 2>&1 &
        echo $! > "$dir/perf.pid"
        ;;
      instrumented)
        if d=$(dump_of); then
          stable_copy "$d" "$dir/instr-first.fdata" || echo "bolt-hook: no steady copy of $d" >> "$dir/hook.log"
        else
          echo "bolt-hook: the server ($pid) has no dump at ${BOLT_DUMP:-}.<child>.fdata" >> "$dir/hook.log"
        fi
        ;;
    esac
    ;;
  window-end)
    pages
    # A jemalloc-prof build's heap profile, dumped by the server itself as the window ends (HEAP_DUMP=1; its
    # _RJEM_MALLOC_CONF's prof_prefix says where); then what its allocator's dirty pages hold resident, the process's
    # rollup before and after jemalloc purges every arena (PURGE_PROBE=1; arena 4096 is all of them). Both stop the
    # server a moment (gdb, by its symbols: an unstripped build), and the purge moves the lab's own scrape after it: a
    # profiling round's, never an A/B's. Goal 11 step 5, part 6.
    if [ "${HEAP_DUMP:-0}" = 1 ]; then
      "${GDB:-gdb}" -p "$pid" -batch -nx -q -ex 'call (int)_rjem_mallctl("prof.dump", (void*)0, (void*)0, (void*)0, 0)' > "$dir/heap-dump.log" 2>&1 ||
        echo "bolt-hook: gdb's heap dump failed (heap-dump.log)" >> "$dir/hook.log"
    fi
    if [ "${PURGE_PROBE:-0}" = 1 ]; then
      { echo before; grep -E '^(Rss|Anonymous):' "/proc/$pid/smaps_rollup"; } > "$dir/purge.txt" 2>> "$dir/hook.log"
      "${GDB:-gdb}" -p "$pid" -batch -nx -q -ex 'call (int)_rjem_mallctl("arena.4096.purge", (void*)0, (void*)0, (void*)0, 0)' > "$dir/purge.log" 2>&1 ||
        echo "bolt-hook: gdb's purge failed (purge.log)" >> "$dir/hook.log"
      { echo after; grep -E '^(Rss|Anonymous):' "/proc/$pid/smaps_rollup"; } >> "$dir/purge.txt" 2>> "$dir/hook.log"
    fi
    if [ -s "$dir/perf.pid" ]; then
      p=$(cat "$dir/perf.pid")
      kill -INT "$p" 2> /dev/null
      # perf writes its build ids as it stops: the g9 lab's hook gives it 30 s.
      for _ in $(seq 300); do kill -0 "$p" 2> /dev/null || break; sleep 0.1; done
    fi
    if [ "${BOLT_RECORD:-none}" = instrumented ] && [ -s "$dir/instr-first.fdata" ] && d=$(dump_of); then
      if stable_copy "$d" "$dir/instr-last.fdata"; then
        "$bolt" window "$dir/instr-first.fdata" "$dir/instr-last.fdata" "$dir/instr-window.fdata" >> "$dir/hook.log" 2>&1
      else
        echo "bolt-hook: no steady copy of $d" >> "$dir/hook.log"
      fi
    fi
    ;;
esac
if [ -n "${NEXT_HOOK:-}" ]; then
  exec "$NEXT_HOOK" "$@"
fi
exit 0
