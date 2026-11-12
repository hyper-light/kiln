#!/usr/bin/env bash
# pin.sh's tests (kiln plan 01a1ce705e4d7008993f5c7291ef4476, step 1), for a Linux runner with
# bash, curl, sha256sum, GNU tar and python3 (which makes the fixtures and reads JSON).
#
#   pin-tests.sh             releases served as file:// fixtures through DEVIN_PIN_BASE: pins that
#                            must succeed; each refusal, run with and without a pin.json already
#                            there, which must exit non-zero naming its check and leave pin.json as
#                            it was; then each check's guard removed from a copy of pin.sh, which
#                            the same test must catch
#   pin-tests.sh verify PIN  downloads each build PIN names again and holds its SHA-256 (sha256sum)
#                            and size (stat) to PIN and to the versioned manifest, read with
#                            python's json rather than pin.sh's matching
set -euo pipefail
export LC_ALL=C

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
pin=$here/pin.sh
command -v python3 >/dev/null 2>&1 || {
    echo "pin-tests.sh: needs python3" >&2
    exit 1
}
mkdir -p "$HOME/.cache"
root=$(mktemp -d "$HOME/.cache/devin-pin-tests.XXXXXX")
trap 'rm -rf "$root"' EXIT

cat >"$root/tools.py" <<'PY'
import io, json, re, sys, tarfile

def bundle(out, kind):
    """A release bundle laid out as Devin's (bin/devin, share/devin/...), crafted as KIND."""
    def add(tf, name, typ=tarfile.REGTYPE, data=b"", mode=0o644, **kw):
        ti = tarfile.TarInfo(name)
        ti.type, ti.mode, ti.mtime = typ, mode, 1762905600
        for k, v in kw.items():
            setattr(ti, k, v)
        if typ == tarfile.REGTYPE:
            ti.size = len(data)
            tf.addfile(ti, io.BytesIO(data))
        else:
            tf.addfile(ti)
    pre = "./" if kind == "dotslash" else ""
    with tarfile.open(out, "w:gz") as tf:
        add(tf, pre + "bin/", tarfile.DIRTYPE, mode=0o755)
        if kind != "no-devin":
            add(tf, pre + "bin/devin", data=b"#!/bin/sh\necho devin\n", mode=0o755)
        add(tf, pre + "share/", tarfile.DIRTYPE, mode=0o755)
        add(tf, pre + "share/devin/", tarfile.DIRTYPE, mode=0o755)
        add(tf, pre + "share/devin/index.mdx", data=b"# Devin\n")
        if kind == "symlink":
            add(tf, "share/devin/link", tarfile.SYMTYPE, linkname="index.mdx", mode=0o777)
        elif kind == "hardlink":
            add(tf, "share/devin/hard", tarfile.LNKTYPE, linkname="share/devin/index.mdx")
        elif kind == "device":
            add(tf, "share/devin/null", tarfile.CHRTYPE, mode=0o666, devmajor=1, devminor=3)
        elif kind == "fifo":
            add(tf, "share/devin/fifo", tarfile.FIFOTYPE)
        elif kind == "dotdot":
            add(tf, "../x", data=b"x\n")
        elif kind == "absolute":
            add(tf, "/x", data=b"x\n")
        elif kind == "setuid":
            add(tf, "bin/helper", data=b"x\n", mode=0o4755)
        elif kind == "setgid":
            add(tf, "bin/helper", data=b"x\n", mode=0o2755)
        elif kind not in ("plain", "no-devin", "dotslash"):
            sys.exit(f"tools.py: unknown bundle kind {kind}")

