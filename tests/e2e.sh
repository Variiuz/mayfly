#!/usr/bin/env bash
# The real binary runs a slot of each kind (vm, lxc, pod) against stubs, and what it does is compared with
# what was recorded in tests/golden/. The recordings were made when the bash loop of 0.9.0 and this loop did
# exactly the same (tests/parity.sh of that release), so they pin the loop to it.
# A vm or lxc slot goes through one job cycle (the guest powers itself off) and a second guest that a TERM
# drains; a pod through one VM that is recycled when its template changes and a second one that a TERM
# drains. Stubs record every qm, pct, lxc-attach, pvesh and pvesm call, the cloud-init snippet and firewall file
# at `qm start` / `pct start` and what was handed to a container or pod, a fake GitHub records its requests (the
# JIT body included), hooks record their arguments. The calls, files, requests, output and exit code are compared.
# Then: a newer mayfly is installed while a guest runs; the loop hands over to it once that guest is gone.
# Usage: bash tests/e2e.sh [path to the mayfly binary]   (default target/release/mayfly)
#        UPDATE_GOLDEN=1 bash tests/e2e.sh [binary]      records tests/golden/ anew (read the diff before committing)
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${1:-$here/target/release/mayfly}
[ -x "$bin" ] || { echo "no binary at $bin (cargo build --release)" >&2; exit 2; }
golden=$here/tests/golden
t=$(mktemp -d)
s=$t/s
srv=""
# The stand-in qemu processes (pid files under $s/qemu).
kill_qemu() {
  local f
  for f in "$s"/qemu/*.pid; do
    [ -e "$f" ] || continue
    kill "$(cat "$f")" 2>/dev/null || true
  done
}
cleanup() {
  kill_qemu
  [ -z "$srv" ] || kill "$srv" 2>/dev/null
  rm -rf "$t"
}
trap cleanup EXIT
n=0 fails=0

# An App key, so that the binary can sign its JWT.
openssl genrsa -traditional -out "$t/app.pem" 2048 2>/dev/null || openssl genrsa -out "$t/app.pem" 2048 2>/dev/null ||
  { echo "openssl is missing" >&2; exit 2; }

# ---- stubs
mkdir -p "$s/bin"
cat >"$s/bin/pvesm" <<'STUB'
#!/bin/sh
echo "pvesm $*" >>"$CALLS"
case "$1" in
  path) echo "$GSNIP/x" ;;
  *) exit 1 ;;
esac
STUB
# The node's name is in the pvesh calls: the same on every machine that runs this.
printf '#!/bin/sh\necho pve1\n' >"$s/bin/hostname"
cat >"$s/bin/qm" <<'STUB'
#!/bin/sh
# A pod's `mayfly-pod put` carries a JIT config on its command line: the log has none.
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
      # pod gets a new template in pool.env and is recycled. With NEWER set, a newer mayfly is installed too.
      2)
        if [ -n "${NEWER:-}" ]; then
          printf '#!/bin/sh\necho "the new mayfly: $*"\n' >"$MAYFLY_SBIN/mayfly.tmp"
          chmod +x "$MAYFLY_SBIN/mayfly.tmp"
          mv "$MAYFLY_SBIN/mayfly.tmp" "$MAYFLY_SBIN/mayfly"
        fi
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
        pid=$(cat "$PIDFILE")
        kill -TERM "$pid"
        # Answer only once the slot has taken the TERM (the kernel has handed it to the handler: no longer
        # pending), so that the loop has it before the call returns and deals with it at its next step, not at
        # some point after it that depends on the scheduler.
        for _ in $(seq 100); do
          pend=0
          while read -r k v; do
            case "$k" in SigPnd: | ShdPnd:) [ $((0x$v & 0x4000)) = 0 ] || pend=1 ;; esac
          done <"/proc/$pid/status"
          [ "$pend" = 0 ] && break
          sleep 0.05
        done ;;
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
srv=$!
for _ in $(seq 50); do [ -s "$t/sport" ] && break; sleep 0.1; done
[ -s "$t/sport" ] || { echo "the fake GitHub for slot did not start" >&2; exit 2; }

slot_reset() {
  kill_qemu
  rm -rf "$s/pve" "$s/fw" "$s/snip" "$s/cap" "$s/qemu" "$s/conf" "$s/ct" "$s/share" "$s/sbin" "$s/mayfly.pid"
  mkdir -p "$s/pve/qemu-server" "$s/pve/lxc" "$s/fw" "$s/snip" "$s/cap" "$s/qemu" "$s/ct" "$s/share" "$s/sbin" "$s/conf/hooks.d" "$s/conf/env"
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

# slot_run <prefix> <slot>: runs the slot (in the background, so that the stubs can send it a TERM), saves its
# output, exit code and the state it left. Epoch seconds in names and the temporary directory are replaced.
slot_run() {
  local p=$1 pid f
  shift
  slot_reset
  env -u INVOCATION_ID "PATH=$s/bin:$PATH" "CALLS=$s/calls" "CAP=$s/cap" "GSNIP=$s/snip" "PIDFILE=$s/mayfly.pid" \
    "MAYFLY_TEST_NAP=0" "MAYFLY_QEMU_RUN_DIR=$s/qemu" "MAYFLY_CONF_DIR=$s/conf" "MAYFLY_PVE_ETC=$s/pve" \
    "MAYFLY_PVE_FW_DIR=$s/fw" "MAYFLY_SHARE=$s/share" "MAYFLY_SBIN=$s/sbin" "GID=$SLOT_GID" "KIND=$SLOT_KIND" \
    "CTRUN=$s/ct" "NEWER=${NEWER:-}" "$bin" slot "$@" >"$p.out" 2>"$p.err" </dev/null &
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
  cp "$s/calls" "$p.calls"
  cp "$s/gh.log" "$p.gh"
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
  sed -i -E -e 's/[0-9]{10}/EPOCH/g' -e "s|$t|TMP|g" "$p.out" "$p.err" "$p.calls" "$p.gh" "$p.state"
}

# golden_slot <name> <title>: one run, compared with tests/golden/<name>.{out,err,rc,state}.
golden_slot() {
  local name=$1 title=$2 ok=1 f
  slot_run "$t/run" a-1
  n=$((n + 1))
  if [ -n "${UPDATE_GOLDEN:-}" ]; then
    mkdir -p "$golden"
    for f in out err rc state; do cp "$t/run.$f" "$golden/$name.$f"; done
  fi
  for f in out err rc state; do cmp -s "$golden/$name.$f" "$t/run.$f" || ok=0; done
  # The JIT config is only ever in the snippet, or on the stdin of a container.
  if grep -q JITSECRET "$t/run.out" "$t/run.err" "$t/run.calls" "$t/run.gh"; then ok=0; echo "the JIT config leaked"; fi
  if [ "$(cat "$t/run.rc")" != 0 ] || ! grep -q 'gone after' "$t/run.out" || [ "$(grep -c "^$SLOT_START " "$t/run.calls")" != "$SLOT_GUESTS" ]; then
    ok=0
    echo "the run did not go through $SLOT_GUESTS guest(s) to its end:"
    cat "$t/run.out" "$t/run.err"
  fi
  if [ "$ok" = 1 ]; then
    echo "ok   $title (exit $(cat "$t/run.rc"), $(wc -l <"$t/run.out") lines, $(wc -l <"$t/run.state") lines of state)"
  else
    echo "FAIL $title (exit $(cat "$t/run.rc"))"
    for f in out err rc state; do
      diff -u --label "recorded $f" --label "now $f" "$golden/$name.$f" "$t/run.$f" | head -60
    done
    fails=$((fails + 1))
  fi
}

SLOT_GUESTS=2 NEWER=""
SLOT_GID=9002 SLOT_KIND=vm SLOT_START="qm start"
SLOT_LINE='a-1 9002 linux,gpu 8 1 60 cache,missing current'
golden_slot slot-vm "slot, one job cycle and a drain (kind=vm)"

SLOT_GID=9051 SLOT_KIND=lxc SLOT_START="pct start"
SLOT_LINE='a-1 9051 linux,gpu 2 4 20 cache,missing current kind=lxc'
golden_slot slot-lxc "slot, one job cycle and a drain (kind=lxc)"

# A pod: its VM is recycled when pool.env names a new template, and drained on the TERM.
SLOT_GID=9041 SLOT_KIND=pod SLOT_START="qm start"
SLOT_LINE='a-1 9041 linux 8 8 60 cache,missing current kind=pod containers=4'
golden_slot slot-pod "slot, a recycled and a drained pod (kind=pod)"

# A leftover engine= (0.6.0 to 0.9.0) changes nothing.
SLOT_GID=9002 SLOT_KIND=vm SLOT_START="qm start"
SLOT_LINE='a-1 9002 linux,gpu 8 1 60 cache,missing current engine=bash'
golden_slot slot-vm "slot, engine=bash is ignored (kind=vm)"

# A newer mayfly appears in MAYFLY_SBIN while the first guest runs: once that guest is gone the loop hands
# over to it (same arguments, same process), instead of cloning the next guest with the old code.
SLOT_GUESTS=1 NEWER=1
SLOT_LINE='a-1 9002 linux,gpu 8 1 60 cache,missing current'
golden_slot renew-vm "slot, hands over to a newer mayfly between two guests (kind=vm)"
SLOT_GID=9041 SLOT_KIND=pod
SLOT_LINE='a-1 9041 linux 8 8 60 cache,missing current kind=pod containers=4'
golden_slot renew-pod "slot, hands over to a newer mayfly between two pod VMs (kind=pod)"

echo
echo "$((n - fails))/$n as recorded"
[ "$fails" = 0 ]
