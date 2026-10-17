# syntax=docker/dockerfile:1.7
#
# kiln — single static binary (server, agent, supervisor, CLI) with the web UI embedded.
#
#   docker build --build-arg KILN_COMMIT=$(git rev-parse HEAD) -t kiln:dev .
#   docker run --rm -p 7070:7070/tcp -p 7071:7071/udp -v kiln-data:/var/lib/kiln kiln:dev
#
# Stages:
#   ui     – Vite build of ui/ → ui/dist (embedded into the binary by rust-embed)
#   build  – static musl kiln binary (cargo registry + target dir are BuildKit cache mounts)
#   final  – alpine with git/bash/ssh (the server shells out to git; the terminal needs a shell),
#            and helm and kustomize, which render apps for Deployments (docs/cd.md)

ARG NODE_IMAGE=node:24-alpine
ARG RUST_IMAGE=rust:1-alpine
ARG RUNTIME_IMAGE=alpine:3.24

# ---------------------------------------------------------------------------- ui
FROM ${NODE_IMAGE} AS ui
WORKDIR /src/ui
COPY ui/package*.json ./
RUN --mount=type=cache,target=/root/.npm \
    if [ -f package-lock.json ]; then npm ci --no-audit --no-fund; else npm install --no-audit --no-fund; fi
COPY ui/ ./
RUN npm run build

# ------------------------------------------------------------------------- build
FROM ${RUST_IMAGE} AS build
# aws-lc-sys (rustls/quinn crypto) wants cmake/clang/perl; rusqlite "bundled" compiles sqlite with cc.
RUN apk add --no-cache musl-dev gcc g++ make cmake perl clang clang-dev linux-headers pkgconf git
ARG TARGETARCH
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY --from=ui /src/ui/dist ui/dist
# The UI is already built in the `ui` stage; a build.rs must not try to run npm again.
# The final binary must be fully static (checked below): job pods copy /kiln into arbitrary images.
# KILN_COMMIT is the commit it's built from (the context has no .git): the server builds every
# other platform's binary from the same one, so every runner can run what it runs.
ARG KILN_COMMIT=""
ENV KILN_SKIP_UI_BUILD=1 \
    KILN_COMMIT=${KILN_COMMIT} \
    CARGO_NET_GIT_FETCH_WITH_CLI=true \
    CARGO_INCREMENTAL=0
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) triple=x86_64-unknown-linux-musl ;; \
      arm64) triple=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    rustup target add "$triple" >/dev/null 2>&1 || true; \
    RUSTFLAGS="-C target-feature=+crt-static" \
      cargo build --locked --release --target "$triple" -p kiln; \
    mkdir -p /out; \
    cp "target/$triple/release/kiln" /out/kiln; \
    if readelf -l /out/kiln | grep -q 'INTERP'; then echo "kiln is not static" >&2; exit 1; fi

# ------------------------------------------------------------------------- final
FROM ${RUNTIME_IMAGE}
RUN set -eux; \
    apk add --no-cache git bash openssh-client ca-certificates tzdata curl less ncurses-terminfo-base helm kustomize; \
    addgroup -S -g 10001 kiln; \
    adduser -S -D -H -u 10001 -G kiln -h /var/lib/kiln -s /bin/bash kiln; \
    mkdir -p /etc/kiln /var/lib/kiln /cache; \
    chown kiln:kiln /var/lib/kiln /cache
# Real file at /usr/local/bin/kiln: job pods' initContainer copies it (`cp /usr/local/bin/kiln …`).
COPY --from=build /out/kiln /usr/local/bin/kiln
RUN ln -s /usr/local/bin/kiln /kiln
COPY --chmod=0644 deploy/docker/kiln.toml /etc/kiln/kiln.toml

ENV KILN_CONFIG=/etc/kiln/kiln.toml \
    KILN_DATA_DIR=/var/lib/kiln \
    HOME=/var/lib/kiln

USER 10001:10001
WORKDIR /var/lib/kiln
EXPOSE 7070/tcp 7071/udp

HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:7070/healthz || exit 1

# `kiln supervise` restarts the server with backoff and reaps zombies, so it is a fine PID 1.
ENTRYPOINT ["/kiln", "supervise", "--"]
CMD ["server"]