def manifest(out, version, base, sha_x86, sha_arm, style="compact"):
    """A manifest in the form Cognition serves (3000.12.43's keys, order and compact JSON), or indented."""
    def e(name, sha):
        return {"url": f"{base}/{version}/devin-{version}-{name}", "sha256": sha}
    p = {
        "aarch64-apple-darwin": e("aarch64-apple-darwin.tar.gz", "1" * 64),
        "aarch64-unknown-linux": e("aarch64-unknown-linux.tar.gz", sha_arm),
        "x86_64-apple-darwin": e("x86_64-apple-darwin.tar.gz", "2" * 64),
        "x86_64-unknown-linux": e("x86_64-unknown-linux.tar.gz", sha_x86),
        "aarch64-pc-windows": e("aarch64-pc-windows.zip", "3" * 64),
        "x86_64-pc-windows": e("x86_64-pc-windows.zip", "4" * 64),
        "x86_64-unknown-linux-musl": e("x86_64-unknown-linux.tar.gz", sha_x86),
        "aarch64-unknown-linux-musl": e("aarch64-unknown-linux.tar.gz", sha_arm),
        "x86_64-pc-windows-gnu": e("x86_64-pc-windows.zip", "4" * 64),
        "aarch64-pc-windows-gnullvm": e("aarch64-pc-windows.zip", "3" * 64),
    }
    with open(out, "w") as f:
        m = {"version": version, "platforms": p}
        f.write(json.dumps(m, indent=2) if style == "pretty" else json.dumps(m, separators=(",", ":")))

def edit(path, op, *a):
    """One change to a manifest, every other byte kept."""
    raw = open(path, "rb").read()
    if op == "nul":
        if raw.count(b'"version"') != 1:
            sys.exit("tools.py: no single version key")
        raw = raw.replace(b'"version"', b'"ver\x00sion"')
    elif op == "dup":  # a second entry for TARGET, first in platforms: JSON parsers disagree on which wins
        url = json.loads(raw)["platforms"][a[0]]["url"]
        if raw.count(b'"platforms":{') != 1:
            sys.exit("tools.py: no single compact platforms object")
        raw = raw.replace(b'"platforms":{', b'"platforms":{' + json.dumps({a[0]: {"url": url, "sha256": "0" * 64}}, separators=(",", ":"))[1:-1].encode() + b",")
    else:
        m = json.loads(raw)
        ps = m["platforms"]
        if op == "version":
            m["version"] = a[0]
        elif op == "drop":
            del ps[a[0]]
        elif op == "url":
            ps[a[0]]["url"] = a[1]
        elif op == "upper-sha":
            ps[a[0]]["sha256"] = ps[a[0]]["sha256"].upper()
        elif op == "flip-darwin":
            s = ps["aarch64-apple-darwin"]["sha256"]
            ps["aarch64-apple-darwin"]["sha256"] = ("0" if s[0] != "0" else "1") + s[1:]
        else:
            sys.exit(f"tools.py: unknown edit {op}")
        new = json.dumps(m, separators=(",", ":")).encode()
        if new == raw:
            sys.exit(f"tools.py: edit {op} changed nothing")
        raw = new
    open(path, "wb").write(raw)

