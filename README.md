# Mayfly

Throwaway GitHub Actions runners on Proxmox: one fresh VM (or LXC container) per job, then gone.

Every job runs in its own guest, a linked clone of a template. It takes exactly one job and is destroyed
afterwards, success or failure. Nothing a job leaves behind (files, Docker images, caches, a broken
toolchain, memory pressure) reaches the next one. Short jobs can share a long-lived pod VM instead, one
throwaway container per job.

Mayfly is a Bash script and a few systemd units on the Proxmox host. It needs a GitHub App, a thin pool
(LVM-thin, ZFS or Ceph) and a bridge. Nothing runs anywhere else.

## Install

On the Proxmox host, as root:

```
curl -fsSL https://github.com/variiuz/mayfly/releases/latest/download/install.sh | bash
```

It downloads the latest release, checks it against `SHA256SUMS`, installs `mayfly` and its units and
creates `/etc/mayfly`. It starts nothing and touches no VM. Run it again to update: config, slots and
running jobs stay as they are.

## Get started

1. **GitHub App.** Create one (organization or user settings, Developer settings, GitHub Apps): no
   webhook, permission **Self-hosted runners: read and write** for organization runners, or
   **Administration: read and write** for the runners of one repository. Install it on the org or the
   repo, generate a private key and copy it to the host:

   ```
   install -m 600 my-app.private-key.pem /etc/mayfly/app.pem
   ```

2. **`/etc/mayfly/pool.env`.** Set `APP_ID`, `ORG=<org>` or `REPO=<owner>/<name>`, and where job VMs
   live: `STORAGE` (a thin pool), `BRIDGE` and `VLAN`. Everything else has a default; all options are
   in [pool.env.example](pool.env.example).

3. **`/etc/mayfly/slots.conf`.** One line per runner slot: name, VMID, labels, vCPU, RAM, disk.
   The [example](slots.conf.example) starts two build slots and one small one.

4. **Check and build.**

   ```
   mayfly check                  # App, storage, network: ends with "All good." or what to fix
   mayfly template build         # Ubuntu 24.04 with runner, Docker, gh (20 to 40 min)
   mayfly template promote 9097  # the VMID the build printed
   mayfly enable                 # starts every slot and the gc timer
   mayfly status
   ```

5. **Use it** in a workflow, by the slot's labels:

   ```yaml
   runs-on: [self-hosted, linux]
   ```

`mayfly help` lists every command. Logs: `journalctl -u 'mayfly-slot@*' -f`.

## How a job runs

1. The slot asks GitHub for a one-job (JIT) runner config with the App. The App key never leaves the host.
2. It writes the config into a cloud-init snippet (0600), clones the template into the slot's VMID,
   sets vCPU, RAM (no balloon, no swap), disk, network and the per-VM firewall, and starts it. The
   snippet is deleted as soon as the VM runs.
3. Inside, `mayfly-job` reads the config, deletes it, runs the job as `runner` (no sudo) and powers off.
4. The slot destroys the VM with its disk, removes the runner registration if it is still there, and
   starts the next clone.

## Files

| File | Where it goes | What it does |
| --- | --- | --- |
| `mayfly` | `/usr/local/sbin` | slot loop (VM, pod or LXC), status, check, gc, templates, enable/disable |
| `mayfly-slot@.service` | systemd | one instance per slot in `slots.conf` |
| `mayfly-gc.{service,timer}` | systemd | every 15 min: stale runner registrations, orphan guests, old snippets |
| `pool.env.example`, `slots.conf.example` | `/etc/mayfly/` | host settings and slots, every option explained |
| `template/` | into the template | core provisioning (runner, Docker, gh), the one-job service, the pod script |
| `profiles/<name>/` | into the template | what your workflows need on top: toolchains, libraries, images |
| `host/pve-on-debian.sh`, `host/nat-bridge.sh` | once, on a new host | Proxmox on Debian, a NAT bridge for job VMs |

## Configuration

`pool.env` holds the host's settings, `slots.conf` one line per slot:

```
# name   vmid  labels  vcpu  ram_gb  disk_gb  env      template  [key=value ...]
build-1  9002  linux   8     16      60       sccache  current
small-1  9011  small   2     4       30       no       current   max_job_hours=1
pods-1   9041  small   12    36      120      no       current   kind=pod containers=6
```

