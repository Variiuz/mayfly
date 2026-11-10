#!/usr/bin/env bash
# Runs once at boot of a job VM (mayfly-job.service): takes the one-use JIT config that
# cloud-init wrote, runs exactly one job as the unprivileged runner user, then powers the VM off.
# The host destroys the VM when it is off, whatever the job did.
set -uo pipefail

jit_file=/etc/mayfly/jit
env_file=/etc/mayfly/job.env
runner_dir=/opt/actions-runner

finish() { sync; systemctl poweroff; }
trap finish EXIT

if [ ! -s "$jit_file" ]; then
  echo "mayfly-job: no JIT config; nothing to run" >&2
  exit 1
fi
jit=$(cat "$jit_file")
rm -f "$jit_file"

# The slot's job environment (slots.conf env column) is appended to the runner's .env.
if [ -s "$env_file" ]; then
  cat "$env_file" >>"$runner_dir/.env"
  rm -f "$env_file"
fi

# Wait for Docker: workflows start service containers straight away.
for _ in $(seq 60); do docker info >/dev/null 2>&1 && break; sleep 1; done

cd "$runner_dir" || exit 1
runuser -u runner -- ./run.sh --jitconfig "$jit"
echo "mayfly-job: runner exited with $?"
