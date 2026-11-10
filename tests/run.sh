#!/usr/bin/env bash
# Variables below are read by the sourced functions and by the eval in ok().
# shellcheck disable=SC2034
# Unit tests for the parts of mayfly that need no Proxmox: config, slots, job env, snippets, firewall,
# the ownership check and the runner registration body. qm/pvesm are stubs. Run: bash tests/run.sh
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
mkdir -p "$t/bin" "$t/conf/env" "$t/snippets" "$t/fw" "$t/qm" "$t/pct" "$t/pve/qemu-server" "$t/pve/lxc" "$t/share/profiles/default"
cat >"$t/bin/pvesm" <<STUB
#!/bin/sh
echo "$t/snippets/x"
STUB
cat >"$t/bin/qm" <<STUB
#!/bin/sh
[ "\$1" = config ] && [ -e "$t/qm/\$2" ] && exec cat "$t/qm/\$2"
exit 2
STUB
cat >"$t/bin/pct" <<STUB
#!/bin/sh
echo "pct \$*" >>"$t/pct.log"
[ "\$1" = config ] && [ -e "$t/pct/\$2" ] && exec cat "$t/pct/\$2"
exit 0
STUB
# lxc-attach: logs its command (and stdin for "cat >" commands); exits with \$LXC_RC (default 0), prints \$LXC_OUT.
cat >"$t/bin/lxc-attach" <<STUB
#!/bin/sh
shift 3
echo "attach \$*" >>"$t/attach.log"
case "\$*" in *"cat >"*) echo "stdin<<" >>"$t/attach.log"; cat >>"$t/attach.log"; echo ">>" >>"$t/attach.log" ;; esac
case "\$*" in "systemctl is-system-running") echo running; exit 0 ;; esac
[ -z "\${LXC_OUT:-}" ] || echo "\$LXC_OUT"
exit "\${LXC_RC:-0}"
STUB
chmod +x "$t/bin/"*
export PATH="$t/bin:$PATH" MAYFLY_CONF_DIR="$t/conf" MAYFLY_SHARE="$t/share" MAYFLY_PVE_ETC="$t/pve" MAYFLY_PVE_FW_DIR="$t/fw"
guest() { printf '%b' "$3" >"$t/$1/$2"; if [ "$1" = qm ]; then : >"$t/pve/qemu-server/$2.conf"; else : >"$t/pve/lxc/$2.conf"; fi; }

fails=0 n=0
ok() { n=$((n + 1)); if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi; }

# shellcheck source=/dev/null
. "$here/mayfly"
set +e

# ---- load_env
: >"$t/conf/pool.env"
ok "no ORG or REPO is refused" '! (load_env) 2>/dev/null'
echo "ORG=acme" >"$t/conf/pool.env"
load_env
ok "ORG gives org scope" '[ "$SCOPE" = orgs/acme ]'
ok "defaults: untagged vmbr0 with firewall" '[ "$NET0" = "virtio,bridge=vmbr0,firewall=1" ]'
ok "runner prefix from NAME_PREFIX and host" '[ "$RUNNER_PREFIX" = "ci-$NODE" ]'
printf 'REPO=acme/app\nFIREWALL=no\nBRIDGE=vmbr2\nVLAN=42\n' >"$t/conf/pool.env"
load_env
ok "REPO gives repo scope" '[ "$SCOPE" = repos/acme/app ]'
ok "VLAN tag and FIREWALL=no in net0" '[ "$NET0" = "virtio,bridge=vmbr2,tag=42,firewall=0" ]'
echo "REPO=acme" >"$t/conf/pool.env"
ok "REPO without owner is refused" '! (load_env) 2>/dev/null'
printf 'ORG=acme\nNAME_PREFIX=Bad_Prefix\n' >"$t/conf/pool.env"
ok "bad NAME_PREFIX is refused" '! (load_env) 2>/dev/null'