- **Labels** are added to `RUNNER_LABELS_BASE` (`self-hosted,Linux,X64`).
- **env** names job environments: `/etc/mayfly/env/<name>.env` files of `KEY=value` lines (0600),
  comma-separated, or `no`. They reach the job's environment through cloud-init and are deleted from
  the VM once read. Keep release slots off any cache a PR job can write to.
- **template** is `current`, or `next` for canary slots that clone the template under test.
- **key=value** fields override pool.env for one slot: `kind=vm|pod|lxc`, `containers=N`, `template=<vmid>`
  (pin a slot, or use a second profile), `bridge=`, `vlan=`, `cpu=`, `disk_cache=`, `max_job_hours=`,
  `max_idle_hours=`.

### Profiles

A template is the core (runner, Docker, gh, the one-job service) plus a profile: a folder with a
`provision.sh` that runs as root inside the template VM and a `template.env` with its versions and
`PREPULL_IMAGES`. Mayfly ships `default` (build-essential, pkg-config, OpenSSL headers, shellcheck).
Your own go in `/etc/mayfly/profiles/<name>/` (copy `default` to start) and win over shipped ones of the
same name. `PROFILE` in pool.env picks one, `mayfly template build <name>` another.
A host can keep templates of two profiles and point slots at them with `template=<vmid>`.

### Hooks

Executables in `HOOK_DIR` (`/etc/mayfly/hooks.d`) run on `vm-up <slot> <vmid> <runner>`,
`vm-gone <slot> <vmid> <minutes>`, `pod-up <slot> <vmid> <containers>` and
`pod-gone <slot> <vmid> <minutes>`: metrics, chat messages, whatever. 30 s each; a failing hook is
logged and ignored.

### GitHub

`ORG` registers organization runners (in `RUNNER_GROUP_ID`), `REPO=owner/name` runners of one
repository. `API_URL` points at GitHub Enterprise Server (`https://<host>/api/v3`). Runner names are
`<RUNNER_PREFIX>-<slot>-<epoch>`; `mayfly gc` only removes offline registrations with that prefix and
no VM.

## Pods: short jobs in containers

A VM per job costs more host CPU in `qm`/`pvesh`/LVM calls than 1 to 2 minute jobs cost to run. A slot
with `kind=pod containers=N` is one long-lived VM, a linked clone of the same template, that runs N
containers, each for ONE job. vcpu, RAM and disk are the whole pod; each container gets `vcpu/N` CPUs
and `ram/N` GB as hard limits.

- **Runner configs.** The App key never leaves the host. Every 15 s the host asks the VM (guest agent)
  how many container loops wait, makes that many one-use JIT configs and hands each over with one
  `qm guest exec ... mayfly-pod put`. The VM stores it 0600 in `/run/mayfly-pod/spool`; an idle loop
  takes it and starts its container with it as an environment variable.
- **Image.** At boot the VM imports its own root filesystem (minus Docker data, logs, keys) as the
  image `mayfly-pod:local`, so containers carry exactly the template's tools. A few minutes, once per pod.
- **Container.** Runs as `runner`, not privileged, all capabilities dropped, `no-new-privileges`, no
  Docker socket, own network namespace without container-to-container traffic, CPU/RAM/pids caps,
  scratch deleted with the container. Jobs that need a Docker daemon or service containers cannot run here.
- **Health.** The host recycles a pod (drain, destroy, reclone) when its VM stops, when the guest has not
  answered for 5 min, after `POD_MAX_AGE_HOURS` (24, plus 0 to 3.5 h by VMID) and when its template
  changes. Draining lets running jobs finish (`MAX_JOB_HOURS`); `systemctl stop` does the same.

Rollback from pods: put VM slots back in `slots.conf`, `mayfly enable` them, `mayfly disable` the pod slots.

## LXC: a fresh container per job

A slot with `kind=lxc` works like a VM slot, but clones an unprivileged LXC container template instead
of a VM: one container per job, destroyed after it. Containers start in about a second and cost the host
far less than a VM boot, so they suit many short jobs where pods don't fit (Docker works inside).

```
ct-1  9051  small  2  4  20  no  current  kind=lxc
```

- **Template.** `mayfly template build --lxc [profile]` creates a container from the newest
  `LXC_OSTEMPLATE` image (`ubuntu-24.04-standard`, downloaded with `pveam`), unprivileged with `nesting`
  and `keyctl` on so Docker runs, and provisions it with the same core and profile as a VM template.
  `canary` and `promote` see that it is a container and set `NEXT_LXC_TEMPLATE_VMID`/`LXC_TEMPLATE_VMID`.
  VM and container templates share `TEMPLATE_VMIDS`; give it five VMIDs when both are in use.
