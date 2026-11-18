#!/bin/bash
# kiln's step35 harness suites on Linux (KVM), inside a container of one distro on this KVM host:
#
#   bash run-linux.sh DISTRO KILN HEX SHARDS      DISTRO: ubuntu, debian, fedora, arch or alpine
#
# The container is the distro's image, pinned by digest, with the packages the build and the suites need
# added. It gets /dev/kvm and runs linux-in.sh (lib.sh's run) as this host's user, never as root, as Shards'
# own Alpine CI runs its VM tests (scripts/ci/alpine, scripts/musl-ci). Each distro has its own folder,
# ~/.cache/xplat/linux-DISTRO (its targets, homes and logs apart from the others'), mounted at the same path
# inside. As it ends it removes the run's work and the image it built, and keeps the logs and the pinned
# base, as Docker Hub's pull limits ask. Exit: lib.sh's.
set -u
C=$(cd "$(dirname "$0")" && pwd)
POLL=5 # lib.sh's look

say() {
	local line
	line="=== $(date -u +%H:%M:%SZ) $*"
	echo "$line" >&2
	if [ -n "${LOGS:-}" ] && [ -d "$LOGS" ]; then echo "$line" >>"$LOGS/summary.log"; fi
	return 0
}
refuse() {
	say "refused: $*"
	exit 2
}

# What a run asks of this host where it waits for room (XPLAT_WAIT_READY), as kiln's needs say it
# (cpus=8,mem=16g,disk=40g): JOBS CPUs (lib.sh's CARGO_BUILD_JOBS), XPLAT_MEM and XPLAT_DISK. They are chosen,
# not measured: plat_measure's lines in the summary are their first measurements.
CPU_ASK=${JOBS:-8}
MEM_ASK=${XPLAT_MEM:-16g}
DISK_ASK=${XPLAT_DISK:-40g}

# bytes SIZE: a size as kiln's needs write it (16g, 512m, or bytes), in bytes; g and m are GiB and MiB.
bytes() {
	local n=${1%[gGmM]}
	case $n in '' | *[!0-9]*) return 1 ;; esac
	case $1 in
	*[gG]) echo $((n * 1073741824)) ;;
	*[mM]) echo $((n * 1048576)) ;;
	*) echo "$n" ;;
	esac
}

# idle_cpus: how many CPUs ran nothing over the last 10 s, the window of the kernel's shortest pressure average
# (Documentation/accounting/psi.rst, avg10), from /proc/stat's all-CPU line (proc(5)): its idle and iowait time
# (a CPU waiting on I/O runs nothing) against all of it, times the CPUs online. Two decimals.
idle_cpus() {
	local a b
	a=$(awk '$1 == "cpu" { t = 0; for (i = 2; i <= 9; i++) t += $i; print $5 + $6, t; exit }' /proc/stat)
	sleep 10
	b=$(awk '$1 == "cpu" { t = 0; for (i = 2; i <= 9; i++) t += $i; print $5 + $6, t; exit }' /proc/stat)
	awk -v a="$a" -v b="$b" -v n="$(getconf _NPROCESSORS_ONLN)" 'BEGIN {
		split(a, x, " "); split(b, y, " "); dt = y[2] - x[2]
		printf "%.2f\n", (dt > 0 ? (y[1] - x[1]) / dt * n : 0)
	}'
}

# parent_cgroup: the cgroup the engine puts its containers under (Docker: system.slice with the systemd driver,
# docker(1)'s --cgroup-parent; Podman: machine.slice rooted, the user's user.slice rootless, podman-run(1));
# empty where it can't be said.
parent_cgroup() {
	local driver
	if [ "$ENGINE" = docker ]; then
		driver=$(docker info -f '{{.CgroupDriver}}' 2>/dev/null)
		[ "$driver" = systemd ] && echo /system.slice
		[ "$driver" = cgroupfs ] && echo /docker
	else
		driver=$(podman info -f '{{.Host.CgroupManager}}' 2>/dev/null)
		if [ "$driver" = systemd ] && [ "$(id -u)" = 0 ]; then
			echo /machine.slice
		elif [ "$driver" = systemd ]; then
			echo "/user.slice/user-$(id -u).slice/user@$(id -u).service/user.slice"
		fi
	fi
	return 0
}

