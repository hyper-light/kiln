#!/bin/bash
# CPU hogs for goal 7's lab (driver.sh): N busy loops on the CPUs given, in the scope this runs in (CPUWeight=1).
n=$1 cpus=$2
echo "hogs: $n busy loops on CPUs $cpus in $(sed -n 's/^0:://p' /proc/self/cgroup) (cpu.weight $(cat /sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)/cpu.weight 2>/dev/null))"
for _ in $(seq "$n"); do
  taskset -c "$cpus" sh -c 'while :; do :; done' &
done
wait
