#!/usr/bin/env bash
# Default profile: a compiler toolchain on top of the core (runner, Docker, gh). Copy this folder to
# /etc/mayfly/profiles/<name>/ to make your own; template.env there is read before this runs.
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q --no-install-recommends build-essential pkg-config libssl-dev shellcheck