# oomd_holds PARENT: what of systemd-oomd's (systemd-oomd.service(8)) would end a container under PARENT now, by its
# own configured limits as oomctl(1) says them: a cgroup it watches at or above PARENT whose memory pressure's avg10
# is at or over that cgroup's limit, or, where it watches swap there, swap used at or over its swap limit. One line
# each; none where nothing would, where no oomd runs, or (PARENT empty) every cgroup it watches is weighed.
oomd_holds() {
	local swap_used
	command -v oomctl >/dev/null 2>&1 && systemctl is-active --quiet systemd-oomd 2>/dev/null || return 0
	swap_used=$(awk '$1 == "SwapTotal:" { t = $2 } $1 == "SwapFree:" { f = $2 } END { if (t > 0) printf "%.2f\n", (t - f) / t * 100 }' /proc/meminfo)
	oomctl 2>/dev/null | awk -v parent="$1" -v swap_used="$swap_used" '
		function above(path) { return parent == "" || index(parent "/", path "/") == 1 }
		/^Swap Used Limit:/ { swap_limit = $4 + 0 }
		/^Swap Monitored CGroups:/ { section = "swap"; next }
		/^Memory Pressure Monitored CGroups:/ { section = "pressure"; next }
		/^[^ \t]/ { section = "" }
		section == "swap" && $1 == "Path:" && above($2) && swap_used != "" && swap_used + 0 >= swap_limit && !said_swap {
			printf "swap %s%% used, systemd-oomd'"'"'s limit %s%% (it watches %s)\n", swap_used, swap_limit, $2; said_swap = 1
		}
		section == "pressure" && $1 == "Path:" { path = $2 }
		section == "pressure" && /Memory Pressure Limit:/ { limit = $4 + 0 }
		section == "pressure" && $1 == "Pressure:" && $2 == "Avg10:" && above(path) {
			avg10 = $3; sub(/,$/, "", avg10)
			if (avg10 + 0 >= limit) printf "%s memory pressure avg10 %s%%, systemd-oomd'"'"'s limit %s%%\n", path, avg10, limit
		}'
}

# ready: before the image build of a run that waits for room (XPLAT_WAIT_READY), wait
# until this host has room for what the run asks, as kiln's placement admits a run by its needs, and nothing of
# systemd-oomd's would end it: idle CPUs over the last 10 s at least CPU_ASK, MemAvailable (proc(5): what can start
# without swapping) at least MEM_ASK, free disk at least DISK_ASK where the run's files go and where the engine keeps
# its image, and oomd_holds empty. What holds it is said as it begins and again as what holds it changes, and the
# wait has no deadline: it waits as a queued kiln run does, and is stopped as one is.
ready() {
	local disk need_mem need_disk d free idle avail held why kinds said='' start=$SECONDS parent
	local -a dirs
	need_mem=$(bytes "$MEM_ASK") || refuse "XPLAT_MEM is a size as kiln's needs say one (16g, 512m), not $MEM_ASK"
	need_disk=$(bytes "$DISK_ASK") || refuse "XPLAT_DISK is a size as kiln's needs say one (40g), not $DISK_ASK"
	parent=$(parent_cgroup)
	if [ "$ENGINE" = docker ]; then
		dirs=("$R" "$(docker info -f '{{.DockerRootDir}}' 2>/dev/null)")
	else
		dirs=("$R" "$(podman info -f '{{.Store.GraphRoot}}' 2>/dev/null)")
	fi
	while :; do
		idle=$(idle_cpus)
		avail=$(awk '$1 == "MemAvailable:" { print $2 * 1024; exit }' /proc/meminfo)
		disk=''
		for d in "${dirs[@]}"; do
			[ -n "$d" ] || continue
			free=$(df -Pk "$d" 2>/dev/null | awk 'NR == 2 { print $4 * 1024 }')
			[ -n "$free" ] && [ "$free" -lt "$need_disk" ] && disk="$disk; $((free / 1073741824)) GiB free at $d"
		done
		held=$(oomd_holds "$parent")
		why='' kinds=''
		if awk -v i="$idle" -v n="$CPU_ASK" 'BEGIN { exit !(i < n) }'; then
			why="$why; $idle CPUs idle" kinds="${kinds}cpu "
		fi
		if [ "${avail:-0}" -lt "$need_mem" ]; then
			why="$why; $((${avail:-0} / 1073741824)) GiB available" kinds="${kinds}memory "
		fi
		[ -z "$disk" ] || why="$why$disk" kinds="${kinds}disk "
		[ -z "$held" ] || why="$why; $(echo "$held" | paste -sd ';' -)" kinds="${kinds}oomd "
		if [ -z "$why" ]; then
			say "ready after $((SECONDS - start)) s: $idle CPUs idle over the last 10 s, $((avail / 1073741824)) GiB available, for the run's $CPU_ASK CPUs and $MEM_ASK; disk free for its $DISK_ASK; systemd-oomd holds nothing over ${parent:-the cgroups it watches}"
			return 0
		fi
		if [ "$kinds" != "$said" ]; then
			say "ready: waiting for room for the run's $CPU_ASK CPUs, $MEM_ASK and $DISK_ASK before the image build:${why#;}"
			said=$kinds
		fi
	done
}