def flip(path):
    b = bytearray(open(path, "rb").read())
    b[len(b) // 2] ^= 1
    open(path, "wb").write(bytes(b))

def check_pin(path, version, verified, murl, msha, *builds):
    """Holds pin.json to step 19's schema and to the values the fixture was made with."""
    pin = json.load(open(path))
    bad = []
    def eq(what, got, want):
        if got != want:
            bad.append(f"{what} is {got!r}, not {want!r}")
    eq("its keys", sorted(pin), sorted(["harness", "version", "verified", "manifest", "read_at", "base", "builds"]))
    eq("harness", pin.get("harness"), "devin")
    eq("version", pin.get("version"), version)
    eq("verified", pin.get("verified"), verified)
    eq("manifest", pin.get("manifest"), {"url": murl, "sha256": msha})
    if not re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", str(pin.get("read_at"))):
        bad.append(f"read_at is {pin.get('read_at')!r}")
    eq("base", pin.get("base"), {"ref": "buildpack-deps:trixie-scm", "digest": "sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10"})
    want = [{"platform": builds[i], "files": [{"url": builds[i + 1], "sha256": builds[i + 2], "size": int(builds[i + 3]), "path": ".", "unpack": "tar.gz"}]}
            for i in range(0, len(builds), 4)]
    eq("builds", pin.get("builds"), want)
    if bad:
        sys.exit("; ".join(bad))

def pin_facts(path):
    pin = json.load(open(path))
    print("manifest", pin["manifest"]["url"], pin["manifest"]["sha256"], pin["version"])
    for b in pin["builds"]:
        if len(b["files"]) != 1:
            sys.exit(f"{b['platform']} has {len(b['files'])} files, not one")
        f = b["files"][0]
        print("build", b["platform"], f["url"], f["sha256"], f["size"], f["path"], f["unpack"])

def entries(path, version):
    m = json.load(open(path))
    if m["version"] != version:
        sys.exit(f"the manifest names {m['version']}, not {version}")
    for t in ("x86_64-unknown-linux", "aarch64-unknown-linux"):
        print(t, m["platforms"][t]["url"], m["platforms"][t]["sha256"])

cmd, args = sys.argv[1], sys.argv[2:]
{"bundle": bundle, "manifest": manifest, "edit": edit, "flip": flip, "check-pin": check_pin,
 "pin-facts": pin_facts, "entries": entries}[cmd](*args)
PY
py() { python3 -I "$root/tools.py" "$@"; }
sha() {
    local line
    line=$(sha256sum <"$1")
    printf '%s' "${line%% *}"
}
standard="sha256 from Cognition's versioned manifest over HTTPS, equal to current/ when pinned; Cognition publishes no signature (.sig, .asc, .sha256, SHA256SUMS and .sigstore.json answer 403)"

verify() { # PIN
    local p=$1 kind murl msha version platform url psha psize path unpack target t u h mu ms got size n=0 builds=0
    py pin-facts "$p" >"$root/facts"
    read -r kind murl msha version <"$root/facts"
    curl -fsSL --proto =https -o "$root/manifest.json" "$murl" </dev/null
    got=$(sha "$root/manifest.json")
    if [[ $got != "$msha" ]]; then
        echo "FAIL $murl's SHA-256 is $got now, not pin.json's $msha"
        return 1
    fi
    echo "PASS $murl downloaded again: sha256 $got, as pin.json records"
    py entries "$root/manifest.json" "$version" >"$root/entries"
    while read -r kind platform url psha psize path unpack; do
        [[ $kind == build ]] || continue
        builds=$((builds + 1)) mu= ms=
        case $platform in
        linux/amd64) target=x86_64-unknown-linux ;;
        linux/arm64) target=aarch64-unknown-linux ;;
        *) target=none ;;
        esac
        while read -r t u h; do
            if [[ $t == "$target" ]]; then
                mu=$u ms=$h
            fi
        done <"$root/entries"
        curl -fsSL --proto =https -o "$root/bundle" "$url" </dev/null
        got=$(sha256sum "$root/bundle" | cut -d' ' -f1)
        size=$(stat -c %s "$root/bundle")
        rm -f "$root/bundle"
        if [[ $url == "$mu" && $psha == "$ms" && $got == "$psha" && $size == "$psize" && $path == . && $unpack == tar.gz ]]; then
            n=$((n + 1))
            echo "PASS $platform ($target): $url downloaded again: sha256sum $got and stat $size bytes equal pin.json's, and the manifest gives this URL and SHA-256"
        else
            echo "FAIL $platform ($target): pin.json $url $psha $psize $path $unpack; manifest ${mu:-none} ${ms:-none}; download sha256 $got, $size bytes"
        fi
    done <"$root/facts"
    echo "builds whose SHA-256 and size match both the manifest and a fresh download: $n of $builds"
    ((n == 2 && builds == 2))
}
if [[ ${1-} == verify ]]; then
    (($# == 2)) || {
        echo "usage: pin-tests.sh verify PIN" >&2
        exit 1
    }
    verify "$2"
    exit
fi
(($# == 0)) || {
    echo "usage: pin-tests.sh [verify PIN]" >&2
    exit 1
}

V=3000.0.1 passed=0 failed=0
good() {
    printf 'PASS %s\n' "$*"
    passed=$((passed + 1))
}
bad() {
    printf 'FAIL %s\n' "$*"
    failed=$((failed + 1))
}

release() { # DIR VERSION X86_KIND ARM_KIND [STYLE]: a release served from DIR/cli, current/ naming it
    local d=$1 v=$2 x86 arm
    mkdir -p "$d/cli/$v" "$d/cli/current"
    x86=$d/cli/$v/devin-$v-x86_64-unknown-linux.tar.gz
    arm=$d/cli/$v/devin-$v-aarch64-unknown-linux.tar.gz
    py bundle "$x86" "$3"
    py bundle "$arm" "$4"
    py manifest "$d/cli/$v/manifest.json" "$v" "file://$d/cli" "$(sha "$x86")" "$(sha "$arm")" "${5:-compact}"
    cp "$d/cli/$v/manifest.json" "$d/cli/current/manifest.json"
}
setup() { # SCENARIO DIR: DIR's fixture for SCENARIO
    local s=$1 d=$2 m=$2/cli/$V/manifest.json c=$2/cli/current/manifest.json
    case $s in
    symlink | hardlink | device | fifo | dotdot | absolute | setuid | setgid | no-devin) release "$d" "$V" "$s" plain ;;
    arm64-symlink) release "$d" "$V" plain symlink ;;
    *) release "$d" "$V" plain plain ;;
    esac
    case $s in
    version-other) py edit "$m" version 3000.0.2 ;;
    current-byte) py edit "$c" flip-darwin ;;
    no-aarch64) py edit "$m" drop aarch64-unknown-linux; cp "$m" "$c" ;;
    dup-x86) py edit "$m" dup x86_64-unknown-linux; cp "$m" "$c" ;;
    url-dir) py edit "$m" url x86_64-unknown-linux "file://$d/cli/3000.0.0/devin-$V-x86_64-unknown-linux.tar.gz"; cp "$m" "$c" ;;
    url-host) py edit "$m" url x86_64-unknown-linux "https://example.com/cli/$V/devin-$V-x86_64-unknown-linux.tar.gz"; cp "$m" "$c" ;;
    sha-upper) py edit "$m" upper-sha x86_64-unknown-linux; cp "$m" "$c" ;;
    digest-flip) py flip "$d/cli/$V/devin-$V-x86_64-unknown-linux.tar.gz" ;;
    signature) printf 'signature\n' >"$m.sig" ;;
    version-format) py edit "$c" version "../$V" ;;
    manifest-nul) py edit "$m" nul; cp "$m" "$c" ;;
    esac
}

