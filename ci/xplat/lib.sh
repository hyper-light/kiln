#!/bin/bash
# lib.sh: kiln's step35 harness suites through kiln, at a kiln revision with the Hex and the Shards it is
# given, on this runner's hypervisor. linux-in.sh (and its macOS and Windows siblings) set PLATFORM, C (this folder) and
# R (their folder on the runner), source this file, replace the hooks whose Unix forms (at its end)
# don't fit their platform, and call main KILN HEX SHARDS.
# Exit: 0 every stage and suite passed; 1 one did not; 2 an input, or this machine, was refused.
set -u

KILN_URL=https://github.com/hyper-light/kiln.git
HEX_URL=https://github.com/hyper-light/hex.git
SHARDS_URL=https://github.com/hyper-light/shards.git
KINDS="claude codex gemini opencode goose amp devin antigravity cursor pi"
# kiln's step35 harness suites, in this order.
SUITES="rest mcp narrowed engine"
# As the earlier runs of these suites had them, none picked here: a
# look every 5 s, so that a signal reaches the cleanup within seconds; 10 s for an ended tree before it
# is reaped; 5 s between a leftover's TERM and its KILL; 20 s between tries of a git transfer.
POLL=5 SETTLE=10 GRACE=5 RETRY_WAIT=20
EXE=
NICE=(nice -n 10)
CHILD='' CHILD_TOP='' SUITE='' SUITE_FROM='' BOUND='' IDENT='' PY=()
PASSED=1
# What a platform marks as this run's where it makes a folder outside W (the Windows run's own target), which the
# frees (let_go) and the cleanup remove; all under W is this run's.
MARK=.xplat-made
# Set by a signal: kiln's stop and timeout send TERM, then KILL 5 s later (kiln's crates/kiln/src/reach/node.rs,
# KILL_AFTER), within which the cleanup samples briefly, ends the run's tree and says its record first.
QUICK=''

say() {
	local line
	line="=== $(date -u +%H:%M:%SZ) $*"
	echo "$line" >&2
	if [ -n "${LOGS:-}" ] && [ -d "$LOGS" ]; then echo "$line" >>"$LOGS/summary.log"; fi
	return 0
}
die() {
	say "$*"
	exit 1
}
refuse() {
	say "refused: $*"
	exit 2
}
# result LINE: a line of the run's record, said now and again as it ends.
result() {
	echo "$*" >&2
	if [ -n "${LOGS:-}" ] && [ -d "$LOGS" ]; then echo "$*" >>"$LOGS/results"; fi
	return 0
}
verdict_of() { if [ "$1" -eq 0 ]; then echo PASS; else echo FAIL; fi; }
short() { echo "${1:0:9}"; }

# tries N CMD...: CMD until it succeeds, N times (3 for a clone, 5 for a checkout): GitHub
# drops a transfer now and then, on HTTP/1.1 too.
tries() {
	local n=$1 i
	shift
	for i in $(seq "$n"); do
		"$@" && return 0
		say "try $i of $n failed: $*"
		[ "$i" -lt "$n" ] && sleep "$RETRY_WAIT"
	done
	return 1
}
fresh_clone() {
	local d=$1
	shift
	rm -rf "$d"
	git clone -q "$@" "$d" </dev/null
}

