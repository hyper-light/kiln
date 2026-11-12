#!/usr/bin/env bash
# The tests of .kiln/images/harness-cursor/pin.sh (kiln plan 01a1ce705e5c7504b88732f443d99288,
# step 1). Each case is a release served over HTTPS from 127.0.0.1 through PIN_TEST_BASE_URL, the
# server's own certificate trusted through curl's CURL_CA_BUNDLE, so pin.sh fetches as it does
# from Cursor. A good release must pin, with every number in pin.json measured again by
# sha256sum, stat and GNU tar; each broken one must exit with its status, name the check that
# failed, and leave no pin.json and no temporary file, an earlier pin.json untouched.
#
#   pin_test.sh [PIN.SH]   PIN.SH: the pin.sh to test, by default this checkout's
#
# Needs what pin.sh needs, openssl, and GNU coreutils and tar (Linux, as kiln's runners have).
set -euo pipefail
export LC_ALL=C

here=$(cd "$(dirname "$0")" && pwd)
pin=${1:-$here/../../../../.kiln/images/harness-cursor/pin.sh}
[ -f "$pin" ] || { echo "pin_test.sh: no pin.sh at $pin" >&2; exit 64; }
cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
T=$(mktemp -d "$cache/cursor-pin-test.XXXXXX")
server=
trap 'if [ -n "$server" ]; then kill "$server" 2> /dev/null || true; wait "$server" 2> /dev/null || true; fi; rm -rf "$T"' EXIT

# The version the installer names, in the form Cursor's named on 2026-11-12, and one asked for.
V=2026.11.11-b912975
V2=2026.11.06-0a1b2c3
BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10

IFS= read -r -d '' FIXTURES << 'EOF' || true
import gzip, hashlib, io, os, sys, tarfile

www, V, V2 = sys.argv[1:4]
PKG = "agent-cli-package.tar.gz"
ARM64 = f"lab/{V}/linux/arm64/{PKG}"