# attempt SCRIPT DIR SENTINEL [ARG]: runs SCRIPT as pin.sh in a fresh directory, beside a pin.json
# already there when SENTINEL is yes, against DIR's fixture (or $base_url); sets status, err, out
# and kept (pin.json byte for byte as it was, or still absent, and no other file left beside it).
sentinel_sha=
attempt() {
    local script=$1 dir=$2 sentinel=$3 box want=pin.sh
    shift 3
    box=$(mktemp -d "$root/run.XXXXXX")
    cp "$script" "$box/pin.sh"
    if [[ $sentinel == yes ]]; then
        printf '{"sentinel": true}\n' >"$box/pin.json"
        sentinel_sha=$(sha "$box/pin.json")
        want=$'pin.json\npin.sh'
    fi
    status=0
    DEVIN_PIN_BASE=${base_url:-file://$dir/cli} bash "$box/pin.sh" "$@" >"$box.out" 2>"$box.err" || status=$?
    out=$(<"$box.out") err=$(<"$box.err")
    kept=no
    if [[ $(ls -A "$box") == "$want" ]] && { [[ $sentinel != yes ]] || [[ $(sha "$box/pin.json") == "$sentinel_sha" ]]; }; then
        kept=yes
    fi
    last=$box
}
refused() { # CHECK: whether the last attempt was refused naming CHECK, pin.json left as it was
    [[ $status != 0 && $err == *"pin.sh: $1:"* && $kept == yes ]]
}
said() { # the last attempt's first line of stderr
    printf '%s' "${err%%$'\n'*}"
}

echo "== host $(hostname) $(uname -srm); $(bash --version | sed -n 1p); $(tar --version | sed -n 1p); $(curl --version | sed -n 1p | cut -d' ' -f1-2); $(python3 --version)"
echo "== pin.sh sha256 $(sha "$pin"); pin-tests.sh sha256 $(sha "$0")"

echo "== pins that must succeed"
for s in default current version-same version-old pretty dotslash; do
    d=$root/fx-$s
    base_url=
    case $s in
    pretty) release "$d" "$V" plain plain pretty ;;
    dotslash) release "$d" "$V" dotslash dotslash ;;
    *) release "$d" "$V" plain plain ;;
    esac
    args=() verified=$standard
    case $s in
    current) args=(current) ;;
    version-same) args=("$V") ;;
    version-old)
        release "$d" 3000.0.2 plain plain
        args=("$V")
        verified="sha256 from Cognition's versioned manifest over HTTPS; current/ named 3000.0.2 when pinned; Cognition publishes no signature (.sig, .asc, .sha256, SHA256SUMS and .sigstore.json answer 403)"
        ;;
    esac
    x86=$d/cli/$V/devin-$V-x86_64-unknown-linux.tar.gz arm=$d/cli/$V/devin-$V-aarch64-unknown-linux.tar.gz
    attempt "$pin" "$d" yes "${args[@]}"
    if [[ $status != 0 ]]; then
        bad "pin.sh${args[*]:+ ${args[*]}} ($s): exit $status: $(said)"
    elif [[ $(ls -A "$last") != $'pin.json\npin.sh' ]]; then
        bad "pin.sh${args[*]:+ ${args[*]}} ($s) left $(ls -A "$last" | tr '\n' ' ')"
    elif why=$(py check-pin "$last/pin.json" "$V" "$verified" "file://$d/cli/$V/manifest.json" "$(sha "$d/cli/$V/manifest.json")" \
        linux/amd64 "file://$x86" "$(sha "$x86")" "$(stat -c %s "$x86")" linux/arm64 "file://$arm" "$(sha "$arm")" "$(stat -c %s "$arm")" 2>&1); then
        good "pin.sh${args[*]:+ ${args[*]}} ($s): exit 0; pin.json holds devin $V, both builds' URL, SHA-256 and size, path . and unpack tar.gz, the manifest's URL and SHA-256, read_at, the base digest and: $(sed -n 's/.*"verified": "\(.*\)",$/\1/p' "$last/pin.json")"
    else
        bad "pin.sh${args[*]:+ ${args[*]}} ($s): pin.json: $why"
    fi
