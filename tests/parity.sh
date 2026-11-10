#!/usr/bin/env bash
# The Rust commands print what mayfly.sh prints: runs version, help, status and check of both against
# one fixture (stubs for qm, pct, pvesm, pvesh, systemctl, ip, pve-firewall and a local GitHub) and
# compares stdout, stderr and the exit code. gc, enable and disable run against stubs that record every
# call: the recorded calls, the requests the fake GitHub saw and the files left behind are compared too.
# install: the bash install.sh of 0.4.1 (tests/fixtures/install-0.4.1.sh) and `mayfly install` run against
# the same stub root; stdout, stderr, the exit code, the stub calls and the tree they leave (paths, modes,
# link targets, checksums) are compared.
# template list, canary, promote and build are compared like gc (a VM build to the end, with a stub qm that
# answers the guest agent and a stand-in qemu process; MAYFLY_TEST_NAP=0 and MAYFLY_QEMU_RUN_DIR keep it
# fast and local).
# slot: one slot of each kind (vm, lxc, pod) runs on bash and on Rust (engine=rust in slots.conf, which bash
# ignores). A vm or lxc slot goes through one job cycle (the guest powers itself off) and a second guest that a
# TERM drains; a pod through one VM that is recycled when its template changes and a second one that a TERM
# drains. Stubs record every qm, pct, lxc-attach, pvesh and pvesm call, the cloud-init snippet and firewall file
# at `qm start` / `pct start` and what was handed to a container or pod, a fake GitHub records its requests (the
# JIT body included), hooks record their arguments. The calls, files, requests, output and exit code are compared.
# Usage: bash tests/parity.sh [path to the mayfly binary]   (default target/release/mayfly)
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-$here/target/release/mayfly}
[ -x "$bin" ] || { echo "no binary at $bin (cargo build --release)" >&2; exit 2; }
t=$(mktemp -d)
srv="" srv2="" srv3=""
# The stand-in qemu processes of the template build (pid files under $t/g/qemu).
kill_qemu() {
  local f
  for f in "$t"/g/qemu/*.pid "$t"/s/qemu/*.pid; do
    [ -e "$f" ] || continue
    kill "$(cat "$f")" 2>/dev/null || true
  done
}
cleanup() {
  kill_qemu
  [ -z "$srv" ] || kill "$srv" 2>/dev/null
  [ -z "$srv2" ] || kill "$srv2" 2>/dev/null
  [ -z "$srv3" ] || kill "$srv3" 2>/dev/null
  rm -rf "$t"
}
trap cleanup EXIT
mkdir -p "$t/bin" "$t/conf/env" "$t/conf/profiles" "$t/share/profiles/default" "$t/pve/qemu-server" "$t/pve/lxc" "$t/fw" "$t/snippets"

# ---- stubs
cat >"$t/bin/pvesm" <<STUB
#!/bin/sh
case "\$1" in
  path) echo "$t/snippets/x" ;;
  status) exit 0 ;;
  *) exit 1 ;;
esac
STUB
cat >"$t/bin/qm" <<'STUB'
#!/bin/sh
case "$1 $2" in
  "status 9002") echo "status: running" ;;
  "status 9041") echo "status: running" ;;
  "config 9002") printf 'name: ci-a\ndescription: runner mf-a-1-111, template 9098\n' ;;
  "config 9098") printf 'name: ci-template\ntemplate: 1\n' ;;
  "guest exec") echo '{"exitcode":0,"out-data":"1"}' ;;
  *) exit 2 ;;
esac
STUB
cat >"$t/bin/pct" <<'STUB'
#!/bin/sh
case "$1 $2" in
  "status 9051") echo "status: running" ;;
  "config 9051") printf 'hostname: ci-c-1\ndescription: runner mf-c-1-222, template 9096\n' ;;
  *) exit 2 ;;
esac
STUB
cat >"$t/bin/lxc-attach" <<'STUB'
#!/bin/sh
exit 1
STUB
cat >"$t/bin/pvesh" <<'STUB'
#!/bin/sh
case "$2" in
  /nodes/*/storage/*/status) echo '{"total":1000,"used":425}' ;;
  /cluster/backup) echo '[{"id":"backup-1","all":1},{"id":"backup-2","all":0}]' ;;
  *) exit 1 ;;
esac
STUB
cat >"$t/bin/systemctl" <<'STUB'
#!/bin/sh
case "$2" in
  mayfly-slot@a-1) echo active ;;
  mayfly-slot@b-1) echo inactive; exit 3 ;;
  mayfly-slot@c-1) echo activating ;;
  *) echo failed; exit 3 ;;
esac
STUB
cat >"$t/bin/ip" <<'STUB'
#!/bin/sh
[ "$3" = vmbr0 ]
STUB
cat >"$t/bin/pve-firewall" <<'STUB'
#!/bin/sh
echo "Status: enabled/running"
STUB
chmod +x "$t/bin/"*

# ---- a GitHub that answers the calls of `check`
cat >"$t/gh.py" <<'PY'
import http.server, json


class Handler(http.server.BaseHTTPRequestHandler):
    def send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/app":
            self.send(200, {"slug": "my-app"})
        elif self.path == "/orgs/acme/installation":
            self.send(200, {"id": 42})
        elif self.path == "/orgs/acme/actions/runners?per_page=100&page=1":
            self.send(200, {"runners": [
                {"id": 1, "name": "mf-a-1-111", "status": "online", "busy": True},
                {"id": 2, "name": "mf-b-1-1-5", "status": "online", "busy": False},
                {"id": 3, "name": "mf-b-1-2-5", "status": "offline", "busy": False},
                {"id": 4, "name": "mf-c-1-222", "status": "offline", "busy": False},
                {"id": 5, "name": "other-9", "status": "online", "busy": False},
            ]})
        elif self.path.startswith("/orgs/acme/actions/runners?per_page=100"):
            self.send(200, {"runners": []})
        elif self.path.startswith("/orgs/acme/actions/runners"):
            self.send(200, {"total_count": 3, "runners": []})
        else:
            self.send(404, {})

    def do_POST(self):
        if self.path == "/app/installations/42/access_tokens":
            self.send(201, {"token": "ghs_fake"})
        else:
            self.send(404, {})

    def log_message(self, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
PY
python3 "$t/gh.py" >"$t/port" &
srv=$!
for _ in $(seq 50); do [ -s "$t/port" ] && break; sleep 0.1; done
[ -s "$t/port" ] || { echo "the fake GitHub did not start" >&2; exit 2; }
openssl genrsa -traditional -out "$t/app.pem" 2048 2>/dev/null || openssl genrsa -out "$t/app.pem" 2048 2>/dev/null

# ---- fixture
printf 'name: ci-a\n' >"$t/pve/qemu-server/9002.conf"
: >"$t/pve/qemu-server/9041.conf"
: >"$t/pve/qemu-server/9098.conf"
: >"$t/pve/lxc/9051.conf"
cat >"$t/conf/pool.env" <<ENV
APP_ID=12345
ORG=acme
KEY_PATH=$t/app.pem
API_URL=http://127.0.0.1:$(cat "$t/port")
TEMPLATE_VMID=9098
NEXT_TEMPLATE_VMID=9099
LXC_TEMPLATE_VMID=9096
RUNNER_PREFIX=mf
ENV
cat >"$t/conf/slots.conf" <<'SLOTS'
a-1 9002 linux 8 16 60 cache current
b-1 9041 small 2 4 30 no current kind=pod containers=4
# off-1 9060 small 1 1 1 no current
c-1 9051 small 2 4 20 no current kind=lxc
long-slot-name-1 9099 small-and-fast 1 1 1 no next
SLOTS
export PATH="$t/bin:$PATH" MAYFLY_STATE_DIR="$t/state" MAYFLY_CONF_DIR="$t/conf" MAYFLY_SHARE="$t/share" MAYFLY_PVE_ETC="$t/pve" MAYFLY_PVE_FW_DIR="$t/fw"
unset NO_COLOR
cp "$here/mayfly.sh" "$t/share/mayfly.sh"

fails=0 n=0
same() {
  local name=$1 rb rr
  shift
  bash "$here/mayfly.sh" "$@" >"$t/bash.out" 2>"$t/bash.err" </dev/null
  rb=$?
  "$bin" "$@" >"$t/rust.out" 2>"$t/rust.err" </dev/null
  rr=$?
  # Known difference: only the Rust status prints the `update <version>, last ...` line (the update
  # state is Rust-only). Take it out of the Rust output before comparing.
  sed -i '/^update /d' "$t/rust.out"
  n=$((n + 1))
  if [ "$rb" = "$rr" ] && cmp -s "$t/bash.out" "$t/rust.out" && cmp -s "$t/bash.err" "$t/rust.err"; then
    echo "ok   $name (exit $rb, $(wc -l <"$t/bash.out") lines)"
  else
    echo "FAIL $name: bash exit $rb, rust exit $rr"
    diff -u --label bash --label rust <(cat "$t/bash.out"; echo "--- stderr"; cat "$t/bash.err") <(cat "$t/rust.out"; echo "--- stderr"; cat "$t/rust.err") | head -40
    fails=$((fails + 1))
  fi
}

same "version" version
same "--version" --version
same "help" help
same "-h" -h
same "status" status
same "check" check
cp "$t/conf/pool.env" "$t/pool.env.good"
sed 's#^API_URL=.*#API_URL=http://127.0.0.1:1#' "$t/pool.env.good" >"$t/conf/pool.env"
same "status with GitHub unreachable" status
cp "$t/pool.env.good" "$t/conf/pool.env"
: >"$t/conf/slots.conf"
same "status without slots" status
same "check without slots" check
printf 'APP_ID=1\nORG=acme\nNAME_PREFIX=Bad_Prefix\n' >"$t/conf/pool.env"
same "bad NAME_PREFIX" status
printf 'APP_ID=\nORG=acme\n' >"$t/conf/pool.env"
same "check without APP_ID" check
printf 'APP_ID=1\nREPO=acme\n' >"$t/conf/pool.env"
same "REPO without owner" check
: >"$t/conf/pool.env"
same "no ORG or REPO" status
rm "$t/conf/pool.env"
same "no pool.env" status

# ---- gc, enable and disable: stubs that record their calls, and a GitHub that records its requests
g=$t/g
mkdir -p "$g/bin" "$g/conf"
cat >"$g/bin/qm" <<'STUB'
#!/bin/sh
echo "qm $*" >>"$CALLS"
case "$1" in
  config) cat "$MAYFLY_PVE_ETC/qemu-server/$2.conf" 2>/dev/null || exit 2 ;;
  destroy) if [ "$2" = "${FAIL_DESTROY:-}" ]; then echo "destroy failed" >&2; exit 5; fi ;;
  create)
    : >"$MAYFLY_PVE_ETC/qemu-server/$2.conf"
    cp "$GSNIP/mayfly-$2.yaml" "$CAP/" 2>/dev/null ;;
  start)
    # A stand-in for the qemu process: vm_running looks for a live pid whose command line has "-id <vmid> ".
    if [ -z "${NO_START_PROC:-}" ]; then
      python3 -c 'import time; time.sleep(300)' -id "$2" x </dev/null >/dev/null 2>&1 &
      echo $! >"$MAYFLY_QEMU_RUN_DIR/$2.pid"
      # Until python has exec'd, the command line is still this script's.
      for _ in $(seq 100); do
        tr '\0' ' ' <"/proc/$!/cmdline" 2>/dev/null | grep -q -- "-id $2 " && break
        sleep 0.05
      done
    fi ;;
  shutdown)
    kill "$(cat "$MAYFLY_QEMU_RUN_DIR/$2.pid")" 2>/dev/null
    rm -f "$MAYFLY_QEMU_RUN_DIR/$2.pid" ;;
  template) echo "template: 1" >>"$MAYFLY_PVE_ETC/qemu-server/$2.conf" ;;
  guest)
    case "$*" in
      *"-- test -e "*".done") if [ -n "${FAIL_PROVISION:-}" ]; then echo '{"exitcode":1}'; else echo '{"exitcode":0}'; fi ;;
      *"-- test -e "*".failed") if [ -n "${FAIL_PROVISION:-}" ]; then echo '{"exitcode":0}'; else echo '{"exitcode":1}'; fi ;;
      *"-- tail "*) printf '{"exitcode":0,"out-data":"boom\\n"}\n' ;;
      *) echo '{"exitcode":0}' ;;
    esac ;;
esac
exit 0
STUB
cat >"$g/bin/pct" <<'STUB'
#!/bin/sh
echo "pct $*" >>"$CALLS"
case "$1" in
  config) cat "$MAYFLY_PVE_ETC/lxc/$2.conf" 2>/dev/null || exit 2 ;;
esac
exit 0
STUB
cat >"$g/bin/lxc-info" <<'STUB'
#!/bin/sh
echo "lxc-info $*" >>"$CALLS"
if [ "$2" = 9051 ]; then echo RUNNING; else echo STOPPED; fi
STUB
cat >"$g/bin/pvesh" <<'STUB'
#!/bin/sh
echo "pvesh $*" >>"$CALLS"
case "$2" in
  /storage/*) echo '{"type":"lvmthin"}' ;;
  *) exec "$TBIN/pvesh" "$@" ;;
esac
STUB
cat >"$g/bin/curl" <<'STUB'
#!/bin/sh
# Only the image host of the template build is faked; every other URL (bash's GitHub calls) goes to the real curl.
out="" prev="" url=""
for a in "$@"; do
  [ "$prev" = -o ] && out=$a
  prev=$a
  url=$a
done
case "$url" in
  http://img.test/*) ;;
  *) PATH=${PATH#*:}; exec curl "$@" ;;
esac
echo "curl $*" >>"$CALLS"
case "$url" in
  */SHA256SUMS) cat "$IMGSUMS" ;;
  *) [ -z "$out" ] || printf IMAGE >"$out" ;;