is_bundle() {
	case $1 in *.bundle) return 0 ;; esac
	return 1
}
carrier_path() {
	case $1 in
	/* | [A-Za-z]:[/\\]*) echo "$1" ;;
	*) echo "$C/$1" ;;
	esac
}
# head_of BUNDLE: "SHA REF", the one commit BUNDLE's heads name.
head_of() {
	local heads n
	heads=$(git bundle list-heads "$1" 2>&1) || {
		echo "$1 is not a git bundle: $heads" >&2
		return 1
	}
	n=$(echo "$heads" | awk 'NF { print $1 }' | sort -u | wc -l | tr -d ' ')
	[ "$n" = 1 ] || {
		echo "$1 names $n commits, not one branch's" >&2
		return 1
	}
	echo "$heads" | awk 'NF { print $1, $2; exit }'
}
is_full_sha() {
	[ ${#1} -eq 40 ] || return 1
	case $1 in *[!0-9a-f]*) return 1 ;; esac
	return 0
}
# resolve URL REF: the commit REF names at URL: a full commit as it is; else a tag's commit, then a
# lightweight tag's, then a branch's.
resolve() {
	local url=$1 ref=$2 out want sha=
	if is_full_sha "$ref"; then
		echo "$ref"
		return 0
	fi
	out=$(tries 3 git ls-remote "$url" "refs/tags/$ref^{}" "refs/tags/$ref" "refs/heads/$ref" </dev/null) || return 1
	for want in "refs/tags/$ref^{}" "refs/tags/$ref" "refs/heads/$ref"; do
		sha=$(echo "$out" | awk -v r="$want" '$2 == r { print $1; exit }')
		[ -n "$sha" ] && break
	done
	[ -n "$sha" ] && echo "$sha"
}
# tags_at URL SHA: the tags at URL whose commit is SHA.
tags_at() {
	git ls-remote --tags "$1" </dev/null 2>/dev/null |
		awk -v s="$2" '$1 == s { t = $2; sub(/^refs\/tags\//, "", t); sub(/\^\{\}$/, "", t); printf "%s%s", (n++ ? " " : ""), t }'
}
pin_of() { sed -n "/^\[projects\.$1\]/,/^commit/s/^commit = \"\(.*\)\"/\1/p" "$K/vendor/MANIFEST.toml"; }

# find_python: a Python of 3.11 or later in PY, as kiln's scripts/vendor.py needs (tomllib).
find_python() {
	local p
	for p in python3.14 python3.13 python3.12 python3.11 python3 python; do
		command -v "$p" >/dev/null 2>&1 || continue
		"$p" -c 'import sys; sys.exit(sys.version_info < (3, 11))' >/dev/null 2>&1 </dev/null || continue
		PY=("$(command -v "$p")")
		return 0
	done
	plat_python
}

inputs() {
	local v b
	[ $# -eq 3 ] || refuse "usage: bash $(basename "$0") ${USAGE:-}KILN HEX SHARDS"
	KILN_IN=$1 HEX_IN=$2 SHARDS_IN=$3
	case $KILN_IN in "" | vendored) refuse "KILN is a kiln branch, tag or full commit on GitHub, or a bundle, not '$KILN_IN'" ;; esac
	for v in "$KILN_IN" "$HEX_IN" "$SHARDS_IN"; do
		[ -n "$v" ] || refuse "an input is empty"
		is_bundle "$v" || continue
		b=$(carrier_path "$v")
		[ -f "$b" ] || refuse "no bundle at $b"
		head_of "$b" >/dev/null || refuse "$b can't be used"
	done
	LINKS_FILE=${LINKS-links.json}
	[ -z "$LINKS_FILE" ] || LINKS_FILE=$(carrier_path "$LINKS_FILE")
	[ -z "$LINKS_FILE" ] || [ -f "$LINKS_FILE" ] || refuse "no links file at $LINKS_FILE"
	for v in git curl; do command -v "$v" >/dev/null 2>&1 || refuse "no $v on this runner"; done
	if [ "$HEX_IN" != vendored ] || [ "$SHARDS_IN" != vendored ]; then
		find_python || refuse "no Python 3.11 or later on this runner: kiln's scripts/vendor.py needs one (tomllib) to re-vendor"
	fi
	return 0
}

# begin: the lock (one run at a time in R), the logs, the cleanup's traps, and a fresh W.
begin() {
	local other
	W=$R/w
	LOGS=${XPLAT_LOGS:-$R/logs/$(date -u +%Y%m%dT%H%M%SZ)}
	mkdir -p "$R/logs" || refuse "can't make $R"
	if ! mkdir "$R/lock" 2>/dev/null; then
		other=$(cat "$R/lock/pid" 2>/dev/null)
		[ -n "$other" ] && kill -0 "$other" 2>/dev/null && refuse "another run (pid $other) is using $W on this runner: run again once it has ended"
		rm -rf "$R/lock"
		mkdir "$R/lock" || refuse "can't take $R/lock"
	fi
	echo $$ >"$R/lock/pid"
	mkdir -p "$LOGS"
	trap cleanup EXIT
	trap 'QUICK=1; exit 143' TERM
	trap 'QUICK=1; exit 130' INT
	trap 'QUICK=1; exit 129' HUP
	if [ -e "$W" ]; then
		say "a run that ended before its cleanup left $W: ending what it left running, removing it"
		sweep
		remove_tree "$W"
	fi
	mkdir -p "$W/t" || exit 1
	# GitHub stalled HTTP/2 transfers on a busy host (2026-11-16): every git fetch over HTTP/1.1, vendor.py's
	# and a blobless clone's lazy ones included.
	export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.version GIT_CONFIG_VALUE_0=HTTP/1.1 GIT_TERMINAL_PROMPT=0
	# One rustc per CPU the run asks for (JOBS). Line tables alone for a test's backtraces, as
	# kiln's own CI builds kiln's tests (.github/workflows/ci.yml, `linux`): its crate takes 7 GiB and more.
	export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=${JOBS:-8} CARGO_TERM_COLOR=never NO_COLOR=1 CARGO_PROFILE_DEV_DEBUG=line-tables-only
	export KILN_SHARDS_PROFILE=test-release
}

paths() {
	if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then refuse "no cargo and rustc on this runner"; fi
	HOST=$(rustc -vV | sed -n 's/^host: //p')
	XP_TARGET=${XP_TARGET:-$W/target}
	CARRIED=$W/carried/$HOST
	export CARGO_TARGET_DIR KILN_SHARDS_DIR S16_DIR KILN_VENDOR_CACHE
	CARGO_TARGET_DIR=$(plat_path "$XP_TARGET")
	KILN_SHARDS_DIR=$(plat_path "$CARRIED")
	S16_DIR=$(plat_path "$W/s16")
	KILN_VENDOR_CACHE=$(plat_path "$W/vendor")
}

facts() {
	local carrier
	plat_facts
	carrier=$(git -C "$C" rev-parse --short=9 HEAD 2>/dev/null) || carrier="none"
	# Content alone: Git Bash reports a script's mode by its shebang, so modes differ on Windows unchanged.
	if [ "$carrier" != none ] && ! git -C "$C" --no-optional-locks -c core.fileMode=false diff --quiet HEAD -- 2>/dev/null; then
		carrier="$carrier with changes not committed"
	fi
	IDENT="$FACT_HOST, $FACT_OS, kernel $FACT_KERNEL"
	result "RECORD $PLATFORM host: $FACT_HOST; $FACT_OS; kernel $FACT_KERNEL; $FACT_MORE; $(rustc -V); carrier $carrier"
}

kiln_checkout() {
	local b head prereq
	K=$W/kiln
	tries 3 fresh_clone "$K" --filter=blob:none --no-checkout "$KILN_URL" || die "kiln: clone failed"
	if is_bundle "$KILN_IN"; then
		b=$(carrier_path "$KILN_IN")
		git -C "$K" bundle verify -q "$b" >"$LOGS/verify-kiln.log" 2>&1 ||
			refuse "kiln: $b needs commits GitHub's kiln lacks: $(tail -3 "$LOGS/verify-kiln.log" | tr '\n' ' ')"
		head=$(head_of "$b")
		KILN_SHA=${head%% *}
		# The bundle's base checked out first, so that the delta bases of its thin pack are here rather than
		# fetched one by one as it is read.
		prereq=$(LC_ALL=C awk '/^$/ { exit } /^-/ { print substr($1, 2); exit }' "$b")
		[ -z "$prereq" ] || tries 5 git -C "$K" checkout -q -f --detach "$prereq" || die "kiln: checkout of $prereq failed"
		tries 3 git -C "$K" fetch -q "$b" "+${head#* }:refs/heads/xplat" || die "kiln: fetch of $b failed"
		KILN_FROM="bundle $(basename "$b") (${head#* })"
	else
		KILN_SHA=$(git -C "$K" rev-parse -q --verify "$KILN_IN^{commit}" 2>/dev/null ||
			git -C "$K" rev-parse -q --verify "origin/$KILN_IN^{commit}" 2>/dev/null ||
			resolve "$KILN_URL" "$KILN_IN") || refuse "kiln: no branch, tag or commit $KILN_IN on GitHub"
		git -C "$K" cat-file -e "$KILN_SHA^{commit}" 2>/dev/null || tries 3 git -C "$K" fetch -q origin "$KILN_SHA" ||
			die "kiln: fetch of $KILN_SHA failed"
		KILN_FROM="GitHub's $KILN_IN"
	fi
	tries 5 git -C "$K" checkout -q -f --detach "$KILN_SHA" || die "kiln: checkout of $KILN_SHA failed"
	BASE=$(git -C "$K" merge-base "$KILN_SHA" origin/main) || die "kiln: $KILN_SHA shares no commit with kiln main"
	result "RECORD $PLATFORM kiln: $KILN_SHA from $KILN_FROM, on kiln main's $(short "$BASE"): $(git -C "$K" log -1 --format=%s | cut -c1-100)"
}

vendor() { "${PY[@]}" "$(plat_path "$K/scripts/vendor.py")" "$@" >>"$LOGS/vendor.log" 2>&1 </dev/null; }
# stopped_at DIR: the recorded change a stopped re-vendor waits on, and its conflicted files (vendor.py's state).
stopped_at() {
	"${PY[@]}" -c 'import json, sys; r = json.load(open(sys.argv[1])); print(r["plan"][r["at"]]["file"], *r["conflicted"])' \
		"$(plat_path "$1/.git/kiln-vendor-replay.json")" </dev/null
}
# take_0003 DIR: Hex's manifest resolved to kiln's change 0003's side where Hex moved what 0003 deletes
# (the shards-gpu entry's pin): 0003's hunks win, and Hex's other changes stay.
take_0003() {
	local f=vendor/MANIFEST.toml n
	for n in 1 2 3; do git -C "$1" show ":$n:$f" >"$W/manifest.$n" || return 1; done
	git merge-file --theirs "$W/manifest.2" "$W/manifest.1" "$W/manifest.3" || return 1
	! grep -q '^\[projects\.shards-gpu\]' "$W/manifest.2" || return 1
	cp "$W/manifest.2" "$1/$f"
}
# revendor NAME SHA [FROM]: kiln's NAME re-vendored at SHA by kiln's scripts/vendor.py (fetched from the
# clone FROM where SHA is no commit on GitHub), the gate left to the caller.
revendor() {
	local name=$1 sha=$2 from=$3 rc pending=$W/vendor/conflict/$1 extra=()
	[ -z "$from" ] || extra=(--from "$(plat_path "$from")")
	say "vendor: $name at $sha"
	vendor "$name" "$sha" ${extra[@]+"${extra[@]}"} --no-gate
	rc=$?
	if [ $rc -ne 0 ] && [ "$name" = hex ] && [ -d "$pending" ] &&
		[ "$(stopped_at "$pending")" = "0003-no-gpu-renderer-build-of-hex-s-in-kiln-s-copy.patch vendor/MANIFEST.toml" ]; then
		take_0003 "$pending" || die "vendor: 0003's conflict did not resolve to its side"
		say "vendor: 0003 conflicted on the shards-gpu entry's moved pin; resolved to 0003's side, then --continue"
		vendor hex --continue --no-gate
		rc=$?
	fi
	if [ $rc -ne 0 ]; then
		[ -d "$pending" ] && say "vendor: stopped at a conflict left to a person: $(stopped_at "$pending")"
		die "vendor $name: exit $rc: $(grep -a '^vendor.py:' "$LOGS/vendor.log" | tail -4 | tr '\n' ' ' | cut -c1-700)"
	fi
}
# take NAME INPUT URL: kiln's vendored NAME as INPUT has it: as kiln vendors it, or
# re-vendored at a commit of NAME on GitHub or in a bundle, unless kiln pins that commit already. Sets
# TAKEN (the commit asked for, or nothing), HOW (what was done) and FROM (the clone vendor.py fetched from).
take() {
	local name=$1 in=$2 url=$3 b head was
	TAKEN='' FROM='' HOW="as kiln vendors it"
	[ "$in" = vendored ] && return 0
	was=$(pin_of "$name")
	if is_bundle "$in"; then
		b=$(carrier_path "$in")
		head=$(head_of "$b")
		TAKEN=${head%% *}
		# Main whole: vendor.py fetches from this clone, and a blobless one serves no blob.
		FROM=$W/$name.git
		tries 3 fresh_clone "$FROM" --bare --single-branch --branch main "$url" || die "$name: clone failed"
		git -C "$FROM" bundle verify -q "$b" >"$LOGS/verify-$name.log" 2>&1 ||
			refuse "$name: $b needs commits $name's main lacks: $(tail -3 "$LOGS/verify-$name.log" | tr '\n' ' ')"
		git -C "$FROM" fetch -q "$b" "+${head#* }:refs/heads/xplat" || die "$name: fetch of $b failed"
		in="$in (${head#* })"
	else
		TAKEN=$(resolve "$url" "$in") || refuse "$name: no branch, tag or full commit $in on GitHub"
	fi
	if [ "$TAKEN" = "$was" ]; then
		HOW="as kiln vendors it, which is $in"
		return 0
	fi
	HOW="re-vendored here at $in, in the place of kiln's $(short "$was")"
	revendor "$name" "$TAKEN" "$FROM"
}

# vendor_inputs: Shards first, then Hex, whose pin of Shards must then agree with kiln's (vendor.py's rule).
vendor_inputs() {
	cd "$K" || exit 1
	take shards "$SHARDS_IN" "$SHARDS_URL"
	SHARDS_WANT=$TAKEN SHARDS_HOW=$HOW SHARDS_FROM=$FROM
	take hex "$HEX_IN" "$HEX_URL"
	HEX_WANT=$TAKEN HEX_HOW=$HOW HEX_FROM=$FROM
	SHARDS_PIN=$(pin_of shards) HEX_PIN=$(pin_of hex)
	[ -z "$SHARDS_WANT" ] || [ "$SHARDS_PIN" = "$SHARDS_WANT" ] || die "vendor: kiln pins Shards ${SHARDS_PIN:-nothing}, not $SHARDS_WANT"
	[ -z "$HEX_WANT" ] || [ "$HEX_PIN" = "$HEX_WANT" ] || die "vendor: kiln pins Hex ${HEX_PIN:-nothing}, not $HEX_WANT"
	SHARDS_TAGS=$(tags_at "$SHARDS_URL" "$SHARDS_PIN")
	result "RECORD $PLATFORM hex: $HEX_PIN, $HEX_HOW"
	result "RECORD $PLATFORM shards: $SHARDS_PIN, $SHARDS_HOW; Shards' tags at it: ${SHARDS_TAGS:-none}"
	IDENT="$IDENT; kiln $(short "$KILN_SHA"), hex $(short "$HEX_PIN"), shards $(short "$SHARDS_PIN")${SHARDS_TAGS:+ ($SHARDS_TAGS)}"
	vendor_check
	# vendor.py's last reader is the check: its fetches (KILN_VENDOR_CACHE) and a bundle's clones go now.
	let_go vendor-check "$W/vendor" ${HEX_FROM:+"$HEX_FROM"} ${SHARDS_FROM:+"$SHARDS_FROM"}
}
# vendor_check: kiln's check of vendored code (its AGENTS.md: scripts/vendor.py check) on the two projects
# the goal names: each tree its upstream at its pin plus kiln's recorded changes, and the pins agreeing.
vendor_check() {
	local rc extra=()
	if [ ${#PY[@]} -eq 0 ] && ! find_python; then
		result "RESULT $PLATFORM vendor-check NOT RUN: no Python 3.11 or later on this runner; $IDENT"
		PASSED=0
		return 0
	fi
	[ -z "$HEX_FROM" ] || extra+=(--from "hex=$(plat_path "$HEX_FROM")")
	[ -z "$SHARDS_FROM" ] || extra+=(--from "shards=$(plat_path "$SHARDS_FROM")")
	say "vendor check: hex shards"
	waited "$LOGS/vendor-check.log" "${PY[@]}" "$(plat_path "$K/scripts/vendor.py")" check hex shards ${extra[@]+"${extra[@]}"}
	rc=$?
	[ $rc -eq 0 ] || PASSED=0
	result "RESULT $PLATFORM vendor-check $(verdict_of $rc): exit $rc: $(grep -a '^vendor.py:' "$LOGS/vendor-check.log" | tail -3 | tr '\n' ' ' | cut -c1-400); $IDENT"
}

# head_on_github: a bundle's commits are on no GitHub, and the stand-in's image fetches its harness from
# GitHub at HEAD (vm_e2e::this_commit): HEAD moves to the bundle's base on kiln main, the tree as it is,
# where the stand-in's files are the same there.
head_on_github() {
	is_bundle "$KILN_IN" || return 0
	git -C "$K" reset -q --soft "$BASE" || die "kiln: reset to $BASE failed"
	git -C "$K" diff --quiet "$BASE" -- crates/kiln/tests/fixtures/vm-stub/harness ||
		die "kiln: the stand-in's harness here differs from $BASE's, which its image fetches from GitHub: stopped"
	result "RECORD $PLATFORM kiln: HEAD at kiln main's $(short "$BASE") with $(short "$KILN_SHA")'s tree, for the stand-in's fetch from GitHub"
}

start_child() {
	local log=$1
	shift
	"$@" >"$log" 2>&1 </dev/null &
	CHILD=$!
	CHILD_TOP=$(plat_root "$CHILD")
}
# waited LOG CMD...: CMD in the background, its output in LOG, waited on, so that a signal reaches the
# cleanup at once (which ends CMD's tree). No bound: a build has no judged wait.
waited() {
	local log=$1 rc
	shift
	start_child "$log" "$@"
	wait "$CHILD"
	rc=$?
	CHILD='' CHILD_TOP=''
	return $rc
}

build() {
	local rc lock
	cd "$K" || exit 1
	plat_disk smoke
	plat_measure smoke
	say "smoke: docs/research/s16-vendor/smoke.sh (Shards packed at $KILN_SHARDS_PROFILE, then a guest booted by the carried VM process)"
	waited "$LOGS/smoke.log" ${NICE[@]+"${NICE[@]}"} bash docs/research/s16-vendor/smoke.sh
	rc=$?
	[ $rc -eq 0 ] || PASSED=0
	result "RESULT $PLATFORM smoke $(verdict_of $rc): exit $rc; $(grep -a '^pack:' "$LOGS/smoke.log" | head -1 | cut -c1-120); $(grep -a '^test result' "$LOGS/smoke.log" | tail -1 | cut -c1-80); $IDENT"
	if [ ! -f "$CARRIED/shards$EXE" ]; then
		grep -aE '^error|^  -->|error:' "$LOGS/smoke.log" | head -30 >&2
		tail -30 "$LOGS/smoke.log" >&2
		die "smoke: no carried shards in $CARRIED: stopped"
	fi
	# Smoke's alone, smoke.sh their last reader: pack.sh's build of Shards (its target, and its build script's
	# helpers' target, CARGO_TARGET_DIR/helpers: the three binaries it made are in CARRIED, which kiln's build script
	# reads to the run's end), smoke's own test binary, and its kernel and init (S16_DIR).
	let_go smoke "$XP_TARGET/shards" "$XP_TARGET/helpers" "$W/s16" "$XP_TARGET"/debug/deps/shards_carried-*
	plat_disk build
	plat_measure build
	say "build: vm_harnesses"
	waited "$LOGS/build.log" ${NICE[@]+"${NICE[@]}"} cargo test -p kiln --test vm_harnesses --no-run
	rc=$?
	lock=$(git diff --stat "$KILN_SHA" -- Cargo.lock | tail -1)
	if [ $rc -ne 0 ]; then
		grep -aE '^error|^  -->' "$LOGS/build.log" | head -40 >&2
		result "RESULT $PLATFORM build FAIL: exit $rc; Cargo.lock against kiln $(short "$KILN_SHA"): ${lock:-unchanged}; $IDENT"
		die "build: exit $rc: stopped"
	fi
	result "RECORD $PLATFORM build: vm_harnesses built; Cargo.lock against kiln $(short "$KILN_SHA"): ${lock:-unchanged}"
}

seed_pull() {
	local b rc=0 home dc cache
	home=$(plat_path "$W/seed/home")
	dc=$(plat_path "$W/seed/docker")
	# Shards' own cache (its helpers, a kernel) in the run's folder, freed after seeding, rather than this user's,
	# where nothing removes it (Shards' helpers.rs `cache`: SHARDS_CACHE where it names a directory).
	cache=$(plat_path "$W/seed/cache")
	mkdir -p "$W/seed/home" "$W/seed/docker" "$W/seed/cache"
	while read -r b; do
		echo "seed: $b"
		SHARDS_HOME=$home SHARDS_CACHE=$cache DOCKER_CONFIG=$dc "$CARRIED/shards$EXE" pull "$b" </dev/null || rc=1
	done < <(grep -h '^ARG BASE=' .kiln/images/*/Agentfile | sed 's/^ARG BASE=//' | sort -u)
	SHARDS_HOME=$home SHARDS_CACHE=$cache DOCKER_CONFIG=$dc "$CARRIED/shards$EXE" stop daemon </dev/null
	return $rc
}
# seed: the images' bases, pulled once into a store each test's runner copies (VM_E2E_SHARDS_STORE), as
# Docker Hub's pull limits ask.
seed() {
	local rc
	plat_measure seed
	say "seed: the images' bases into $W/seed/home"
	waited "$LOGS/seed.log" seed_pull
	rc=$?
	if [ $rc -ne 0 ]; then
		grep -aiE 'too many requests|429' "$LOGS/seed.log" | head -3 >&2
		result "RESULT $PLATFORM seed FAIL: exit $rc: $(tail -2 "$LOGS/seed.log" | tr '\n' ' ' | cut -c1-200); $IDENT"
		die "seed: the images' bases were not pulled: stopped"
	fi
	# The seeding was its cache's last reader. The store it filled stays: every suite's runners copy it.
	let_go seed "$W/seed/cache"
}