done

# Each refusal: SCENARIO:CHECK.
scenarios=(
    base-http:base
    version-other:manifest-version
    version-format:version-format
    manifest-nul:manifest-text
    current-byte:current
    no-aarch64:platform
    dup-x86:platform
    url-dir:url
    url-host:url
    sha-upper:sha256-form
    signature:signature
    digest-flip:digest
    symlink:entry-type
    hardlink:entry-type
    device:entry-type
    fifo:entry-type
    arm64-symlink:entry-type
    absolute:absolute
    dotdot:dotdot
    setuid:set-id
    setgid:set-id
    no-devin:bin-devin
)
echo "== refusals: each must exit non-zero naming its check, with pin.json left as it was"
for sc in "${scenarios[@]}"; do
    s=${sc%%:*} check=${sc#*:} d=$root/fx-$s
    setup "$s" "$d"
    base_url=
    [[ $s != base-http ]] || base_url=http://127.0.0.1:9/cli
    for sentinel in yes no; do
        attempt "$pin" "$d" "$sentinel"
        what="$s (pin.json $([[ $sentinel == yes ]] && echo there || echo absent))"
        if refused "$check"; then
            good "$what: exit $status, $(said)"
        else
            bad "$what: exit $status, kept $kept, wanted check $check: $(said)"
        fi
    done
done

echo "== each guard removed from a copy of pin.sh: its tests must now fail"
checks=$(printf '%s\n' "${scenarios[@]}" | cut -d: -f2 | sort -u)
for check in $checks; do
    mutant=$root/without-$check.sh
    sed "s/die \"$check:/true \"$check:/g" "$pin" >"$mutant"
    removed=$(grep -c "true \"$check:" "$mutant" || true)
    if ((removed == 0)); then
        bad "no guard named $check in pin.sh to remove"
        continue
    fi
    for sc in "${scenarios[@]}"; do
        [[ ${sc#*:} == "$check" ]] || continue
        s=${sc%%:*} d=$root/fx-$s
        base_url=
        [[ $s != base-http ]] || base_url=http://127.0.0.1:9/cli
        attempt "$mutant" "$d" yes
        if refused "$check"; then
            bad "without its $removed $check guard(s), pin.sh still refuses $s naming $check"
        elif [[ $status == 0 ]]; then
            good "without its $removed $check guard(s), $s is caught: pin.sh exits 0 and writes pin.json"
        else
            good "without its $removed $check guard(s), $s is caught: exit $status, $(said)"
        fi
    done
done

echo "== $passed passed, $failed failed"
((failed == 0))
