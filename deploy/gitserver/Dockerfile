# kiln-git: tiny read-only git smart-HTTP server for ArgoCD inside the dev cluster.
#
# nginx -> fcgiwrap -> git-http-backend, anonymous read-only (receive-pack is disabled because
# REMOTE_USER is never set). Content is written by `deploy/gitserver/push.sh`, which streams a
# git bundle through `kubectl exec` — so pushing needs cluster access, not a git password.
#
#   docker build -t kiln-git:dev deploy/gitserver
FROM alpine:3.24
RUN set -eux; \
    apk add --no-cache git git-daemon nginx fcgiwrap tini; \
    git config --system safe.directory '*'; \
    git config --system uploadpack.allowReachableSHA1InWant true; \
    git config --system uploadpack.allowFilter true; \
    addgroup -S -g 10002 git; \
    adduser -S -D -H -u 10002 -G git -h /srv/git -s /bin/sh git; \
    mkdir -p /srv/git; chown git:git /srv/git; \
    test -x /usr/libexec/git-core/git-http-backend
COPY --chmod=0644 nginx.conf /etc/nginx/kiln-git.conf
COPY --chmod=0755 entrypoint.sh /usr/local/bin/kiln-git-entrypoint
USER 10002:10002
EXPOSE 8080
VOLUME /srv/git
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/kiln-git-entrypoint"]
