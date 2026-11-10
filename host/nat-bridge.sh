#!/usr/bin/env bash
# Host-local NAT bridge for job VMs on a Proxmox host without a job VLAN. Run as root, re-runnable.
# Creates a bridge (vmbr1, 10.66.0.1/24), DHCP+DNS for it (dnsmasq), forwarding and masquerade (nftables).
# Job VMs reach the internet only: no private ranges, no IPv6, no VM-to-VM traffic.
#   BRIDGE=vmbr1 NET=10.66.0 bash nat-bridge.sh      (NET: the first three octets of a free /24)
set -euo pipefail

BRIDGE=${BRIDGE:-vmbr1}
NET=${NET:-10.66.0}
[[ $NET =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "nat-bridge: NET=$NET: three octets, like 10.66.0" >&2; exit 1; }
SUBNET=$NET.0/24
GW=$NET.1
DHCP_FROM=$NET.10
DHCP_TO=$NET.250
UPLINK=$(ip -4 route show default | awk '{for (i=1;i<NF;i++) if ($i=="dev") {print $(i+1); exit}}')

log() { echo "nat-bridge: $*"; }
die() { echo "nat-bridge: $*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root"
[ -n "$UPLINK" ] || die "no default route, cannot tell the uplink"
for c in nft ifup; do command -v "$c" >/dev/null || die "$c missing (run pve-on-debian.sh first)"; done
log "uplink $UPLINK, bridge $BRIDGE $GW/24"

# --- bridge --------------------------------------------------------------------------------------
cat >/etc/network/interfaces.d/mayfly-nat <<IFACE
# Written by nat-bridge.sh: host-local NAT bridge for mayfly job VMs
auto $BRIDGE
iface $BRIDGE inet static
    address $GW/24
    bridge-ports none
    bridge-stp off
    bridge-fd 0
IFACE
if command -v ifreload >/dev/null; then
  ifreload -a
else
  ip link show "$BRIDGE" >/dev/null 2>&1 || ifup "$BRIDGE"
fi
log "bridge up"

# --- forwarding ----------------------------------------------------------------------------------
echo 'net.ipv4.ip_forward=1' >/etc/sysctl.d/90-mayfly.conf
sysctl -q -p /etc/sysctl.d/90-mayfly.conf
log "ip_forward on"

# --- DHCP and DNS for the bridge -----------------------------------------------------------------
# bind-interfaces + except-interface=lo keep dnsmasq off 127.0.0.53 (systemd-resolved).
# Upstream: the host's real resolvers, named explicitly with no-resolv. Without them Debian's dnsmasq
# reads /run/dnsmasq/resolv.conf, which only resolvconf writes, and answers every query REFUSED.
upstream=""
for f in /run/systemd/resolve/resolv.conf /etc/resolv.conf; do
  [ -r "$f" ] || continue
  upstream=$(awk '$1=="nameserver" && $2 ~ /^[0-9.]+$/ && $2 !~ /^127\./ {print $2}' "$f" | tr '\n' ' ')
  [ -n "$upstream" ] && break
done
[ -n "$upstream" ] || die "no upstream DNS found in /run/systemd/resolve/resolv.conf or /etc/resolv.conf"
log "upstream DNS: $upstream"
install -d /etc/dnsmasq.d
cat >/etc/dnsmasq.d/mayfly.conf <<DNSMASQ
# Written by nat-bridge.sh
interface=$BRIDGE
except-interface=lo
bind-interfaces
dhcp-range=$DHCP_FROM,$DHCP_TO,255.255.255.0,12h
dhcp-option=option:router,$GW
dhcp-option=option:dns-server,$GW
no-hosts
no-resolv
$(for s in $upstream; do echo "server=$s"; done)
DNSMASQ
# Installed only now, with its config in place: a stock dnsmasq would try to bind :53 on every address.
if ! dpkg -s dnsmasq >/dev/null 2>&1; then
  DEBIAN_FRONTEND=noninteractive apt-get -y -q install dnsmasq
fi
systemctl enable dnsmasq >/dev/null
systemctl restart dnsmasq
log "dnsmasq restarted"

# --- nftables ------------------------------------------------------------------------------------
install -d /etc/mayfly
cat >/etc/mayfly/nat.nft <<NFT
# Written by nat-bridge.sh. Loaded by mayfly-nat.service; "destroy table" first makes reloads clean.
destroy table inet mayfly
destroy table ip mayfly
destroy table bridge mayfly

table inet mayfly {
  chain forward {
    type filter hook forward priority -5; policy accept;
    iifname "$BRIDGE" ip daddr { 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 100.64.0.0/10, 169.254.0.0/16 } drop
    iifname "$BRIDGE" meta nfproto ipv6 drop
    iifname "$BRIDGE" oifname "$UPLINK" accept
    iifname "$BRIDGE" drop
    oifname "$BRIDGE" ct state established,related accept
    oifname "$BRIDGE" drop
  }
  chain input {
    type filter hook input priority -5; policy accept;
    iifname "$BRIDGE" udp dport { 53, 67 } accept
    iifname "$BRIDGE" tcp dport 53 accept
    iifname "$BRIDGE" drop
  }
}

table ip mayfly {
  chain postrouting {
    type nat hook postrouting priority srcnat; policy accept;
    ip saddr $SUBNET oifname "$UPLINK" masquerade
  }
}

# Job VMs may sit behind PVE fwbr bridges whose fwpr port is on $BRIDGE, so match the bridge
# the frame came in on (ibrname) rather than a port.
table bridge mayfly {
  chain forward {
    type filter hook forward priority -5; policy accept;
    meta ibrname "$BRIDGE" drop comment "no VM-to-VM traffic"
  }
}
NFT
cat >/etc/systemd/system/mayfly-nat.service <<UNIT
[Unit]
Description=mayfly NAT and isolation rules for $BRIDGE
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/sbin/nft -f /etc/mayfly/nat.nft
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable mayfly-nat.service >/dev/null
systemctl restart mayfly-nat.service
log "nftables rules loaded"

# --- ufw: its iptables-nft chains are separate base chains and must accept too ---------------------
if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q '^Status: active'; then
  ufw allow in on "$BRIDGE" to any port 67 proto udp >/dev/null
  ufw allow in on "$BRIDGE" to any port 53 proto udp >/dev/null
  ufw allow in on "$BRIDGE" to any port 53 proto tcp >/dev/null
  ufw route allow in on "$BRIDGE" out on "$UPLINK" >/dev/null
  log "ufw: allowed DHCP/DNS in on $BRIDGE and routing $BRIDGE -> $UPLINK"
else
  log "ufw not active, nothing to add"
fi

# --- check ---------------------------------------------------------------------------------------
echo
echo "bridge:     $(ip -4 -o addr show dev "$BRIDGE" | awk '{print $4}')"
echo "dnsmasq:    $(systemctl is-active dnsmasq)"
if command -v dig >/dev/null; then
  echo "dns:        github.com via $GW -> $(dig +short +time=3 +tries=2 @"$GW" github.com A | sed -n 1p)"
else
  echo "dns:        (install dnsutils to check: dig @$GW github.com)"
fi
echo "ip_forward: $(sysctl -n net.ipv4.ip_forward)"
nft list table inet mayfly | sed -n 1,12p
