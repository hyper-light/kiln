#!/bin/bash
# linux-in.sh: lib.sh's run inside run-linux.sh's container, as this host's user, never as root.
# run-linux.sh runs it; it is not run by hand:
#
#   bash linux-in.sh DISTRO KILN HEX SHARDS
set -u
C=$(cd "$(dirname "$0")" && pwd)
PLATFORM=linux-${1:?run-linux.sh names the distro}
shift
R=${XPLAT_R:?run-linux.sh sets XPLAT_R}
# shellcheck source=lib.sh
. "$C/lib.sh"
# rustup-init 1.29.3, rustup's release as of 2026-11-17 (static.rust-lang.org/rustup/release-stable.toml),
# checked by the digests rustup publishes beside each build.
RUSTUP_VERSION=1.29.3
# gdb's time with each process, as Shards CI's hang watcher gives it (scripts/ci/hang-watch.sh).
GDB_SECS=15

# run-linux.sh holds this container's stdin open while it lives: its end, however it comes, ends this run,
# cleanup included. Read through fd 3: a background job's own stdin is /dev/null in a shell without job
# control.
exec 3<&0
{
	cat <&3 >/dev/null
	kill -TERM $$ 2>/dev/null
} &
exec 3<&-

rustup_sum() {
	case $1 in
	x86_64-unknown-linux-gnu) echo b1c8cb14e30889e7e2999e6b69e62d9e9b9ffc0f4df8180564d306d9680e6c55 ;;
	x86_64-unknown-linux-musl) echo 1552169aba2836777136ccff7a381e95c17233b5d8250cd5e10ae3cb971bfcfc ;;
	aarch64-unknown-linux-gnu) echo 9f608c142e0e38a78957d0661ba1dfaf0b6fb37184cfaa4b9a0aa57af6184282 ;;
	aarch64-unknown-linux-musl) echo 956deacdc27ab82ae7ac821d5d3e9e95e4795d476d17efe99743bb8531ca1031 ;;
	esac
}

