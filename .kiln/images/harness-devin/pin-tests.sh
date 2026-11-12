#!/usr/bin/env bash
# pin.sh's tests (kiln plan 01a1ce705e4d7008993f5c7291ef4476, step 1), for Linux with bash, curl,
# sha256sum, GNU tar, openssl and python3 (which makes and serves the fixtures and reads JSON).
#
#   pin-tests.sh             releases served over HTTPS from 127.0.0.1 through DEVIN_PIN_BASE, the
#                            server's own certificate trusted through CURL_CA_BUNDLE: pins that must
#                            succeed; each refusal, run with and without a pin.json already there,
#                            which must exit non-zero naming its check and leave pin.json as it was;
#                            then each check taken out of a copy of pin.sh, which the same test must
#                            catch
#   pin-tests.sh verify PIN  downloads each build PIN names again and holds its SHA-256 (sha256sum)
#                            and size (stat) to PIN and to the versioned manifest, read with
#                            python's json rather than pin.sh's matching
set -euo pipefail
export LC_ALL=C

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
pin=$here/pin.sh
for c in python3 openssl; do
    command -v "$c" >/dev/null 2>&1 || {
        echo "pin-tests.sh: needs $c" >&2
        exit 1
    }
done
mkdir -p "$HOME/.cache"
root=$(mktemp -d "$HOME/.cache/devin-pin-tests.XXXXXX")
server=
trap 'if [[ -n $server ]]; then kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true; fi; rm -rf "$root"' EXIT

