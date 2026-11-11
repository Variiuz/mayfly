//! `kind=pod`: one long-lived VM (a clone of the template) that runs `containers=N` throwaway containers,
//! one job each, from an image the VM builds from its own filesystem (`template/pod-vm.sh`). The App key
//! stays here: the host makes the one-use JIT configs and hands each to the VM (`mayfly-pod put`), which
//! feeds an idle container. The VM is recycled after `POD_MAX_AGE_HOURS` (staggered by VMID) or when its
//! template stops being the slot's template, after running jobs finish. Step for step the `cmd_pod` of the
//! bash loop (0.9.0 and earlier).

use super::{Applied, Loop};
use crate::cmd_template::base64;
use crate::config::Config;
use crate::out::{die, R};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

/// The last `NAME=` line of pool.env as the VM-recycle check reads it: `sed -n 's/^NAME=//p' | tail -n1`.
fn raw_setting(pool_env: &str, name: &str) -> String {
    let text = fs::read(pool_env)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let prefix = format!("{name}=");
    text.split('\n')
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .next_back()
        .unwrap_or("")
        .to_string()
}

impl Loop<'_> {
    /// bash `pod_exec`: runs `mayfly-pod <args>` in the pod VM through the guest agent. Its output, or
    /// None when the agent did not answer in time or the command failed.
    fn pod_exec(&self, args: &[&str]) -> Option<String> {
        let id = self.slot.vmid.as_str();
        let mut argv = vec![
            "40",
            "qm",
            "guest",
            "exec",
            id,
            "--timeout",
            "30",
            "--",
            "/usr/local/sbin/mayfly-pod",
        ];
        argv.extend_from_slice(args);
        let r = self.cx.pve.run("timeout", &argv);
        if r.code != 0 {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(&r.stdout).ok()?;
        let ok = match v.get("exitcode") {
            Some(serde_json::Value::Number(n)) => n.as_f64() == Some(0.0),
            Some(serde_json::Value::String(s)) => s == "0",
            _ => false,
        };
        if !ok {
            return None;
        }
        let out = match v.get("out-data") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        Some(out.trim_end_matches('\n').to_string())
    }

    /// cloud-init user-data of a pod: the in-VM script, its limits and the slot's job environment. 0600, and
    /// gone once the VM has started.
    fn write_pod_snippet(&mut self, c: &Config, host: &str, n: i64) -> R<()> {
        let id = self.slot.vmid.clone();
        let lines = self.job_env(&self.slot.envspec.clone())?;
        let ram: i64 = self.slot.ram.trim().parse().unwrap_or(0);
        let vcpu: f64 = self.slot.vcpu.trim().parse().unwrap_or(0.0);
        let script = format!("{}/pod-vm.sh", self.cx.paths.share);
        let encoded = match fs::read(&script) {
            Ok(b) => base64(&b),
            Err(_) => {
                let _ = writeln!(self.ew, "base64: {script}: No such file or directory");
                String::new()
            }
        };
        let job_h = self.hours(c, "MAX_JOB_HOURS")?;
        let idle_h = self.hours(c, "MAX_IDLE_HOURS")?;
        let mut t = String::new();
        t += "#cloud-config\n";
        t += &format!("hostname: {host}\n");
        t += "write_files:\n";
        t += "  - path: /usr/local/sbin/mayfly-pod\n";
        t += "    permissions: '0755'\n";
        t += "    encoding: b64\n";
        t += &format!("    content: {encoded}\n");
        t += "  - path: /etc/mayfly/pod.env\n";
        t += "    permissions: '0600'\n";
        t += "    content: |\n";
        t += &format!("      CONTAINERS={n}\n");
        t += &format!("      CPUS={:.2}\n", vcpu / n as f64);
        t += &format!("      MEMORY={}g\n", ram / n);
        t += &format!("      MAX_JOB_SECS={}\n", job_h * 3600);
        t += &format!("      MAX_IDLE_SECS={}\n", idle_h * 3600);
        if !lines.is_empty() {
            t += "  - path: /etc/mayfly/job.env\n";
            t += "    permissions: '0600'\n";
            t += "    content: |\n";
            for l in lines.split('\n') {
                t += &format!("      {l}\n");
            }
        }
        t += "runcmd:\n";
        t += "  - [bash, -c, '/usr/local/sbin/mayfly-pod image > /var/log/mayfly-pod-image.log 2>&1']\n";
        let f = format!("{}/mayfly-{id}.yaml", c.snippet_dir);
        let written = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&f)
            .and_then(|mut h| h.write_all(t.as_bytes()));
        match written {
            Ok(()) => Ok(()),
            Err(e) => die(format!("cannot write {f}: {e}")),
        }
    }

    /// bash `cmd_pod`.
    pub(super) fn pod_loop(&mut self, c: &mut Config) -> R<i32> {
        let name = self.name.clone();
        let vmid = self.slot.vmid.clone();
        let labels = self.slot.labels.clone();
        let n_text = crate::config::slot_opt("containers", &self.slot.extra, "4");
        let n = match (n_text.parse::<i64>(), self.slot.ram.trim().parse::<i64>()) {
            (Ok(n), Ok(ram))
                if n_text.bytes().all(|b| b.is_ascii_digit()) && n >= 1 && n <= ram =>
            {
                n
            }
            _ => {
                return die(format!(
                    "slot {name}: containers={n_text} must be a number from 1 to ram_gb"
                ))
            }
        };
        let vmid_num: i64 = vmid.trim().parse().unwrap_or(0);
        let mut unready: i64 = 0;
        while !self.stopping() {
            let code = self.destroy(c, &vmid)?;
            if code != 0 {
                return Ok(code);
            }
            if self.renew("pod") {
                return Ok(super::RENEW);
            }
            *c = self.load()?;
            let applied = self.apply_slot(c)?;
            let tpl = self.slot_template(c, &applied.template);
            if tpl.is_empty() || !crate::guest::is_template(self.cx, &tpl) {
                let shown = if tpl.is_empty() {
                    String::new()
                } else {
                    format!("{tpl} ")
                };
                self.log(&format!(
                    "pod {name}: no template {shown}(TEMPLATE_VMID in {} or template= in {}); run mayfly template build",
                    self.cx.paths.pool_env, self.cx.paths.slots_conf
                ));
                self.nap(300);
                continue;
            }
            let ram = self.slot.ram.trim().parse::<i64>().unwrap_or(0);
            if !self.has_capacity(c, ram) {
                self.nap(60);
                continue;
            }
            if self.stopping() {
                break;
            }
            let host = format!("{}{name}", c.get("NAME_PREFIX"));
            self.write_pod_snippet(c, &host, n)?;
            if !self.create_pod_vm(c, &applied, &tpl, &host, &name) {
                self.log(&format!(
                    "pod {name}: could not create VM {vmid} from template {tpl}"
                ));
                self.remove_snippet(c, &vmid);
                if self.kind(&vmid).is_some() {
                    let code = self.destroy(c, &vmid)?;
                    if code != 0 {
                        return Ok(code);
                    }
                }
                self.nap(120);
                continue;
            }
            self.write_firewall(c, &vmid)?;
            if self.cx.pve.run_stderr("qm", &["start", &vmid]) != 0 {
                self.log(&format!("pod {name}: VM {vmid} did not start"));
                self.remove_snippet(c, &vmid);
                let code = self.destroy(c, &vmid)?;
                if code != 0 {
                    return Ok(code);
                }
                self.nap(120);
                continue;
            }
            self.remove_snippet(c, &vmid);
            let born = (self.now)();
            self.log(&format!(
                "pod {name}: VM {vmid} booting ({n} containers, template {tpl})"
            ));
            self.run_hook(c, &["pod-up", &name, &vmid, &n.to_string()]);

            // Ready = the VM has built its container image and started the container loops (minutes).
            let mut ready = false;
            for _ in 0..90 {
                if self.stopping() || !self.running(&vmid) {
                    break;
                }
                if self.pod_exec(&["ready"]).is_some() {
                    ready = true;
                    break;
                }
                self.poll();
                self.nap(20);
            }
            if ready {
                self.log(&format!(
                    "pod {name}: ready after {} min",
                    ((self.now)() - born) / 60
                ));
                unready = 0;
            } else if !self.stopping() {
                unready += 1;
                self.log(&format!(
                    "pod {name}: VM {vmid} never got ready (see /var/log/mayfly-pod-image.log inside); replacing it"
                ));
            }

            let mut misses = 0;
            let mut recycle = String::new();
            while ready && !self.stopping() && self.running(&vmid) {
                let age = c.number("POD_MAX_AGE_HOURS").unwrap_or(0) * 3600 + (vmid_num % 8) * 1800;
                if (self.now)() - born >= age {
                    recycle = format!("older than {} h", c.get("POD_MAX_AGE_HOURS"));
                    break;
                }
                let pool_env = &self.cx.paths.pool_env;
                let cur = if !applied.template.is_empty() {
                    applied.template.clone()
                } else if self.slot.which == "next" {
                    let next = raw_setting(pool_env, "NEXT_TEMPLATE_VMID");
                    if next.is_empty() {
                        raw_setting(pool_env, "TEMPLATE_VMID")
                    } else {
                        next
                    }
                } else {
                    raw_setting(pool_env, "TEMPLATE_VMID")
                };
                if cur != tpl {
                    recycle = format!("template {cur} replaces {tpl}");
                    break;
                }
                // want = container loops waiting for a runner config. One JIT config per waiting loop.
                let want = self
                    .pod_exec(&["want"])
                    .filter(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit()));
                if let Some(want) = want {
                    misses = 0;
                    let want: i64 = want.parse().unwrap_or(i64::MAX);
                    if want > 0 {
                        if let Ok(token) = self.gh.install_token(c) {
                            for i in 1..=want.min(8) {
                                let rname = format!(
                                    "{}-{name}-{i}-{}",
                                    c.get("RUNNER_PREFIX"),
                                    (self.now)()
                                );
                                let Ok(jit) = self.gh.jit_config(c, &token, &rname, &labels) else {
                                    self.log(&format!(
                                        "pod {name}: GitHub refused the runner registration; retrying in 1 min"
                                    ));
                                    self.poll();
                                    self.nap(60);
                                    break;
                                };
                                if self.pod_exec(&["put", &rname, &jit.encoded]).is_none() {
                                    self.delete_runner(c, &token, &jit.runner_id);
                                }
                            }
                        }
                    }
                } else {
                    misses += 1;
                    if misses >= 20 {
                        recycle = "the guest has not answered for 5 min".to_string();
                        break;
                    }
                }
                self.ticks += 1;
                if self.ticks % 4 == 0 {
                    if let Some(pct) = self.cx.pve.pool_percent(self.cx.node, c.get("STORAGE")) {
                        if c.number("POOL_PANIC_PERCENT").is_some_and(|p| pct >= p) {
                            self.log(&format!(
                                "pod {name}: {} is {pct}% full (limit {}%); stopping VM {vmid} (its jobs are cancelled)",
                                c.get("STORAGE"),
                                c.get("POOL_PANIC_PERCENT")
                            ));
                            ready = false;
                            break;
                        }
                    }
                }
                self.poll();
                self.nap(15);
            }

            // Drain: no new configs, idle containers go, running jobs finish (MAX_JOB_HOURS caps them in the VM).
            if !recycle.is_empty() {
                self.log(&format!("pod {name}: recycling VM {vmid} ({recycle})"));
            }
            if ready {
                let _ = self.pod_exec(&["drain"]);
                let mut waited = 0;
                let cap = self.hours(c, "MAX_JOB_HOURS")? * 3600 + 600;
                while self.running(&vmid) {
                    let Some(busy) = self.pod_exec(&["busy"]) else {
                        break;
                    };
                    let busy = if busy.is_empty() { "1" } else { busy.as_str() };
                    if !matches!(busy.trim().parse::<i64>(), Ok(b) if b > 0) || waited >= cap {
                        break;
                    }
                    self.poll();
                    self.nap(30);
                    waited += 30;
                }
            }
            let code = self.destroy(c, &vmid)?;
            if code != 0 {
                return Ok(code);
            }
            let minutes = ((self.now)() - born) / 60;
            self.log(&format!("pod {name}: VM {vmid} gone after {minutes} min"));
            let minutes = ((self.now)() - born) / 60;
            self.run_hook(c, &["pod-gone", &name, &vmid, &minutes.to_string()]);
            // A pod that never gets ready again and again: back off instead of rebuilding its image every half hour.
            if !self.stopping() && !ready {
                let secs = if unready < 2 {
                    120
                } else {
                    (unready * 120).min(1800)
                };
                self.nap(secs as u64);
            }
        }
        Ok(0)
    }

    /// Clone, size and configure the pod VM like a job VM, described as `pod <name>, template <tpl>`.
    fn create_pod_vm(&self, c: &Config, a: &Applied, tpl: &str, host: &str, name: &str) -> bool {
        self.create_vm(c, a, tpl, host, &format!("pod {name}, template {tpl}"))
    }
}
