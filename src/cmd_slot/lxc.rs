//! `kind=lxc`: one throwaway container per job. The loop is the one of the VM slots (`Loop::iteration`);
//! this is what differs, as in the LXC section of `mayfly.sh`: a linked clone of the container template,
//! sized with `pct`, and the runner config handed in over `lxc-attach` stdin (a container has no
//! cloud-init) so that it never touches the host's disk or a command line.

use super::{Applied, Loop};
use crate::config::Config;

impl Loop<'_> {
    /// bash `ct_create`: a linked clone of the container template, sized for the slot.
    pub(super) fn ct_create(
        &self,
        c: &Config,
        a: &Applied,
        tpl: &str,
        host: &str,
        desc: &str,
    ) -> bool {
        let s = &self.slot;
        let pve = self.cx.pve;
        let id = s.vmid.as_str();
        if pve.run_stderr("pct", &["clone", tpl, id, "--hostname", host]) != 0 {
            return false;
        }
        let Ok(ram) = s.ram.trim().parse::<i64>() else {
            return false;
        };
        let memory = (ram * 1024).to_string();
        let set = [
            "set",
            id,
            "--cores",
            s.vcpu.as_str(),
            "--memory",
            memory.as_str(),
            "--swap",
            "0",
            "--onboot",
            "0",
            "--tags",
            c.get("TAG"),
            "--description",
            desc,
            "--net0",
            a.ct_net0.as_str(),
        ];
        pve.run_stderr("pct", &set) == 0 && self.ct_grow(id, &s.disk)
    }

    /// bash `ct_grow`: the rootfs grows to `want` GB only when it is smaller.
    fn ct_grow(&self, id: &str, want: &str) -> bool {
        if let (Some(have), Ok(w)) = (
            self.size_gb("pct", "rootfs:", id),
            want.trim().parse::<i64>(),
        ) {
            if w <= have {
                return true;
            }
        }
        self.cx
            .pve
            .run_stderr("pct", &["resize", id, "rootfs", &format!("{want}G")])
            == 0
    }

    /// bash `ct_deliver`: waits for the container to boot (up to 60 looks, 2 s apart), then the JIT config
    /// and the job environment go in over `lxc-attach` stdin and the job service starts.
    pub(super) fn ct_deliver(&mut self, id: &str, jit: &str) -> bool {
        let spec = self.slot.envspec.clone();
        // `lines=$(job_env ...)`: a bad env name only ends that subshell, so the delivery goes on without.
        let lines = match self.job_env(&spec) {
            Ok(l) => l,
            Err(d) => {
                let _ = writeln!(self.ew, "error: {}", d.0);
                String::new()
            }
        };
        let pve = self.cx.pve;
        for i in 1..=60 {
            let st = pve.run(
                "timeout",
                &[
                    "10",
                    "lxc-attach",
                    "-n",
                    id,
                    "--",
                    "systemctl",
                    "is-system-running",
                ],
            );
            if matches!(st.stdout.trim_end_matches('\n'), "running" | "degraded") {
                break;
            }
            if i < 60 {
                self.poll();
                self.nap(2);
            }
        }
        let attach =
            |script: &'static str| ["30", "lxc-attach", "-n", id, "--", "sh", "-c", script];
        let a = attach("umask 077; mkdir -p /etc/mayfly; cat >/etc/mayfly/jit");
        if pve.run_stdin("timeout", &a, jit) != 0 {
            return false;
        }
        if !lines.is_empty() {
            let a = attach("umask 077; cat >/etc/mayfly/job.env");
            if pve.run_stdin("timeout", &a, &format!("{lines}\n")) != 0 {
                return false;
            }
        }
        pve.run_stderr(
            "timeout",
            &[
                "30",
                "lxc-attach",
                "-n",
                id,
                "--",
                "systemctl",
                "start",
                "--no-block",
                "mayfly-job.service",
            ],
        ) == 0
    }
}