cat >"$root/tools.py" <<'PY'
import io, json, os, re, sys, tarfile

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
    m = {"version": version, "platforms": p}
    with open(out, "w") as f:
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
    for b in pin.get("builds") or []:
        for f in b.get("files") or []:
            if isinstance(f.get("unpack"), str):
                bad.append(f"{b.get('platform')}'s unpack is the string {f['unpack']!r}: every pin writes the object form")
    want = [{"platform": builds[i], "files": [{"url": builds[i + 1], "sha256": builds[i + 2], "size": int(builds[i + 3]), "path": ".", "unpack": {"format": "tar.gz"}}]}
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
        print("build", b["platform"], f["url"], f["sha256"], f["size"], f["path"], json.dumps(f["unpack"], separators=(",", ":"), sort_keys=True))

def entries(path, version):
    m = json.load(open(path))
    if m["version"] != version:
        sys.exit(f"the manifest names {m['version']}, not {version}")
    for t in ("x86_64-unknown-linux", "aarch64-unknown-linux"):
        print(t, m["platforms"][t]["url"], m["platforms"][t]["sha256"])

def serve(root, cert, key, portfile):
    """Serves ROOT over HTTPS on 127.0.0.1 as a CDN does: 200 with a file, 403 for one it doesn't
    hold, unless <scenario>/codes.json says otherwise: {"missing": 404} or {"/path": 500}."""
    import http.server, ssl
    root = os.path.realpath(root)
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            path = self.path.split("?", 1)[0]
            scenario = path.lstrip("/").split("/", 1)[0]
            codes, body = {}, b""
            cf = os.path.join(root, scenario, "codes.json")
            if scenario and os.path.isfile(cf):
                with open(cf) as f:
                    codes = json.load(f)
            fs = os.path.realpath(os.path.join(root, path.lstrip("/")))
            if path in codes:
                code = codes[path]
            elif fs.startswith(root + os.sep) and os.path.isfile(fs):
                code = 200
                with open(fs, "rb") as f:
                    body = f.read()
            else:
                code = codes.get("missing", 403)
            self.send_response(code)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def log_message(self, *args):
            pass
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(cert, key)
    httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
    with open(portfile + ".tmp", "w") as f:
        f.write(str(httpd.server_address[1]))
    os.replace(portfile + ".tmp", portfile)
    httpd.serve_forever()

def mutate(src, dst, old, new):
    """DST is SRC with every OLD made NEW; prints how many there were, refused when none."""
    s = open(src, encoding="utf-8").read()
    n = s.count(old)
    if n == 0:
        sys.exit(f"tools.py: {old!r} is not in {src}")
    open(dst, "w", encoding="utf-8").write(s.replace(old, new))
    print(n)

cmd, args = sys.argv[1], sys.argv[2:]
{"bundle": bundle, "manifest": manifest, "edit": edit, "flip": flip, "check-pin": check_pin,
 "pin-facts": pin_facts, "entries": entries, "serve": serve, "mutate": mutate}[cmd](*args)
PY
py() { python3 -I "$root/tools.py" "$@"; }
sha() {
    local line
    line=$(sha256sum <"$1")
    printf '%s' "${line%% *}"
}

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
        if [[ $url == "$mu" && $psha == "$ms" && $got == "$psha" && $size == "$psize" && $path == . && $unpack == '{"format":"tar.gz"}' ]]; then
            n=$((n + 1))
            echo "PASS $platform ($target): $url downloaded again: sha256sum $got and stat $size bytes equal pin.json's, the manifest gives this URL and SHA-256, and unpack is $unpack"
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

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 1 -subj /CN=127.0.0.1 \
    -addext subjectAltName=IP:127.0.0.1 -keyout "$root/key.pem" -out "$root/cert.pem" 2>"$root/openssl.err" || {
    echo "pin-tests.sh: openssl could not make the server's certificate: $(<"$root/openssl.err")" >&2
    exit 1
}
mkdir -p "$root/srv"
python3 -I "$root/tools.py" serve "$root/srv" "$root/cert.pem" "$root/key.pem" "$root/port" 2>"$root/server.err" &
server=$!
until [[ -s $root/port ]]; do
    kill -0 "$server" 2>/dev/null || {
        echo "pin-tests.sh: the HTTPS server did not start: $(<"$root/server.err")" >&2
        exit 1
    }
    sleep 0.1
done
port=$(<"$root/port")
export CURL_CA_BUNDLE=$root/cert.pem

V=3000.0.1 passed=0 failed=0
standard="sha256 from Cognition's versioned manifest over HTTPS, equal to current/ when pinned; Cognition publishes no signature (.sig, .asc, .sha256, SHA256SUMS and .sigstore.json answer 403 or 404)"
older="sha256 from Cognition's versioned manifest over HTTPS; current/ named 3000.0.2 when pinned; Cognition publishes no signature (.sig, .asc, .sha256, SHA256SUMS and .sigstore.json answer 403 or 404)"
good() {
    printf 'PASS %s\n' "$*"
    passed=$((passed + 1))
}
bad() {
    printf 'FAIL %s\n' "$*"
    failed=$((failed + 1))
}
dir_of() { printf '%s' "$root/srv/fx-$1"; }
base_of() { printf '%s' "https://127.0.0.1:$port/fx-$1/cli"; }

release() { # DIR BASE VERSION X86_KIND ARM_KIND [STYLE]: a release in DIR/cli served at BASE, current/ naming it
    local d=$1 b=$2 v=$3 x86 arm
    mkdir -p "$d/cli/$v" "$d/cli/current"
    x86=$d/cli/$v/devin-$v-x86_64-unknown-linux.tar.gz
    arm=$d/cli/$v/devin-$v-aarch64-unknown-linux.tar.gz
    py bundle "$x86" "$4"
    py bundle "$arm" "$5"
    py manifest "$d/cli/$v/manifest.json" "$v" "$b" "$(sha "$x86")" "$(sha "$arm")" "${6:-compact}"
    cp "$d/cli/$v/manifest.json" "$d/cli/current/manifest.json"
}
setup() { # SCENARIO: its fixture, served at $(base_of SCENARIO)
    local s=$1 d b m c
    d=$(dir_of "$s") b=$(base_of "$s")
    m=$d/cli/$V/manifest.json c=$d/cli/current/manifest.json
    case $s in
    symlink | hardlink | device | fifo | dotdot | absolute | setuid | setgid | no-devin) release "$d" "$b" "$V" "$s" plain ;;
    arm64-symlink) release "$d" "$b" "$V" plain symlink ;;
    pretty) release "$d" "$b" "$V" plain plain pretty ;;
    dotslash) release "$d" "$b" "$V" dotslash dotslash ;;
    version-old)
        release "$d" "$b" "$V" plain plain
        release "$d" "$b" 3000.0.2 plain plain
        ;;
    *) release "$d" "$b" "$V" plain plain ;;
    esac
    case $s in
    version-other) py edit "$m" version 3000.0.2 ;;
    current-byte) py edit "$c" flip-darwin ;;
    no-aarch64) py edit "$m" drop aarch64-unknown-linux; cp "$m" "$c" ;;
    dup-x86) py edit "$m" dup x86_64-unknown-linux; cp "$m" "$c" ;;
    url-dir) py edit "$m" url x86_64-unknown-linux "$b/3000.0.0/devin-$V-x86_64-unknown-linux.tar.gz"; cp "$m" "$c" ;;
    url-host) py edit "$m" url x86_64-unknown-linux "https://static.devin.ai.invalid/cli/$V/devin-$V-x86_64-unknown-linux.tar.gz"; cp "$m" "$c" ;;
    sha-upper) py edit "$m" upper-sha x86_64-unknown-linux; cp "$m" "$c" ;;
    digest-flip) py flip "$d/cli/$V/devin-$V-x86_64-unknown-linux.tar.gz" ;;
    sig-404) printf '{"missing": 404}\n' >"$d/codes.json" ;;
    sig-200) printf 'signature\n' >"$m.sig" ;;
    sig-500) printf '{"/fx-sig-500/cli/%s/manifest.json.sig": 500}\n' "$V" >"$d/codes.json" ;;
    version-format) py edit "$c" version "../$V" ;;
    manifest-nul) py edit "$m" nul; cp "$m" "$c" ;;
    esac
}

