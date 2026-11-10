#!/usr/bin/env bash
# Runs once inside a fresh Ubuntu cloud VM (cloud-init runcmd from `mayfly template build`), or with
# MAYFLY_GUEST=lxc inside a fresh container (`mayfly template build --lxc`), and turns it into a CI template: the GitHub runner, Docker, the one-job service, then the profile's own
# provision.sh (toolchains, libraries), then the profile's pre-pulled images, then the seal.
# On success the host shuts the VM down and converts it into a template. On failure it stays up
# with the log in /var/log/mayfly-provision.log.
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive
# Core settings, then the profile's (which may override them). Exported, so the profile's own
# provision.sh, which runs in a child bash, sees them too.
set -a
# shellcheck source=/dev/null
. /root/template.env
# The profile goes to a world-readable directory: its provision.sh runs from there, and commands it
# runs as runner (runuser keeps the working directory) would fail in root's 0700 home.
install -d -m 0755 /opt/mayfly-profile
tar -xzf /root/profile.tgz -C /opt/mayfly-profile --no-same-owner
chmod -R a+rX /opt/mayfly-profile
# shellcheck source=/dev/null
[ ! -e /opt/mayfly-profile/template.env ] || . /opt/mayfly-profile/template.env
set +a
INSTALL_DOCKER=${INSTALL_DOCKER:-yes}
INSTALL_GH=${INSTALL_GH:-yes}
PREPULL_IMAGES=${PREPULL_IMAGES:-}
codename=$(. /etc/os-release && echo "$VERSION_CODENAME")
guest=${MAYFLY_GUEST:-vm}
agent=qemu-guest-agent
[ "$guest" = vm ] || agent=""

# ---- base packages
apt-get update -q
apt-get install -y -q --no-install-recommends \
  ca-certificates curl gnupg git git-lfs jq unzip zip xz-utils openssl procps python3 $agent
install -m 0755 -d /etc/apt/keyrings
if [ "$INSTALL_DOCKER" = yes ]; then
  curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
  echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $codename stable" \
    >/etc/apt/sources.list.d/docker.list
fi
if [ "$INSTALL_GH" = yes ]; then
  curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli.gpg
  echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/githubcli.gpg] https://cli.github.com/packages stable main" \
    >/etc/apt/sources.list.d/github-cli.list
fi
apt-get update -q
[ "$INSTALL_DOCKER" != yes ] || apt-get install -y -q --no-install-recommends \
  docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
[ "$INSTALL_GH" != yes ] || apt-get install -y -q --no-install-recommends gh

# A job VM lives minutes: no background apt, no swap, small journal.
systemctl disable --now apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service 2>/dev/null || true
apt-get purge -y -q unattended-upgrades snapd 2>/dev/null || true
swapoff -a || true
sed -i '/\sswap\s/d' /etc/fstab
mkdir -p /etc/systemd/journald.conf.d
printf '[Journal]\nSystemMaxUse=200M\n' >/etc/systemd/journald.conf.d/mayfly.conf
[ "$guest" != vm ] || systemctl enable qemu-guest-agent

# ---- Docker: log caps, optional pull-through mirror on the cache VM
if [ "$INSTALL_DOCKER" = yes ]; then
  mkdir -p /etc/docker
  if [ -n "${CACHE_IP:-}" ]; then
    mirror=",\"registry-mirrors\": [\"http://$CACHE_IP:5000\"], \"insecure-registries\": [\"$CACHE_IP:5000\"]"
  else
    mirror=""
  fi
  cat >/etc/docker/daemon.json <<JSON
{"log-driver": "local", "log-opts": {"max-size": "20m", "max-file": "3"}$mirror}
JSON
  systemctl restart docker
fi

# ---- runner user: Docker group, no sudo (the job cannot change the VM it runs in beyond its home and /tmp)
id runner >/dev/null 2>&1 || useradd -m -s /bin/bash -u 1001 runner
[ "$INSTALL_DOCKER" != yes ] || usermod -aG docker runner
install -d -o runner -g runner /opt/actions-runner /opt/hostedtoolcache
# Root-only: cloud-init drops the JIT config and the slot's job environment here for ci-job.sh.
install -d -m 0700 /etc/mayfly

# ---- GitHub runner, pinned, checked against the SHA in its release notes
notes=$(curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 "https://api.github.com/repos/actions/runner/releases/tags/v${RUNNER_VERSION}" | jq -r .body)
sha=$(sed -n 's/.*<!-- BEGIN SHA linux-x64 -->\([0-9a-f]\{64\}\)<!-- END SHA linux-x64 -->.*/\1/p' <<<"$notes" | head -1)
[ -n "$sha" ]
f="actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 -o "/tmp/$f" "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/$f"
echo "$sha  /tmp/$f" | sha256sum -c -
runuser -u runner -- tar -xzf "/tmp/$f" -C /opt/actions-runner
rm -f "/tmp/$f"
/opt/actions-runner/bin/installdependencies.sh

# Job environment. The runner reads .env (variables) and .path (PATH) from its own folder; the
# profile may append to .env and replace .path.
cat >/opt/actions-runner/.env <<'ENV'
LANG=C.UTF-8
LC_ALL=C.UTF-8
RUNNER_TOOL_CACHE=/opt/hostedtoolcache
AGENT_TOOLSDIRECTORY=/opt/hostedtoolcache
ENV
echo "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" >/opt/actions-runner/.path

# ---- the profile: toolchains, libraries, anything the workflows expect
if [ -e /opt/mayfly-profile/provision.sh ]; then
  (cd /opt/mayfly-profile && bash -euxo pipefail ./provision.sh)
fi
chown runner:runner /opt/actions-runner/.env /opt/actions-runner/.path

# ---- images the workflows start
if [ "$INSTALL_DOCKER" = yes ]; then
  for image in $PREPULL_IMAGES; do docker pull -q "$image"; done
fi

# ---- the one-job service
install -m 0755 /root/ci-job.sh /usr/local/sbin/mayfly-job
install -m 0644 /root/mayfly-job.service /etc/systemd/system/mayfly-job.service
systemctl enable mayfly-job.service

# ---- seal: every clone gets its own machine id, SSH host keys and cloud-init run
rm -rf /root/provision.sh /root/ci-job.sh /root/mayfly-job.service /root/template.env /opt/mayfly-profile /root/profile.tgz
apt-get clean
rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*
rm -f /etc/ssh/ssh_host_*
truncate -s 0 /etc/machine-id
rm -f /var/lib/dbus/machine-id
if command -v cloud-init >/dev/null; then cloud-init clean --logs --seed; fi
[ "$guest" != vm ] || fstrim -av || true
sync