# plat_setup: rustup in this run's home (removed with the run), with kiln's toolchain, stable, as kiln's
# own CI takes it (.github/workflows/ci.yml).
plat_setup() {
	local libc=gnu triple sum
	[ -e /etc/alpine-release ] && libc=musl
	triple=$(uname -m)-unknown-linux-$libc
	sum=$(rustup_sum "$triple")
	[ -n "$sum" ] || refuse "no rustup-init digest here for $triple"
	if [ ! -c /dev/kvm ] || [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then refuse "this container's user can't open /dev/kvm"; fi
	mkdir -p "$HOME" || refuse "can't make $HOME"
	export PATH="$HOME/.cargo/bin:$PATH"
	if [ "$libc" = musl ]; then
		# Linked by Alpine's own cc, as Shards' Alpine CI links (scripts/ci/alpine).
		export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=cc
	fi
	say "rust: rustup-init $RUSTUP_VERSION for $triple, then stable"
	tries 3 curl -fsSL -o "$W/rustup-init" "https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/$triple/rustup-init" || die "rustup-init: download failed"
	echo "$sum  $W/rustup-init" | sha256sum -c - >&2 || die "rustup-init: not the build whose digest rustup publishes"
	chmod +x "$W/rustup-init"
	waited "$LOGS/rustup.log" "$W/rustup-init" -y --no-modify-path --profile minimal --default-toolchain stable ||
		die "rustup: $(tail -3 "$LOGS/rustup.log" | tr '\n' ' ')"
}
# plat_toolchain: Shards' pinned toolchain (vendor/shards/rust-toolchain.toml), with the musl targets its
# guests need, before pack.sh builds there.
plat_toolchain() {
	cd "$K/vendor/shards" || exit 1
	say "rust: Shards' pinned toolchain, $(sed -n 's/^channel *= *//p' rust-toolchain.toml | tr -d '"')"
	waited "$LOGS/rustup-shards.log" rustup toolchain install || die "rustup: Shards' toolchain: $(tail -3 "$LOGS/rustup-shards.log" | tr '\n' ' ')"
	cd "$K" || exit 1
}
plat_facts() {
	local name libc cc_said py_said
	# shellcheck source=/dev/null
	name=$(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME")
	if [ -e /etc/alpine-release ]; then libc="musl $(apk info -v musl 2>/dev/null | head -1)"; else libc=$(ldd --version 2>&1 | head -1); fi
	# What differs from image to image besides the C library: the C compiler of the build's C (AWS-LC and
	# SQLite) and the Python of vendor.py and the stand-in harness.
	cc_said=$(cc --version 2>/dev/null | head -1)
	py_said=$(python3 -V 2>/dev/null)
	FACT_HOST=${XPLAT_HOST_NAME:-$(uname -n)}
	FACT_OS="${name:-$PLATFORM} in a container on ${XPLAT_HOST_OS:-an unnamed host}"
	FACT_KERNEL="$(uname -r), the host kernel"
	FACT_MORE="$(uname -m), $libc, ${cc_said:-no cc}, ${py_said:-no python3}, $(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo), $(nproc) CPUs, $(awk '/^MemTotal/ { printf "%d GiB", $2 / 1048576 }' /proc/meminfo); ${XPLAT_ENGINE:-}; image ${XPLAT_IMAGE:-}"
}
# plat_sample DIR PID...: every thread of the container (ps), each process's threads' states and wait
# channels (/proc), and gdb's backtraces where ptrace lets this user attach (a sibling, under Yama's
# ptrace_scope 1, refuses it: the wait channels stand), every process at once. After a signal (QUICK), no
# gdb: its 15 s outlast kiln's 5 s before its KILL.
plat_sample() {
	local dir=$1 p t gdbs=""
	shift
	ps -ww -eLo pid,tid,ppid,pgid,etimes,stat,wchan:32,comm,args >"$dir/ps.txt" 2>&1
	for p in "$@"; do
		{
			echo "== $p: $(tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | cut -c1-300)"
			for t in /proc/"$p"/task/*; do
				echo "-- thread ${t##*/} $(cat "$t/comm" 2>/dev/null): state $(awk '{ print $3 }' "$t/stat" 2>/dev/null), wchan $(cat "$t/wchan" 2>/dev/null)"
			done
		} >"$dir/threads-$p.txt"
		[ -z "$QUICK" ] || continue
		timeout "$GDB_SECS" gdb -p "$p" -batch -ex "set pagination off" -ex "thread apply all bt" >"$dir/gdb-$p.txt" 2>&1 </dev/null &
		gdbs="$gdbs $!"
	done
	# A bare wait would wait on the suite and on the stdin watcher too: only gdbs, where there are any.
	# shellcheck disable=SC2086 # one argument per gdb
	[ -z "$gdbs" ] || wait $gdbs
}

# The VM process's userfaultfd hold (Shards D36, e3d6ef3445): shards-vm registers its own program's
# never-written segments with a write-protect userfaultfd for its life where the host lets it, and where
# refused (a seccomp profile without userfaultfd: Docker's default) says so at debug alone and runs on. The
# kernel marks a held mapping `uw` in its VmFlags (VM_UFFD_WP, /proc/pid/smaps; proc(5)). Each VM process a
# suite runs is looked at once (its pid and start time), on the judge's looks.
HOLD_SEEN=" " HOLD_SUITE='' HOLD_ON=0 HOLD_ALL=0
plat_watch() {
	local d key state
	if [ "$1" != "$HOLD_SUITE" ]; then HOLD_SUITE=$1 HOLD_ON=0 HOLD_ALL=0; fi
	for d in /proc/[0-9]*; do
		case $(readlink "$d/exe" 2>/dev/null) in */shards-vm) ;; *) continue ;; esac
		key="${d#/proc/}:$(awk '{ print $22 }' "$d/stat" 2>/dev/null)"
		case $HOLD_SEEN in *" $key "*) continue ;; esac
		HOLD_SEEN="$HOLD_SEEN$key "
		state=$(hold_of "$d")
		HOLD_ALL=$((HOLD_ALL + 1))
		case $state in held*) HOLD_ON=$((HOLD_ON + 1)) ;; esac
		say "HOLD: $1: shards-vm ${d#/proc/}: $state"
	done
}
# hold_of /proc/PID: "held" where each mapping of its program's code is marked `uw`, else "not held", with
# what of its code is resident (smaps' Rss).
hold_of() {
	local exe
	exe=$(readlink "$1/exe" 2>/dev/null) || {
		echo "unreadable"
		return 0
	}
	awk -v exe="$exe" '
		/^[0-9a-f]+-[0-9a-f]+ / { mine = ($NF == exe && $2 ~ /x/); if (mine) maps++; next }
		mine && /^Rss:/ { rss += $2 }
		mine && /^VmFlags:/ && / uw( |$)/ { held++ }
		END {
			if (!maps) { print "no mapping of its code read"; exit }
			printf "%s, its code %d kB resident in %d mapping(s)\n", (held == maps ? "held" : "not held"), rss, maps
		}' "$1/smaps" 2>/dev/null
}
# plat_measure STAGE: the run's asks measured (cpus, mem and disk, which the calls ask without measuring them):
# as STAGE begins, this container's cgroup's memory.peak (the most memory it has held at once since it started,
# cgroup v2's memory.peak) and its CPU time (cpu.stat usage_usec), with the CPUs that time came to over the stage
# before, and how much of the disk the run's folder takes (du -sm), said in the summary.
MEASURED_AT='' MEASURED_CPU=''
plat_measure() {
	local cg peak usage now used cpus='' said
	cg=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup 2>/dev/null)
	peak=$(cat "$cg/memory.peak" 2>/dev/null)
	usage=$(awk '$1 == "usage_usec" { print $2 }' "$cg/cpu.stat" 2>/dev/null)
	now=$(date +%s)
	used=$(du -sm "$W" 2>/dev/null | cut -f1)
	if [ -n "$MEASURED_AT" ] && [ -n "$usage" ] && [ "$now" -gt "$MEASURED_AT" ]; then
		cpus=$(awk -v u="$((usage - MEASURED_CPU))" -v t="$((now - MEASURED_AT))" 'BEGIN { printf "%.2f", u / 1e6 / t }')
	fi
	if [ -n "$peak" ]; then said="memory.peak $((peak / 1048576)) MiB"; else said="memory.peak unread"; fi
	if [ -n "$usage" ]; then said="$said, CPU $((usage / 1000000)) s${cpus:+ ($cpus CPUs over the stage before)}"; else said="$said, CPU unread"; fi
	say "measure: $1: $said, the run's folder ${used:-unread} MiB"
	MEASURED_AT=$now MEASURED_CPU=${usage:-0}
}
# plat_note SUITE: printed in a subshell, so it only reads what plat_watch counted for SUITE.
plat_note() {
	if [ "$1" = "$HOLD_SUITE" ]; then echo "; userfaultfd hold: held in $HOLD_ON of the $HOLD_ALL VM processes seen"; else echo "; userfaultfd hold: no VM process seen"; fi
}

main "$@"
