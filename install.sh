#!/usr/bin/env bash
# Installs or updates Mayfly on a Proxmox host. Run as root:
#   curl -fsSL https://github.com/variiuz/mayfly/releases/latest/download/install.sh | bash
#   bash install.sh [--no-timer]                     (from a release tarball or a checkout)
# Without the files next to it, it downloads the release it belongs to (checked against SHA256SUMS).
# From a checkout it builds the binary with cargo when there is none next to it, or takes the released
# binary of the checkout's version when there is no cargo.
# It only finds and checks the files; `mayfly install` (the binary) puts them in place: the command, mayfly.sh,
# the units, the template files and the shipped profiles, /etc/mayfly/{pool.env,slots.conf} only when missing,
# the timers. See src/cmd_install.rs. The arguments (--no-timer) go to it.
# MAYFLY_SBIN, MAYFLY_SHARE, MAYFLY_SYSTEMD_DIR and MAYFLY_CONF_DIR move the command, the share directory, the
# units and /etc/mayfly (tests; the binary reads them from the environment).
set -euo pipefail

VERSION=main # set to the release version when the release is built
REPO_URL=${MAYFLY_REPO_URL:-https://github.com/variiuz/mayfly}

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  C_RED=$'\e[31m' C_BLU=$'\e[36m' C_0=$'\e[0m'
else
  C_RED="" C_BLU="" C_0=""
fi
step() { printf '%s›%s %s\n' "$C_BLU" "$C_0" "$*"; }
die() { printf '%s✗%s %s\n' "$C_RED" "$C_0" "$*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root"
command -v qm >/dev/null || die "not a Proxmox host (qm is missing)"

src=""
[ -z "${BASH_SOURCE[0]:-}" ] || [ ! -f "${BASH_SOURCE[0]}" ] || src=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
if [ -z "$src" ] || [ ! -f "$src/mayfly.sh" ]; then
  # Run from a pipe or on its own: fetch the release.
  [ "$VERSION" != main ] || die "this install.sh is not from a release; use $REPO_URL/releases/latest/download/install.sh"
  command -v curl >/dev/null || die "curl is missing"
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  step "downloading Mayfly $VERSION"
  curl -fsSL --retry 3 -o "$tmp/mayfly.tar.gz" "$REPO_URL/releases/download/v$VERSION/mayfly-$VERSION.tar.gz"
  curl -fsSL --retry 3 -o "$tmp/SHA256SUMS" "$REPO_URL/releases/download/v$VERSION/SHA256SUMS"
  want=$(awk -v f="mayfly-$VERSION.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
  [ -n "$want" ] && [ "$(sha256sum "$tmp/mayfly.tar.gz" | awk '{print $1}')" = "$want" ] || die "checksum mismatch for mayfly-$VERSION.tar.gz"
  tar -C "$tmp" -xzf "$tmp/mayfly.tar.gz"
  bash "$tmp/mayfly-$VERSION/install.sh" "$@"
  exit
fi

bin=$src/mayfly
if [ ! -f "$bin" ]; then
  # A checkout has no binary yet.
  if command -v cargo >/dev/null && [ -f "$src/Cargo.toml" ]; then
    step "building the mayfly binary"
    (cd "$src" && cargo build --release --locked) || die "cargo build failed"
    bin=$src/target/release/mayfly
  else
    # No cargo (a normal Proxmox host): take the released binary of the version this checkout says.
    command -v curl >/dev/null || die "no mayfly binary next to install.sh and no curl or cargo; use a release"
    v=$(awk -F'[= ]' '/^MAYFLY_VERSION=/ { print $2; exit }' "$src/mayfly.sh")
    step "no cargo here: downloading the released mayfly $v binary"
    btmp=$(mktemp -d)
    trap 'rm -rf "$btmp"' EXIT
    curl -fsSL --retry 3 -o "$btmp/mayfly" "$REPO_URL/releases/download/v$v/mayfly-$v-x86_64-linux" &&
      curl -fsSL --retry 3 -o "$btmp/SHA256SUMS" "$REPO_URL/releases/download/v$v/SHA256SUMS" ||
      die "release v$v has no mayfly binary; use a release, or install cargo to build this checkout"
    want=$(awk -v f="mayfly-$v-x86_64-linux" '$2 == f || $2 == "*" f { print $1 }' "$btmp/SHA256SUMS")
    [ -n "$want" ] && [ "$(sha256sum "$btmp/mayfly" | awk '{print $1}')" = "$want" ] || die "checksum mismatch for mayfly-$v-x86_64-linux"
    chmod 0755 "$btmp/mayfly"
    bin=$btmp/mayfly
  fi
fi
"$bin" version >/dev/null 2>&1 || die "$bin does not run on this host"

# A temporary binary (downloaded) must be cleaned up afterwards, so it cannot be exec'd.
if [ -n "${btmp:-}" ]; then
  rc=0
  "$bin" install --from "$src" "$@" || rc=$?
  exit "$rc"
fi
exec "$bin" install --from "$src" "$@"
