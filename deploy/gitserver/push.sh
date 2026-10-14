#!/usr/bin/env bash
# Push the local kiln repository into the in-cluster git server (kiln-git) that ArgoCD reads.
#
#   deploy/gitserver/push.sh                # snapshot of the working tree (committed + uncommitted)
#   KILN_GIT_PUSH_MODE=head deploy/gitserver/push.sh   # only the committed HEAD
#
# Transport: a git bundle streamed through `kubectl exec` — no git credentials, no port-forward;
# whoever can exec into the pod can publish. The server itself is read-only over HTTP.
#
# Snapshot mode builds a commit object from the working tree using a temporary index
# (respecting .gitignore, plus hard excludes for build output) WITHOUT touching your branch,
# index or stash. The commit is published as refs/heads/<branch> on the in-cluster server.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
NAMESPACE="${KILN_GIT_NAMESPACE:-kiln-git}"
TARGET="${KILN_GIT_TARGET:-deploy/kiln-git}"
REPO_NAME="${KILN_GIT_REPO:-kiln}"
BRANCH="${KILN_GIT_BRANCH:-main}"
MODE="${KILN_GIT_PUSH_MODE:-snapshot}"   # snapshot | head
KUBECTL="${KUBECTL:-kubectl}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"            # e.g. kiln (the minikube profile)

log() { printf '\033[1;34m[push]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[push] error:\033[0m %s\n' "$*" >&2; exit 1; }

command -v git >/dev/null || die "git not found"
command -v "$KUBECTL" >/dev/null || die "kubectl not found"
kc() {
    if [ -n "$KUBE_CONTEXT" ]; then "$KUBECTL" --context "$KUBE_CONTEXT" "$@"; else "$KUBECTL" "$@"; fi
}
git -C "$REPO_ROOT" rev-parse --git-dir >/dev/null 2>&1 || die "$REPO_ROOT is not a git repository"

workdir="$(mktemp -d "${TMPDIR:-/tmp}/kiln-push.XXXXXX")"
tmpref="refs/kiln-push/snapshot-$$"
cleanup() {
    git -C "$REPO_ROOT" update-ref -d "$tmpref" >/dev/null 2>&1 || true
    rm -rf -- "${workdir:?}"
}
trap cleanup EXIT

parent=""
if git -C "$REPO_ROOT" rev-parse --verify -q HEAD >/dev/null; then
    parent="$(git -C "$REPO_ROOT" rev-parse HEAD)"
fi

case "$MODE" in
head)
    [ -n "$parent" ] || die "KILN_GIT_PUSH_MODE=head but the repository has no commits yet"
    commit="$parent"
    ;;
snapshot)
    export GIT_INDEX_FILE="$workdir/index"
    if [ -n "$parent" ]; then
        git -C "$REPO_ROOT" read-tree "$parent"
    else
        git -C "$REPO_ROOT" read-tree --empty
    fi
    # Hard excludes even without a .gitignore: build output and dependency trees are huge.
    git -C "$REPO_ROOT" add -A -- . \
        ':(exclude,glob)**/target/**' \
        ':(exclude,glob)**/node_modules/**' \
        ':(exclude,glob)**/.env' \
        ':(exclude,glob)**/*.token'
    tree="$(git -C "$REPO_ROOT" write-tree)"
    unset GIT_INDEX_FILE
    if [ -n "$parent" ] && [ "$tree" = "$(git -C "$REPO_ROOT" rev-parse "$parent^{tree}")" ]; then
        commit="$parent"   # clean working tree: publish HEAD as-is
    else
        export GIT_AUTHOR_NAME="${GIT_AUTHOR_NAME:-$(git -C "$REPO_ROOT" config user.name || echo kiln-bootstrap)}"
        export GIT_AUTHOR_EMAIL="${GIT_AUTHOR_EMAIL:-$(git -C "$REPO_ROOT" config user.email || echo kiln-bootstrap@localhost)}"
        export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME" GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"
        msg="kiln working-tree snapshot $(date -u +%Y-%m-%dT%H:%M:%SZ)${parent:+ on ${parent:0:12}}"
        if [ -n "$parent" ]; then
            commit="$(git -C "$REPO_ROOT" commit-tree "$tree" -p "$parent" -m "$msg")"
        else
            commit="$(git -C "$REPO_ROOT" commit-tree "$tree" -m "$msg")"
        fi
    fi
    ;;
*) die "unknown KILN_GIT_PUSH_MODE=$MODE (snapshot|head)" ;;
esac

git -C "$REPO_ROOT" update-ref "$tmpref" "$commit"
git -C "$REPO_ROOT" bundle create "$workdir/repo.bundle" "$tmpref" 2>/dev/null
log "publishing $commit ($MODE) as $BRANCH → $NAMESPACE/$TARGET:/srv/git/$REPO_NAME.git"

kc -n "$NAMESPACE" rollout status "$TARGET" --timeout=180s >/dev/null
# shellcheck disable=SC2016 # expanded by the remote shell
kc -n "$NAMESPACE" exec -i "$TARGET" -- sh -c '
    set -eu
    repo="/srv/git/$1.git"
    [ -d "$repo" ] || git init --quiet --bare --initial-branch=main "$repo"
    b="$(mktemp /tmp/push.XXXXXX)"
    cat > "$b"
    git -C "$repo" fetch --quiet --force "$b" "$2:refs/heads/$3"
    git -C "$repo" symbolic-ref HEAD "refs/heads/$3"
    rm -f "$b"
    git -C "$repo" gc --auto --quiet || true
    git -C "$repo" rev-parse "refs/heads/$3"
' sh "$REPO_NAME" "$tmpref" "$BRANCH" < "$workdir/repo.bundle"

# Ask ArgoCD to re-read the repo now instead of waiting for its poll interval.
if kc get crd applications.argoproj.io >/dev/null 2>&1; then
    for app in $(kc -n argocd get applications.argoproj.io -o name 2>/dev/null); do
        kc -n argocd annotate "$app" argocd.argoproj.io/refresh=normal --overwrite >/dev/null 2>&1 || true
    done
fi
log "done"