# ---- per-slot settings
printf 'ORG=acme\nBRIDGE=vmbr2\nVLAN=42\nTEMPLATE_VMID=9098\nNEXT_TEMPLATE_VMID=9099\n' >"$t/conf/pool.env"
load_env
name=s1 which=current
apply_slot ""
ok "no overrides keep pool.env" '[ "$NET0" = "virtio,bridge=vmbr2,tag=42,firewall=1" ] && [ "$MAX_JOB_HOURS" = 3 ]'
ok "current slot clones TEMPLATE_VMID" '[ "$(slot_template)" = 9098 ]'
which=next
ok "next slot clones NEXT_TEMPLATE_VMID" '[ "$(slot_template)" = 9099 ]'
apply_slot "kind=vm bridge=vmbr9 vlan= max_job_hours=6 max_idle_hours=1 cpu=x86-64-v3 disk_cache=unsafe template=9097"
ok "bridge= and empty vlan= override" '[ "$NET0" = "virtio,bridge=vmbr9,firewall=1" ]'
ok "max_job_hours= and max_idle_hours=" '[ "$MAX_JOB_HOURS" = 6 ] && [ "$MAX_IDLE_HOURS" = 1 ]'
ok "cpu= and disk_cache=" '[ "$CPU_TYPE" = x86-64-v3 ] && [ "$DISK_CACHE" = unsafe ]'
ok "template= wins over next" '[ "$(slot_template)" = 9097 ]'
ok "max_job_hours=abc is refused" '! (apply_slot "max_job_hours=abc") 2>/dev/null'

# ---- job environment
load_env
ok "env no is empty" '[ -z "$(job_env no)" ] && [ -z "$(job_env -)" ]'
printf 'SCCACHE_BUCKET=new\n' >"$t/conf/env/cache.env"
printf 'TOKEN=t\n# comment\nnot a line\n' >"$t/conf/env/extra.env"
ok "one env name" '[ "$(job_env cache)" = SCCACHE_BUCKET=new ]'
ok "several env names, KEY=value lines only" '[ "$(job_env cache,extra)" = "$(printf "SCCACHE_BUCKET=new\nTOKEN=t")" ]'
ok "missing env file: nothing in the env, a note on stderr" '[ -z "$(job_env missing 2>/dev/null)" ] && job_env missing 2>&1 >/dev/null | grep -q "missing or empty"'
ok "env name with a slash is refused" '! (job_env "../x") 2>/dev/null'

# ---- snippet
write_snippet 9002 ci-s1 JITCONFIG extra
f="$t/snippets/mayfly-9002.yaml"
ok "snippet is 0600" '[ "$(stat -c %a "$f")" = 600 ]'
ok "snippet has the JIT config and job env" 'grep -q "content: JITCONFIG" "$f" && grep -q "^      TOKEN=t$" "$f"'
ok "snippet starts mayfly-job" 'grep -q "mayfly-job.service" "$f"'
write_snippet 9002 ci-s1 JITCONFIG extra polara-ci
ok "polara-ci layout: its jit path and job unit" 'grep -q "path: /etc/polara-ci/jit" "$f" && grep -q "path: /etc/polara-ci/job.env" "$f" && grep -q "polara-ci-job.service" "$f" && ! grep -q /etc/mayfly "$f"'
guest qm 9091 'name: ci-template-20261101\ntags: polara-ci\ntemplate: 1\n'
guest qm 9092 'description: profile polara\nname: ci-template-polara-20261110\ntags: mayfly\ntemplate: 1\n'
ok "template_layout: polara-ci template without a profile" '[ "$(template_layout 9091)" = polara-ci ]'
ok "template_layout: mayfly template" '[ "$(template_layout 9092)" = mayfly ]'

# ---- firewall
printf 'ORG=acme\nGATEWAY=10.66.0.1\nALLOW_OUT="10.1.2.3 10.9.0.0/16"\nCACHE_IP=10.66.0.5\nBLOCK_NETS="10.0.0.0/8"\n' >"$t/conf/pool.env"
load_env
write_firewall 9002
ok "firewall: DNS to the gateway, allows, blocks" 'grep -q "OUT ACCEPT -dest 10.66.0.1 -p udp -dport 53" "$t/fw/9002.fw" && grep -q "OUT ACCEPT -dest 10.1.2.3$" "$t/fw/9002.fw" && grep -q "OUT ACCEPT -dest 10.66.0.5$" "$t/fw/9002.fw" && [ "$(grep -c "OUT DROP" "$t/fw/9002.fw")" = 1 ]'
FIREWALL=no write_firewall 9002
ok "FIREWALL=no removes the rules" '[ ! -e "$t/fw/9002.fw" ]'