esac
exit 0
STUB
cat >"$g/bin/systemd-run" <<'STUB'
#!/bin/sh
echo "systemd-run $*" >>"$CALLS"
STUB
cat >"$g/bin/pvesm" <<'STUB'
#!/bin/sh
echo "pvesm $*" >>"$CALLS"
case "$1" in
  path) echo "$GSNIP/x" ;;
  *) exit 0 ;;
esac
STUB
cat >"$g/bin/systemctl" <<'STUB'
#!/bin/sh
echo "systemctl $*" >>"$CALLS"
case "$1" in
  is-active) case "$3" in mayfly-slot@a-1|mayfly-slot@c-1) exit 0 ;; *) exit 3 ;; esac ;;
  list-units) cat "$ACTIVE" 2>/dev/null; exit 0 ;;
esac
exit 0
STUB
chmod +x "$g/bin/"*

cat >"$t/gc.py" <<'PY'
import http.server, json, sys, time

LOG = sys.argv[1]


def runners(page):
    now = int(time.time())
    if page == 1:
        return [
            {"id": 1, "name": "mf-a-1-111", "status": "offline", "busy": False},
            {"id": 2, "name": "mf-old-1-5", "status": "offline", "busy": False},
            {"id": 3, "name": "mf-busy-1-6", "status": "online", "busy": False},
            {"id": 4, "name": "mf-offbusy-1-7", "status": "offline", "busy": True},
            {"id": 5, "name": "other-9", "status": "offline", "busy": False},
            {"id": 6, "name": "mf-fresh-1-%d" % (now - 60), "status": "offline", "busy": False},
            {"id": 7, "name": "mf-weird-xyz", "status": "offline", "busy": False},
            {"id": 8, "name": "mf-c-1-222", "status": "offline", "busy": False},
        ]
    if page == 2:
        return [{"id": 9, "name": "mf-p2-1-8", "status": "offline", "busy": False}]
    return []


