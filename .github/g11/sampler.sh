#!/usr/bin/env bash
# Goal 11 (plan 01a1a87b2e56 steps 1 and 3), trial only: the lab server's memory while it runs: every 2 s a line of
# /proc/<pid>/status and smaps_rollup (rss.csv); every 20 s its whole smaps (kept: the last 4), each followed by
# a clear of the pages' referenced bits (clear_refs), so each smaps' Referenced is what the server touched in
# the 20 s before it: the binary's working set, beside what's merely mapped.
P=$1 O=$2
i=0
echo "t,vmrss_kb,anon_kb,file_kb,shmem_kb,threads,pss_kb,private_dirty_kb,swap_kb" > "$O/rss.csv"
while [ -d "/proc/$P" ]; do
  t=$(date +%s.%N)
  st=$(awk '/^VmRSS|^RssAnon|^RssFile|^RssShmem|^Threads/ {printf "%s ", $2}' "/proc/$P/status" 2>/dev/null)
  ro=$(awk '/^Pss:|^Private_Dirty:|^Swap:/ {printf "%s ", $2}' "/proc/$P/smaps_rollup" 2>/dev/null)
  echo "$t $st $ro" | tr -s ' ' ',' | sed 's/,$//' >> "$O/rss.csv"
  if [ $((i % 10)) -eq 0 ]; then
    if cp "/proc/$P/smaps" "$O/smaps.tmp" 2>/dev/null; then
      mv "$O/smaps.tmp" "$O/smaps.$(( (i / 10) % 4 ))" && echo "$(( (i / 10) % 4 )) $t $([ $i -gt 0 ] && echo referenced-20s || echo referenced-since-start)" >> "$O/smaps.index"
      echo 1 > "/proc/$P/clear_refs" 2>/dev/null || echo "clear_refs refused $t"
    fi
  fi
  i=$((i + 1))
  sleep 2
done
echo "ended $(date +%s.%N)"
