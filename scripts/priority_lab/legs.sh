#!/bin/bash
# The legs' stand-in for goal 8's lab (plan 01a1a59c2b29 step 7): run inside a scope delegated to it
# (systemd-run --user --scope -p Delegate=yes), it arranges its unit as kiln's runner does
# (crates/kiln/src/os/contain_linux.rs, arrange): its own process into the unit's leaf `kiln`, +cpu
# for the unit's children and for the jobs' folder, and one job's group holding N busy loops on the
# lab's CPUs: `idle-group`, the job's group's cpu.idle 1, goal 8's first design; `normal`, as they
# come. The unit's own cpu.idle is the driver's to set (CPUWeight=idle).
set -e
mode=$1 n=$2 cpus=$3
own=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
mkdir -p "$own/kiln"
echo $$ > "$own/kiln/cgroup.procs"
echo +cpu > "$own/cgroup.subtree_control"
mkdir -p "$own/kiln-jobs/job"
echo +cpu > "$own/kiln-jobs/cgroup.subtree_control"
case "$mode" in idle-group) echo 1 > "$own/kiln-jobs/job/cpu.idle" ;; esac
echo "legs: $mode, $n busy loops on CPUs $cpus in $own/kiln-jobs/job (cpu.idle $(cat "$own/kiln-jobs/job/cpu.idle"), cpu.weight $(cat "$own/kiln-jobs/job/cpu.weight")); unit cpu.idle $(cat "$own/cpu.idle"), cpu.weight $(cat "$own/cpu.weight")"
for _ in $(seq "$n"); do
  sh -c "echo \$\$ > '$own/kiln-jobs/job/cgroup.procs' && exec taskset -c '$cpus' sh -c 'while :; do :; done'" &
done
wait
