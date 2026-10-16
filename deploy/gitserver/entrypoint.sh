#!/bin/sh
# Start fcgiwrap + nginx serving bare repositories under /srv/git (read-only).
set -eu

ROOT=/srv/git
# Space-separated repository names to create (empty, bare) if missing.
: "${KILN_GIT_REPOS:=kiln}"

for name in $KILN_GIT_REPOS; do
    repo="$ROOT/$name.git"
    if [ ! -d "$repo" ]; then
        git init --quiet --bare --initial-branch=main "$repo"
        echo "kiln-git: created $repo"
    fi
    git -C "$repo" config http.uploadpack true
    git -C "$repo" config http.receivepack false
done

SOCK=/tmp/fcgiwrap.sock
# /tmp is an emptyDir, which outlives container restarts: a socket left by the previous run would
# make fcgiwrap fail to bind (and look like a live socket below), so every restart would 502.
rm -f "$SOCK"
fcgiwrap -c "${FCGI_CHILDREN:-4}" -s "unix:$SOCK" &
FCGI_PID=$!

# wait for the socket so the first request does not 502
i=0
while [ ! -S "$SOCK" ] && [ "$i" -lt 50 ]; do
    i=$((i + 1))
    sleep 0.1
done
if ! kill -0 "$FCGI_PID" 2>/dev/null || [ ! -S "$SOCK" ]; then
    echo "kiln-git: fcgiwrap did not start" >&2
    exit 1
fi

exec nginx -e /dev/stderr -c /etc/nginx/kiln-git.conf -g 'daemon off;'
