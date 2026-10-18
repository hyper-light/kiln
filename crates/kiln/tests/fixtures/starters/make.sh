#!/usr/bin/env bash
# Regenerates the fixture's generated files: the autotools' (configure, Makefile.in and the
# helper scripts), npm's and Composer's lock files, and the Gradle wrapper. Needs autoconf,
# automake, npm and Docker.
set -euo pipefail
cd "$(dirname "$0")"
autoreconf -i && rm -rf autom4te.cache
npm install --package-lock-only --ignore-scripts --no-audit --no-fund
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD:/w" -w /w composer:2 composer update --no-install --no-interaction
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -e GRADLE_USER_HOME=/tmp/g -v "$PWD:/w" -w /w gradle:8.14.3-jdk21 gradle wrapper --gradle-version 8.14.3 --no-daemon -q
rm -rf .gradle
