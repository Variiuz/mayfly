#!/usr/bin/env bash
# Builds the release files of the checked-out commit into <outdir>:
#   mayfly-<version>.tar.gz           the files install.sh needs (no tests, no CI), with the binary
#   mayfly-<version>-x86_64-linux     the binary on its own
#   install.sh                        the one-line installer, pinned to <version>
#   SHA256SUMS
# <binary> is the static x86_64 build of this commit (cargo build --release --target x86_64-unknown-linux-musl).
# Cargo.toml, the binary and <version> must agree.
# Usage: scripts/package.sh <version> <outdir> <binary>
set -euo pipefail
usage="usage: scripts/package.sh <version> <outdir> <binary>"
v=${1:?$usage} out=${2:?$usage} bin=${3:?$usage}
[[ $v =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version $v: x.y.z" >&2; exit 1; }
[ -x "$bin" ] || { echo "$bin is not an executable file" >&2; exit 1; }
root=$(cd "$(dirname "$0")/.." && pwd)
crate=$(awk -F'"' '/^version *=/ { print $2; exit }' "$root/Cargo.toml")
[ "$crate" = "$v" ] || { echo "Cargo.toml says $crate, not $v" >&2; exit 1; }
[ "$("$bin" version)" = "mayfly $v" ] || { echo "$bin says $("$bin" version), not mayfly $v" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$out"
git -C "$root" archive --format=tar --prefix="mayfly-$v/" HEAD | tar -C "$tmp" -xf -
sed -i "s/^VERSION=main /VERSION=$v /" "$tmp/mayfly-$v/install.sh"
grep -q "^VERSION=$v " "$tmp/mayfly-$v/install.sh" || { echo "could not pin install.sh" >&2; exit 1; }
install -m 0755 "$bin" "$tmp/mayfly-$v/mayfly"
mtime=$(git -C "$root" log -1 --format=%ct)
tar -C "$tmp" --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$mtime" -czf "$out/mayfly-$v.tar.gz" "mayfly-$v"
install -m 0755 "$tmp/mayfly-$v/install.sh" "$out/install.sh"
install -m 0755 "$bin" "$out/mayfly-$v-x86_64-linux"
(cd "$out" && sha256sum "mayfly-$v.tar.gz" install.sh "mayfly-$v-x86_64-linux" >SHA256SUMS)
echo "packaged Mayfly $v in $out:"
ls -l "$out"