[ $# -eq 4 ] || refuse "usage: bash run-linux.sh DISTRO KILN HEX SHARDS"
DISTRO=$1
shift
# Each distro's supported release (each project's own release page), its image by its index's
# digest (Docker Hub, read 2026-11-17) so that each host pulls its own architecture's, and the packages the
# build and the suites need there, each name checked in that
# release's package index, and named even where the base may carry it already. Arch's keyring is brought up
# to date first, so that a package signed by a key newer than the pinned base's installs.
APT='export DEBIAN_FRONTEND=noninteractive; apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git xz-utils build-essential python3 gdb procps'
case $DISTRO in
ubuntu)
	IMAGE=docker.io/library/ubuntu:24.04@sha256:73b999393d07f2bf65a40373778a44dc9a6d54ad7718d29247d8ff9413c4d6a8
	PACKAGES=$APT
	;;
debian)
	IMAGE=docker.io/library/debian:13@sha256:9c6e381421d0f48d1b8133e573f33c185ee16cbf505eda9c4a8a4791b25ea8a6
	PACKAGES=$APT
	;;
fedora)
	IMAGE=docker.io/library/fedora:45@sha256:a7d72ba2745b100a3351087472e5ee7b2450af7cf331e7673007e3817612839c
	PACKAGES='dnf -y install --setopt=install_weak_deps=False --allowerasing ca-certificates curl git-core xz tar gcc gcc-c++ make python3 gdb procps-ng findutils gawk util-linux-core'
	;;
arch)
	IMAGE=docker.io/library/archlinux:latest@sha256:d4594bb2300cba11f41c2f27923eb56f63d9988909c402f9c95517d1e362c899
	PACKAGES='pacman -Sy --noconfirm --needed archlinux-keyring && pacman -Su --noconfirm --needed ca-certificates curl git xz tar gcc make python gdb procps-ng findutils gawk util-linux'
	;;
alpine)
	IMAGE=docker.io/library/alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf
	PACKAGES='apk add --no-cache bash ca-certificates curl git xz build-base linux-headers python3 gdb procps-ng'
	;;
*) refuse "no distro $DISTRO: ubuntu, debian, fedora, arch or alpine" ;;
esac

[ "$(uname -s)" = Linux ] || refuse "run-linux.sh runs on a Linux host with KVM; this is $(uname -s)"
[ "$(id -u)" != 0 ] || refuse "run it as a user: the tests run as their checkout's owner, never as root (Shards' scripts/musl-ci)"
[ -c /dev/kvm ] || refuse "no /dev/kvm: this host runs no KVM guests"
# XPLAT_R: where each distro's folder goes, where it isn't ~/.cache/xplat (a runner whose home disk is small).
R=${XPLAT_R:-$HOME/.cache/xplat}/linux-$DISTRO
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
LOGS=$R/logs/$STAMP
NAME=xplat-$DISTRO-$$
TAG=localhost/xplat-$DISTRO:$STAMP
mkdir -p "$R" || refuse "can't make $R"
if ! mkdir "$R/host.lock" 2>/dev/null; then
	other=$(cat "$R/host.lock/pid" 2>/dev/null)
	[ -n "$other" ] && kill -0 "$other" 2>/dev/null && refuse "another run (pid $other) has $DISTRO's container on this host: run again once it has ended"
	rm -rf "$R/host.lock"
	mkdir "$R/host.lock" || refuse "can't take $R/host.lock"
fi
echo $$ >"$R/host.lock/pid"