class Handler(http.server.BaseHTTPRequestHandler):
    def note(self):
        with open(LOG, "a") as f:
            f.write("%s %s\n" % (self.command, self.path))

    def send(self, code, obj=None):
        body = b"" if obj is None else json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.note()
        prefix = "/orgs/acme/actions/runners?per_page=100&page="
        if self.path == "/orgs/acme/installation":
            self.send(200, {"id": 42})
        elif self.path.startswith(prefix):
            self.send(200, {"total_count": 9, "runners": runners(int(self.path[len(prefix):]))})
        else:
            self.send(404, {})

    def do_POST(self):
        self.note()
        if self.path == "/app/installations/42/access_tokens":
            self.send(201, {"token": "ghs_fake"})
        else:
            self.send(404, {})

    def do_DELETE(self):
        self.note()
        self.send(204)

    def log_message(self, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
PY
python3 "$t/gc.py" "$g/gh.log" >"$t/gport" &
srv2=$!
for _ in $(seq 50); do [ -s "$t/gport" ] && break; sleep 0.1; done
[ -s "$t/gport" ] || { echo "the fake GitHub for gc did not start" >&2; exit 2; }

cat >"$g/pool.env.orig" <<ENV
APP_ID=12345
ORG=acme
KEY_PATH=$t/app.pem
API_URL=http://127.0.0.1:$(cat "$t/gport")
RUNNER_PREFIX=mf
LEGACY_TAGS=oldci
ENV
cat >"$g/slots.conf" <<'SLOTS'
a-1 9002 linux 8 16 60 no current
b-1 9004 linux 8 16 60 no current
# off-1 9060 small 1 1 1 no current
c-1 9051 small 2 4 20 no current kind=lxc
SLOTS

# A guest's config file: what the qm and pct stubs print.
guest() { printf '%b' "$3" >"$g/pve/$1/$2.conf"; }
# A file in a directory, with an age.
aged() { : >"$1"; touch -d "$2" "$1"; }

# The same host before every run, so that the second run does not see what the first one removed.
state_reset() {
  rm -rf "$g/pve" "$g/fw" "$g/snip"
  mkdir -p "$g/pve/qemu-server" "$g/pve/lxc" "$g/fw" "$g/snip"
  cp "$g/slots.conf" "$g/conf/slots.conf"
  cp "$g/pool.env.orig" "$g/conf/pool.env"
  kill_qemu
  rm -rf "$g/cap" "$g/qemu" "$g/build.lock" "$t/state/images"
  mkdir -p "$g/cap" "$g/qemu"
  printf '%s  disk.img\n' "$(printf IMAGE | sha256sum | cut -d' ' -f1)" >"$g/sums"
  guest qemu-server 9002 'name: ci-a\ntags: mayfly\ndescription: runner mf-a-1-111, template 9098\n'
  guest qemu-server 9003 'name: ci-orphan\ntags: mayfly\n'
  guest qemu-server 9004 'name: ci-b\ntags: mayfly\ndescription: runner mf-b-1-5\n'
  guest qemu-server 9005 'name: web\ntags: mayfly\n'
  guest qemu-server 9006 'name: ci-x\ntags: other\n'
  guest qemu-server 9007 'name: ci-legacy\ntags: oldci\n'
  guest qemu-server 9008 'name: ci-template\ntags: mayfly\ntemplate: 1\n'
  guest qemu-server 9009 'name: ci-elsewhere\ntags: mayfly\n'
  guest qemu-server 9097 'name: ci-building\ntags: mayfly\n'
  guest qemu-server 9100 'name: ci-outside\ntags: mayfly\n'
  guest lxc 9051 'hostname: ci-c-1\ntags: mayfly\ndescription: runner mf-c-1-222\n'
  guest lxc 9052 'hostname: ci-c-2\ntags: mayfly\n'
  printf '{"ids":{"9009":{"node":"othernode"}}}\n' >"$g/pve/.vmlist"
  : >"$g/fw/9003.fw"
  : >"$g/fw/9002.fw"
  aged "$g/snip/mayfly-9003.yaml" '3 hours ago'
  aged "$g/snip/mayfly-9500.yaml" '40 minutes ago'
  aged "$g/snip/mayfly-9501.yaml" '20 minutes ago'
  aged "$g/snip/mayfly-9051.yaml" '3 hours ago'
  aged "$g/snip/mayfly-9053.yaml" '3 hours ago'
  aged "$g/snip/mayfly-abc.yaml" '3 hours ago'
  aged "$g/snip/other.yaml" '3 hours ago'
  : >"$g/active"
  : >"$g/calls"
  : >"$g/gh.log"
}

# The unit a detached build starts runs this program: bash runs `bash mayfly.sh`, Rust runs itself.
binreal=$(readlink -f "$bin")
norm() {
  local l
  while IFS= read -r l; do
    l=${l//" bash $here/mayfly.sh "/" SELF "}
    l=${l//" $binreal "/" SELF "}
    printf '%s\n' "$l"
  done
}

# snap_run <prefix> <command ...>: resets the host, runs the command and saves its output, its exit code
# and the state it left: the calls the stubs recorded (the parallel `systemctl stop`s sorted), the
# requests the GitHub saw and the files that are left.
snap_run() {
  local p=$1
  shift
  state_reset
  [ -z "${PRE:-}" ] || "$PRE"
  env -u INVOCATION_ID "PATH=$g/bin:$PATH" "CALLS=$g/calls" "ACTIVE=$g/active" "GSNIP=$g/snip" \
    "CAP=$g/cap" "IMGSUMS=$g/sums" "TBIN=$t/bin" "MAYFLY_TEST_NAP=0" "MAYFLY_QEMU_RUN_DIR=$g/qemu" \
    "MAYFLY_BUILD_LOCK=$g/build.lock" \
    "MAYFLY_CONF_DIR=$g/conf" "MAYFLY_PVE_ETC=$g/pve" "MAYFLY_PVE_FW_DIR=$g/fw" \
    "$@" >"$p.out" 2>"$p.err" </dev/null
  echo "$?" >"$p.rc"
  {
    echo "--- calls"
    grep -v '^systemctl stop ' "$g/calls" | norm
    echo "--- parallel stops, sorted"
    grep '^systemctl stop ' "$g/calls" | sort
    echo "--- github"
    cat "$g/gh.log"
    echo "--- files"
    (cd "$g" && ls fw snip)
    echo "--- pool.env"
    cat "$g/conf/pool.env"
    echo "--- firewall and snippets"
    local f
    for f in "$g"/fw/* "$g"/cap/*; do
      [ -s "$f" ] || continue
      echo "== ${f#"$g"/}"
      cat "$f"
    done
  } >"$p.state"
}

same_state() {
  local name=$1 ok=1 f
  shift
  snap_run "$t/sb" bash "$here/mayfly.sh" "$@"
  snap_run "$t/sr" "$bin" "$@"
  n=$((n + 1))
  for f in out err rc state; do cmp -s "$t/sb.$f" "$t/sr.$f" || ok=0; done
  if [ "$ok" = 1 ]; then
    echo "ok   $name (exit $(cat "$t/sb.rc"), $(wc -l <"$t/sb.out") lines, $(wc -l <"$t/sb.state") lines of state)"
  else
    echo "FAIL $name: bash exit $(cat "$t/sb.rc"), rust exit $(cat "$t/sr.rc")"
    for f in out err rc state; do
      diff -u --label "bash $f" --label "rust $f" "$t/sb.$f" "$t/sr.$f" | head -40
    done
    fails=$((fails + 1))
  fi
}

pre_active() { echo "mayfly-slot@b-1.service loaded active running Mayfly slot b-1" >"$g/active"; }
pre_no_slots() { : >"$g/conf/slots.conf"; }

same_state "gc" gc
FAIL_DESTROY=9004 same_state "gc, a destroy fails" gc
same_state "enable" enable
same_state "enable a-1 c-1" enable a-1 c-1
same_state "enable, unknown slot" enable a-1 nope
same_state "disable" disable
same_state "disable a-1 b-1 c-1" disable a-1 b-1 c-1
PRE=pre_active same_state "disable a-1, b-1 still active" disable a-1
PRE=pre_no_slots same_state "enable without slots" enable
PRE=pre_no_slots same_state "disable without slots" disable
PRE=pre_no_slots same_state "enable a slot that is not there" enable a-1

# ---- template list, canary, promote and build
printf 'P\n' >"$t/share/provision.sh"
printf 'J\n' >"$t/share/ci-job.sh"
printf 'S\n' >"$t/share/mayfly-job.service"
printf 'E\n' >"$t/share/template.env"
printf 'echo profile\n' >"$t/share/profiles/default/provision.sh"
pre_tpl() {
  printf 'IMAGE_URL=http://img.test/x/disk.img\nTEMPLATE_VMIDS="9094 9095 9096 9054"\nTEMPLATE_VMID=9094\n# keep me\nNEXT_TEMPLATE_VMID=9096\nLXC_TEMPLATE_VMID=9054\n' >>"$g/conf/pool.env"
  guest qemu-server 9094 'name: ci-template-old\ntags: mayfly\ndescription: profile default\ntemplate: 1\n'
  guest qemu-server 9096 'name: ci-template-poly\ntags: mayfly\ntemplate: 1\n'
  guest lxc 9054 'hostname: ci-ct-template\ntags: mayfly\ntemplate: 1\n'
}
pre_badsums() { pre_tpl; echo "0000000000000000000000000000000000000000000000000000000000000000  disk.img" >"$g/sums"; }
pre_held() {
  pre_tpl
  exec 8>>"$g/build.lock"
  flock -n 8
}

PRE=pre_tpl same_state "template list" template list
PRE=pre_tpl same_state "template canary, a mayfly template" template canary 9094
PRE=pre_tpl same_state "template canary, a polara-ci template" template canary 9096
PRE=pre_tpl same_state "template canary, a container template" template canary 9054
PRE=pre_tpl same_state "template promote, a mayfly template" template promote 9094
PRE=pre_tpl same_state "template promote, a polara-ci template" template promote 9096
PRE=pre_tpl same_state "template promote, a container template" template promote 9054
PRE=pre_tpl same_state "template canary, not a template" template canary 9002
PRE=pre_tpl same_state "template canary, no vmid" template canary
PRE=pre_tpl same_state "template promote, unknown vmid" template promote 9095
PRE=pre_tpl same_state "template, no subcommand" template
PRE=pre_tpl same_state "template build, bad option" template build --bogus
PRE=pre_tpl same_state "template build, unknown profile" template build nope --foreground
PRE=pre_tpl same_state "template build, VM, to the end" template build --foreground
PRE=pre_tpl same_state "template build, VM, named profile" template build default --foreground
PRE=pre_tpl FAIL_PROVISION=1 same_state "template build, VM, provisioning fails" template build --foreground
PRE=pre_tpl NO_START_PROC=1 same_state "template build, VM stops early" template build --foreground
PRE=pre_badsums same_state "template build, bad checksum" template build --foreground
PRE=pre_tpl same_state "template build, detached" template build default
PRE=pre_held same_state "template build, lock held" template build --foreground
exec 8>&-
PRE=pre_held same_state "template build, lock held, detached" template build
exec 8>&-
PRE=pre_no_slots same_state "template list without slots" template list
# ---- install: the bash install.sh of 0.4.1 against `mayfly install`
# One release directory holds this checkout's files, the binary and the old install.sh (made to read
# MAYFLY_CONF_DIR like the binary does; the 0.4.1 script has /etc/mayfly built in). Stubs for id, qm,
# apt-get, pvesh, pvesm and systemctl log every call; `pvesm set` changes the storage's content types.
w=$t/w
mkdir -p "$w/bin" "$w/rel"
stub() { printf '#!/bin/sh\n%s\n' "$2" >"$w/bin/$1"; chmod +x "$w/bin/$1"; }
stub id 'echo 0'
stub qm 'exit 0'
stub apt-get 'echo "apt-get $*" >>"$CALLS"'
stub pvesh 'echo "pvesh $*" >>"$CALLS"; printf "{\"content\":\"%s\"}\n" "$(cat "$STORE")"'
stub pvesm 'echo "pvesm $*" >>"$CALLS"; [ "$1" != set ] || printf %s "$4" >"$STORE"'
stub systemctl 'echo "systemctl $*" >>"$CALLS"'
cp -r "$here/template" "$here/profiles" "$here/mayfly.sh" "$here"/mayfly-*.service "$here"/mayfly-*.timer \
  "$here"/pool.env.example "$here"/slots.conf.example "$w/rel/"
cp "$bin" "$w/rel/mayfly"
sed 's#^conf=/etc/mayfly$#conf=${MAYFLY_CONF_DIR:-/etc/mayfly}#' "$here/tests/fixtures/install-0.4.1.sh" >"$w/rel/install.sh"
grep -q '^conf=\${MAYFLY_CONF_DIR' "$w/rel/install.sh" || { echo "could not patch the old install.sh" >&2; exit 2; }
root=$w/root
instenv=("PATH=$w/bin:$PATH" "CALLS=$w/calls" "STORE=$w/store" "MAYFLY_SBIN=$root/sbin" "MAYFLY_SHARE=$root/share"
  "MAYFLY_SYSTEMD_DIR=$root/units" "MAYFLY_CONF_DIR=$root/conf")

# The host the install starts from; $1 is a function that changes it, $2... the command.
inst_run() {
  local p=$1 pre=$2
  shift 2
  rm -rf "$root"
  mkdir -p "$root"
  printf %s 'iso,vztmpl' >"$w/store"
  [ -z "$pre" ] || "$pre"
  : >"$w/calls"
  env "${instenv[@]}" "$@" >"$p.out" 2>"$p.err" </dev/null
  echo "$?" >"$p.rc"
  {
    echo "--- calls"
    cat "$w/calls"
    echo "--- storage"
    cat "$w/store"
    echo "--- tree"
    (cd "$root" && find . -printf '%p %m %y %l\n' | sort)
    echo "--- contents"
    (cd "$root" && find . -type f -print0 | sort -z | xargs -0 sha256sum)
  } >"$p.state"
}

same_install() {
  local name=$1 pre=$2 ok=1 f
  shift 2
  inst_run "$t/xb" "$pre" bash "$w/rel/install.sh" "$@"
  inst_run "$t/xr" "$pre" "$w/rel/mayfly" install --from "$w/rel" "$@"
  n=$((n + 1))
  for f in out err rc state; do cmp -s "$t/xb.$f" "$t/xr.$f" || ok=0; done
  if [ "$ok" = 1 ]; then
    echo "ok   $name (exit $(cat "$t/xb.rc"), $(wc -l <"$t/xb.out") lines, $(wc -l <"$t/xb.state") lines of state)"
  else
    echo "FAIL $name: bash exit $(cat "$t/xb.rc"), rust exit $(cat "$t/xr.rc")"
    for f in out err rc state; do
      diff -u --label "bash $f" --label "rust $f" "$t/xb.$f" "$t/xr.$f" | head -40
    done
    fails=$((fails + 1))
  fi
}

# An installed host: a first install by the binary, then your own config and a stale share directory.
pre_installed() {
  env "${instenv[@]}" "$w/rel/mayfly" install --from "$w/rel" >/dev/null 2>&1 </dev/null
  printf 'APP_ID=7\nORG=acme\n' >"$root/conf/pool.env"
  echo 'a-1 9001 x 1 1 1 no current' >"$root/conf/slots.conf"
  mkdir -p "$root/conf/profiles/mine" "$root/share/profiles/gone"
  echo mine >"$root/conf/profiles/mine/provision.sh"
  echo old >"$root/share/profiles/gone/provision.sh"
  echo stale >"$root/share/profiles/default/provision.sh"
  echo stale >"$root/share/mayfly.sh"
}
pre_no_update_units() {
  pre_installed
  rm "$root/units/mayfly-update.timer"
}
pre_pool_env() { # a pool.env that is already there, in a config directory that is 0700 like install makes it
  install -d -m 0700 "$root/conf"
  printf '%s\n' "$POOL_ENV_TEXT" >"$root/conf/pool.env"
}
pre_snippets_there() { printf %s 'iso,snippets,vztmpl' >"$w/store"; }

same_install "install, first" ""
same_install "install --no-timer, first" "" --no-timer
same_install "install, second" pre_installed
same_install "install --no-timer, second" pre_installed --no-timer
same_install "install, second, update units new" pre_no_update_units
POOL_ENV_TEXT='UPDATE="off" # by hand' same_install "install, UPDATE=off" pre_pool_env
POOL_ENV_TEXT=$'APP_ID=3\nSNIPPET_STORAGE=tank\nUPDATE=patch' same_install "install, SNIPPET_STORAGE and APP_ID set" pre_pool_env
same_install "install, snippets already there" pre_snippets_there

# ---- slot: kind=vm, one job cycle and a drain, on bash and on Rust
s=$t/s
mkdir -p "$s/bin"
cat >"$s/bin/pvesm" <<'STUB'
#!/bin/sh
echo "pvesm $*" >>"$CALLS"
case "$1" in
  path) echo "$GSNIP/x" ;;
  *) exit 1 ;;
esac
STUB
cat >"$s/bin/qm" <<'STUB'
#!/bin/sh
# A pod's `mayfly-pod put` carries a JIT config on its command line (as in bash): the log has none.
echo "qm $*" | sed 's/JITSECRET-[0-9]*/<jit>/g' >>"$CALLS"
etc=$MAYFLY_PVE_ETC/qemu-server
case "$1" in
  config) cat "$etc/$2.conf" 2>/dev/null || exit 2 ;;
  clone) printf 'name: %s\ntags: mayfly\nscsi0: local-lvm:vm-%s-disk-0,size=32G\n' "$5" "$3" >"$etc/$3.conf" ;;
  start)
    # What the VM starts from: the snippet (and its mode) and the firewall file, as they are right now.
    n=$(cat "$CAP/n" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" >"$CAP/n"
    cp "$GSNIP/mayfly-$2.yaml" "$CAP/snippet-$n.yaml" 2>/dev/null
    stat -c %a "$GSNIP/mayfly-$2.yaml" >"$CAP/snippet-$n.mode" 2>/dev/null
    cp "$MAYFLY_PVE_FW_DIR/$2.fw" "$CAP/fw-$n.fw" 2>/dev/null
    # A stand-in for the qemu process: vm_running looks for a live pid whose command line has "-id <vmid> ".
    python3 -c 'import time; time.sleep(300)' -id "$2" x </dev/null >/dev/null 2>&1 &
    echo $! >"$MAYFLY_QEMU_RUN_DIR/$2.pid"
    for _ in $(seq 100); do
      tr '\0' ' ' <"/proc/$!/cmdline" 2>/dev/null | grep -q -- "-id $2 " && break
      sleep 0.05
    done ;;
  stop)
    if [ -f "$MAYFLY_QEMU_RUN_DIR/$2.pid" ]; then
      kill "$(cat "$MAYFLY_QEMU_RUN_DIR/$2.pid")" 2>/dev/null
      rm -f "$MAYFLY_QEMU_RUN_DIR/$2.pid"
    fi ;;
  destroy) rm -f "$etc/$2.conf" ;;
  guest)
    case "$*" in
      *"/usr/local/sbin/mayfly-pod "*)
        # The pod script in the VM: qm guest exec <id> --timeout 30 -- /usr/local/sbin/mayfly-pod <cmd> [args]
        while [ "$1" != "--" ]; do shift; done
        sub=$3
        shift 3
        case "$sub" in
          want)
            # Two idle containers on the first look, none after.
            m=$(cat "$CAP/want.n" 2>/dev/null || echo 0)
            echo $((m + 1)) >"$CAP/want.n"
            if [ "$m" = 0 ]; then printf '%s\n' '{"exitcode":0,"out-data":"2\n"}'; else printf '%s\n' '{"exitcode":0,"out-data":"0\n"}'; fi ;;
          busy) printf '%s\n' '{"exitcode":0,"out-data":"0\n"}' ;;
          put)
            # The first config is taken, the second refused.
            m=$(cat "$CAP/put.n" 2>/dev/null || echo 0)
            m=$((m + 1))
            echo "$m" >"$CAP/put.n"
            echo "$*" >"$CAP/put-$m"
            if [ "$m" = 2 ]; then echo '{"exitcode":1}'; else echo '{"exitcode":0}'; fi ;;
          *) echo '{"exitcode":0}' ;;
        esac ;;
      *) echo '{"exitcode":3}' ;;
    esac ;;
