#!/usr/bin/env bash
# Goal 11 (plan 01a1a87b2e56 step 1): the lab server's memory while it runs: every 2 s a line of
# /proc/<pid>/status and smaps_rollup (rss.csv); every 20 s its whole smaps (kept: the last 4); with G11_GDB=1,
# glibc's malloc_info every 30 s (gdb, malloc-<n>.xml).
P=$1 O=$2
i=0
echo "t,vmrss_kb,anon_kb,file_kb,shmem_kb,threads,pss_kb,private_dirty_kb,swap_kb" > "$O/rss.csv"
while [ -d "/proc/$P" ]; do
  t=$(date +%s.%N)
  st=$(awk '/^VmRSS|^RssAnon|^RssFile|^RssShmem|^Threads/ {printf "%s ", $2}' "/proc/$P/status" 2>/dev/null)
  ro=$(awk '/^Pss:|^Private_Dirty:|^Swap:/ {printf "%s ", $2}' "/proc/$P/smaps_rollup" 2>/dev/null)
  echo "$t $st $ro" | tr -s ' ' ',' | sed 's/,$//' >> "$O/rss.csv"
  if [ $((i % 10)) -eq 0 ]; then
    cp "/proc/$P/smaps" "$O/smaps.tmp" 2>/dev/null && mv "$O/smaps.tmp" "$O/smaps.$(( (i / 10) % 4 ))" && echo "$(( (i / 10) % 4 )) $t" >> "$O/smaps.index"
  fi
  if [ "${G11_GDB:-}" = 1 ] && [ $((i % 15)) -eq 14 ]; then
    n=$((i / 15))
    timeout 60 gdb -p "$P" -batch -nx -q -ex "call (int)malloc_info(0, (void*)fopen(\"$O/malloc-$n.xml\", \"w\"))" -ex 'call (int)fflush(0)' > "$O/gdb-$n.log" 2>&1
    echo "$n $t $(date +%s.%N)" >> "$O/malloc.index"
  fi
  i=$((i + 1))
  sleep 2
done
echo "ended $(date +%s.%N)"