# window_now LOG: the window, in whole seconds, that the suite gave the wait it is in: its last WAIT:
# line's "refused after <Duration> without progress". None where that wait has none (a cold start) or
# where kiln said it holds the wait's message ("held by kiln, the wait going on"): the suite's own waits
# follow liveness alone there (vm_e2e::Pace; kiln's docs/research/s35-every-harness/waits/README.md).
window_now() {
	local line d
	line=$(grep -aE '^WAIT: |: held by kiln, the wait going on: ' "$1" | tail -n 1)
	case $line in
	"WAIT: "*": refused after "*" without progress") ;;
	*) return 0 ;;
	esac
	d=${line##*: refused after }
	d=${d% without progress}
	# Rust's Debug of a Duration (47.25s, 850ms, 12µs, 7ns), rounded up to whole seconds.
	awk -v d="$d" 'BEGIN { n = d + 0; if (d ~ /[0-9]ms$/) n /= 1000; else if (d !~ /[0-9]s$/) n /= 1000000; s = int(n); if (s < n) s++; print s }'
}
# judge NAME LOG: suite NAME (CHILD) bounded by its own judged waits, not by a time. A wait it says
# it begins with a window is one it refuses once nothing of its session moves for that window. kiln writes
# each update of a session to its store as it comes (kiln's docs/conversations.md, "Per event"), and the
# run's stores, logs and homes are all under W/t: once nothing there moves and the suite says nothing for
# a whole window of the wait it is in, the suite's own refusal is due. The run is then sampled, and ended
# unless it moved meanwhile. A wait with no window, or one kiln holds, follows the suite's liveness alone.
judge() {
	local name=$1 log=$2 size=-1 now newest window='' said=$SECONDS moved=$SECONDS noted=''
	: >"$W/judge.seen"
	while kill -0 "$CHILD" 2>/dev/null; do
		sleep "$POLL"
		plat_watch "$name"
		keep_look "$name"
		: >"$W/judge.next"
		now=$(wc -c <"$log" | tr -d ' ')
		newest=$(find "$W/t" -newer "$W/judge.seen" -print 2>/dev/null | head -n 1)
		mv -f "$W/judge.next" "$W/judge.seen"
		if [ "$now" != "$size" ]; then
			size=$now said=$SECONDS moved=$SECONDS noted=
			window=$(window_now "$log")
			continue
		fi
		[ -z "$newest" ] || moved=$SECONDS
		[ -n "$window" ] || continue
		if [ -z "$noted" ] && [ -n "$newest" ] && [ $((SECONDS - said)) -ge "$window" ]; then
			say "JUDGE: $name has said nothing for its wait's window (${window}s) while its run's files move ($newest)"
			noted=1
		fi
		[ $((SECONDS - moved)) -ge "$window" ] || continue
		say "JUDGE: nothing of $name's run has moved for $((SECONDS - moved))s, its wait's window (${window}s): sampling ($(grep -a '^WAIT: ' "$log" | tail -n 1 | cut -c1-200))"
		# shellcheck disable=SC2046 # one argument per process
		sample_set "$name" $(run_pids "$CHILD_TOP")
		: >"$W/judge.next"
		now=$(wc -c <"$log" | tr -d ' ')
		newest=$(find "$W/t" -newer "$W/judge.seen" -print 2>/dev/null | head -n 1)
		mv -f "$W/judge.next" "$W/judge.seen"
		if [ "$now" != "$size" ] || [ -n "$newest" ]; then
			say "JUDGE: $name moved while it was sampled: its wait goes on"
			size=-1
			continue
		fi
		BOUND="nothing of its run moved for $((SECONDS - moved))s, its wait's window being ${window}s"
		say "BOUND: $name: $BOUND: ending it"
		end_child
		return 0
	done
}

