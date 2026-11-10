# Mayfly

Ephemeral GitHub Actions runners on Proxmox VE. Each job runs in its own VM or LXC container, a linked
clone of a template, which is destroyed when the job ends. Short jobs can instead run in pods: one
long-lived VM that runs one throwaway container per job.

Mayfly is a static binary, a Bash script and a set of systemd units on the Proxmox host. The binary
(Rust) runs `version`, `help`, `status`, `check`, `update`, `gc`, `enable`, `disable` and `template` and
hands `slot` to the script. It requires a GitHub App, a thin-provisioned storage pool (LVM-thin, ZFS or Ceph) and a network bridge.

## Install

On the Proxmox host, as root:

```
curl -fsSL https://github.com/variiuz/mayfly/releases/latest/download/install.sh | bash
```

The installer downloads the latest release, verifies it against `SHA256SUMS` and runs its
`mayfly install`, which installs `mayfly` and its units and creates `/etc/mayfly`. It does not start any unit or touch any guest. To update, run it
again; configuration, slots and running jobs are kept.

## Setup

1. Create a GitHub App without a webhook. Permissions: *Self-hosted runners: read and write* for
   organization runners, or *Administration: read and write* for repository runners. Install it on the
   organization or repository, generate a private key, and copy it to the host:

   ```
   install -m 600 my-app.private-key.pem /etc/mayfly/app.pem
   ```

2. In `/etc/mayfly/pool.env`, set `APP_ID`, either `ORG=<org>` or `REPO=<owner>/<name>`, and
   `STORAGE` (thin pool), `BRIDGE` and `VLAN` for job guests. All other options have defaults; see
   [pool.env.example](pool.env.example). The file is read as plain `KEY=value` lines, optionally
   quoted, never run as shell: `$VAR`, `$(...)` and other commands are refused, and `mayfly check`
   lists such lines.

3. In `/etc/mayfly/slots.conf`, add one line per runner slot: name, VMID, labels, vCPU, RAM, disk. See
   [slots.conf.example](slots.conf.example).

4. Check the host, build a template and start the slots:

   ```
   mayfly check                  # App, storage and network
   mayfly template build         # Ubuntu 24.04 with runner, Docker, gh; 20 to 40 min
   journalctl -fu mayfly-template-build
   mayfly template promote 9097  # VMID printed by the build
   mayfly enable                 # starts all slots and the gc timer
   mayfly status
   ```

5. Target the slot's labels in a workflow:

   ```yaml
   runs-on: [self-hosted, linux]
   ```

`mayfly status` lists each slot's service, guest, job and the state of its runner on GitHub
(`online`, `busy`, `offline`, `none`). `mayfly help` lists all commands. Logs:
`journalctl -u 'mayfly-slot@*' -f`.

## Job lifecycle

1. The slot requests a single-job (JIT) runner config from GitHub using the App. The App key stays on
   the host.
2. The config is written to a cloud-init snippet (mode 0600). The template is cloned to the slot's
   VMID with the slot's vCPU, RAM (no balloon, no swap), disk, network and firewall settings, and
   started. The snippet is deleted once the VM is running.
3. In the guest, `mayfly-job` reads and deletes the config, runs the job as `runner` (no sudo) and
   powers off.
   If the runner is not online on GitHub within 10 minutes, the slot replaces the guest, runs the
   `vm-stuck` hook and backs off (2 min per repeat, at most 30 min).
4. The slot destroys the VM and its disk, removes the runner registration if it still exists, and
   starts the next clone.

## Files

| File | Installed to | Purpose |
| --- | --- | --- |
| `mayfly` | `/usr/local/sbin` | the command: `version`, `help`, `status`, `check`, `update`, `gc`, `enable`, `disable`, `template`; runs `mayfly.sh` for `slot` |
| `mayfly.sh` | `/usr/local/share/mayfly` | slot loop (VM, pod, LXC); also still has its own `gc`, `enable`, `disable` and `template` |
| `mayfly-slot@.service` | systemd | one instance per slot in `slots.conf` |
| `mayfly-gc.{service,timer}` | systemd | every 15 min: removes stale runner registrations, orphan guests, old snippets |
| `mayfly-update.{service,timer}` | systemd | daily: `mayfly update --auto` (see Updates) |
| `pool.env.example`, `slots.conf.example` | `/etc/mayfly/` | host settings and slots, with all options documented |
| `template/` | template guest | core provisioning (runner, Docker, gh), single-job service, pod script |
| `profiles/<name>/` | template guest | additional toolchains, libraries and images |
| `host/pve-on-debian.sh`, `host/nat-bridge.sh` | new host, once | Proxmox on Debian; NAT bridge for job guests |

