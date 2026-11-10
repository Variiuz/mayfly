#!/usr/bin/env bash
# Installs or updates Mayfly on a Proxmox host. Run as root:
#   curl -fsSL https://github.com/variiuz/mayfly/releases/latest/download/install.sh | bash
#   bash install.sh [--no-timer]                     (from a release tarball or a checkout)
# Without the files next to it, it downloads the release it belongs to (checked against SHA256SUMS).
# Installs the command, the units, the template files and the shipped profiles. Creates
# /etc/mayfly/{pool.env,slots.conf} only when missing. Starts no slot and touches no VM.
# The gc timer is switched on by the first install only; later installs keep it as it is.
# --no-timer leaves it off (while another tool still runs slots in the same VMID range).
set -euo pipefail

VERSION=main # set to the release version when the release is built
REPO_URL=${MAYFLY_REPO_URL:-https://github.com/variiuz/mayfly}

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  C_RED=$'\e[31m' C_GRN=$'\e[32m' C_YEL=$'\e[33m' C_BLU=$'\e[36m' C_DIM=$'\e[2m' C_B=$'\e[1m' C_0=$'\e[0m'
else
  C_RED="" C_GRN="" C_YEL="" C_BLU="" C_DIM="" C_B="" C_0=""
fi
step() { printf '%s›%s %s\n' "$C_BLU" "$C_0" "$*"; }
done_() { printf '%s✓%s %s\n' "$C_GRN" "$C_0" "$*"; }
warn() { printf '%s!%s %s\n' "$C_YEL" "$C_0" "$*"; }
die() { printf '%s✗%s %s\n' "$C_RED" "$C_0" "$*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root"
command -v qm >/dev/null || die "not a Proxmox host (qm is missing)"

src=""
[ -z "${BASH_SOURCE[0]:-}" ] || [ ! -f "${BASH_SOURCE[0]}" ] || src=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
if [ -z "$src" ] || [ ! -f "$src/mayfly" ]; then
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

timer=keep
[ "${1:-}" != --no-timer ] || timer=no
conf=/etc/mayfly
share=/usr/local/share/mayfly
first=no
[ -e /usr/local/sbin/mayfly ] || first=yes
[ "$timer" = no ] || [ "$first" = no ] || timer=yes

old=$( [ "$first" = no ] && /usr/local/sbin/mayfly version 2>/dev/null | awk '{print $2}' || true)
new=$(awk -F'[= ]' '/^MAYFLY_VERSION=/ { print $2; exit }' "$src/mayfly")
step "installing Mayfly $new${old:+ (was $old)}"

apt-get install -y -q jq curl openssl >/dev/null
install -d -m 0700 "$conf" "$conf/env"
install -d -m 0755 "$conf/hooks.d" "$conf/profiles"
install -m 0755 "$src/mayfly" /usr/local/sbin/mayfly
install -d "$share"
install -m 0644 "$src"/template/{provision.sh,ci-job.sh,pod-vm.sh,mayfly-job.service,template.env} "$share/"
# Shipped profiles are replaced on every install; your own go in /etc/mayfly/profiles/<name>/.
rm -rf "$share/profiles"
cp -r "$src/profiles" "$share/profiles"
chmod -R u=rwX,go=rX "$share/profiles"
install -m 0644 "$src"/mayfly-slot@.service "$src"/mayfly-gc.service "$src"/mayfly-gc.timer /etc/systemd/system/
created=()
[ -e "$conf/pool.env" ] || { install -m 0600 "$src/pool.env.example" "$conf/pool.env"; created+=(pool.env); }
[ -e "$conf/slots.conf" ] || { install -m 0644 "$src/slots.conf.example" "$conf/slots.conf"; created+=(slots.conf); }

# Cloud-init snippets need a storage with "snippets" content. Add it to the snippet storage,
# keeping whatever content types it already has.
SNIPPET_STORAGE=$(sed -n 's/^SNIPPET_STORAGE=//p' "$conf/pool.env" | tail -n1)
SNIPPET_STORAGE=${SNIPPET_STORAGE:-local}
content=$(pvesh get /storage/"$SNIPPET_STORAGE" --output-format json | jq -r .content)
case ",$content," in
  *,snippets,*) ;;
  *) pvesm set "$SNIPPET_STORAGE" --content "$content,snippets"; done_ "storage $SNIPPET_STORAGE now holds snippets" ;;
esac

systemctl daemon-reload
case "$timer" in
  yes) systemctl enable -q --now mayfly-gc.timer ;;
  no) systemctl disable -q --now mayfly-gc.timer 2>/dev/null || true ;;
esac
# Running slots pick up the new command at their next job; restarting them would cancel jobs.
done_ "Mayfly $new installed${created:+, created ${created[*]} in $conf}"

if [ "$first" = yes ] || [ ${#created[@]} -gt 0 ]; then
  echo
  printf '%sNext%s\n' "$C_B" "$C_0"
  n=0
  next() { n=$((n + 1)); printf '  %s. %s\n' "$n" "$*"; }
  [ -e "$conf/app.pem" ] || next "GitHub App private key to $conf/app.pem (chmod 600)"
  grep -q '^APP_ID=.' "$conf/pool.env" || next "APP_ID and ORG (or REPO) in $conf/pool.env"
  next "slots in $conf/slots.conf"
  next "mayfly check"
  next "mayfly template build, then mayfly template promote <vmid>"
  next "mayfly enable"
  printf '  %sGetting started: %s#get-started%s\n' "$C_DIM" "$REPO_URL" "$C_0"
fi
[ "$timer" != no ] || warn "gc timer is off (--no-timer); mayfly enable starts it"