# sample_set NAME PID...: each process's state, in LOGS/samples, before it is ended (plat_sample).
sample_set() {
	local dir
	dir=$LOGS/samples/$1-$(date -u +%H%M%SZ)
	shift
	if [ $# -eq 0 ]; then
		say "sample: no process of this run to sample"
		return 0
	fi
	mkdir -p "$dir"
	plat_sample "$dir" "$@"
	say "sample: $# process(es) of this run, in $dir"
}
end_child() {
	[ -n "$CHILD_TOP" ] || return 0
	kill_tree TERM "$CHILD_TOP"
	sleep "$SETTLE"
	kill_tree KILL "$CHILD_TOP"
}
# sweep: every process left whose command line names W (this run's tests, daemons and VMs), sampled (but
# after a signal, whose cleanup sampled them already), then ended; fails when there was one.
sweep() {
	local left
	left=$(run_pids 0 | tr '\n' ' ')
	[ -n "${left// /}" ] || return 0
	say "left running under $W: $(echo "$left" | wc -w | tr -d ' ') process(es): ended"
	# shellcheck disable=SC2086 # one argument per process
	[ -n "$QUICK" ] || sample_set leftover $left
	# shellcheck disable=SC2086
	end_pids $left
	return 1
}
remove_tree() {
	chmod -R u+w "$1" 2>/dev/null
	rm -rf "$1"
}
# made_here PATH: whether PATH is this run's own: under W, which the run makes and removes, or in a folder of
# XP_TARGET's that this run made and marked (MARK; the Windows run's own target), that folder itself included.
made_here() {
	local d=$1
	case $d in "$W"/*) return 0 ;; esac
	[ -n "${XP_TARGET:-}" ] || return 1
	while :; do
		[ -f "$d/$MARK" ] && return 0
		[ "$d" != "$XP_TARGET" ] || return 1
		case $d in "$XP_TARGET"/*) d=${d%/*} ;; *) return 1 ;; esac
	done
}
# let_go STAGE PATH...: what STAGE left that no later stage reads, removed now that its last reader is done. Each
# path this run's own (made_here); another's is kept, and said. The disk's free space is said just before and after
# (plat_free), so each run's summary measures what each freed.
let_go() {
	local stage=$1 p
	local freed=()
	shift
	for p in "$@"; do
		[ -e "$p" ] || continue
		if made_here "$p"; then freed+=("$p"); else say "kept $p after $stage: not this run's"; fi
	done
	[ ${#freed[@]} -gt 0 ] || return 0
	say "freeing after $stage, $(plat_free) before: ${freed[*]#"$W"/}"
	for p in "${freed[@]}"; do remove_tree "$p"; done
	say "freed after $stage: $(plat_free) now"
}

# The kiln under test's own logs. Each test's server and runners write theirs in the
# test's own folder under W/t, which the test removes as it ends: kiln's tests/common `start`, server.log;
# vm_e2e's `runner`, NAME.log; its Shards home, NAME-state/shards-home/daemon.log. On each of the judge's looks
# keep_look SUITE links each such file into LOGS/kiln/SUITE/.live as it appears: a hard link, the same file,
# so what kiln writes to it later is in it too, and the test's removal leaves it. Once for each file a path
# names; a file that comes and goes between two looks (POLL apart) is missed.
stamp() { date -u +%Y-%m-%dT%H:%M:%S; }
keep_look() {
	local live=$LOGS/kiln/$1/.live f n
	mkdir -p "$live" 2>/dev/null || return 0
	[ -f "$live/index" ] || : >"$live/index"
	# Depth 4 reaches each test's own logs and its runners' Shards homes', never the image stores below them.
	find "$W/t" -maxdepth 4 -type f \( -name '*.log' -o -name '*.log.[0-9]' \) 2>/dev/null |
		while IFS= read -r f; do
			linked "$live" "$f" && continue
			n=$(($(wc -l <"$live/index") + 1))
			ln "$f" "$live/$n" 2>/dev/null || continue
			printf '%s\t%s\n' "$n" "${f#"$W/t/"}" >>"$live/index"
		done
	return 0
}
# linked LIVE FILE: whether the file FILE's path names now is linked in LIVE already.
linked() {
	local n p
	while IFS=$'\t' read -r n p; do
		if [ "$p" = "${2#"$W/t/"}" ] && [ "$2" -ef "$1/$n" ]; then return 0; fi
	done <"$1/index"
	return 1
}
# window_cut FROM TO: stdin's lines stamped from FROM to TO, as `stamp` writes them (UTC, to the second).
# kiln stamps each line so, RFC 3339 first (its default text log), on the clock FROM and TO were read from:
# this host's. A line with no stamp goes with the stamped line before it; the lines before the first stamp
# are kept, so a file with no stamp at all is kept whole (keep_look linked it as it appeared, in the suite).
window_cut() {
	awk -v from="$1" -v to="$2" '
		/^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]/ { t = substr($0, 1, 19); keep = (t >= from && t <= to); stamped = 1 }
		keep || !stamped'
}
# keep_end SUITE VERDICT LOG: what keep_look linked, each cut to the suite's window (window_cut, SUITE_FROM to
# now) into LOGS/kiln/SUITE as N-<its path under W/t, / as _>. Each test's own logs, one folder down (its
# server's and its runners'), from every suite; the deeper ones (a runner's Shards home's) where the suite
# didn't pass, for the harness agents' sessions. Then two RECORD lines: what was kept, with its count of
# headroom's lines, and the strain its runners logged (strain_of).
keep_end() {
	local name=$1 verdict=$2 log=$3 dir=$LOGS/kiln/$1 to n p out files=0 bytes=0 headroom=0
	to=$(stamp)
	if [ -f "$dir/.live/index" ]; then
		while IFS=$'\t' read -r n p; do
			case $p in */*/*) [ "$verdict" != PASS ] || continue ;; esac
			out=$dir/$n-$(printf '%s' "$p" | tr '/' '_')
			window_cut "$SUITE_FROM" "$to" <"$dir/.live/$n" >"$out" 2>/dev/null || continue
			files=$((files + 1))
			bytes=$((bytes + $(wc -c <"$out")))
			headroom=$((headroom + $(grep -ac 'headroom' "$out")))
		done <"$dir/.live/index"
		rm -rf "$dir/.live"
	fi
	result "RECORD $PLATFORM $name kiln logs: $files file(s), $bytes bytes, in kiln/$name, cut to ${SUITE_FROM}Z..${to}Z; $headroom line(s) of headroom's"
	result "RECORD $PLATFORM $name strain: $(strain_of "$dir" "$log")"
}
# strain_of DIR LOG: the strain the suite's runners logged in the logs kept in DIR, as kiln eb798aba1 and later
# log it ("placement: this machine is under strain: ... why=..." as it begins, "placement: this machine is no
# longer under strain" as it ends), and the starts its drives saw queued for it: kiln's queued words, "machine
# under strain", in LOG's FAILED: and WAIT: lines, which earlier kilns say too.
strain_of() {
	local dir=$1 log=$2 queued said='' q=''
	queued=$(grep -aE '^(FAILED|WAIT): ' "$log" 2>/dev/null | grep -ac 'machine under strain')
	[ "${queued:-0}" -gt 0 ] && q="; $queued start(s) seen queued for it"
	set -- "$dir"/[0-9]*-*
	if [ -e "$1" ]; then
		said=$(awk '
			FNR == 1 { open[FILENAME] = 0 }
			/placement: this machine is under strain/ {
				on++
				if (first == "" || substr($0, 12, 8) < first) { first = substr($0, 12, 8); why = $0; sub(/.* why=/, "", why) }
				open[FILENAME] = 1
			}
			/placement: this machine is no longer under strain/ {
				open[FILENAME] = 0
				if (substr($0, 12, 8) > last) last = substr($0, 12, 8)
			}
			END {
				if (!on) exit
				left = 0
				for (f in open) if (open[f]) left++
				printf "%d onset(s), the first at %sZ: %s", on, first, substr(why, 1, 140)
				if (left) printf "; still strained at the end in %d runner log(s)", left
				else printf "; each ended, the last at %sZ", last
			}' "$@" 2>/dev/null)
	fi
	if [ -n "$said" ]; then
		echo "$said$q"
	elif [ -n "$q" ]; then
		echo "none logged by its runners (kilns before eb798aba1 log none)$q"
	else
		echo "none logged"
	fi
}

filter_of() {
	case $1 in
	rest) echo step35::rest:: ;;
	mcp) echo step35::mcp:: ;;
	narrowed) echo narrowed:: ;;
	engine) echo engine:: ;;
	esac
}
suite() {
	local name=$1 filter log=$LOGS/$1.log rc summary passed failed verdict
	filter=$(filter_of "$name")
	plat_disk "$name"
	plat_measure "$name"
	say "$name: $filter, bounded by its own judged waits"
	SUITE=$name BOUND='' SUITE_FROM=$(stamp)
	start_child "$log" ${NICE[@]+"${NICE[@]}"} cargo test -p kiln --test vm_harnesses "$filter" -- --nocapture --test-threads 1
	judge "$name" "$log"
	wait "$CHILD"
	rc=$?
	CHILD='' CHILD_TOP='' SUITE=''
	summary=$(grep -a '^test result' "$log" | tail -n 1 | cut -c1-110)
	passed=$(echo "$summary" | sed -n 's/.* \([0-9][0-9]*\) passed.*/\1/p')
	failed=$(echo "$summary" | sed -n 's/.* \([0-9][0-9]*\) failed.*/\1/p')
	if [ -n "$BOUND" ]; then
		verdict=BOUND
	elif [ $rc -ne 0 ]; then
		verdict=FAIL
	elif [ "${passed:-0}" -gt 0 ] && [ "${failed:-0}" -eq 0 ]; then
		verdict=PASS
	else
		verdict=NONE
	fi
	[ "$verdict" = PASS ] || PASSED=0
	grep -aE '^test .* \.\.\. |^test result|^(SKIP|GAP|FAILED):|^Error: |of the (drives|checks) failed|panicked' "$log" | cut -c1-300 | head -40 >&2
	result "RESULT $PLATFORM $name $verdict: exit $rc; ${summary:-no test result}; $(grep -ac '^SKIP' "$log") SKIP, $(grep -ac '^FAILED: ' "$log") FAILED:${BOUND:+; $BOUND}$(plat_note "$name"); $IDENT"
	keep_end "$name" "$verdict" "$log"
	sweep || result "RECORD $PLATFORM $name: left processes running, sampled and ended ($LOGS/samples)"
	# What the suite's tests left in TMPDIR (a killed test's folder with its copy of the seeded store; on Windows a
	# folder its test couldn't remove): its logs are kept (keep_end), and no later suite reads it.
	let_go "$name" "$W/t"
	mkdir -p "$W/t"
}
run_suites() {
	local k kind s
	cd "$K" || exit 1
	unset VM_E2E_PROMPT VM_E2E_RESUMED_PROMPT VM_E2E_STEERED_PROMPT VM_E2E_STOPPED_PROMPT VM_E2E_DONE_MARKER VM_E2E_SERVER \
		VM_E2E_TOKEN VM_E2E_MACHINE VM_E2E_RUNNER_PID VM_E2E_RUNNER_LOG VM_E2E_PROJECT VM_E2E_KINDS VM_E2E_HOST_ROOTS VM_E2E_LINKS
	export VM_E2E=1 VM_E2E_SHARDS_STORE
	VM_E2E_SHARDS_STORE=$(plat_path "$W/seed/home")
	if [ -n "$LINKS_FILE" ]; then
		export VM_E2E_LINKS
		VM_E2E_LINKS=$(cat "$LINKS_FILE")
		result "RECORD $PLATFORM links: VM_E2E_LINKS from $(basename "$LINKS_FILE")"
	else
		result "RECORD $PLATFORM links: none (LINKS is empty): every step35 drive fails at check_all_links"
	fi
	plat_tmp "$W/t"
	for k in $KINDS; do
		kind=$(echo "$k" | tr '[:lower:]' '[:upper:]')
		unset "VM_E2E_OWN_$kind"
		export "VM_E2E_IMAGE_$kind=stub"
	done
	for s in $SUITES; do suite "$s"; done
	plat_measure end
}

cleanup() {
	local rc=$?
	trap - EXIT
	trap '' TERM INT HUP
	if [ -n "$CHILD" ]; then
		# Only a signal ends this script while a child runs (QUICK): sampled briefly, its tree told to end.
		say "ended from outside${SUITE:+ during $SUITE} (exit $rc): its processes sampled, then ended"
		# shellcheck disable=SC2046 # one argument per process
		sample_set "${SUITE:-stage}" $(run_pids "${CHILD_TOP:-0}")
		kill_tree TERM "$CHILD_TOP"
		if [ -n "$SUITE" ]; then
			result "RESULT $PLATFORM $SUITE ENDED: the run was ended from outside while it ran (exit $rc), sampled first; $IDENT"
			keep_end "$SUITE" ENDED "$LOGS/$SUITE.log"
		fi
	fi
	# The record before the removal, which a KILL may cut short (the next run removes what it leaves).
	if [ -f "${LOGS:-}/results" ]; then
		echo "=== results: $PLATFORM; logs in $LOGS on $(uname -n)" >&2
		cat "$LOGS/results" >&2
	fi
	if [ -n "${W:-}" ] && [ -e "$W" ]; then
		sweep
		remove_tree "$W"
	fi
	plat_cleanup
	rm -rf "$R/lock"
	if [ -n "${W:-}" ] && [ -e "$W" ]; then say "cleanup: $W is still there; exit $rc"; else say "cleanup: done; exit $rc"; fi
	exit "$rc"
}

main() {
	inputs "$@"
	begin
	plat_setup
	paths
	facts
	plat_measure checkout
	kiln_checkout
	plat_measure vendor
	vendor_inputs
	head_on_github
	plat_measure toolchain
	plat_toolchain
	build
	seed
	run_suites
	[ "$PASSED" = 1 ]
}

# The hooks, as Unix has them; a platform's script replaces those that differ there.
plat_setup() { export PATH="$HOME/.cargo/bin:/usr/local/bin:$PATH"; }
plat_path() { printf '%s\n' "$1"; }
plat_tmp() { export TMPDIR="$1"; }
plat_root() { echo "$1"; }
plat_python() { return 1; }
plat_toolchain() { return 0; }
plat_disk() { return 0; }
# plat_measure STAGE: what the run has used so far, said as STAGE begins (and as its suites end, STAGE end), where
# the platform can read it (linux-in.sh: its container's cgroup).
plat_measure() { return 0; }
# plat_free: the free space of the disk the run works on, said (Windows: C:'s).
plat_free() { df -Pm "$R" 2>/dev/null | awk 'NR == 2 { print $4 " MB free" }'; }
plat_cleanup() { return 0; }
# plat_watch SUITE: a look at the run's processes on each of the judge's looks; plat_note SUITE: what it saw,
# "; ..." for the suite's RESULT line (Linux: the VM processes' userfaultfd hold, linux-in.sh).
plat_watch() { return 0; }
plat_note() { return 0; }
plat_facts() {
	FACT_HOST=$(uname -n) FACT_OS=$(uname -s) FACT_KERNEL=$(uname -r) FACT_MORE=$(uname -m)
}
plat_sample() {
	local dir=$1
	shift
	ps -ww -o pid,ppid,pgid,stat,etime,time,wchan,args -p "$(echo "$@" | tr ' ' ',')" >"$dir/ps.txt" 2>&1
}
# run_pids TOP: this run's processes: TOP's tree (none for 0), and every process whose command line
# names W (a daemon that left its starter's tree), this script itself left out.
run_pids() {
	ps -ww -A -o pid=,ppid=,args= | XP_W="$W/" XP_TOP="${1:-0}" XP_ME="$$" awk '
		{ pid[NR] = $1; ppid[NR] = $2; named[NR] = index($0, ENVIRON["XP_W"]) > 0 }
		END {
			if (ENVIRON["XP_TOP"] != "0") keep[ENVIRON["XP_TOP"]] = 1
			do {
				grew = 0
				for (i = 1; i <= NR; i++) if (!(pid[i] in keep) && (ppid[i] in keep)) { keep[pid[i]] = 1; grew = 1 }
			} while (grew)
			for (i = 1; i <= NR; i++) if ((pid[i] in keep || named[i]) && pid[i] != ENVIRON["XP_ME"]) print pid[i]
		}'
}
# kill_tree SIG PID: PID and its descendants sent SIG, children first.
kill_tree() {
	local sig=$1 p=$2 c
	for c in $(ps -A -o pid=,ppid= | awk -v p="$p" '$2 == p { print $1 }'); do kill_tree "$sig" "$c"; done
	kill "-$sig" "$p" 2>/dev/null
	return 0
}
end_pids() {
	local p
	for p in "$@"; do kill -TERM "$p" 2>/dev/null; done
	sleep "$GRACE"
	for p in "$@"; do kill -0 "$p" 2>/dev/null && kill -KILL "$p" 2>/dev/null; done
	return 0
}