esac
exit 0
STUB
cat >"$s/bin/pct" <<'STUB'
#!/bin/sh
echo "pct $*" >>"$CALLS"
etc=$MAYFLY_PVE_ETC/lxc
case "$1" in
  config) cat "$etc/$2.conf" 2>/dev/null || exit 2 ;;
  clone) printf 'hostname: %s\ntags: mayfly\nrootfs: local-lvm:vm-%s-disk-0,size=8G\n' "$5" "$3" >"$etc/$3.conf" ;;
  start)
    # What the container starts from: the firewall file, and whether a cloud-init snippet is there (it must not be).
    n=$(cat "$CAP/n" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" >"$CAP/n"
    cp "$MAYFLY_PVE_FW_DIR/$2.fw" "$CAP/fw-$n.fw" 2>/dev/null
    ls "$GSNIP" >"$CAP/snip-$n.ls"
    touch "$CTRUN/$2.running" ;;
  stop) rm -f "$CTRUN/$2.running" ;;
  destroy) rm -f "$etc/$2.conf" ;;
esac
exit 0
STUB
cat >"$s/bin/lxc-info" <<'STUB'
#!/bin/sh
if [ -e "$CTRUN/$2.running" ]; then echo RUNNING; else echo STOPPED; fi
STUB
cat >"$s/bin/lxc-attach" <<'STUB'
#!/bin/sh
# lxc-attach -n <id> -- <command...>; what comes in on stdin is saved (it is the JIT config and the job env).
echo "lxc-attach $*" >>"$CALLS"
shift 3
n=$(cat "$CAP/n" 2>/dev/null || echo 0)
case "$1" in
  systemctl) case "$2" in is-system-running) echo running ;; esac ;;
  sh)
    case "$3" in
      *'cat >/etc/mayfly/jit'*) cat >"$CAP/ct-jit-$n" ;;
      *'cat >/etc/mayfly/job.env'*) cat >"$CAP/ct-env-$n" ;;
      # No job (3, not 1: lxc-attach itself fails with 1).
      *pgrep*) exit 3 ;;
    esac ;;
  pgrep) exit 1 ;;