CPID='' ENGINE=''
# shellcheck disable=SC2329 # the EXIT trap's
cleanup() {
	local rc=$?
	trap - EXIT
	trap '' TERM INT HUP
	if [ -n "$CPID" ] && kill -0 "$CPID" 2>/dev/null; then
		say "ending the container's run with TERM: it samples what runs, ends it and cleans up"
		"$ENGINE" kill --signal TERM "$NAME" >/dev/null 2>&1 || kill -TERM "$CPID" 2>/dev/null
		wait "$CPID"
	fi
	if [ -n "$ENGINE" ]; then
		"$ENGINE" rm -f "$NAME" >/dev/null 2>&1
		"$ENGINE" rmi "$TAG" >/dev/null 2>&1 && say "image $TAG removed"
	fi
	if [ -e "$R/w" ]; then
		chmod -R u+w "$R/w" 2>/dev/null
		rm -rf "$R/w"
		say "removed $R/w, which the container's run left"
	fi
	rm -rf "$R/image" "$R/host.lock"
	say "logs in $LOGS on $(uname -n); exit $rc"
	exit "$rc"
}
trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
trap 'exit 129' HUP
# This lock lets one container of this distro run here: an inner run's lock left now is a dead container's.
rm -rf "$R/lock"
mkdir -p "$LOGS" "$R/image"

# Said before the engine is asked anything: an engine that doesn't answer holds the run right here, and the summary
# says where (one run sat over five hours in `docker info` on a host deep in swap, with no stage logged).
say "engine: asking Docker, else Podman, for its state ($(uname -n))"
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
	ENGINE=docker
	if docker info -f '{{json .SecurityOptions}}' 2>/dev/null | grep -q rootless; then
		refuse "this Docker is rootless: its containers' users map to subordinate ids, which own none of this run's files; use rootful Docker or Podman"
	fi
	# As this user, with /dev/kvm's group, which Docker gives the device node it makes.
	USERS=(--user "$(id -u):$(id -g)" --group-add "$(stat -c %g /dev/kvm)")
