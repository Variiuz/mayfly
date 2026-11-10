#!/usr/bin/env bash
# The Rust commands print what mayfly.sh prints: runs version, help, status and check of both against
# one fixture (stubs for qm, pct, pvesm, pvesh, systemctl, ip, pve-firewall and a local GitHub) and
# compares stdout, stderr and the exit code. gc, enable and disable run against stubs that record every
# call: the recorded calls, the requests the fake GitHub saw and the files left behind are compared too.
# install: the bash install.sh of 0.4.1 (tests/fixtures/install-0.4.1.sh) and `mayfly install` run against
# the same stub root; stdout, stderr, the exit code, the stub calls and the tree they leave (paths, modes,
# link targets, checksums) are compared.
# Usage: bash tests/parity.sh [path to the mayfly binary]   (default target/release/mayfly)
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-$here/target/release/mayfly}
[ -x "$bin" ] || { echo "no binary at $bin (cargo build --release)" >&2; exit 2; }
t=$(mktemp -d)
srv="" srv2=""
cleanup() {
  [ -z "$srv" ] || kill "$srv" 2>/dev/null
  [ -z "$srv2" ] || kill "$srv2" 2>/dev/null
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

cat >"$g/conf/pool.env" <<ENV
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

# snap_run <prefix> <command ...>: resets the host, runs the command and saves its output, its exit code
# and the state it left: the calls the stubs recorded (the parallel `systemctl stop`s sorted), the
# requests the GitHub saw and the files that are left.
snap_run() {
  local p=$1
  shift
  state_reset
  [ -z "${PRE:-}" ] || "$PRE"
  env "PATH=$g/bin:$PATH" "CALLS=$g/calls" "ACTIVE=$g/active" "GSNIP=$g/snip" \
    "MAYFLY_CONF_DIR=$g/conf" "MAYFLY_PVE_ETC=$g/pve" "MAYFLY_PVE_FW_DIR=$g/fw" \
    "$@" >"$p.out" 2>"$p.err" </dev/null
  echo "$?" >"$p.rc"
  {
    echo "--- calls"
    grep -v '^systemctl stop ' "$g/calls"
    echo "--- parallel stops, sorted"
    grep '^systemctl stop ' "$g/calls" | sort
    echo "--- github"
    cat "$g/gh.log"
    echo "--- files"
    (cd "$g" && ls fw snip)
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

echo
echo "$((n - fails))/$n identical"
[ "$fails" = 0 ]
