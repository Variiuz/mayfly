#!/usr/bin/env bash
# Builds the release files of the checked-out commit into <outdir>:
#   mayfly-<version>.tar.gz   the files install.sh needs (no tests, no CI)
#   install.sh                the one-line installer, pinned to <version>
#   SHA256SUMS
# Usage: scripts/package.sh <version> <outdir>
set -euo pipefail
v=${1:?usage: scripts/package.sh <version> <outdir>} out=${2:?usage: scripts/package.sh <version> <outdir>}
[[ $v =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version $v: x.y.z" >&2; exit 1; }
root=$(cd "$(dirname "$0")/.." && pwd)
have=$(awk -F'[= ]' '/^MAYFLY_VERSION=/ { print $2; exit }' "$root/mayfly")
[ "$have" = "$v" ] || { echo "mayfly says $have, not $v" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$out"
git -C "$root" archive --format=tar --prefix="mayfly-$v/" HEAD | tar -C "$tmp" -xf -
sed -i "s/^VERSION=main /VERSION=$v /" "$tmp/mayfly-$v/install.sh"
grep -q "^VERSION=$v " "$tmp/mayfly-$v/install.sh" || { echo "could not pin install.sh" >&2; exit 1; }
mtime=$(git -C "$root" log -1 --format=%ct)
tar -C "$tmp" --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$mtime" -czf "$out/mayfly-$v.tar.gz" "mayfly-$v"
install -m 0755 "$tmp/mayfly-$v/install.sh" "$out/install.sh"
(cd "$out" && sha256sum "mayfly-$v.tar.gz" install.sh >SHA256SUMS)
echo "packaged Mayfly $v in $out:"
ls -l "$out"
