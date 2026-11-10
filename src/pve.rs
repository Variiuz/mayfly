//! The Proxmox host: `qm`, `pct`, `pvesh`, `pvesm`, `systemctl`, `ip` and `pve-firewall` behind one
//! trait, so commands can be tested against a recording fake.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

pub struct RunOut {
    pub code: i32,
    pub stdout: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuestKind {
    Vm,
    Ct,
}

/// A VM or container with this VMID on this node: decided by its config file, like bash `guest_kind`
/// (no `qm`/`pct` call, which starts the Proxmox Perl CLI).
pub fn guest_kind(pve_etc: &str, id: &str) -> Option<GuestKind> {
    if Path::new(&format!("{pve_etc}/qemu-server/{id}.conf")).exists() {
        Some(GuestKind::Vm)
    } else if Path::new(&format!("{pve_etc}/lxc/{id}.conf")).exists() {
        Some(GuestKind::Ct)
    } else {
        None
    }
}

/// The node a VMID belongs to anywhere in the cluster (`/etc/pve/.vmlist`), if it is used.
pub fn vmid_node(pve_etc: &str, id: &str) -> Option<String> {
    let text = fs::read_to_string(format!("{pve_etc}/.vmlist")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let node = v.get("ids")?.get(id)?.get("node")?.as_str()?;
    if node.is_empty() {
        None
    } else {
        Some(node.to_string())
    }
}

pub trait Pve {
    /// Runs a program with stdin closed and stderr discarded. A program that cannot start is code 127.
    fn run(&self, prog: &str, args: &[&str]) -> RunOut;
    /// `command -v name`.
    fn has_command(&self, name: &str) -> bool;

    /// `pvesm path <volid>`: stdout, empty on failure.
    fn pvesm_path(&self, volid: &str) -> String {
        self.run("pvesm", &["path", volid]).stdout
    }

    fn storage_exists(&self, storage: &str) -> bool {
        self.run("pvesm", &["status", "--storage", storage]).code == 0
    }

    /// `qm config` / `pct config`.
    fn guest_config(&self, kind: GuestKind, id: &str) -> Option<String> {
        let r = match kind {
            GuestKind::Vm => self.run("qm", &["config", id]),
            GuestKind::Ct => self.run("pct", &["config", id]),
        };
        if r.code == 0 {
            Some(r.stdout)
        } else {
            None
        }
    }

    /// The second word of each line of `qm status` / `pct status` (`running`, `stopped`), "" when it fails.
    fn guest_state(&self, kind: GuestKind, id: &str) -> String {
        let r = match kind {
            GuestKind::Vm => self.run("qm", &["status", id]),
            GuestKind::Ct => self.run("pct", &["status", id]),
        };
        let words: Vec<&str> = r
            .stdout
            .lines()
            .map(|l| l.split_whitespace().nth(1).unwrap_or(""))
            .collect();
        words.join("\n").trim_end_matches('\n').to_string()
    }

    /// True while the runner in the guest runs a job (guest agent for VMs, lxc-attach for containers).
    fn job_active(&self, kind: GuestKind, id: &str) -> bool {
        const WORKER: &str = "bin/Runner\\.Worker";
        match kind {
            GuestKind::Ct => {
                self.run(
                    "timeout",
                    &["10", "lxc-attach", "-n", id, "--", "pgrep", "-f", WORKER],
                )
                .code
                    == 0
            }
            GuestKind::Vm => {
                let r = self.run(
                    "timeout",
                    &[
                        "15",
                        "qm",
                        "guest",
                        "exec",
                        id,
                        "--timeout",
                        "10",
                        "--",
                        "pgrep",
                        "-f",
                        WORKER,
                    ],
                );
                if r.code != 0 {
                    return false;
                }
                serde_json::from_str::<serde_json::Value>(&r.stdout)
                    .ok()
                    .and_then(|v| v.get("exitcode").and_then(|c| c.as_i64()))
                    .unwrap_or(124)
                    == 0
            }
        }
    }

    /// `systemctl is-active <unit>`: its answer without the final newline, "" when it prints none.
    fn service_state(&self, unit: &str) -> String {
        let r = self.run("systemctl", &["is-active", unit]);
        r.stdout.trim_end_matches('\n').to_string()
    }

    /// Percent of the storage in use, rounded down; 100 when it reports no size. None when `pvesh` fails.
    fn pool_percent(&self, node: &str, storage: &str) -> Option<i64> {
        let path = format!("/nodes/{node}/storage/{storage}/status");
        let r = self.run("pvesh", &["get", &path, "--output-format", "json"]);
        if r.code != 0 {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(&r.stdout).ok()?;
        let total = v.get("total").and_then(|t| t.as_f64()).unwrap_or(0.0);
        let used = v.get("used").and_then(|t| t.as_f64()).unwrap_or(0.0);
        if total > 0.0 {
            Some((used * 100.0 / total).floor() as i64)
        } else {
            Some(100)
        }
    }

    fn bridge_exists(&self, bridge: &str) -> bool {
        self.run("ip", &["link", "show", bridge]).code == 0
    }

    /// `pve-firewall status`, "" when it fails.
    fn firewall_status(&self) -> String {
        self.run("pve-firewall", &["status"]).stdout
    }

    /// Ids of the cluster backup jobs that back up every VM.
    fn backup_jobs_all(&self) -> Vec<String> {
        let r = self.run(
            "pvesh",
            &["get", "/cluster/backup", "--output-format", "json"],
        );
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&r.stdout) else {
            return vec![];
        };
        let Some(jobs) = v.as_array() else {
            return vec![];
        };
        jobs.iter()
            .filter(|j| j.get("all").and_then(|a| a.as_f64()) == Some(1.0))
            .map(|j| match j.get("id") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "null".to_string(),
            })
            .collect()
    }
}

/// The real thing: runs the programs on `PATH`.
pub struct RealPve;

impl Pve for RealPve {
    fn run(&self, prog: &str, args: &[&str]) -> RunOut {
        match Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            Ok(o) => RunOut {
                code: o.status.code().unwrap_or(1),
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            },
            Err(_) => RunOut {
                code: 127,
                stdout: String::new(),
            },
        }
    }

