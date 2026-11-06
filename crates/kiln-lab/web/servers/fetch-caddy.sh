#!/bin/bash
# Fetches the lab's Caddy (a lab program, never a kiln dependency): the pinned release, checked
# against the digest its release publishes, unpacked under ~/.cache.
set -euo pipefail
V=2.10.2
SHA512=747df7ee74de188485157a383633a1a963fd9233b71fbb4a69ddcbcc589ce4e2cc82dacf5dbbe136cb51d17e14c59daeb5d9bc92487610b0f3b93680b2646546
D=$HOME/.cache/kiln-bp/g1/tools/caddy
mkdir -p $D
builtin cd $D
[ -f caddy.tar.gz ] || curl -sSfL -o caddy.tar.gz https://github.com/caddyserver/caddy/releases/download/v$V/caddy_${V}_linux_amd64.tar.gz
echo "$SHA512  caddy.tar.gz" | sha512sum -c --quiet
tar xzf caddy.tar.gz caddy
./caddy version
sha256sum caddy