elif command -v podman >/dev/null 2>&1 && podman info >/dev/null 2>&1; then
	ENGINE=podman
	# As this user's own id, and, where /dev/kvm is its group's alone, with this user's groups (crun's alone).
	USERS=(--userns=keep-id)
	mode=$(stat -c %a /dev/kvm)
	if [ $((8#$mode & 6)) -ne 6 ]; then
		if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then refuse "this user can't open /dev/kvm, which rootless Podman passes on as it is"; fi
		[ "$(podman info -f '{{.Host.OCIRuntime.Name}}' 2>/dev/null)" = crun ] ||
			refuse "/dev/kvm is open to its group alone, and only crun keeps a user's groups (--group-add keep-groups); this Podman runs another runtime"
		USERS+=(--group-add keep-groups)
	fi
else
	refuse "neither Docker nor Podman answers on this host"
fi
# What a run of this distro here left when kiln's KILL (5 s after its TERM) cut its cleanup short: its
# container and its image, by the names only this script gives them, which this lock now covers.
for old in $("$ENGINE" ps -a --format '{{.Names}}' 2>/dev/null | grep "^xplat-$DISTRO-"); do
	"$ENGINE" rm -f "$old" >/dev/null 2>&1 && say "removed $old, a killed run's container"
done
for old in $("$ENGINE" images --format '{{.Repository}}:{{.Tag}}' 2>/dev/null | grep "^localhost/xplat-$DISTRO:"); do
	"$ENGINE" rmi "$old" >/dev/null 2>&1 && say "removed $old, a killed run's image"
done
# Docker's default seccomp profile and one rule more, userfaultfd (seccomp-userfaultfd.json): the default
# allows no userfaultfd (defaultAction SCMP_ACT_ERRNO), so under it the VM process's hold of its own code
# (Shards D36) is refused, and its E2E isn't what a native host runs. Podman takes the same file. Never
# unconfined: everything else is the default's.
PROFILE=$C/seccomp-userfaultfd.json
[ -f "$PROFILE" ] || refuse "no seccomp profile at $PROFILE"
SEC=(--security-opt "seccomp=$PROFILE")
selinux="not enforcing"
if [ "$(cat /sys/fs/selinux/enforce 2>/dev/null)" = 1 ]; then
	# SELinux keeps a container from this host's folders and from /dev/kvm unless they are relabeled: the
	# container runs unlabeled instead, rather than relabel the runner's synced copy (podman-run(1),
	# --security-opt label=disable).
	SEC+=(--security-opt label=disable)
	selinux=enforcing
fi
# shellcheck source=/dev/null
HOST_OS=$(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME")
ENGINE_SAID=$("$ENGINE" --version 2>/dev/null | head -1)
say "host $(uname -n): ${HOST_OS:-Linux}, kernel $(uname -r), $ENGINE_SAID, /dev/kvm $(stat -c '%A %U:%G' /dev/kvm), SELinux $selinux, seccomp $(basename "$PROFILE") (Docker's default and userfaultfd); image $IMAGE"

{
	printf 'FROM %s\n' "$IMAGE"
	# Tried five times, 20 s apart, as a checkout is.
	printf 'RUN for i in 1 2 3 4 5; do (%s) && exit 0; sleep 20; done; exit 1\n' "$PACKAGES"
	cat <<'EOF'
ARG XPLAT_UID
ARG XPLAT_GID
ARG XPLAT_HOME
RUN grep -q "^[^:]*:[^:]*:$XPLAT_GID:" /etc/group || echo "xplat:x:$XPLAT_GID:" >>/etc/group; grep -q "^[^:]*:[^:]*:$XPLAT_UID:" /etc/passwd || echo "xplat:x:$XPLAT_UID:$XPLAT_GID::$XPLAT_HOME:/bin/bash" >>/etc/passwd
EOF
} >"$R/image/Dockerfile"
[ -z "${XPLAT_WAIT_READY:-}" ] || ready
say "image: $DISTRO's base and the packages the build needs, as $TAG ($LOGS/image.log)"
"$ENGINE" build -t "$TAG" --build-arg "XPLAT_UID=$(id -u)" --build-arg "XPLAT_GID=$(id -g)" --build-arg "XPLAT_HOME=$R/w/home" \
	-f "$R/image/Dockerfile" "$R/image" >"$LOGS/image.log" 2>&1 </dev/null &
CPID=$!
wait "$CPID"
rc=$?
CPID=''
if [ $rc -ne 0 ]; then
	tail -20 "$LOGS/image.log" >&2
	say "RESULT linux-$DISTRO image FAIL: exit $rc: $(grep -aiE 'error|toomanyrequests|429' "$LOGS/image.log" | tail -2 | tr '\n' ' ' | cut -c1-300)"
	exit 1
fi

PASS=()
[ -z "${JOBS+x}" ] || PASS+=(-e "JOBS=$JOBS")
[ -z "${LINKS+x}" ] || PASS+=(-e "LINKS=$LINKS")
# A stalled git transfer's bound, where the run's caller set one (clones on a busy host once sat for hours): git
# reads it from the environment (git-config(1), http.lowSpeedLimit and http.lowSpeedTime),
# and the container's environment is its own, so the clones and fetches inside it got none before.
for v in GIT_HTTP_LOW_SPEED_LIMIT GIT_HTTP_LOW_SPEED_TIME; do
	[ -z "${!v+x}" ] || PASS+=(-e "$v=${!v}")
done
HOSTPID=$$
# alive: this script's life, held as the container's stdin, whose end ends the run inside (linux-in.sh).
alive() { while kill -0 "$HOSTPID" 2>/dev/null; do sleep "$POLL"; done; }
# No cap on the container's processes and threads but this host's own, as on a native run: Podman caps a
# container at 2048 where the pids controller is there (podman-run(1), --pids-limit) and Docker caps none;
# -1 means none to both.
alive | "$ENGINE" run --rm -i --init --name "$NAME" --hostname "$(uname -n)" --device /dev/kvm --pids-limit -1 "${USERS[@]}" ${SEC[@]+"${SEC[@]}"} \
	-v "$C:$C:ro" -v "$R:$R" -w "$R" \
	-e "HOME=$R/w/home" -e USER=xplat -e LOGNAME=xplat -e "XPLAT_R=$R" -e "XPLAT_LOGS=$LOGS" \
	-e "XPLAT_HOST_NAME=$(uname -n)" -e "XPLAT_HOST_OS=${HOST_OS:-Linux}" -e "XPLAT_ENGINE=$ENGINE_SAID" -e "XPLAT_IMAGE=$IMAGE" \
	${PASS[@]+"${PASS[@]}"} "$TAG" bash "$C/linux-in.sh" "$DISTRO" "$@" &
CPID=$!
wait "$CPID"
rc=$?
CPID=''
exit "$rc"