# attempt SCRIPT SCENARIO SENTINEL [ARG]: runs SCRIPT as pin.sh in a fresh directory, beside a
# pin.json already there when SENTINEL is yes, against SCENARIO's release; sets status, err, last
# and kept (pin.json byte for byte as it was, or still absent, and no other file left beside it).
attempt() {
    local script=$1 s=$2 sentinel=$3 box want=pin.sh base sentinel_sha=
    shift 3
    case $s in
    base-http) base=http://127.0.0.1:$port/fx-$s/cli ;;
    base-file) base=file://$(dir_of "$s")/cli ;;
    *) base=$(base_of "$s") ;;
    esac
    box=$(mktemp -d "$root/run.XXXXXX")
    cp "$script" "$box/pin.sh"
    if [[ $sentinel == yes ]]; then
        printf '{"sentinel": true}\n' >"$box/pin.json"
        sentinel_sha=$(sha "$box/pin.json")
        want=$'pin.json\npin.sh'
    fi
    status=0
    DEVIN_PIN_BASE=$base bash "$box/pin.sh" "$@" >"$box.out" 2>"$box.err" || status=$?
    err=$(<"$box.err")
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
# pins SCRIPT SCENARIO [ARG]: whether SCRIPT pins SCENARIO's release as it should; sets why
pins() {
    local script=$1 s=$2 d b x86 arm verified=$standard
    shift 2
    d=$(dir_of "$s") b=$(base_of "$s")
    [[ $s != version-old ]] || verified=$older
    x86=$d/cli/$V/devin-$V-x86_64-unknown-linux.tar.gz arm=$d/cli/$V/devin-$V-aarch64-unknown-linux.tar.gz
    attempt "$script" "$s" yes "$@"
    if [[ $status != 0 ]]; then
        why="exit $status, $(said)"
        return 1
    fi
    if [[ $(ls -A "$last") != $'pin.json\npin.sh' ]]; then
        why="it left $(ls -A "$last" | tr '\n' ' ')"
        return 1
    fi
    why=$(py check-pin "$last/pin.json" "$V" "$verified" "$b/$V/manifest.json" "$(sha "$d/cli/$V/manifest.json")" \
        linux/amd64 "$b/$V/devin-$V-x86_64-unknown-linux.tar.gz" "$(sha "$x86")" "$(stat -c %s "$x86")" \
        linux/arm64 "$b/$V/devin-$V-aarch64-unknown-linux.tar.gz" "$(sha "$arm")" "$(stat -c %s "$arm")" 2>&1) || return 1
}