esac
exit 0
STUB
cat >"$s/bin/pvesh" <<'STUB'
#!/bin/sh
echo "pvesh $*" >>"$CALLS"
case "$2" in
  /nodes/*/storage/*/status)
    n=$(cat "$CAP/pvesh.n" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" >"$CAP/pvesh.n"
    case "$n" in
      # Capacity (1), the first guest's second pass (2): a VM or container has run its job and powers off; a
      # pod gets a new template in pool.env and is recycled.
      2)
        case "$KIND" in
          lxc) rm -f "$CTRUN/$GID.running" ;;
          pod) sed -i 's/^TEMPLATE_VMID=9098$/TEMPLATE_VMID=9099/' "$MAYFLY_CONF_DIR/pool.env" ;;
          *)
            kill "$(cat "$MAYFLY_QEMU_RUN_DIR/$GID.pid")" 2>/dev/null
            rm -f "$MAYFLY_QEMU_RUN_DIR/$GID.pid" ;;
        esac ;;
      # Capacity of the second guest (3), its second pass (4; a pod's fourth look at the pool): the unit is stopped.
      4)
        for _ in $(seq 50); do [ -s "$PIDFILE" ] && break; sleep 0.1; done
        kill -TERM "$(cat "$PIDFILE")" ;;
    esac
    echo '{"total":1000,"used":100}' ;;
  *) exit 1 ;;