- **Runner config.** Containers have no cloud-init. Once the container has booted, the host writes the
  JIT config and the job environment into it over `lxc-attach`'s stdin (never onto the host's disk or a
  command line) and starts `mayfly-job`. The App key stays on the host.
- **Limits.** `vcpu` and `ram_gb` become the container's cores and memory (no swap), `disk_gb` its
  rootfs (grown only when bigger than the template's). The per-VM firewall applies as for a VM. `cpu=`
  and `disk_cache=` don't apply.
- **Isolation.** A container shares the host's kernel. Unprivileged with user namespaces is a real
  boundary, but a weaker one than a VM: a kernel bug reachable from inside reaches the host. Use LXC
  slots for repositories whose code you trust (your own org's private repos), and VM slots for anything
  that runs code from outside, such as pull requests from forks.
- **Storage.** Linked clones need LVM-thin, ZFS or Ceph. Docker's overlay2 in a container on ZFS needs
  ZFS 2.2 or newer.

## Safety rails

- Only VMs and containers with tag `TAG` (`mayfly`), a name starting with `NAME_PREFIX` (`ci-`) and a VMID in
  `VMID_MIN..VMID_MAX` are ever stopped or destroyed (and, while moving from another tool, guests with
  one of `LEGACY_TAGS` and a `ci-` name).
  Other VMs on the host are never touched.
- No new job VM while the thin pool is over `MAX_POOL_PERCENT` (70) or while it would leave less than
  `MIN_FREE_RAM_GB` (8) for the rest of the host. The slot logs why it waits.
- A job that has run `MAX_JOB_HOURS` (3) is stopped and its VM destroyed, and so is every job VM while
  the pool is over `POOL_PANIC_PERCENT` (85). A VM that got no job in `MAX_IDLE_HOURS` (2) is replaced.
- Per-VM firewall (`FIREWALL=yes`): nothing in; out only DNS to `GATEWAY`, `CACHE_IP`, `ALLOW_OUT` and
  anything outside `BLOCK_NETS` (private ranges, Tailscale, link-local), so job VMs reach the internet
  but not each other or the LAN. It only acts with the cluster firewall on (`mayfly check` says). Do not
  switch that on without first allowing SSH and 8006 from your admin network: its default input policy
  drops everything else to the host.

## New template

`mayfly template build [profile]` builds into a free template VMID (`--lxc`: a container template). `mayfly template canary <vmid>`
points the slots marked `next` at it. When their jobs pass, `mayfly template promote <vmid>` moves every
slot over at its next job. The previous template stays as the rollback: `mayfly template promote <old>`.

The template disk is built at `TEMPLATE_DISK` GB. Set it to the largest slot disk: clones keep that size
and are only resized when a slot's `disk_gb` is bigger, because growing a disk makes ext4 in the guest
zero-fill new inode tables (about 32 MB per GB added). The pool is thin, so unwritten space costs nothing.

## A host behind NAT

For a host without a VLAN for job VMs: `host/pve-on-debian.sh` turns a Debian 13 machine into a
Proxmox host (two stages, a reboot after each), `host/nat-bridge.sh` adds `vmbr1` (10.66.0.0/24, or
`BRIDGE=` and `NET=` of your choice) with dnsmasq, forwarding and nftables isolation
(`mayfly-nat.service`). Then `BRIDGE=vmbr1`, `VLAN=` and `GATEWAY=10.66.0.1` in pool.env. Use bare metal: a nested Proxmox host makes every fsync slow enough
to break file-lock and timing tests, even with `DISK_CACHE=unsafe`.

## Checks after setup

- Runners show up with the base labels plus the slot's, and vanish after their job.
- A job's `env` shows no `ACTIONS_RUNNER_INPUT_JITCONFIG`.
- `mayfly status` and the host's thin pool stay flat over a day of jobs.

## Releases

Releases come from [release-please](https://github.com/googleapis/release-please): merged
conventional commits collect in a release PR, and merging it tags the version and attaches
`mayfly-<version>.tar.gz`, the pinned `install.sh` and `SHA256SUMS`. A specific version:

```
curl -fsSL https://github.com/variiuz/mayfly/releases/download/v0.1.0/install.sh | bash
```

From a checkout instead: `scp -r mayfly root@HOST:/opt/mayfly-src && ssh root@HOST bash /opt/mayfly-src/install.sh`.
Tests need no Proxmox: `bash tests/run.sh`.

## License

[MIT](LICENSE)