## Configuration

`pool.env` holds host settings. `slots.conf` has one line per slot:

```
# name   vmid  labels  vcpu  ram_gb  disk_gb  env      template  [key=value ...]
build-1  9002  linux   8     16      60       sccache  current
small-1  9011  small   2     4       30       no       current   max_job_hours=1
pods-1   9041  small   12    36      120      no       current   kind=pod containers=6
```

- `labels` are appended to `RUNNER_LABELS_BASE` (`self-hosted,Linux,X64`).
- `env` is a comma-separated list of job environments, or `no`. Each is a file
  `/etc/mayfly/env/<name>.env` of `KEY=value` lines (mode 0600), passed to the job through cloud-init
  and deleted from the guest after it is read. Release slots should not use a cache that PR jobs can
  write to.
- `template` is `current`, or `next` for canary slots that use the template under test.
- `key=value` fields override `pool.env` for that slot: `kind=vm|pod|lxc`, `containers=N`,
  `template=<vmid>` (pin a template or use another profile), `bridge=`, `vlan=`, `cpu=`, `disk_cache=`,
  `max_job_hours=`, `max_idle_hours=`.

### Profiles

A template consists of the core (runner, Docker, gh, single-job service) and a profile. A profile is a
directory with a `provision.sh`, run as root in the template guest from `/opt/mayfly-profile`
(readable for `runner`, removed when the template is sealed), and a `template.env` with versions
and `PREPULL_IMAGES`. The shipped `default` profile installs build-essential, pkg-config, OpenSSL
headers and shellcheck. Profiles in `/etc/mayfly/profiles/<name>/` take precedence over shipped profiles
of the same name. `PROFILE` in `pool.env` sets the default; `mayfly template build <name>` builds
another. Slots can use templates of different profiles via `template=<vmid>`.

### Hooks

Executables in `HOOK_DIR` (`/etc/mayfly/hooks.d`) are called with:

- `vm-up <slot> <vmid> <runner>`
- `vm-gone <slot> <vmid> <minutes>`
- `vm-stuck <slot> <vmid> <runner>` (the runner was not online on GitHub within 10 min)
- `pod-up <slot> <vmid> <containers>`
- `pod-gone <slot> <vmid> <minutes>`

Each hook has a 30 s timeout. Failures are logged and otherwise ignored.

### GitHub

`ORG` registers organization runners in `RUNNER_GROUP_ID`; `REPO=owner/name` registers repository
runners. For GitHub Enterprise Server, set `API_URL` to `https://<host>/api/v3`. Runners are named
`<RUNNER_PREFIX>-<slot>-<epoch>`. `mayfly gc` removes only offline registrations with that prefix and
no matching guest.

## Pods

For jobs of one to two minutes, the host CPU spent on `qm`, `pvesh` and LVM calls for a per-job VM
exceeds the job itself. A slot with `kind=pod containers=N` is a long-lived VM, cloned from the same
template, that runs N containers, each for a single job. The slot's vCPU, RAM and disk apply to the
whole pod; each container is limited to `vcpu/N` CPUs and `ram/N` GB.

- Runner configs: every 15 s the host queries the guest agent for idle container loops, creates one JIT
  config per idle loop, and passes each with `qm guest exec ... mayfly-pod put`. The guest stores it in
  `/run/mayfly-pod/spool` (mode 0600); an idle loop takes it and starts a container with it as an
  environment variable. The App key stays on the host.
- Image: at boot, the guest imports its root filesystem (excluding Docker data, logs and keys) as the
  image `mayfly-pod:local`, so containers have the template's tools. This takes a few minutes per pod.
- Containers run as `runner`, unprivileged, with all capabilities dropped, `no-new-privileges`, no
  Docker socket, a separate network namespace without container-to-container traffic, CPU, memory and
  pid limits, and scratch space removed with the container. Jobs that need a Docker daemon or service
  containers cannot run in pods.
- Recycling: the host drains, destroys and reclones a pod when its VM stops, when the guest agent has not
  responded for 5 min, after `POD_MAX_AGE_HOURS` (24, plus 0 to 3.5 h offset by VMID), and when its
  template changes. Draining lets running jobs finish within `MAX_JOB_HOURS`; `systemctl stop` drains
  as well.