# ---- ownership
printf 'ORG=acme\n' >"$t/conf/pool.env"
load_env
guest qm 9002 'name: ci-s1\ntags: mayfly\n'
guest qm 9003 'name: ci-s2\ntags: oldtool\n'
guest qm 9004 'name: web\ntags: mayfly\n'
guest qm 9005 'name: ci-x\ntags: prod\n'
guest qm 100 'name: ci-s1\ntags: mayfly\n'
guest pct 9006 'hostname: ci-c1\ntags: mayfly\nrootfs: local-lvm:vm-9006-disk-0,size=30G\n'
guest pct 9007 'hostname: web\ntags: mayfly\n'
ok "guest kind: vm, ct, none" '[ "$(guest_kind 9002)" = vm ] && [ "$(guest_kind 9006)" = ct ] && [ -z "$(guest_kind 9050)" ]'
ok "ours: container with tag mayfly, hostname ci-" 'is_ours 9006'
ok "not ours: container with another hostname" '! is_ours 9007'
ok "not ours: VMID with no guest" '! is_ours 9050' 
ok "ours: tag mayfly, name ci-" 'is_ours 9002'
ok "not ours: other tag while LEGACY_TAGS is empty" '! is_ours 9003'
LEGACY_TAGS="oldtool"
ok "ours: a LEGACY_TAGS tag, name ci-" 'is_ours 9003'
ok "not ours: wrong name" '! is_ours 9004'
ok "not ours: wrong tag" '! is_ours 9005'
ok "not ours: outside the VMID range" '! is_ours 100'
LEGACY_TAGS=""

# ---- GitHub's view of a slot
RUNNER_PREFIX=ci-blake
GH_RUNNERS=$'ci-blake-s1-100 online false\nci-blake-s2-200 offline false\nci-blake-s3-300 online true\nci-blake-p1-1-400 online false\nci-blake-p1-2-401 offline false'
guest qm 9011 'description: runner ci-blake-s1-100, template 9098\nname: ci-s1\n'
guest qm 9012 'description: runner ci-blake-s2-200, template 9097\nname: ci-s2\n'
guest qm 9013 'description: runner ci-blake-s3-300, template 9098\nname: ci-s3\n'
guest qm 9014 'description: runner ci-blake-s4-500, template 9098\nname: ci-s4\n'
ok "github_state: online, offline, busy, none" '[ "$(github_state s1 9011 vm)" = online ] && [ "$(github_state s2 9012 vm)" = offline ] && [ "$(github_state s3 9013 vm)" = busy ] && [ "$(github_state s4 9014 vm)" = none ]'
ok "github_state: pod counts online runners" '[ "$(github_state p1 9041 pod)" = 1/2 ] && [ "$(github_state p9 9049 pod)" = none ]'
GH_RUNNERS=""

# ---- containers
vcpu=2 ram=4 disk=20
: >"$t/pct.log"
ct_grow 9006 20
ok "ct_grow: no resize when the rootfs is big enough" '! grep -q resize "$t/pct.log"'
ct_grow 9006 50
ok "ct_grow: resize when the slot wants more" 'grep -q "pct resize 9006 rootfs 50G" "$t/pct.log"'
: >"$t/attach.log"
ct_deliver 9006 JITSECRET extra
ok "ct_deliver: JIT config over stdin, not on a command line" 'grep -q "^JITSECRET" "$t/attach.log" && ! grep "^attach" "$t/attach.log" | grep -q JITSECRET'
ok "ct_deliver: job env, then the job service" 'grep -q "^TOKEN=t$" "$t/attach.log" && tail -n1 "$t/attach.log" | grep -q "systemctl start --no-block mayfly-job.service"'
LXC_RC=3 job_seconds 9006 >/dev/null; rc=$?
ok "job_seconds: 3 from the container = no job" '[ "$rc" = 1 ]'
LXC_RC=1 job_seconds 9006 >/dev/null; rc=$?
ok "job_seconds: lxc-attach failing = no answer, not idle" '[ "$rc" = 2 ]'
LXC_RC=0 LXC_OUT=" 42" job_seconds 9006 >/dev/null; rc=$?
ok "job_seconds: running job" '[ "$rc" = 0 ] && [ "$(LXC_OUT=" 42" job_seconds 9006)" = 42 ]'
printf 'ORG=acme\nTEMPLATE_VMID=9098\nLXC_TEMPLATE_VMID=9096\nNEXT_LXC_TEMPLATE_VMID=9095\n' >"$t/conf/pool.env"
load_env
kind=lxc which=current SLOT_TEMPLATE=""
ok "lxc slot clones LXC_TEMPLATE_VMID" '[ "$(slot_template)" = 9096 ]'
which=next
ok "lxc canary slot clones NEXT_LXC_TEMPLATE_VMID" '[ "$(slot_template)" = 9095 ]'
kind=vm which=current
ok "vm slot still clones TEMPLATE_VMID" '[ "$(slot_template)" = 9098 ]'
ok "CT_NET0 for containers" '[ "$CT_NET0" = "name=eth0,bridge=vmbr0,ip=dhcp,firewall=1" ]'
guest pct 9097 'hostname: ci-template-default-1\ntags: mayfly\ntemplate: 1\n'
(cmd_template promote 9097 >/dev/null)
ok "promote a container template sets LXC_TEMPLATE_VMID only" 'grep -q "^LXC_TEMPLATE_VMID=9097$" "$t/conf/pool.env" && grep -q "^TEMPLATE_VMID=9098$" "$t/conf/pool.env"'

