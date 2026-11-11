#!/usr/bin/env bash
# Turns a Debian 13 (trixie) VM into a Proxmox VE host, in two stages split by a reboot.
# Run as root, twice (it detects the stage itself), with the Proxmox archive keyring copied from any
# Proxmox host (its SHA-256 is checked below):
#   scp root@<proxmox host>:/usr/share/keyrings/proxmox-archive-keyring.gpg /root/
#   bash pve-on-debian.sh /root/proxmox-archive-keyring.gpg      # stage 1, then reboot
#   bash pve-on-debian.sh /root/proxmox-archive-keyring.gpg      # stage 2 (running the -pve kernel), then reboot
# Never touches disks or partitions. Re-runnable.
set -euo pipefail

KEYRING_SHA256=136673be77aba35dcce385b28737689ad64fd785a797e57897589aed08db6e45
KEYRING=/usr/share/keyrings/proxmox-archive-keyring.gpg

log() { echo "pve-on-debian: $*"; }
die() { echo "pve-on-debian: $*" >&2; exit 1; }
export DEBIAN_FRONTEND=noninteractive
# Keep changed config files (sshd, ufw, ...) without asking.
APT_OPTS=(-o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold)

[ "$(id -u)" = 0 ] || die "run as root"
[ $# -eq 1 ] || die "usage: $0 /path/to/proxmox-archive-keyring.gpg"
src_key=$1

# shellcheck source=/dev/null
codename=$(. /etc/os-release && echo "${ID:-}:${VERSION_CODENAME:-}")
[ "$codename" = "debian:trixie" ] || die "this needs Debian 13 (trixie), found $codename"
if [ ! -e /dev/kvm ]; then
  die "/dev/kvm is missing: nested virtualization is off. Power the VM off in ESXi and tick Edit settings > CPU > Hardware virtualization > 'Expose hardware assisted virtualization to the guest OS'."
fi

# --- keyring -------------------------------------------------------------------------------------
[ -r "$src_key" ] || die "keyring $src_key not readable"
got=$(sha256sum "$src_key" | awk '{print $1}')
[ "$got" = "$KEYRING_SHA256" ] || die "keyring sha256 is $got, expected $KEYRING_SHA256"
install -m 0644 "$src_key" "$KEYRING"
log "keyring installed at $KEYRING (sha256 ok)"

host=$(hostname -s)

if ! uname -r | grep -q -- '-pve'; then
  # ===== Stage 1: network pinned, repo added, Proxmox kernel installed ===============================
  log "stage 1 (kernel $(uname -r) is not a -pve kernel)"

  ifname=$(ip -4 route show default | awk '{for (i=1;i<NF;i++) if ($i=="dev") {print $(i+1); exit}}')
  gw=$(ip -4 route show default | awk '{for (i=1;i<NF;i++) if ($i=="via") {print $(i+1); exit}}')
  [ -n "$ifname" ] && [ -n "$gw" ] || die "no IPv4 default route found"
  cidr=$(ip -4 -o addr show dev "$ifname" scope global | awk '{print $4; exit}')
  [ -n "$cidr" ] || die "no IPv4 address on $ifname"
  ip4=${cidr%/*}
  log "uplink $ifname, address $cidr, gateway $gw"

  dns=""
  if command -v resolvectl >/dev/null; then
    dns=$(resolvectl dns "$ifname" 2>/dev/null | sed 's/^[^:]*://' | tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | tr '\n' ' ' || true)
  fi
  dns=${dns% }
  [ -n "$dns" ] || { dns=$gw; log "no DNS found on $ifname, using the gateway $gw"; }
  log "DNS: $dns"

  # /etc/hosts: PVE needs the hostname to resolve to the real address, not 127.0.1.1.
  sed -i -e '/^127\.0\.1\.1[[:space:]]/d' -e "/[[:space:]]${host}\.local[[:space:]]/d" /etc/hosts
  echo "$ip4 $host.local $host" >>/etc/hosts
  log "/etc/hosts: $ip4 $host.local $host"

  # cloud-init would rewrite the network config at boot.
  install -d /etc/cloud
  touch /etc/cloud/cloud-init.disabled
  log "cloud-init disabled"

  # ifupdown2 reads this after the stage-2 reboot; netplan stays in charge until then.
  cat >/etc/network/interfaces <<IFACES
# Written by pve-on-debian.sh
auto lo
iface lo inet loopback

# Static on purpose: keep the DHCP reservation for this IP.
auto $ifname
iface $ifname inet static
    address $cidr
    gateway $gw

source /etc/network/interfaces.d/*
IFACES
  install -d /etc/systemd/resolved.conf.d
  printf '[Resolve]\nDNS=%s\n' "$dns" >/etc/systemd/resolved.conf.d/mayfly.conf
  log "wrote /etc/network/interfaces and /etc/systemd/resolved.conf.d/mayfly.conf"

  cat >/etc/apt/sources.list.d/pve-install-repo.sources <<SRC
Types: deb
URIs: http://download.proxmox.com/debian/pve
Suites: trixie
Components: pve-no-subscription
Signed-By: $KEYRING
SRC
  log "added the pve-no-subscription repository"

  # Debian cloud images boot through grub-cloud-amd64; proxmox-default-kernel pulls in grub-pc,
  # whose postinst stops to ask for the install device. Answer it with the root disk.
  root_src=$(findmnt -no SOURCE /)
  root_disk=$(lsblk -no PKNAME "$root_src" 2>/dev/null | head -n1 || true)
  if [ -n "$root_disk" ]; then
    echo "grub-pc grub-pc/install_devices multiselect /dev/$root_disk" | debconf-set-selections
    log "grub-pc install device: /dev/$root_disk"
  fi
  # A previous run may have left grub-pc half-configured.
  dpkg --configure -a
  apt-get update
  apt-get "${APT_OPTS[@]}" -y -q full-upgrade
  apt-get "${APT_OPTS[@]}" -y -q install proxmox-default-kernel
  log "Proxmox kernel installed"

  for u in docker.service docker.socket containerd.service; do
    if systemctl list-unit-files "$u" 2>/dev/null | grep -q "^$u"; then
      systemctl disable --now "$u" || log "could not stop $u"
      log "stopped and disabled $u"
    fi
  done

  log "stage 1 done: reboot now, then run this script again"
  exit 0
fi

# ===== Stage 2: Proxmox VE itself =======================================================================
log "stage 2 (running $(uname -r))"

echo "postfix postfix/main_mailer_type select Local only" | debconf-set-selections
echo "postfix postfix/mailname string $host" | debconf-set-selections
apt-get update
apt-get "${APT_OPTS[@]}" -y -q install proxmox-ve postfix open-iscsi chrony ifupdown2
log "proxmox-ve and friends installed"

# ifupdown2 is in, so the network can change hands at the next boot.
if [ -d /etc/netplan ]; then
  install -d /etc/netplan/disabled
  for f in /etc/netplan/*.yaml; do
    [ -e "$f" ] || continue
    mv "$f" /etc/netplan/disabled/
    log "moved $f to /etc/netplan/disabled/"
  done
fi
systemctl disable systemd-networkd.service systemd-networkd.socket systemd-networkd-wait-online.service 2>/dev/null || true
systemctl enable networking.service
log "networking.service owns the network from the next boot"

# Enterprise repositories need a subscription.
for f in /etc/apt/sources.list.d/*.sources; do
  [ -e "$f" ] || continue
  grep -Eq 'enterprise\.proxmox\.com' "$f" || continue
  if grep -q '^Enabled:' "$f"; then
    sed -i 's/^Enabled:.*/Enabled: false/' "$f"
  else
    printf 'Enabled: false\n' >>"$f"
  fi
  log "disabled $f"
done
for f in /etc/apt/sources.list.d/*.list; do
  [ -e "$f" ] || continue
  grep -Eq '^[[:space:]]*deb .*enterprise\.proxmox\.com' "$f" || continue
  sed -i -E 's/^([[:space:]]*deb .*enterprise\.proxmox\.com)/# \1/' "$f"
  log "disabled $f"
done

# Debian kernels and os-prober are not wanted next to the Proxmox kernel (the running one is a -pve kernel).
old=$(dpkg-query -W -f='${db:Status-Abbrev} ${Package}\n' 'linux-image-amd64' 'linux-image-cloud-amd64' 'linux-image-6.12*' 2>/dev/null |
  awk '$1 == "ii" && $2 !~ /-pve/ {print $2}' || true)
if [ -n "$old" ]; then
  log "removing: $(echo $old)"
  # shellcheck disable=SC2086
  apt-get -y -q purge $old
fi
if dpkg -s os-prober >/dev/null 2>&1; then
  apt-get -y -q purge os-prober
fi
update-grub

log "stage 2 done (ufw stays installed and active)."
log "next: reboot once more, then run nat-bridge.sh, then install.sh (see README: Hosts behind NAT)"
