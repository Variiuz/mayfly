#!/usr/bin/env bash
# Updates a real older release to the build under test, the way a host does: installs <old> from its
# GitHub release into a stub root, then runs that old binary's `mayfly update` against a local release
# server that offers the files scripts/package.sh made. Checks that the new version is installed, the
# config (pool.env, slots.conf, env/, hooks.d, profiles/) and the units survive, the old version is kept in
# previous/, and that `mayfly update --rollback` brings it back.
# Usage: bash tests/update.sh <dist dir from scripts/package.sh> [old version]
#   old version: default the newest release older than the one in <dist>. Needs github.com, jq, python3.
# shellcheck disable=SC2034 # rc, before: read by the eval in ok()
set -uo pipefail
dist=$(cd "${1:?usage: tests/update.sh <dist dir> [old version]}" && pwd)
new=$(sed -n 's/^mayfly-\([0-9.]*\)\.tar\.gz$/\1/p' < <(ls "$dist"))
[ -n "$new" ] || { echo "no mayfly-<version>.tar.gz in $dist" >&2; exit 2; }
api=https://api.github.com/repos/variiuz/mayfly
auth=() # GH_TOKEN, when set, avoids the anonymous API rate limit (CI)
[ -z "${GH_TOKEN:-}" ] || auth=(-H "Authorization: Bearer $GH_TOKEN")
old=${2:-$(curl -fsSL "${auth[@]}" "$api/releases?per_page=100" |
  jq -r --arg v "$new" '[.[] | select(.draft | not) | select(.prerelease | not) | .tag_name | ltrimstr("v")
    | select(split(".") | map(tonumber) as $a | ($v | split(".") | map(tonumber)) as $b | $a < $b)] | first // empty')}
[ -n "$old" ] || { echo "no release older than $new" >&2; exit 2; }

t=$(mktemp -d)
srv=""
cleanup() { [ -z "$srv" ] || kill "$srv" 2>/dev/null; rm -rf "$t"; }
trap cleanup EXIT
pass=0 fail=0
ok() {
  if eval "$2"; then pass=$((pass + 1)); echo "ok   $1"; else fail=$((fail + 1)); echo "FAIL $1"; fi
}

# ---- the old release, from GitHub
mkdir -p "$t/old"
curl -fsSL --retry 3 -o "$t/old/mayfly.tar.gz" "https://github.com/variiuz/mayfly/releases/download/v$old/mayfly-$old.tar.gz" ||
  { echo "cannot download release $old" >&2; exit 2; }
tar -C "$t/old" -xzf "$t/old/mayfly.tar.gz"

# ---- stub root: every command install, update and check call is answered and logged
mkdir -p "$t/bin" "$t/root/conf" "$t/root/share" "$t/root/sbin" "$t/root/systemd" "$t/root/state" "$t/root/tmp" "$t/root/pve"
for c in qm pct apt-get systemctl lxc-attach pve-firewall; do
  printf '#!/bin/sh\necho "%s $*" >>"%s/calls"\nexit 0\n' "$c" "$t" >"$t/bin/$c"
done
# The snippet storage already holds snippets, so install changes nothing there.
printf '#!/bin/sh\necho "pvesh $*" >>"%s/calls"\ncase "$2" in /storage/*) echo '"'"'{"content":"iso,snippets"}'"'"' ;; esac\nexit 0\n' "$t" >"$t/bin/pvesh"
printf '#!/bin/sh\necho "pvesm $*" >>"%s/calls"\ncase "$1" in path) echo "%s/snippets/x" ;; esac\nexit 0\n' "$t" "$t" >"$t/bin/pvesm"
printf '#!/bin/sh\n[ "$1" = -u ] && { echo 0; exit 0; }\nexec /usr/bin/id "$@"\n' >"$t/bin/id"
chmod +x "$t"/bin/*
export PATH="$t/bin:$PATH" NO_COLOR=1
export MAYFLY_CONF_DIR=$t/root/conf MAYFLY_SHARE=$t/root/share MAYFLY_SBIN=$t/root/sbin \
  MAYFLY_SYSTEMD_DIR=$t/root/systemd MAYFLY_STATE_DIR=$t/root/state MAYFLY_TMP=$t/root/tmp MAYFLY_PVE_ETC=$t/root/pve

# A configured host: the files an update must leave as they are.
printf 'APP_ID=1\nORG=acme\nSNIPPET_STORAGE=local\n# keep me\nUPDATE=auto\n' >"$t/root/conf/pool.env"
printf 'a-1 9002 linux 2 4 20 - current\n' >"$t/root/conf/slots.conf"
mkdir -p "$t/root/conf/env" "$t/root/conf/hooks.d" "$t/root/conf/profiles/mine"
echo 'A=1' >"$t/root/conf/env/x.env"
printf '#!/bin/sh\n' >"$t/root/conf/hooks.d/10-mine"
echo 'echo mine' >"$t/root/conf/profiles/mine/provision.sh"
conf_sum() { (cd "$t/root/conf" && find . -type f -print0 | sort -z | xargs -0 sha256sum); }
before=$(conf_sum)

out=$(bash "$t/old/mayfly-$old/install.sh" 2>&1); rc=$?
echo "$out" | sed 's/^/  | /'
ok "release $old installs" '[ "$rc" = 0 ] && [ "$("$t/root/sbin/mayfly" version)" = "mayfly $old" ]'

# ---- a local release server offering the build under test as the latest release
mkdir -p "$t/www"
cp "$dist"/* "$t/www/"
cat >"$t/serve.py" <<'PY'
import http.server, json, os, sys
www, new, port_file = sys.argv[1], sys.argv[2], sys.argv[3]
rel = {"tag_name": "v" + new, "draft": False, "prerelease": False, "body": "### Bug Fixes\n", "html_url": ""}
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def send(self, code, body, ctype="application/octet-stream"):
        self.send_response(code); self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        p = self.path.split("?")[0]
        if p == "/api/releases/latest" or p == "/api/releases/tags/v" + new:
            return self.send(200, json.dumps(rel).encode(), "application/json")
        if p == "/api/releases":
            return self.send(200, json.dumps([rel]).encode(), "application/json")
        pre = "/repo/releases/download/v" + new + "/"
        f = os.path.join(www, p[len(pre):]) if p.startswith(pre) else ""
        if f and "/" not in p[len(pre):] and os.path.isfile(f):
            return self.send(200, open(f, "rb").read())
        self.send(404, b"not found")
s = http.server.HTTPServer(("127.0.0.1", 0), H)
open(port_file, "w").write(str(s.server_port))
s.serve_forever()
PY
python3 -I "$t/serve.py" "$t/www" "$new" "$t/port" & srv=$!
for _ in $(seq 50); do [ -s "$t/port" ] && break; sleep 0.1; done
port=$(cat "$t/port")
export MAYFLY_RELEASE_API=http://127.0.0.1:$port/api MAYFLY_REPO_URL=http://127.0.0.1:$port/repo
unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy ALL_PROXY all_proxy

# ---- the old binary updates itself to the new build
out=$("$t/root/sbin/mayfly" update 2>&1); rc=$?
echo "$out" | sed 's/^/  | /'
ok "mayfly $old updates to $new" '[ "$rc" = 0 ] && [ "$("$t/root/sbin/mayfly" version)" = "mayfly $new" ]'
ok "the config is unchanged" '[ "$(conf_sum)" = "$before" ]'
ok "the share directory has the new mayfly.sh" 'grep -q "^MAYFLY_VERSION=$new" "$t/root/share/mayfly.sh"'
for u in mayfly-slot@.service mayfly-gc.service mayfly-gc.timer mayfly-update.service mayfly-update.timer; do
  ok "unit $u is installed" '[ -f "$t/root/systemd/$u" ]'
done
ok "the old version is kept in previous/" '[ "$(cat "$t/root/share/previous/VERSION")" = "$old" ] && [ "$("$t/root/share/previous/sbin/mayfly" version)" = "mayfly $old" ]'
ok "a second update finds nothing newer" '"$t/root/sbin/mayfly" update --check 2>&1 | grep -q "up to date"'

# ---- and goes back
out=$("$t/root/sbin/mayfly" update --rollback 2>&1); rc=$?
echo "$out" | sed 's/^/  | /'
ok "rollback restores $old" '[ "$rc" = 0 ] && [ "$("$t/root/sbin/mayfly" version)" = "mayfly $old" ]'
ok "the config is still unchanged" '[ "$(conf_sum)" = "$before" ]'
ok "the timer then skips $new" '"$t/root/sbin/mayfly" update --auto 2>&1 | grep -q "skipped"'

echo
echo "$pass/$((pass + fail)) passed ($old -> $new)"
[ "$fail" = 0 ]