- A pod that is not ready within 30 min is replaced; each repeat waits longer before the next clone
  (2 min per repeat, at most 30 min).

To move back from pods to VMs, restore the VM slots in `slots.conf`, `mayfly enable` them and
`mayfly disable` the pod slots.

## LXC

A slot with `kind=lxc` behaves like a VM slot but clones an unprivileged LXC container template: one
container per job, destroyed afterwards. Containers start in about a second and cost the host less than
a VM boot. Docker works inside.

```
ct-1  9051  small  2  4  20  no  current  kind=lxc
```

- Template: `mayfly template build --lxc [profile]` creates a container from the newest `LXC_OSTEMPLATE`
  image (`ubuntu-24.04-standard`, downloaded with `pveam`), unprivileged with `nesting` and `keyctl`
  enabled, and provisions it with the same core and profile as a VM template. `canary` and `promote`
  detect container templates and set `NEXT_LXC_TEMPLATE_VMID` / `LXC_TEMPLATE_VMID`. VM and container
  templates share `TEMPLATE_VMIDS`; allocate five VMIDs when using both.
- Runner config: containers have no cloud-init. After boot, the host writes the JIT config and job
  environment into the container via `lxc-attach` stdin (not to the host disk or a command line) and
  starts `mayfly-job`. The App key stays on the host.
- Limits: `vcpu` and `ram_gb` set cores and memory (no swap); `disk_gb` sets the rootfs size, grown only
  if larger than the template's. The per-guest firewall applies. `cpu=` and `disk_cache=` are ignored.
- Isolation: containers share the host kernel. An unprivileged container with user namespaces is a
  weaker boundary than a VM; a kernel vulnerability reachable from inside affects the host. Use LXC
  slots for trusted code (for example, private repositories of your own organization) and VM slots for
  untrusted code such as pull requests from forks.
- Storage: linked clones require LVM-thin, ZFS or Ceph. Docker overlay2 in a container on ZFS requires
  ZFS 2.2 or later.

## Safety limits

- Mayfly stops or destroys only guests that have the tag `TAG` (`mayfly`), a name starting with
  `NAME_PREFIX` (`ci-`) and a VMID in `VMID_MIN..VMID_MAX`. During a migration from another tool,
  guests with a tag in `LEGACY_TAGS` and a `ci-` name are included. No other guests are touched.
- No new job guest is created while the thin pool is above `MAX_POOL_PERCENT` (70) or while it would
  leave less than `MIN_FREE_RAM_GB` (8) for the host. The slot logs the reason.
- A job running longer than `MAX_JOB_HOURS` (3) is stopped and its guest destroyed. All job guests are
  destroyed while the pool is above `POOL_PANIC_PERCENT` (85). A guest without a job for
  `MAX_IDLE_HOURS` (2) is replaced.
- Per-guest firewall (`FIREWALL=yes`): no inbound traffic; outbound only DNS to `GATEWAY`, `CACHE_IP`,
  `ALLOW_OUT`, and destinations outside `BLOCK_NETS` (private ranges, Tailscale, link-local). Job guests
  reach the internet but not each other or the LAN. This requires the cluster firewall to be enabled
  (`mayfly check` reports it). Before enabling the cluster firewall, allow SSH and port 8006 from your
  admin network: its default input policy drops all other traffic to the host.

## Templates

`mayfly template build [profile]` builds into a free template VMID (`--lxc` for a container template).
The build runs in the systemd unit `mayfly-template-build`, so it survives the SSH session; one build
runs at a time. `--foreground` runs it in the terminal instead.
`mayfly template canary <vmid>` points slots marked `next` at it. `mayfly template promote <vmid>`
switches all slots at their next job. The previous template is kept for rollback with
`mayfly template promote <old>`.

Templates built by polara-ci (no `profile` description) expect the job files in `/etc/polara-ci`.
Mayfly writes them there for such templates, and `mayfly check`, `canary` and `promote` warn about them.

The template disk size is `TEMPLATE_DISK` GB. Set it to the largest slot disk: clones keep the template
size and are resized only when `disk_gb` is larger, because growing a disk makes ext4 in the guest
zero-fill new inode tables (about 32 MB per GB added). Unwritten space in a thin pool is not allocated.

## Hosts behind NAT

