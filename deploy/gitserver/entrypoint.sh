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

fcgiwrap -c "${FCGI_CHILDREN:-4}" -s unix:/tmp/fcgiwrap.sock &

# wait for the socket so the first request does not 502
i=0
while [ ! -S /tmp/fcgiwrap.sock ] && [ "$i" -lt 50 ]; do
    i=$((i + 1))
    sleep 0.1
done

exec nginx -e /dev/stderr -c /etc/nginx/kiln-git.conf -g 'daemon off;'