echo "== host $(hostname) $(uname -srm); $(bash --version | sed -n 1p); $(tar --version | sed -n 1p); $(curl --version | sed -n 1p | cut -d' ' -f1-2); $(openssl version); $(python3 --version)"
echo "== pin.sh sha256 $(sha "$pin"); pin-tests.sh sha256 $(sha "$0"); HTTPS fixtures at https://127.0.0.1:$port/"

# Each pin that must succeed: SCENARIO:ARG.
successes=(default: current:current version-same:$V version-old:$V pretty: dotslash: sig-404:)
echo "== pins that must succeed, each with unpack {\"format\": \"tar.gz\"} and never a string"
for sc in "${successes[@]}"; do
    s=${sc%%:*} arg=${sc#*:}
    setup "$s"
    args=()
    [[ -z $arg ]] || args=("$arg")
    if pins "$pin" "$s" "${args[@]}"; then
        good "pin.sh${arg:+ $arg} ($s): exit 0; pin.json holds devin $V, both builds' URL, SHA-256 and size, path . and unpack {\"format\": \"tar.gz\"}, the manifest's URL and SHA-256, read_at, the base digest and: $(sed -n 's/.*"verified": "\(.*\)",$/\1/p' "$last/pin.json"); it said: $(grep -o 'no signature: [^:]*' "$last.out")"
    else
        bad "pin.sh${arg:+ $arg} ($s): $why"
    fi
done

# Each refusal: SCENARIO:CHECK.
scenarios=(
    base-http:base
    base-file:base
    version-other:manifest-version
    version-format:version-format
    manifest-nul:manifest-text
    current-byte:current
    no-aarch64:platform
    dup-x86:platform
    url-dir:url
    url-host:url
    sha-upper:sha256-form
    sig-200:signature
    sig-500:signature-unknown
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
    s=${sc%%:*} check=${sc#*:}
    setup "$s"
    for sentinel in yes no; do
        attempt "$pin" "$s" "$sentinel"
        what="$s (pin.json $([[ $sentinel == yes ]] && echo there || echo absent))"
        if refused "$check"; then
            good "$what: exit $status, $(said)"
        else
            bad "$what: exit $status, kept $kept, wanted check $check: $(said)"
        fi
    done
done

echo "== each check taken out of a copy of pin.sh: its tests must now fail"
checks=$(printf '%s\n' "${scenarios[@]}" | cut -d: -f2 | sort -u)
for check in $checks; do
    mutant=$root/without-$check.sh
    if ! removed=$(py mutate "$pin" "$mutant" "die \"$check:" "true \"$check:"); then
        bad "no check named $check in pin.sh to take out"
        continue
    fi
    for sc in "${scenarios[@]}"; do
        [[ ${sc#*:} == "$check" ]] || continue
        s=${sc%%:*}
        attempt "$mutant" "$s" yes
        if refused "$check"; then
            bad "without its $removed $check refusal(s), pin.sh still refuses $s naming $check"
        elif [[ $status == 0 ]]; then
            good "without its $removed $check refusal(s), $s is caught: pin.sh exits 0 and writes pin.json"
        else
            good "without its $removed $check refusal(s), $s is caught: exit $status, $(said)"
        fi
    done
done
mutant=$root/without-404.sh
if py mutate "$pin" "$mutant" $'    404) n404=$((n404 + 1)) ;;\n' '' >/dev/null; then
    if pins "$mutant" sig-404; then
        bad "with 404 no longer taken as absent, pin.sh still pins sig-404"
    else
        good "with 404 no longer taken as absent, sig-404 is caught: $why"
    fi
else
    bad "pin.sh takes no 404 as absent to take out"
fi
mutant=$root/unpack-string.sh
if py mutate "$pin" "$mutant" '"unpack": {\n            "format": "tar.gz"\n          }' '"unpack": "tar.gz"' >/dev/null; then
    if pins "$mutant" default; then
        bad "pin.sh writing unpack as the string \"tar.gz\" still passes"
    elif [[ $why == *"is the string"* ]]; then
        good "pin.sh writing unpack as the string \"tar.gz\" is caught: $why"
    else
        bad "pin.sh writing unpack as the string \"tar.gz\" fails, but not for its string: $why"
    fi
else
    bad "pin.sh writes no unpack object to turn into a string"
fi

echo "== $passed passed, $failed failed"
((failed == 0))