    fn has_command(&self, name: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| {
            fs::metadata(dir.join(name))
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
    }
}

/// A fake for tests: answers from a table keyed by the command line (`"qm status 9002"`) and records
/// every call. Anything not in the table fails with code 127.
#[derive(Default)]
pub struct FakePve {
    answers: HashMap<String, (i32, String)>,
    commands: Vec<String>,
    calls: RefCell<Vec<String>>,
}

impl FakePve {
    pub fn new() -> FakePve {
        FakePve::default()
    }

    /// The command line succeeds and prints `stdout`.
    pub fn answer(mut self, line: &str, stdout: &str) -> FakePve {
        self.answers
            .insert(line.to_string(), (0, stdout.to_string()));
        self
    }

    /// The command line exits with `code` and prints nothing.
    pub fn fail(mut self, line: &str, code: i32) -> FakePve {
        self.answers.insert(line.to_string(), (code, String::new()));
        self
    }

    /// `command -v name` finds it.
    pub fn with_command(mut self, name: &str) -> FakePve {
        self.commands.push(name.to_string());
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Pve for FakePve {
    fn run(&self, prog: &str, args: &[&str]) -> RunOut {
        let line = std::iter::once(prog)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        self.calls.borrow_mut().push(line.clone());
        match self.answers.get(&line) {
            Some((code, out)) => RunOut {
                code: *code,
                stdout: out.clone(),
            },
            None => RunOut {
                code: 127,
                stdout: String::new(),
            },
        }
    }

    fn has_command(&self, name: &str) -> bool {
        self.commands.iter().any(|c| c == name)
    }
}
