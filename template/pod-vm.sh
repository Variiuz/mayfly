#!/usr/bin/env bash
# mayfly-pod: runs inside a pod VM (cloud-init puts it in /usr/local/sbin; `mayfly slot` of a kind=pod
# slot drives it through the guest agent). The VM is a clone of the CI template; its containers are
# built from the VM's own filesystem, so they carry exactly the template's toolchains.
#   image        build the container image, start one `loop` per container
#   loop <n>     container n: take a JIT config from the spool, run ONE job in a fresh container, repeat
#   want|put|busy|drain|ready   called by the host: loops waiting / store a config / running jobs / stop / up?
set -uo pipefail
. /etc/mayfly/pod.env   # CONTAINERS CPUS MEMORY MAX_JOB_SECS MAX_IDLE_SECS
D=/run/mayfly-pod
IMAGE=mayfly-pod:local

image() {
  install -d -m 0700 "$D/spool"
  # Containers cannot reach each other (no icc); each has its own network namespace behind the VM.
  docker network create -o com.docker.network.bridge.enable_icc=false mayfly-pod >/dev/null 2>&1 || true
  # The whole root filesystem minus Docker's own data, logs and secrets, as one image (about 15 GB).
  { tar -C / --one-file-system -cf - \
      --exclude='./var/lib/docker/*' --exclude='./var/lib/containerd/*' --exclude='./var/lib/cloud/*' \
      --exclude='./var/log/*' --exclude='./var/cache/*' --exclude='./tmp/*' --exclude='./var/tmp/*' \
      --exclude='./root/*' --exclude='./etc/mayfly/*' --exclude='./etc/ssh/ssh_host_*' . || [ $? = 1 ]; } |
    docker import --change 'USER runner' --change 'WORKDIR /opt/actions-runner' - "$IMAGE" >/dev/null || exit 1
  cat >/etc/systemd/system/mayfly-pod@.service <<'UNIT'
[Unit]
Description=mayfly-pod container loop %i
After=docker.service
[Service]
ExecStart=/usr/local/sbin/mayfly-pod loop %i
Restart=on-failure
RestartSec=5
UNIT
  systemctl daemon-reload
  for i in $(seq "$CONTAINERS"); do systemctl start --no-block "mayfly-pod@$i"; done
  touch "$D/ready"
}

loop() {
  local n=$1 cn=pod-$1 f jit t0 secs rc envf=()
  [ ! -s /etc/mayfly/job.env ] || envf=(--env-file /etc/mayfly/job.env)
  docker rm -f "$cn" >/dev/null 2>&1
  while [ ! -e "$D/drain" ]; do
    f=$(ls "$D/spool" 2>/dev/null | head -n1)
    if [ -z "$f" ] || ! mv "$D/spool/$f" "$D/claim.$n" 2>/dev/null; then
      touch "$D/wait.$n"; sleep 2; continue
    fi
    rm -f "$D/wait.$n"
    jit=$(<"$D/claim.$n"); rm -f "$D/claim.$n"
    logger -t mayfly-pod "container $n runs $f"
    # Unprivileged (no --privileged, no Docker socket), capped CPU/RAM/pids, scratch on the overlay layer
    # that --rm deletes with the container. The JIT config goes in by environment name, never on a command line.
    MAYFLY_JIT=$jit docker run -d --rm --name "$cn" --label mayfly-pod --network mayfly-pod \
      --cpus "$CPUS" --memory "$MEMORY" --memory-swap "$MEMORY" --pids-limit 4096 --shm-size 2g \
      --cap-drop ALL --security-opt no-new-privileges "${envf[@]}" -e MAYFLY_JIT \
      "$IMAGE" sh -c 'j=$MAYFLY_JIT; unset MAYFLY_JIT; exec ./run.sh --jitconfig "$j"' >/dev/null || sleep 10
    unset jit
    t0=$(date +%s)
    while [ -n "$(docker ps -q -f "name=^$cn\$")" ]; do
      # Seconds the job has run (empty and rc 1 = no job yet). Idle too long, or draining: remove it.
      secs=$(docker exec "$cn" sh -c 'p=$(pgrep -of "bin/Runner\.Worker") && ps -o etimes= -p "$p"' 2>/dev/null); rc=$?
      secs=${secs//[!0-9]/}
      if [ -n "$secs" ] && [ "$secs" -ge "$MAX_JOB_SECS" ]; then
        logger -t mayfly-pod "container $n: job ran $secs s; killing it"; docker kill "$cn" >/dev/null 2>&1
      elif [ "$rc" = 1 ] && { [ -e "$D/drain" ] || [ $(($(date +%s) - t0)) -ge "$MAX_IDLE_SECS" ]; }; then
        docker kill "$cn" >/dev/null 2>&1
      fi
      sleep 20
    done
  done
  rm -f "$D/wait.$n"
}

want() { local w s; w=$(find "$D" -maxdepth 1 -name 'wait.*' | wc -l); s=$(find "$D/spool" -mindepth 1 -maxdepth 1 -name '[!.]*' | wc -l); echo $((w > s ? w - s : 0)); }
put() { umask 077; printf '%s' "$2" >"$D/spool/.$1" && mv "$D/spool/.$1" "$D/spool/$1"; }
# Containers with a job running. A docker exec that fails for another reason than "no such process" counts as busy.
busy() {
  local c k=0
  for c in $(docker ps -q -f label=mayfly-pod); do
    docker exec "$c" pgrep -f 'bin/Runner\.Worker' >/dev/null 2>&1; [ $? = 1 ] || k=$((k + 1))
  done
  echo "$k"
}

case "${1:-}" in
  image) image ;;
  loop) loop "$2" ;;
  want) want ;;
  put) put "$2" "$3" ;;
  busy) busy ;;
  drain) touch "$D/drain" ;;
  ready) [ -e "$D/ready" ] ;;
  *) echo "usage: mayfly-pod image|loop|want|put|busy|drain|ready" >&2; exit 2 ;;
esac
