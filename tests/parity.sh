#!/usr/bin/env bash
# The Rust commands print what mayfly.sh prints: runs version, help, status and check of both against
# one fixture (stubs for qm, pct, pvesm, pvesh, systemctl, ip, pve-firewall and a local GitHub) and
# compares stdout, stderr and the exit code.
# Usage: bash tests/parity.sh [path to the mayfly binary]   (default target/release/mayfly)
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-$here/target/release/mayfly}
[ -x "$bin" ] || { echo "no binary at $bin (cargo build --release)" >&2; exit 2; }
t=$(mktemp -d)
srv=""
cleanup() { [ -z "$srv" ] || kill "$srv" 2>/dev/null; rm -rf "$t"; }
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

echo
echo "$((n - fails))/$n identical"
[ "$fails" = 0 ]