For hosts without a VLAN for job guests: `host/pve-on-debian.sh` installs Proxmox on Debian 13 (two
stages, each followed by a reboot). `host/nat-bridge.sh` adds `vmbr1` (10.66.0.0/24, or set `BRIDGE=`
and `NET=`) with dnsmasq, forwarding and nftables isolation (`mayfly-nat.service`). Then set
`BRIDGE=vmbr1`, `VLAN=` and `GATEWAY=10.66.0.1` in `pool.env`. Use bare metal: on nested Proxmox, fsync
latency breaks file-lock and timing tests, even with `DISK_CACHE=unsafe`.

## Verifying a setup

- Runners register with the base labels plus the slot's labels and are removed after their job.
- `ACTIONS_RUNNER_INPUT_JITCONFIG` is not present in a job's environment.
- `mayfly status` and thin pool usage stay stable over a day of jobs.

## Updates

`mayfly update` installs the latest release: it downloads `mayfly-<version>.tar.gz` and `SHA256SUMS`
from the GitHub release (no token), checks the checksum, runs the new `mayfly version`, saves the
installed files in `/usr/local/share/mayfly/previous/` and runs the new `mayfly install --from <release>`. Running slots
are not restarted; they use the new command from their next job. Then it runs the new `mayfly check`: if
the old one passed and the new one fails, everything is restored, the version is skipped and the exit code is 1.

```
mayfly update                    # latest release
mayfly update --check            # only say whether a newer release exists
mayfly update --version 0.3.1    # that release (also an older one, or one that was skipped)
mayfly update --rollback         # restore previous/ (the version before the last update)
```

`mayfly-update.timer` runs `mayfly update --auto` daily, up to an hour late. The first install that has
the timer switches it on. `UPDATE` in `pool.env` sets what `--auto` does: `auto` (default) takes any newer
release, `patch` only a newer patch of the installed `major.minor`, `off` nothing (also keeps the timer
from being switched on). `mayfly update` by hand ignores `UPDATE`. `journalctl -u mayfly-update` has the log.

`--auto` never installs a release whose notes have a `BREAKING CHANGES` section, nor one with such a
release between it and the installed version; it logs that, records `last_result=breaking` and leaves the
update to `mayfly update` by hand (`--check` says so too). It also waits when the release list cannot be read.

`/var/lib/mayfly/update.state` holds `last_attempt`, `last_result` (`ok`, `up-to-date`, `rolled-back`,
`failed`, `breaking`) and `skipped=<version>`. A skipped version is not installed again by the timer or by a plain
`mayfly update`; `mayfly update --version <version>` forces it. `mayfly update --rollback` skips the
version it removed. `mayfly status` ends with the installed version and the last attempt.

Tests and other installs can move the paths: `MAYFLY_SBIN`, `MAYFLY_SHARE`, `MAYFLY_SYSTEMD_DIR`,
`MAYFLY_CONF_DIR`, `MAYFLY_STATE_DIR`, `MAYFLY_TMP` (default `/var/tmp`), `MAYFLY_RELEASE_API` and `MAYFLY_REPO_URL`.

## Releases

Releases are managed by [release-please](https://github.com/googleapis/release-please). Merging the
release PR tags the version and attaches `mayfly-<version>.tar.gz`, the binary on its own as
`mayfly-<version>-x86_64-linux`, a version-pinned `install.sh` and `SHA256SUMS`. To install a specific version:

```
curl -fsSL https://github.com/variiuz/mayfly/releases/download/v0.1.0/install.sh | bash
```

To install from a checkout (the binary is built with `cargo` when the host has it, otherwise the released
binary of the checkout's version is downloaded and checked against `SHA256SUMS`):
`scp -r mayfly root@HOST:/opt/mayfly-src && ssh root@HOST bash /opt/mayfly-src/install.sh`.
`install.sh` only finds and checks the files; `mayfly install [--no-timer] [--from <dir>]` does the rest
(`<dir>` is a release or checkout directory with `mayfly.sh`; by default the directory of the binary).

Tests run without Proxmox: `cargo test` and `bash tests/run.sh`. `bash tests/parity.sh` checks that the
binary and `mayfly.sh` print the same for `version`, `help`, `status`, `check`, `gc`, `enable` and `disable`
(for the last three it also compares the commands the stubs recorded and the files left behind). It also
runs the 0.4.1 `install.sh` (`tests/fixtures/install-0.4.1.sh`) and `mayfly install` against the same stub
root and compares what they print, the calls and the resulting files.

## License

[MIT](LICENSE)