def put(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def installer(*lines):
    """A stand-in for https://cursor.com/install: pin.sh reads its DOWNLOAD_URL line alone."""
    return ("#!/usr/bin/env bash\n" + "".join(line + "\n" for line in lines)).encode()


def download_url(version, host="downloads.cursor.com"):
    return f'DOWNLOAD_URL="https://{host}/lab/{version}/${{OS}}/${{ARCH}}/{PKG}"'


def tgz(members, trailing=b"", end=True):
    """A gzip tar of members (name, kind, mode, data or link target), then trailing; without its
    end-of-archive blocks when end is false."""
    raw = io.BytesIO()
    tf = tarfile.open(fileobj=raw, mode="w", format=tarfile.GNU_FORMAT)
    for name, kind, mode, data in members:
        t = tarfile.TarInfo(name)
        t.mode, t.mtime = mode, 0
        if kind == "file":
            t.size = len(data)
            tf.addfile(t, io.BytesIO(data))
            continue
        t.type = {"dir": tarfile.DIRTYPE, "symlink": tarfile.SYMTYPE, "hardlink": tarfile.LNKTYPE,
                  "fifo": tarfile.FIFOTYPE, "chardev": tarfile.CHRTYPE}[kind]
        if data is not None:
            t.linkname = data
        tf.addfile(t)
    if end:
        tf.close()
    return gzip.compress(raw.getvalue() + trailing, mtime=0)


def good(arch):
    return [
        ("dist-package", "dir", 0o755, None),
        ("dist-package/cursor-agent", "file", 0o755, f"#!/usr/bin/env bash\necho {arch}\n".encode()),
        ("dist-package/index.js", "file", 0o644, b"// index.js\n"),
        ("dist-package/node_modules", "dir", 0o755, None),
        ("dist-package/node_modules/a.js", "file", 0o644, b"// a.js\n" * 3),
    ]


def sha256sum(blob, name=PKG):
    return f"{hashlib.sha256(blob).hexdigest()}  {name}\n".encode()


def tree(case, version=V, install=None, arm64=None, arm64_sum=None, extra=None, drop=()):
    """www/<case>: the installer naming V and both builds of version good, with arm64's archive
    (its .sha256 made to match) or .sha256 put in their place, extra's files added, drop's taken."""
    layout = {"install": install if install is not None else installer(download_url(V))}
    for arch in ("x64", "arm64"):
        blob = arm64 if arch == "arm64" and arm64 is not None else tgz(good(arch))
        path = f"lab/{version}/linux/{arch}/{PKG}"
        layout[path] = blob
        layout[path + ".sha256"] = arm64_sum if arch == "arm64" and arm64_sum is not None else sha256sum(blob)
    layout.update(extra or {})
    for name in drop:
        del layout[name]
    for name, data in layout.items():
        put(os.path.join(www, case, name), data)


g = good("arm64")
whole = tgz(g)
s = sha256sum(whole).decode()


def plus(*members):
    return tgz(g + list(members))


def swap(name, member):
    return tgz([member if m[0] == name else m for m in g])


def file(name, mode=0o644, data=b"x\n"):
    return (name, "file", mode, data)


tree("good")
tree("asked", version=V2, drop=("install",))
tree("installer-missing", drop=("install",))
tree("no-download-url", install=installer("echo no URL here"))
tree("two-download-urls", install=installer(download_url(V), download_url(V)))
tree("other-host", install=installer(download_url(V, "downloads.example.com")))
tree("installer-dotdot", install=installer(download_url("..")))
tree("installer-space", install=installer(download_url("2026.11.11 b912975")))
tree("installer-redirect-http", drop=("install",),
     extra={"install.redirect": b"{http}/installer-redirect-http/install-there\n", "install-there": installer(download_url(V))})
tree("sidecar-digit", arm64_sum=(("1" if s[0] == "0" else "0") + s[1:]).encode())
tree("sidecar-404", drop=(ARM64 + ".sha256",))
tree("sidecar-other-file", arm64_sum=s.replace(PKG, "agent-cli-package.tgz").encode())
tree("sidecar-uppercase", arm64_sum=(s[:64].upper() + s[64:]).encode())
tree("sidecar-two-lines", arm64_sum=(s + s).encode())
tree("archive-404", drop=(ARM64,))
tree("dotdot", arm64=plus(file("../x")))
tree("dotdot-inside", arm64=plus(file("dist-package/../../x")))
tree("absolute", arm64=plus(file("/dist-package/x")))
tree("outside", arm64=plus(file("other/x")))
tree("dot-component", arm64=plus(file("dist-package/./x")))
tree("symlink", arm64=plus(("dist-package/agent", "symlink", 0o777, "cursor-agent")))
tree("hardlink", arm64=plus(("dist-package/agent", "hardlink", 0o755, "dist-package/cursor-agent")))
tree("fifo", arm64=plus(("dist-package/pipe", "fifo", 0o644, None)))
tree("chardev", arm64=plus(("dist-package/null", "chardev", 0o666, None)))
tree("no-agent", arm64=tgz([m for m in g if m[0] != "dist-package/cursor-agent"]))
tree("agent-not-exec", arm64=swap("dist-package/cursor-agent", file("dist-package/cursor-agent")))
tree("agent-dir", arm64=swap("dist-package/cursor-agent", ("dist-package/cursor-agent", "dir", 0o755, None)))
tree("top-file", arm64=tgz([file("dist-package")] + g[1:]))
tree("setuid", arm64=swap("dist-package/cursor-agent", file("dist-package/cursor-agent", 0o4755)))
tree("sticky", arm64=swap("dist-package/node_modules", ("dist-package/node_modules", "dir", 0o1777, None)))
tree("duplicate", arm64=plus(file("dist-package/index.js")))
tree("case-fold", arm64=plus(file("dist-package/Index.js")))
tree("normalization", arm64=plus(file("dist-package/caf\u00e9.js"), file("dist-package/cafe\u0301.js")))
tree("parent-file", arm64=plus(file("dist-package/lib"), file("dist-package/lib/x.js")))
tree("child-first", arm64=plus(file("dist-package/lib/x.js"), file("dist-package/lib")))
tree("root-harness-json", arm64=plus(file("dist-package/harness.json", data=b"{}\n")))
tree("not-gzip", arm64=b"not a tar.gz\n")
tree("gzip-not-tar", arm64=gzip.compress(b"not a tar\n" * 100, mtime=0))
tree("truncated", arm64=whole[: len(whole) // 2])
tree("garbage-header", arm64=tgz(g, trailing=b"not a header" * 64, end=False))
tree("trailing-data", arm64=tgz(g, trailing=b"hidden\n"))
EOF

IFS= read -r -d '' SERVE << 'EOF' || true
import functools, http.server, os, ssl, sys, threading

root, cert, key = sys.argv[1:4]


class Handler(http.server.SimpleHTTPRequestHandler):
    """Files as a CDN serves them: 200 with the file, 404 without it; FILE.redirect answers a
    request for FILE with a 302 to the URL it holds, {http} there the plain server's origin."""

    def do_GET(self):
        redirect = self.translate_path(self.path) + ".redirect"
        if os.path.isfile(redirect):
            with open(redirect) as f:
                location = f.read().strip().replace("{http}", f"http://127.0.0.1:{plain.server_address[1]}")
            self.send_response(302)
            self.send_header("Location", location)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        super().do_GET()

    def log_message(self, format, *args):
        pass


# The same files over HTTPS and over plain HTTP: what pin.sh must refuse to fetch is there to be
# fetched, so only its refusal of HTTP keeps it out.
serve = functools.partial(Handler, directory=root)
secure = http.server.ThreadingHTTPServer(("127.0.0.1", 0), serve)
plain = http.server.ThreadingHTTPServer(("127.0.0.1", 0), serve)
tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
tls.load_cert_chain(cert, key)
secure.socket = tls.wrap_socket(secure.socket, server_side=True)
threading.Thread(target=plain.serve_forever, daemon=True).start()
print(secure.server_address[1], plain.server_address[1], flush=True)
secure.serve_forever()
EOF

IFS= read -r -d '' EXPECT << 'EOF' || true
import json, re, sys

path, base, version, source, installer_sha, image_base, *measured = sys.argv[1:]
with open(path) as f:
    pin = json.load(f)
at = pin.get("version_read", {}).get("at", "")
how = pin.get("verified", {}).get("how", "")
problems = []
if not re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", at):
    problems.append(f"version_read.at {at!r} is not a UTC time")
if "no signature" not in how:
    problems.append(f"verified.how does not say Cursor publishes no signature: {how!r}")
read = {"from": source}
if source == "installer":
    read |= {"url": f"{base}/install", "sha256": installer_sha}
builds = []
for i, (platform, arch) in enumerate((("linux/amd64", "x64"), ("linux/arm64", "arm64"))):
    sha, size, entries, unpacked = measured[4 * i:4 * i + 4]
    unpack = {"format": "tar.gz", "strip": 1, "unpacked_bytes": int(unpacked), "entries": int(entries)}
    url = f"{base}/lab/{version}/linux/{arch}/agent-cli-package.tar.gz"
    builds.append({"platform": platform, "files": [{"url": url, "sha256": sha, "size": int(size), "path": ".", "unpack": unpack}]})
want = {
    "harness": "cursor",
    "version": version,
    "version_read": read | {"at": at},
    "verified": {"checksum": "same-origin", "signed": False, "how": how},
    "base": image_base,
    "builds": builds,
}
for key in sorted(set(want) | set(pin)):
    if pin.get(key) != want.get(key):
        problems.append(f"{key} is {json.dumps(pin.get(key))}, not {json.dumps(want.get(key))}")
print("; ".join(problems))
EOF

# A certificate for 127.0.0.1 alone, for this run: one day is the shortest -days takes.
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -subj /CN=127.0.0.1 \
  -addext subjectAltName=IP:127.0.0.1 -days 1 -keyout "$T/key.pem" -out "$T/cert.pem" 2> "$T/openssl.log" \
  || { cat "$T/openssl.log" >&2; exit 1; }
python3 -I -c "$FIXTURES" "$T/www" "$V" "$V2"
coproc SERVER { exec python3 -I -c "$SERVE" "$T/www" "$T/cert.pem" "$T/key.pem"; }
server=$SERVER_PID
read -r port http_port <&"${SERVER[0]}" || { echo "pin_test.sh: the test server did not start" >&2; exit 1; }

# verify PIN TREE VERSION SOURCE: what in pin.json differs from what TREE serves, as sha256sum,
# stat and GNU tar measure it rather than pin.sh's Python.
verify() {
  local pin=$1 tree=$2 version=$3 source=$4 arch f installer_sha= measured=()
  for arch in x64 arm64; do
    f=$T/www/$tree/lab/$version/linux/$arch/agent-cli-package.tar.gz
    measured+=("$(sha256sum < "$f" | cut -d' ' -f1)" "$(stat -c %s "$f")" "$(tar -tzf "$f" | wc -l)"
      "$(tar -tvzf "$f" | awk '$1 ~ /^-/ { s += $3 } END { print s + 0 }')")
  done
  [ "$source" != installer ] || installer_sha=$(sha256sum < "$T/www/$tree/install" | cut -d' ' -f1)
  python3 -I -c "$EXPECT" "$pin" "https://127.0.0.1:$port/$tree" "$version" "$source" "$installer_sha" "$BASE" "${measured[@]}"
}

passed=0 failed=0 n=0
# run TREE STATUS SAYS [ARG...]: pin.sh given ARGs against fixture TREE must exit STATUS and print
# SAYS. OLD=1 puts an earlier pin.json beside it first; HTTP=1 points it at the plain server.
run() {
  local tree=$1 want=$2 says=$3
  shift 3
  local label dir out status=0 why=() left origin=https://127.0.0.1:$port
  [ -z "${HTTP:-}" ] || origin=http://127.0.0.1:$http_port
  label="$tree${*+ (argument${2+s} $*)}${OLD:+, with an earlier pin.json}${HTTP:+, over plain HTTP}"
  n=$((n + 1))
  dir=$T/run/$n
  mkdir -p "$dir" "$T/cache"
  cp "$pin" "$dir/pin.sh"
  [ -z "${OLD:-}" ] || printf 'an earlier pin\n' > "$dir/pin.json"
  out=$(XDG_CACHE_HOME=$T/cache PIN_TEST_BASE_URL=$origin/$tree CURL_CA_BUNDLE=$T/cert.pem bash "$dir/pin.sh" "$@" 2>&1) || status=$?
  [ "$status" = "$want" ] || why+=("exit $status, not $want")
  grep -qF -- "$says" <<< "$out" || why+=("it did not say: $says")
  [ -z "$(ls -A "$T/cache")" ] || why+=("it left $(ls -A "$T/cache" | tr '\n' ' ')in its cache")
  left=$(ls -A "$dir" | tr '\n' ' ')
  if [ "$want" = 0 ]; then
    [ "$left" = "pin.json pin.sh " ] || why+=("its directory holds $left")
    local differs
    differs=$(verify "$dir/pin.json" "$tree" "${1:-$V}" "$([ $# -eq 0 ] && echo installer || echo argument)")
    [ -z "$differs" ] || why+=("$differs")
  elif [ -n "${OLD:-}" ]; then
    [ "$left" = "pin.json pin.sh " ] && [ "$(cat "$dir/pin.json")" = "an earlier pin" ] || why+=("the earlier pin.json changed, or files were left: $left")
  else
    [ "$left" = "pin.sh " ] || why+=("its directory holds $left")
  fi
  rm -rf "$T/cache"
  if [ ${#why[@]} -eq 0 ]; then
    passed=$((passed + 1))
    printf 'ok - %s: exit %s: %s\n' "$label" "$status" "$(grep -F -m 1 -- "$says" <<< "$out")"
  else
    failed=$((failed + 1))
    printf 'not ok - %s: %s\n' "$label" "$(printf '%s; ' "${why[@]}")"
    printf '%s\n' "$out" | sed 's/^/    /'
  fi
}

run good 0 "pinned cursor-agent $V for linux/amd64 and linux/arm64"
OLD=1 run good 0 "pinned cursor-agent $V for linux/amd64 and linux/arm64"
run asked 0 "version $V2, as asked" "$V2"
run installer-missing 69 "/installer-missing/install answered 404, not 200"
run no-download-url 65 "it has no DOWNLOAD_URL line"
run two-download-urls 65 "it has 2 DOWNLOAD_URL lines"
run other-host 65 "its DOWNLOAD_URL line is not the form pin.sh reads"
run installer-dotdot 65 "its DOWNLOAD_URL names version .., not one URL path segment"
run installer-space 65 "its DOWNLOAD_URL names version 2026.11.11\\ b912975, not one URL path segment"
run installer-redirect-http 69 'Protocol "http"'
HTTP=1 run good 69 'Protocol "http"'
run good 64 "VERSION 2026.11.11/x is not one URL path segment" 2026.11.11/x
run good 64 "VERSION .. is not one URL path segment" ..
run good 64 "VERSION . is not one URL path segment" .
run good 64 "VERSION a\\ b is not one URL path segment" "a b"
run good 64 "VERSION '' is not one URL path segment" ""
run good 64 "VERSION 2026.11.11-b912975\\? is not one URL path segment" "2026.11.11-b912975?"
run good 64 "unknown option -x" -x
run good 64 "one VERSION at the most" "$V" "$V2"
run sidecar-digit 65 "the one its .sha256 gives"
OLD=1 run sidecar-digit 65 "the one its .sha256 gives"
run sidecar-404 69 "agent-cli-package.tar.gz.sha256 answered 404, not 200"
run sidecar-other-file 65 "not the one line"
run sidecar-uppercase 65 "not the one line"
run sidecar-two-lines 65 "not the one line"
run archive-404 69 "arm64/agent-cli-package.tar.gz answered 404, not 200"
run dotdot 65 "../x has a .. component"
run dotdot-inside 65 "dist-package/../../x has a .. component"
run absolute 65 "/dist-package/x is an absolute path"
run outside 65 "other/x is outside dist-package/"
run dot-component 65 "dist-package/./x has an empty or . component"
run symlink 65 "dist-package/agent is a symbolic link"
run hardlink 65 "dist-package/agent is a hard link"
run fifo 65 "dist-package/pipe is a FIFO"
run chardev 65 "dist-package/null is a character device"
run no-agent 65 "it holds no dist-package/cursor-agent"
run agent-not-exec 65 "dist-package/cursor-agent is not a regular file its owner may execute"
run agent-dir 65 "dist-package/cursor-agent is not a regular file its owner may execute"
run top-file 65 "dist-package is not a directory, and the strip leaves nothing of it"
run setuid 65 "dist-package/cursor-agent has a set-ID or sticky bit (mode 4755)"
run sticky 65 "dist-package/node_modules has a set-ID or sticky bit (mode 1777)"
run duplicate 65 "dist-package/index.js is in the archive twice"
run case-fold 65 "dist-package/Index.js and dist-package/index.js are one path"
run normalization 65 "are one path on a filesystem that folds case or normalizes names"
run parent-file 65 "dist-package/lib/x.js is under dist-package/lib, a regular file"
run child-first 65 "dist-package/lib is a regular file, but an earlier entry is under it"
run root-harness-json 65 "dist-package/harness.json is where kiln writes the harness's harness.json"
run not-gzip 65 "it is not a whole tar.gz"
run gzip-not-tar 65 "it is not a whole tar.gz"
run truncated 65 "it is not a whole tar.gz"
run garbage-header 65 "it holds data past the end of its tar archive"
run trailing-data 65 "it holds data past the end of its tar archive"

if [ "$failed" -eq 0 ]; then
  echo "all $passed cases passed"
else
  echo "$failed of $((passed + failed)) cases failed"
  exit 1
fi