esac
STUB
chmod +x "$s/bin/"*

cat >"$t/slot.py" <<'PY'
import http.server, json, sys

LOG = sys.argv[1]
state = {"n": 76}


class Handler(http.server.BaseHTTPRequestHandler):
    def note(self, body=""):
        auth = self.headers.get("Authorization", "")
        if auth.count(".") == 2:
            auth = "Bearer <jwt>"
        with open(LOG, "a") as f:
            f.write("%s %s %s %s\n" % (self.command, self.path, auth, body))

    def send(self, code, obj=None):
        body = b"" if obj is None else json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.note()
        if self.path == "/orgs/acme/installation":
            self.send(200, {"id": 42})
        elif self.path.startswith("/orgs/acme/actions/runners/"):
            self.send(200, {"status": "online"})
        else:
            self.send(404, {})

    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(n).decode() if n else ""
        if self.path == "/__reset":
            state["n"] = 76
            self.send(200, {})
            return
        self.note(body)
        if self.path == "/app/installations/42/access_tokens":
            self.send(201, {"token": "ghs_fake"})
        elif self.path == "/orgs/acme/actions/runners/generate-jitconfig":
            state["n"] += 1
            self.send(201, {"runner": {"id": state["n"], "name": "x"}, "encoded_jit_config": "JITSECRET-%d" % state["n"]})
        else:
            self.send(404, {})

    def do_DELETE(self):
        self.note()
        self.send(204)

    def log_message(self, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
PY
python3 "$t/slot.py" "$s/gh.log" >"$t/sport" &
srv3=$!
for _ in $(seq 50); do [ -s "$t/sport" ] && break; sleep 0.1; done
[ -s "$t/sport" ] || { echo "the fake GitHub for slot did not start" >&2; exit 2; }

slot_reset() {
  kill_qemu
  rm -rf "$s/pve" "$s/fw" "$s/snip" "$s/cap" "$s/qemu" "$s/conf" "$s/ct" "$s/share" "$s/mayfly.pid"
  mkdir -p "$s/pve/qemu-server" "$s/pve/lxc" "$s/fw" "$s/snip" "$s/cap" "$s/qemu" "$s/ct" "$s/share" "$s/conf/hooks.d" "$s/conf/env"
  printf 'name: ci-template\ntags: mayfly\ndescription: profile default\ntemplate: 1\n' >"$s/pve/qemu-server/9098.conf"
  # The next template of the pod (pool.env moves to it), the container template, and the pod script.
  cp "$s/pve/qemu-server/9098.conf" "$s/pve/qemu-server/9099.conf"
  printf 'hostname: ci-ct-template\ntags: mayfly\ntemplate: 1\n' >"$s/pve/lxc/9099.conf"
  printf '#!/bin/sh\necho pod\n' >"$s/share/pod-vm.sh"
  cat >"$s/conf/pool.env" <<ENV
APP_ID=12345
ORG=acme
KEY_PATH=$t/app.pem
API_URL=http://127.0.0.1:$(cat "$t/sport")
RUNNER_PREFIX=mf
TEMPLATE_VMID=9098
LXC_TEMPLATE_VMID=9099
GATEWAY=10.0.0.1
CACHE_IP=10.0.0.5
HOOK_DIR=$s/conf/hooks.d
MIN_FREE_RAM_GB=0
ENV
  # engine=rust is for the Rust binary; bash reads the slot and ignores the key.
  echo "$SLOT_LINE" >"$s/conf/slots.conf"
  printf 'A=1\nB=two\n# comment\nnot a line\n' >"$s/conf/env/cache.env"
  printf '#!/bin/sh\necho "hook $*" >>"$CALLS"\n' >"$s/conf/hooks.d/10-rec"
  printf '#!/bin/sh\necho "hook-fail $*" >>"$CALLS"\nexit 1\n' >"$s/conf/hooks.d/20-fail"
  printf '#!/bin/sh\necho "never $*" >>"$CALLS"\n' >"$s/conf/hooks.d/30-not-executable"
  chmod +x "$s/conf/hooks.d/10-rec" "$s/conf/hooks.d/20-fail"
  curl -s -X POST "http://127.0.0.1:$(cat "$t/sport")/__reset" >/dev/null
  : >"$s/gh.log"
  : >"$s/calls"
}

# slot_run <prefix> <command ...>: runs the slot (in the background, so that the stubs can send it a TERM),
# saves its output, exit code and the state it left. Epoch seconds in names are replaced.
slot_run() {
  local p=$1 pid f
  shift
  slot_reset
  env -u INVOCATION_ID "PATH=$s/bin:$PATH" "CALLS=$s/calls" "CAP=$s/cap" "GSNIP=$s/snip" "PIDFILE=$s/mayfly.pid" \
    "MAYFLY_TEST_NAP=0" "MAYFLY_QEMU_RUN_DIR=$s/qemu" "MAYFLY_CONF_DIR=$s/conf" "MAYFLY_PVE_ETC=$s/pve" \
    "MAYFLY_PVE_FW_DIR=$s/fw" "MAYFLY_SHARE=$s/share" "GID=$SLOT_GID" "KIND=$SLOT_KIND" "CTRUN=$s/ct" "$@" >"$p.out" 2>"$p.err" </dev/null &
  pid=$!
  echo "$pid" >"$s/mayfly.pid"
  for _ in $(seq 600); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
  if kill -0 "$pid" 2>/dev/null; then
    kill -KILL "$pid" 2>/dev/null
    echo "timed out" >>"$p.err"
  fi
  wait "$pid"
  echo "$?" >"$p.rc"
  kill_qemu
  sed -i -E 's/[0-9]{10}/EPOCH/g' "$p.out" "$p.err"
  sed -E 's/[0-9]{10}/EPOCH/g' "$s/calls" >"$p.calls"
  sed -E 's/[0-9]{10}/EPOCH/g' "$s/gh.log" >"$p.gh"
  {
    echo "--- calls"
    cat "$p.calls"
    echo "--- github"
    cat "$p.gh"
    echo "--- what each VM started from"
    for f in "$s"/cap/snippet-* "$s"/cap/fw-* "$s"/cap/snip-* "$s"/cap/ct-* "$s"/cap/put-*; do
      [ -e "$f" ] || continue
      echo "== ${f#"$s"/cap/}"
      cat "$f"
    done
    echo "--- files left"
    (cd "$s" && ls fw snip pve/qemu-server pve/lxc ct)
  } >"$p.state"
  # The runner names in what a pod was handed carry the time.
  sed -i -E 's/[0-9]{10}/EPOCH/g' "$p.state"
}

same_slot() {
  local name=$1 ok=1 f
  shift
  slot_run "$t/lb" bash "$here/mayfly.sh" "$@"
  slot_run "$t/lr" "$bin" "$@"
  n=$((n + 1))
  for f in out err rc state; do cmp -s "$t/lb.$f" "$t/lr.$f" || ok=0; done
  # The JIT config is only ever in the snippet.
  if grep -q JITSECRET "$t/lr.out" "$t/lr.err" "$t/lr.calls" "$t/lr.gh"; then ok=0; echo "the JIT config leaked"; fi
  if [ "$(cat "$t/lr.rc")" != 0 ] || ! grep -q 'gone after' "$t/lr.out" || [ "$(grep -c "^$SLOT_START " "$t/lr.calls")" != 2 ]; then
    ok=0
    echo "the run did not go through two guests and a drain:"
    cat "$t/lr.out" "$t/lr.err"
  fi
  if [ "$ok" = 1 ]; then
    echo "ok   $name (exit $(cat "$t/lb.rc"), $(wc -l <"$t/lb.out") lines, $(wc -l <"$t/lb.state") lines of state)"
  else
    echo "FAIL $name: bash exit $(cat "$t/lb.rc"), rust exit $(cat "$t/lr.rc")"
    for f in out err rc state; do
      diff -u --label "bash $f" --label "rust $f" "$t/lb.$f" "$t/lr.$f" | head -60
    done
    fails=$((fails + 1))
  fi
}

SLOT_GID=9002 SLOT_KIND=vm SLOT_START="qm start"
SLOT_LINE='a-1 9002 linux,gpu 8 1 60 cache,missing current engine=rust'
same_slot "slot, one job cycle and a drain (kind=vm)" slot a-1

SLOT_GID=9051 SLOT_KIND=lxc SLOT_START="pct start"
SLOT_LINE='a-1 9051 linux,gpu 2 4 20 cache,missing current kind=lxc engine=rust'
same_slot "slot, one job cycle and a drain (kind=lxc)" slot a-1

# A pod: its VM is recycled when pool.env names a new template, and drained on the TERM.
SLOT_GID=9041 SLOT_KIND=pod SLOT_START="qm start"
SLOT_LINE='a-1 9041 linux 8 8 60 cache,missing current kind=pod containers=4 engine=rust'
same_slot "slot, a recycled and a drained pod (kind=pod)" slot a-1

echo
echo "$((n - fails))/$n identical"
[ "$fails" = 0 ]