# ---- profiles
mkdir -p "$t/conf/profiles/mine"
ok "profile from /etc/mayfly/profiles first" '[ "$(find_profile mine)" = "$t/conf/profiles/mine" ]'
ok "shipped profile" '[ "$(find_profile default)" = "$t/share/profiles/default" ]'
ok "unknown or odd profile names fail" '! find_profile nope && ! find_profile "../x"'

# ---- output and commands
ok "no colour when not a terminal" '[ "$TTY" = 0 ] && [ -z "$C_GRN" ]'
ok "warn: prefixed off a terminal" '[ "$(warn x)" = "warning: x" ]'
ok "item: label column, counts warnings" 'CHECK_WARN=0; out=$(item warn storage "70% used"; echo "$CHECK_WARN"); [ "$out" = "$(printf "warn storage   70%% used\n1")" ]'
ok "paint pads plain text" '[ "$(paint busy 6 busy=BLU)|" = "busy  |" ]'
BUILD_LOCK="$t/build.lock"
exec 8>>"$BUILD_LOCK"; flock -n 8
(cmd_template build) >"$t/lock.out" 2>&1; rc=$?
ok "template build refuses while another build holds the lock" '[ "$rc" = 1 ] && grep -q "another template build is running" "$t/lock.out"'
exec 8>&-
# ---- provision.sh: template.env values reach the profile's provision.sh (a child bash)
mkdir -p "$t/prov/profile"
echo 'CORE_X=core' >"$t/prov/template.env"
echo 'PROFILE_X=profile' >"$t/prov/profile/template.env"
tar -C "$t/prov/profile" -czf "$t/prov/profile.tgz" .
sed -n '/^set -a$/,/^set +a$/p' "$here/template/provision.sh" | sed "s|/root/|$t/prov/|g" >"$t/prov/env.sh"
ok "provision.sh exports core and profile template.env" '[ "$(bash -c ". \"$t/prov/env.sh\"; bash -c \"echo \\\$CORE_X-\\\$PROFILE_X\"")" = core-profile ]'
ok "help lists the commands" 'cmd_help | grep -q "template build" && cmd_help | grep -q "enable"'
ok "version" '[[ "$(bash "$here/mayfly" version)" =~ ^mayfly\ [0-9]+\.[0-9]+\.[0-9]+$ ]]'
ok "unknown command fails" '! bash "$here/mayfly" nope >/dev/null 2>&1'
cat >"$t/bin/systemctl" <<STUB
#!/bin/sh
echo "systemctl \$*" >>"$t/systemctl.log"
STUB
chmod +x "$t/bin/systemctl"
printf 'build-1 9002 linux 8 16 60 no current\n# off 9009 x 1 1 1 no current\nsmall-1 9011 small 2 4 30 no current\n' >"$t/conf/slots.conf"
(cmd_slots enable >/dev/null)
ok "enable: every slot, then the gc timer" '[ "$(cat "$t/systemctl.log")" = "$(printf "systemctl enable -q --now mayfly-slot@build-1\nsystemctl enable -q --now mayfly-slot@small-1\nsystemctl enable -q --now mayfly-gc.timer")" ]'
ok "enable: an unknown slot is refused" '! (cmd_slots enable nope) 2>/dev/null'
: >"$t/systemctl.log"
(cmd_slots disable small-1 >/dev/null)
ok "disable: only the named slot" 'grep -q "disable -q mayfly-slot@small-1" "$t/systemctl.log" && ! grep -q build-1 "$t/systemctl.log"'

# ---- runner registration body
gh_api() { shift 3; [ "$1" = -d ] && echo "$2"; }
RUNNER_LABELS_BASE="self-hosted,Linux,X64"
body=$(jit_config tok ci-x-s1-1 "linux,small")
ok "labels: base plus slot, unique" '[ "$(jq -c .labels <<<"$body")" = "[\"Linux\",\"X64\",\"linux\",\"self-hosted\",\"small\"]" ]'
RUNNER_LABELS_BASE=""
body=$(jit_config tok ci-x-s1-1 "linux")
ok "empty base labels" '[ "$(jq -c .labels <<<"$body")" = "[\"linux\"]" ]'

echo
echo "$((n - fails))/$n passed"
[ "$fails" = 0 ]
