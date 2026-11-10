//! `mayfly template build [profile] [--lxc] [--foreground]`, `canary <vmid>`, `promote <vmid>` and
//! `list`. The command sequence, the waits and the messages are those of bash `cmd_template`,
//! `template_build`, `template_build_ct` and `free_template_vmid`.
//!
//! Where bash relied on `set -e`, a failing command ends the build with its own exit code (`Stop::Code`);
//! `die` is `Stop::Die`.

use crate::config::{Config, Paths};
use crate::guest::{is_ours, is_template};
use crate::out::{die, Die, Out, R};
use crate::pve::{guest_kind, vmid_node, GuestKind};
use crate::{cmd_check, config, Ctx};
use std::cmp::Ordering;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const BUILD_UNIT: &str = "mayfly-template-build";
const USAGE: &str =
    "usage: mayfly template build [profile] [--lxc]|canary <vmid>|promote <vmid>|list";
/// The provisioning of a VM template is given this long (seconds).
const PROVISION_LIMIT: u64 = 5400;
const POLL: u64 = 30;

/// Everything a build takes from its surroundings besides the config, so that tests can fix it.
pub struct Opts {
    pub now: SystemTime,
    /// Where cloud images are kept.
    pub images_dir: String,
    /// Running inside a systemd unit (`INVOCATION_ID` is set): build right here.
    pub in_unit: bool,
    /// `--setenv=NAME=value` for the unit a detached build starts.
    pub unit_env: Vec<String>,
    /// This program, as the unit runs it.
    pub self_exe: String,
    /// Where `mktemp` makes the profile tarball of a container build.
    pub tmp_dir: String,
    /// That tarball's path, when a test wants it fixed.
    pub tgz: Option<String>,
}

impl Opts {
    pub fn from_env(paths: &Paths) -> Opts {
        let unit_env = [
            "MAYFLY_CONF_DIR",
            "MAYFLY_SHARE",
            "MAYFLY_PVE_ETC",
            "MAYFLY_PVE_FW_DIR",
            "MAYFLY_BUILD_LOCK",
            "MAYFLY_STATE_DIR",
            "NO_COLOR",
        ]
        .iter()
        .filter_map(|v| {
            let val = std::env::var(v).unwrap_or_default();
            (!val.is_empty()).then(|| format!("--setenv={v}={val}"))
        })
        .collect();
        let self_exe = std::env::current_exe()
            .and_then(fs::canonicalize)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| format!("{}/mayfly", paths.sbin));
        Opts {
            now: SystemTime::now(),
            images_dir: format!("{}/images", paths.state_dir),
            in_unit: !config::env_or("INVOCATION_ID", "").is_empty(),
            unit_env,
            self_exe,
            tmp_dir: config::env_or("TMPDIR", "/tmp"),
            tgz: None,
        }
    }
}

enum Stop {
    Die(String),
    Code(i32),
}

type T<X> = Result<X, Stop>;

impl From<Die> for Stop {
    fn from(d: Die) -> Stop {
        Stop::Die(d.0)
    }
}

fn tdie<X>(msg: impl Into<String>) -> T<X> {
    Err(Stop::Die(msg.into()))
}

/// `set -e`: a command that fails ends the script with its exit code.
fn must(code: i32) -> T<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Stop::Code(code))
    }
}

fn finish(r: T<()>) -> R<i32> {
    match r {
        Ok(()) => Ok(0),
        Err(Stop::Code(c)) => Ok(c),
        Err(Stop::Die(m)) => Err(Die(m)),
    }
}

pub fn run(cx: &Ctx, args: &[String], out: &Out, w: &mut dyn Write) -> R<i32> {
    let (c, _) = config::load(cx.paths, cx.pve, cx.node, true)?;
    let o = Opts::from_env(cx.paths);
    run_with(
        cx,
        &c,
        args,
        &o,
        out,
        w,
        &mut io::stderr().lock() as &mut dyn Write,
    )
}

/// `w` is stdout, `ew` stderr.
pub fn run_with(
    cx: &Ctx,
    c: &Config,
    args: &[String],
    o: &Opts,
    out: &Out,
    w: &mut dyn Write,
    ew: &mut dyn Write,
) -> R<i32> {
    let mut r = Run {
        cx,
        c,
        o,
        out,
        w,
        ew,
    };
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    match arg(0) {
        "build" => finish(r.build_cmd(&args[1..])),
        "canary" => {
            r.usable_template(arg(1), "canary")?;
            r.canary(arg(1))
        }
        "promote" => {
            r.usable_template(arg(1), "promote")?;
            r.promote(arg(1))
        }
        "list" => {
            r.list();
            Ok(0)
        }
        _ => die(USAGE),
    }
}

struct Run<'a> {
    cx: &'a Ctx<'a>,
    c: &'a Config,
    o: &'a Opts,
    out: &'a Out,
    w: &'a mut dyn Write,
    ew: &'a mut dyn Write,
}

impl Run<'_> {
    fn paths(&self) -> &Paths {
        self.cx.paths
    }

    fn kind(&self, id: &str) -> Option<GuestKind> {
        guest_kind(&self.paths().pve_etc, id)
    }

    fn kind_name(&self, id: &str) -> &'static str {
        match self.kind(id) {
            Some(GuestKind::Vm) => "vm",
            Some(GuestKind::Ct) => "ct",
            None => "",
        }
    }

    fn exists(&self, id: &str) -> bool {
        self.kind(id).is_some()
    }

    fn cfg(&self, id: &str) -> String {
        self.kind(id)
            .and_then(|k| self.cx.pve.guest_config(k, id))
            .unwrap_or_default()
    }

    fn running(&self, id: &str) -> bool {
        self.cx.pve.guest_running(self.kind(id), id)
    }

    fn used_elsewhere(&self, id: &str) -> bool {
        vmid_node(&self.paths().pve_etc, id).is_some_and(|n| n != self.cx.node)
    }

    fn done(&mut self, msg: &str) {
        self.out.done(self.w, msg);
    }

    fn hint(&mut self, msg: &str) {
        self.out.hint(self.w, msg);
    }

    fn step(&mut self, msg: &str) {
        self.out.step(self.w, msg);
    }

    fn num(&self, key: &str) -> T<i64> {
        match self.c.number(key) {
            Some(n) => Ok(n),
            None => tdie(format!("{key}={}: not a number", self.c.get(key))),
        }
    }

    // -------------------------------------------------------------- canary, promote, list

    /// `[ -n "$id" ] && is_template && is_ours || die usage`.
    fn usable_template(&self, id: &str, sub: &str) -> R<()> {
        if !id.is_empty() && is_template(self.cx, id) && is_ours(self.cx, self.c, id) {
            Ok(())
        } else {
            die(format!("usage: mayfly template {sub} <template vmid>"))
        }
    }

    fn legacy_template_note(&mut self, id: &str) {
        if self.kind(id) != Some(GuestKind::Vm)
            || cmd_check::template_layout(self.cx, id) == "mayfly"
        {
            return;
        }
        self.out.warn(
            self.w,
            &format!("template {id} was built by polara-ci: its jobs get the /etc/polara-ci paths it expects"),
        );
        self.hint("rebuild it with mayfly template build when convenient");
    }

    fn set_value(&self, key: &str, value: &str) -> R<()> {
        set_pool_value(&self.paths().pool_env, key, value)
            .map_err(|e| Die(format!("cannot write {}: {e}", self.paths().pool_env)))
    }

    fn canary(&mut self, id: &str) -> R<i32> {
        self.legacy_template_note(id);
        if self.kind(id) == Some(GuestKind::Ct) {
            self.set_value("NEXT_LXC_TEMPLATE_VMID", id)?;
            self.done(&format!("lxc slots marked next now clone {id}"));
        } else {
            self.set_value("NEXT_TEMPLATE_VMID", id)?;
            self.done(&format!("slots marked next now clone {id}"));
        }
        self.hint(&format!(
            "when their jobs pass: mayfly template promote {id}"
        ));
        Ok(0)
    }

    fn promote(&mut self, id: &str) -> R<i32> {
        self.legacy_template_note(id);
        if self.kind(id) == Some(GuestKind::Ct) {
            self.set_value("LXC_TEMPLATE_VMID", id)?;
            self.set_value("NEXT_LXC_TEMPLATE_VMID", "")?;
            self.done(&format!("every lxc slot clones {id} from its next job on"));
        } else {
            self.set_value("TEMPLATE_VMID", id)?;
            self.set_value("NEXT_TEMPLATE_VMID", "")?;
            self.done(&format!("every slot clones {id} from its next job on"));
        }
        Ok(0)
    }

    fn list(&mut self) {
        let (cur, next) = (
            [self.c.get("TEMPLATE_VMID"), self.c.get("LXC_TEMPLATE_VMID")],
            [
                self.c.get("NEXT_TEMPLATE_VMID"),
                self.c.get("NEXT_LXC_TEMPLATE_VMID"),
            ],
        );
        for t in self.c.get("TEMPLATE_VMIDS").split_whitespace() {
            if !self.exists(t) {
                continue;
            }
            let names: Vec<String> = self
                .cfg(t)
                .split('\n')
                .filter_map(|l| {
                    l.strip_prefix("name: ")
                        .or_else(|| l.strip_prefix("hostname: "))
                        .map(str::to_string)
                })
                .collect();
            let name = names.join("\n");
            let name = name.trim_end_matches('\n');
            let _ = writeln!(
                self.w,
                "{t} {} {name}{}{}",
                self.kind_name(t),
                if cur.contains(&t) { " (current)" } else { "" },
                if next.contains(&t) { " (next)" } else { "" },
            );
        }
    }

    // -------------------------------------------------------------- build

    fn build_cmd(&mut self, args: &[String]) -> T<()> {
        let (mut lxc, mut fg) = (false, false);
        let mut profile = self.c.get("PROFILE").to_string();
        let mut passed: Vec<&str> = Vec::new();
        for a in args {
            match a.as_str() {
                "--lxc" => {
                    lxc = true;
                    passed.push(a);
                }
                "--foreground" => fg = true,
                s if s.starts_with('-') => {
                    return tdie("usage: mayfly template build [profile] [--lxc] [--foreground]")
                }
                s => {
                    profile = s.to_string();
                    passed.push(a);
                }
            }
        }
        // One build at a time: a second one would pick the same free VMID and destroy the first's VM.
        let lock_path = self.paths().build_lock.clone();
        if let Some(dir) = Path::new(&lock_path).parent() {
            if !dir.as_os_str().is_empty() {
                if let Err(e) = fs::create_dir_all(dir) {
                    return tdie(format!("cannot create {}: {e}", dir.display()));
                }
            }
        }
        let lock = match fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&lock_path)
        {
            Ok(f) => f,
            Err(e) => return tdie(format!("cannot open {lock_path}: {e}")),
        };
        if lock.try_lock().is_err() {
            return tdie(format!(
                "another template build is running (lock {lock_path}); follow it: journalctl -fu {BUILD_UNIT}"
            ));
        }
        // A build takes 20 to 40 min: run it in its own unit, so a closed SSH session doesn't kill it.
        // Already in a unit (INVOCATION_ID, e.g. systemd-run by hand): build right here.
        if !fg && !self.o.in_unit && self.cx.pve.has_command("systemd-run") {
            drop(lock);
            return self.build_detached(&passed);
        }
        let r = if lxc {
            self.template_build_ct(&profile)
        } else {
            self.template_build(&profile)
        };
        drop(lock);
        r
    }

    /// Starts `template build <args> --foreground` in the transient unit BUILD_UNIT and returns.
    fn build_detached(&mut self, passed: &[&str]) -> T<()> {
        let pve = self.cx.pve;
        if pve.unit_active(BUILD_UNIT) {
            return tdie(format!(
                "{BUILD_UNIT} is already running; follow it: journalctl -fu {BUILD_UNIT}"
            ));
        }
        pve.run("systemctl", &["reset-failed", BUILD_UNIT]);
        let mut argv: Vec<String> = vec![
            "-q".to_string(),
            format!("--unit={BUILD_UNIT}"),
            "--description=mayfly template build".to_string(),
            "--collect".to_string(),
        ];
        argv.extend(self.o.unit_env.iter().cloned());
        argv.push(self.o.self_exe.clone());
        argv.extend(["template".to_string(), "build".to_string()]);
        argv.extend(passed.iter().map(|a| a.to_string()));
        argv.push("--foreground".to_string());
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        if pve.run_loud("systemd-run", &argv) != 0 {
            return tdie(format!("could not start {BUILD_UNIT}"));
        }
        self.done(&format!("template build started in {BUILD_UNIT}"));
        self.hint(&format!(
            "follow it: journalctl -fu {BUILD_UNIT} (done when it prints 'ready'; mayfly template list)"
        ));
        Ok(())
    }

    /// The folder of a template profile: /etc/mayfly/profiles/<name> first, then the shipped ones.
    fn find_profile(&self, name: &str) -> Option<String> {
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return None;
        }
        [
            format!("{}/profiles/{name}", self.paths().conf_dir),
            format!("{}/profiles/{name}", self.paths().share),
        ]
        .into_iter()
        .find(|d| Path::new(d).is_dir())
    }

    fn profile_dir(&self, profile: &str) -> T<String> {
        match self.find_profile(profile) {
            Some(d) => Ok(d),
            None => tdie(format!(
                "profile {profile} not found in {}/profiles or {}/profiles",
                self.paths().conf_dir,
                self.paths().share
            )),
        }
    }

    /// The first template VMID that is no current or next template (VM or container), emptied: an old
    /// template there is destroyed, unless a VM or container is still a linked clone of it.
    fn free_template_vmid(&mut self) -> T<String> {
        let c = self.c;
        let taken = [
            c.get("TEMPLATE_VMID"),
            c.get("NEXT_TEMPLATE_VMID"),
            c.get("LXC_TEMPLATE_VMID"),
            c.get("NEXT_LXC_TEMPLATE_VMID"),
        ];
        let id = c
            .get("TEMPLATE_VMIDS")
            .split_whitespace()
            .find(|t| !taken.contains(t) && !self.used_elsewhere(t));
        let Some(id) = id else {
            return tdie(format!(
                "no free template VMID in TEMPLATE_VMIDS ({}); add one",
                c.get("TEMPLATE_VMIDS")
            ));
        };
        if self.exists(id) {
            if !is_ours(self.cx, c, id) {
                return tdie(format!("VMID {id} exists and is not a mayfly guest"));
            }
            if !is_template(self.cx, id) && self.running(id) {
                return tdie(format!(
                    "VMID {id} is running and not a template yet (a build in progress?); stop it first if it is stale"
                ));
            }
            for dir in ["qemu-server", "lxc"] {
                let mut files: Vec<String> =
                    fs::read_dir(format!("{}/{dir}", self.paths().pve_etc))
                        .map(|rd| {
                            rd.filter_map(|e| e.ok())
                                .filter_map(|e| e.file_name().into_string().ok())
                                .filter(|n| n.ends_with(".conf"))
                                .collect()
                        })
                        .unwrap_or_default();
                files.sort();
                for f in files {
                    let other = f.trim_end_matches(".conf");
                    if other == id {
                        continue;
                    }
                    let text = fs::read(format!("{}/{dir}/{f}", self.paths().pve_etc))
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                    if text.contains(&format!("base-{id}-disk")) {
                        return tdie(format!("template {id} still has clones ({other})"));
                    }
                }
            }
            // The step goes to stderr (the function's stdout is the VMID). A destroy that fails is not
            // fatal here (bash runs this in a command substitution, where `set -e` is off).
            self.out
                .step(self.ew, &format!("removing old template {id}"));
            let pve = self.cx.pve;
            let destroy = [
                "destroy",
                id,
                "--purge",
                "1",
                "--destroy-unreferenced-disks",
                "1",
            ];
            if self.kind(id) == Some(GuestKind::Ct) {
                pve.run_stderr("pct", &destroy);
            } else {
                pve.run_stderr("qm", &destroy);
            }
        }
        Ok(id.to_string())
    }

    /// Per-VM firewall: nothing in, DNS to the gateway, CACHE_IP and ALLOW_OUT, nothing to BLOCK_NETS,
    /// the rest out. FIREWALL=no writes none.
    fn write_firewall(&self, id: &str) -> T<()> {
        let c = self.c;
        let path = format!("{}/{id}.fw", self.paths().pve_fw_dir);
        if c.get("FIREWALL") != "yes" {
            let _ = fs::remove_file(&path);
            return Ok(());
        }
        let mut t = String::from(
            "[OPTIONS]\nenable: 1\ndhcp: 1\npolicy_in: DROP\npolicy_out: ACCEPT\n\n[RULES]\n",
        );
        let gw = c.get("GATEWAY");
        if !gw.is_empty() {
            t += &format!("OUT ACCEPT -dest {gw} -p udp -dport 53\n");
            t += &format!("OUT ACCEPT -dest {gw} -p tcp -dport 53\n");
        }
        for dest in c
            .get("CACHE_IP")
            .split_whitespace()
            .chain(c.get("ALLOW_OUT").split_whitespace())
        {
            t += &format!("OUT ACCEPT -dest {dest}\n");
        }
        for net in c.get("BLOCK_NETS").split_whitespace() {
            t += &format!("OUT DROP -dest {net}\n");
        }
        match fs::write(&path, t) {
            Ok(()) => Ok(()),
            Err(e) => tdie(format!("cannot write {path}: {e}")),
        }
    }

    /// `base64 -w0 <file>`; a file that cannot be read is an error line and no content, as in bash.
    fn base64_file(&mut self, path: &str) -> String {
        match fs::read(path) {
            Ok(b) => base64(&b),
            Err(_) => {
                let _ = writeln!(self.ew, "base64: {path}: No such file or directory");
                String::new()
            }
        }
    }

    /// True when <path> exists in VM <id>, asked through the guest agent.
    fn guest_file_exists(&self, id: &str, path: &str) -> bool {
        let r = self.cx.pve.run(
            "qm",
            &[
                "guest",
                "exec",
                id,
                "--timeout",
                "20",
                "--",
                "test",
                "-e",
                path,
            ],
        );
        r.code == 0
            && serde_json::from_str::<serde_json::Value>(&r.stdout)
                .ok()
                .is_some_and(|v| v.get("exitcode").and_then(|e| e.as_i64()) == Some(0))
    }

    fn net0(&self) -> String {
        let (bridge, vlan) = (self.c.get("BRIDGE"), self.c.get("VLAN"));
        let fw = if self.c.get("FIREWALL") == "yes" {
            1
        } else {
            0
        };
        let tag = if vlan.is_empty() {
            String::new()
        } else {
            format!(",tag={vlan}")
        };
        format!("virtio,bridge={bridge}{tag},firewall={fw}")
    }

    fn ct_net0(&self) -> String {
        let (bridge, vlan) = (self.c.get("BRIDGE"), self.c.get("VLAN"));
        let fw = if self.c.get("FIREWALL") == "yes" {
            1
        } else {
            0
        };
        let tag = if vlan.is_empty() {
            String::new()
        } else {
            format!(",tag={vlan}")
        };
        format!("name=eth0,bridge={bridge}{tag},ip=dhcp,firewall={fw}")
    }

    fn write_snippet(&mut self, snippet: &str, pdir: &str) -> T<()> {
        let share = self.paths().share.clone();
        let c = self.c;
        let mut t = String::new();
        t += "#cloud-config\n";
        t += &format!("hostname: {}template\n", c.get("NAME_PREFIX"));
        t += "write_files:\n";
        t += "  - path: /root/provision.sh\n    permissions: '0700'\n    encoding: b64\n";
        t += &format!(
            "    content: {}\n",
            self.base64_file(&format!("{share}/provision.sh"))
        );
        for f in ["ci-job.sh", "mayfly-job.service", "template.env"] {
            t += &format!("  - path: /root/{f}\n    encoding: b64\n");
            t += &format!(
                "    content: {}\n",
                self.base64_file(&format!("{share}/{f}"))
            );
        }
        let (_, tgz) = self
            .cx
            .pve
            .run_bytes("tar", &["-C", pdir, "-czf", "-", "."]);
        t += "  - path: /root/profile.tgz\n    encoding: b64\n";
        t += &format!("    content: {}\n", base64(&tgz));
        t += "runcmd:\n";
        // The host waits for the done marker and shuts the VM down; a VM that stops without it is no template.
        t += &format!(
            "  - [bash, -c, 'CACHE_IP={} /root/provision.sh > /var/log/mayfly-provision.log 2>&1 && touch /var/log/mayfly-provision.done || touch /var/log/mayfly-provision.failed']\n",
            c.get("CACHE_IP")
        );
        // umask 077
        let written = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(snippet)
            .and_then(|mut f| f.write_all(t.as_bytes()));
        match written {
            Ok(()) => Ok(()),
            Err(e) => tdie(format!("cannot write {snippet}: {e}")),
        }
    }

    fn template_build(&mut self, profile: &str) -> T<()> {
        let pve = self.cx.pve;
        let c = self.c;
        let pdir = self.profile_dir(profile)?;
        let id = self.free_template_vmid()?;
        let stamp = ymd(self.o.now);
        let dir = self.o.images_dir.clone();
        if let Err(e) = fs::create_dir_all(&dir) {
            return tdie(format!("cannot create {dir}: {e}"));
        }
        let url = c.get("IMAGE_URL");
        let base = basename(url);
        let img = format!("{dir}/{base}");
        // Ubuntu and Debian publish SHA256SUMS / SHA512SUMS next to their cloud images.
        let sums_url = format!("{}/SHA256SUMS", dirname(url));
        let sums = pve.run_keep_err(
            "curl",
            &[
                "-fsSL",
                "--retry",
                "5",
                "--retry-all-errors",
                "--retry-delay",
                "5",
                &sums_url,
            ],
        );
        if sums.code != 0 {
            return tdie(format!(
                "no SHA256SUMS next to {url}; refusing an unchecked image"
            ));
        }
        self.step(&format!("downloading {base}"));
        let part = format!("{img}.part");
        must(pve.run_loud(
            "curl",
            &[
                "-fsSL",
                "--retry",
                "5",
                "--retry-all-errors",
                "--retry-delay",
                "5",
                "-o",
                &part,
                url,
            ],
        ))?;
        // echo "$(grep ... | awk ...)  $img.part" | sha256sum -c - >/dev/null || die ...
        let input = format!("{}  {part}\n", listed_sums(&sums.stdout, &base));
        if pve.run_stdin("sha256sum", &["-c", "-"], &input) != 0 {
            return tdie("checksum of the cloud image does not match SHA256SUMS");
        }
        if fs::rename(&part, &img).is_err() {
            return Err(Stop::Code(1));
        }

        let snippet = format!("{}/mayfly-{id}.yaml", c.snippet_dir);
        self.write_snippet(&snippet, &pdir)?;

        self.step(&format!(
            "building template VM {id}, profile {profile} (20 to 40 min)"
        ));
        let name_prefix = c.get("NAME_PREFIX");
        let memory = (self.num("TEMPLATE_MEMORY_GB")? * 1024).to_string();
        let net0 = self.net0();
        let cicustom = format!(
            "user={}:snippets/mayfly-{id}.yaml",
            c.get("SNIPPET_STORAGE")
        );
        let create_name = format!("{name_prefix}template-build");
        let desc = format!("profile {profile}");
        must(pve.run_stderr(
            "qm",
            &[
                "create",
                &id,
                "--name",
                &create_name,
                "--tags",
                c.get("TAG"),
                "--ostype",
                "l26",
                "--cpu",
                "host",
                "--cores",
                c.get("TEMPLATE_CORES"),
                "--memory",
                &memory,
                "--description",
                &desc,
                "--balloon",
                "0",
                "--scsihw",
                "virtio-scsi-single",
                "--agent",
                "enabled=1",
                "--serial0",
                "socket",
                "--vga",
                "serial0",
                "--net0",
                &net0,
                "--ipconfig0",
                "ip=dhcp",
                "--cicustom",
                &cicustom,
            ],
        ))?;
        // File-based storage needs qcow2 so that linked clones work; LVM-thin keeps its raw volumes.
        let storage = c.get("STORAGE");
        let stype = {
            let r = pve.run(
                "pvesh",
                &[
                    "get",
                    &format!("/storage/{storage}"),
                    "--output-format",
                    "json",
                ],
            );
            if r.code == 0 {
                serde_json::from_str::<serde_json::Value>(&r.stdout)
                    .ok()
                    .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                    .unwrap_or_default()
            } else {
                String::new()
            }
        };
        let fmt = if matches!(stype.as_str(), "dir" | "nfs" | "cifs") {
            ",format=qcow2"
        } else {
            ""
        };
        must(pve.run_stderr(
            "qm",
            &[
                "set",
                &id,
                "--scsi0",
                &format!("{storage}:0,import-from={img},discard=on,iothread=1,ssd=1{fmt}"),
            ],
        ))?;
        must(pve.run_stderr(
            "qm",
            &[
                "set",
                &id,
                "--ide2",
                &format!("{storage}:cloudinit"),
                "--boot",
                "order=scsi0",
            ],
        ))?;
        must(pve.run_stderr(
            "qm",
            &[
                "resize",
                &id,
                "scsi0",
                &format!("{}G", c.get("TEMPLATE_DISK")),
            ],
        ))?;
        self.write_firewall(&id)?;
        must(pve.run_stderr("qm", &["start", &id]))?;

        let mut waited = 0;
        loop {
            if !self.exists(&id) {
                return tdie(format!(
                    "template VM {id} is gone (destroyed from outside?); nothing was built"
                ));
            }
            if !self.running(&id) {
                return tdie(format!(
                    "template VM {id} stopped before provisioning finished; nothing was built (see /var/log/mayfly-provision.log in its disk, or build again)"
                ));
            }
            if waited >= PROVISION_LIMIT {
                return tdie(format!(
                    "template VM {id} still provisioning after 90 min; see 'qm terminal {id}' or /var/log/mayfly-provision.log inside"
                ));
            }
            pve.nap(POLL);
            waited += POLL;
            if self.guest_file_exists(&id, "/var/log/mayfly-provision.done") {
                break;
            } else if self.guest_file_exists(&id, "/var/log/mayfly-provision.failed") {
                let r = pve.run(
                    "qm",
                    &[
                        "guest",
                        "exec",
                        &id,
                        "--timeout",
                        "20",
                        "--",
                        "tail",
                        "-n",
                        "30",
                        "/var/log/mayfly-provision.log",
                    ],
                );
                // jq -r '."out-data" // empty' >&2
                if let Some(text) = serde_json::from_str::<serde_json::Value>(&r.stdout)
                    .ok()
                    .and_then(|v| {
                        v.get("out-data")
                            .and_then(|d| d.as_str().map(str::to_string))
                    })
                {
                    let _ = writeln!(self.ew, "{text}");
                }
                return tdie(format!(
                    "provisioning failed in template VM {id} (log above; VM left running for a look, 'mayfly template build' again replaces it)"
                ));
            }
        }
        self.step(&format!("provisioned; shutting down VM {id}"));
        pve.run(
            "qm",
            &[
                "guest",
                "exec",
                &id,
                "--timeout",
                "20",
                "--",
                "sh",
                "-c",
                "rm -f /var/log/mayfly-provision.done; sync",
            ],
        );
        if pve.run_stderr("qm", &["shutdown", &id, "--timeout", "300"]) != 0
            && pve.run_stderr("qm", &["stop", &id]) != 0
        {
            return tdie(format!("could not stop template VM {id}"));
        }
        if !(self.exists(&id) && !self.running(&id)) {
            return tdie(format!(
                "template VM {id} did not stop cleanly; nothing was built"
            ));
        }
        let _ = fs::remove_file(&snippet);
        let final_name = format!("{name_prefix}template-{profile}-{stamp}");
        if pve.run_stderr(
            "qm",
            &["set", &id, "--delete", "cicustom", "--name", &final_name],
        ) != 0
        {
            return tdie(format!("could not rename template VM {id}"));
        }
        if pve.run_stderr("qm", &["template", &id]) != 0 {
            return tdie(format!(
                "qm template {id} failed; VM {id} is not a template"
            ));
        }
        if !is_template(self.cx, &id) {
            return tdie(format!("VM {id} is not a template after qm template"));
        }
        self.done(&format!("template {id} ready ({final_name})"));
        self.hint(&format!(
            "next: mayfly template canary {id}, watch a job, then mayfly template promote {id}"
        ));
        Ok(())
    }

    /// Container template: an unprivileged LXC container (nesting and keyctl on, for Docker) from the
    /// distribution's container image, provisioned with the same core and profile as a VM template, then
    /// converted. pveam checks the image against its index's checksum.
    fn template_build_ct(&mut self, profile: &str) -> T<()> {
        let pve = self.cx.pve;
        let c = self.c;
        let pdir = self.profile_dir(profile)?;
        if !(pve.has_command("pveam") && pve.has_command("lxc-attach")) {
            return tdie("pveam/lxc-attach missing: not a Proxmox host?");
        }
        let id = self.free_template_vmid()?;
        let stamp = ymd(self.o.now);
        if pve.run_stderr("pveam", &["update"]) != 0 {
            self.out
                .warn(self.w, "pveam update failed; using the cached image index");
        }
        // file=$(pveam available --section system | awk '{print $2}' | grep "^$LXC_OSTEMPLATE" | sort -V | tail -n1)
        // Under `set -euo pipefail` a failing pipeline ends the script right there, so an empty answer
        // exits 1 without the "no container image" message below it ever being reached.
        let avail = pve.run_keep_err("pveam", &["available", "--section", "system"]);
        let ostemplate = c.get("LXC_OSTEMPLATE");
        let mut files: Vec<&str> = avail
            .stdout
            .lines()
            .map(|l| l.split_whitespace().nth(1).unwrap_or(""))
            .filter(|f| f.starts_with(ostemplate))
            .collect();
        files.sort_by(|a, b| version_cmp(a, b));
        let Some(file) = files.last().copied() else {
            return Err(Stop::Code(1));
        };
        if avail.code != 0 {
            return Err(Stop::Code(avail.code));
        }
        let storage = c.get("LXC_TEMPLATE_STORAGE");
        let listed = pve.run("pveam", &["list", storage]);
        if !(listed.code == 0 && listed.stdout.contains(&format!("vztmpl/{file}"))) {
            self.step(&format!("downloading {file}"));
            must(pve.run_stderr("pveam", &["download", storage, file]))?;
        }

        self.step(&format!(
            "building container template {id}, profile {profile} (15 to 30 min)"
        ));
        let name_prefix = c.get("NAME_PREFIX");
        let memory = (self.num("TEMPLATE_MEMORY_GB")? * 1024).to_string();
        let ostemplate_vol = format!("{storage}:vztmpl/{file}");
        let hostname = format!("{name_prefix}template-build");
        let rootfs = format!("{}:{}", c.get("STORAGE"), c.get("TEMPLATE_DISK"));
        let net0 = self.ct_net0();
        let desc = format!("profile {profile}");
        must(pve.run_stderr(
            "pct",
            &[
                "create",
                &id,
                &ostemplate_vol,
                "--hostname",
                &hostname,
                "--unprivileged",
                "1",
                "--features",
                "nesting=1,keyctl=1",
                "--ostype",
                "ubuntu",
                "--onboot",
                "0",
                "--cores",
                c.get("TEMPLATE_CORES"),
                "--memory",
                &memory,
                "--swap",
                "0",
                "--rootfs",
                &rootfs,
                "--net0",
                &net0,
                "--tags",
                c.get("TAG"),
                "--description",
                &desc,
            ],
        ))?;
        self.write_firewall(&id)?;
        must(pve.run_stderr("pct", &["start", &id]))?;
        for i in 1..=60 {
            let r = pve.run(
                "timeout",
                &[
                    "10",
                    "lxc-attach",
                    "-n",
                    &id,
                    "--",
                    "systemctl",
                    "is-system-running",
                ],
            );
            if matches!(r.stdout.trim_end_matches('\n'), "running" | "degraded") {
                break;
            }
            if i < 60 {
                pve.nap(2);
            }
        }
        for i in 1..=30 {
            let r = pve.run(
                "timeout",
                &[
                    "10",
                    "lxc-attach",
                    "-n",
                    &id,
                    "--",
                    "getent",
                    "hosts",
                    "github.com",
                ],
            );
            if r.code == 0 {
                break;
            }
            if i < 30 {
                pve.nap(2);
            }
        }
        let share = self.paths().share.clone();
        for f in [
            "provision.sh",
            "ci-job.sh",
            "mayfly-job.service",
            "template.env",
        ] {
            must(pve.run_stderr(
                "pct",
                &["push", &id, &format!("{share}/{f}"), &format!("/root/{f}")],
            ))?;
        }
        let tgz = match &self.o.tgz {
            Some(p) => p.clone(),
            None => mktemp(&self.o.tmp_dir),
        };
        let _ = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&tgz);
        must(pve.run_stderr("tar", &["-C", &pdir, "-czf", &tgz, "."]))?;
        must(pve.run_stderr("pct", &["push", &id, &tgz, "/root/profile.tgz"]))?;
        let _ = fs::remove_file(&tgz);
        let cache = format!("CACHE_IP={}", c.get("CACHE_IP"));
        if pve.run_loud(
            "timeout",
            &[
                "5400",
                "pct",
                "exec",
                &id,
                "--",
                "env",
                "MAYFLY_GUEST=lxc",
                &cache,
                "bash",
                "-c",
                "bash /root/provision.sh > /var/log/mayfly-provision.log 2>&1",
            ],
        ) != 0
        {
            let r = pve.run(
                "timeout",
                &[
                    "20",
                    "lxc-attach",
                    "-n",
                    &id,
                    "--",
                    "tail",
                    "-n",
                    "30",
                    "/var/log/mayfly-provision.log",
                ],
            );
            let _ = self.ew.write_all(r.stdout.as_bytes());
            return tdie(format!(
                "provisioning failed in container {id} (log above; left running for a look, 'mayfly template build --lxc' again replaces it)"
            ));
        }
        if !self.exists(&id) {
            return tdie(format!(
                "container {id} is gone (destroyed from outside?); nothing was built"
            ));
        }
        if pve.run_stderr("pct", &["stop", &id]) != 0 {
            return tdie(format!("could not stop container {id}"));
        }
        let final_name = format!("{name_prefix}template-{profile}-{stamp}");
        if pve.run_stderr("pct", &["set", &id, "--hostname", &final_name]) != 0 {
            return tdie(format!("could not rename container {id}"));
        }
        if pve.run_stderr("pct", &["template", &id]) != 0 {
            return tdie(format!(
                "pct template {id} failed; container {id} is not a template"
            ));
        }
        if !is_template(self.cx, &id) {
            return tdie(format!(
                "container {id} is not a template after pct template"
            ));
        }
        self.done(&format!("container template {id} ready ({final_name})"));
        self.hint(&format!(
            "next: mayfly template canary {id}, watch a job, then mayfly template promote {id}"
        ));
        Ok(())
    }
}

// ------------------------------------------------------------------ pool.env

/// bash `set_pool_value`: every line `KEY=...` becomes `KEY=value`, else `KEY=value` is appended. Every
/// other byte of the file stays as it is. The new file is written next to the old one and renamed over it,
/// with the old one's mode and owner, so a reader never sees half a file.
pub fn set_pool_value(path: &str, key: &str, value: &str) -> io::Result<()> {
    let old = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    let prefix = format!("{key}=").into_bytes();
    let mut new: Vec<u8> = Vec::with_capacity(old.len() + key.len() + value.len() + 2);
    let mut found = false;
    let mut rest: &[u8] = &old;
    while !rest.is_empty() {
        let (line, tail, nl) = match rest.iter().position(|b| *b == b'\n') {
            Some(i) => (&rest[..i], &rest[i + 1..], true),
            None => (rest, &rest[rest.len()..], false),
        };
        if line.starts_with(&prefix) {
            found = true;
            new.extend_from_slice(&prefix);
            new.extend_from_slice(value.as_bytes());
        } else {
            new.extend_from_slice(line);
        }
        if nl {
            new.push(b'\n');
        }
        rest = tail;
    }
    if !found {
        if !new.is_empty() && new.last() != Some(&b'\n') {
            new.push(b'\n');
        }
        new.extend_from_slice(&prefix);
        new.extend_from_slice(value.as_bytes());
        new.push(b'\n');
    }
    let p = Path::new(path);
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = p.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        if let Ok(meta) = fs::metadata(p) {
            fs::set_permissions(&tmp, meta.permissions())?;
            // Best effort, like sed -i: only root can hand the file back to another owner.
            let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
        }
        f.write_all(&new)?;
        f.sync_all()?;
        fs::rename(&tmp, p)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

// ------------------------------------------------------------------ small helpers

/// `base64 -w0`.
fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for ch in data.chunks(3) {
        let n = (ch[0] as u32) << 16
            | (*ch.get(1).unwrap_or(&0) as u32) << 8
            | *ch.get(2).unwrap_or(&0) as u32;
        s.push(A[(n >> 18) as usize & 63] as char);
        s.push(A[(n >> 12) as usize & 63] as char);
        s.push(if ch.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        s.push(if ch.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    s
}

/// `date -u +%Y%m%d`.
fn ymd(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let z = secs.div_euclid(86400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}

/// `basename`.
fn basename(p: &str) -> String {
    let t = p.trim_end_matches('/');
    if t.is_empty() {
        return if p.is_empty() {
            String::new()
        } else {
            "/".into()
        };
    }
    t.rsplit('/').next().unwrap_or(t).to_string()
}

/// `dirname`.
fn dirname(p: &str) -> String {
    let t = p.trim_end_matches('/');
    if t.is_empty() {
        return if p.is_empty() { ".".into() } else { "/".into() };
    }
    match t.rfind('/') {
        None => ".".into(),
        Some(i) => {
            let d = t[..i].trim_end_matches('/');
            if d.is_empty() {
                "/".into()
            } else {
                d.to_string()
            }
        }
    }
}

/// `grep -E " \*?<name>$" | awk '{print $1}'`: the checksums `SHA256SUMS` lists for the image, one per line.
fn listed_sums(sums: &str, name: &str) -> String {
    sums.lines()
        .filter(|l| l.ends_with(&format!(" {name}")) || l.ends_with(&format!(" *{name}")))
        .filter_map(|l| l.split_whitespace().next())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `sort -V` on two names: digit runs compare as numbers, the rest byte by byte; a tie falls back to the
/// whole line.
fn version_cmp(a: &str, b: &str) -> Ordering {
    fn chunks(s: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut start = 0;
        let bytes = s.as_bytes();
        for i in 1..=bytes.len() {
            if i == bytes.len() || bytes[i].is_ascii_digit() != bytes[start].is_ascii_digit() {
                out.push(&s[start..i]);
                start = i;
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(cb.iter()) {
        let both_num = x.as_bytes()[0].is_ascii_digit() && y.as_bytes()[0].is_ascii_digit();
        let o = if both_num {
            let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
            x.len().cmp(&y.len()).then_with(|| x.cmp(y))
        } else {
            x.cmp(y)
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    ca.len().cmp(&cb.len()).then_with(|| a.cmp(b))
}

/// A path like `mktemp` makes one: `<dir>/tmp.XXXXXXXXXX`.
fn mktemp(dir: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut n = (nanos as u64) << 20 | std::process::id() as u64;
    const A: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut s = String::new();
    for _ in 0..10 {
        s.push(A[(n % 62) as usize] as char);
        n /= 62;
        n ^= nanos as u64;
    }
    format!("{}/tmp.{s}", dir.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_tool() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xfb, 0xff, 0xfe]), "+//+");
    }

    #[test]
    fn the_stamp_is_the_utc_date() {
        let at = |s: u64| ymd(UNIX_EPOCH + std::time::Duration::from_secs(s));
        assert_eq!(at(0), "19700101");
        assert_eq!(at(1_700_000_000), "20231114");
        assert_eq!(at(1_709_164_800), "20240229");
        assert_eq!(at(1_709_251_199), "20240229");
        assert_eq!(at(1_709_251_200), "20240301");
    }

    #[test]
    fn dirname_and_basename_as_the_tools() {
        let u = "https://cloud-images.ubuntu.com/noble/current/noble.img";
        assert_eq!(basename(u), "noble.img");
        assert_eq!(dirname(u), "https://cloud-images.ubuntu.com/noble/current");
        assert_eq!(basename("a/b/"), "b");
        assert_eq!(dirname("a/b/"), "a");
        assert_eq!(dirname("x"), ".");
        assert_eq!(dirname("/x"), "/");
    }

    #[test]
    fn version_sort_compares_numbers_as_numbers() {
        let mut v = [
            "ubuntu-24.04-standard_24.04-10_amd64.tar.zst",
            "ubuntu-24.04-standard_24.04-2_amd64.tar.zst",
            "ubuntu-24.04-standard_24.04-1_amd64.tar.zst",
        ];
        v.sort_by(|a, b| version_cmp(a, b));
        assert_eq!(v[2], "ubuntu-24.04-standard_24.04-10_amd64.tar.zst");
        assert_eq!(v[0], "ubuntu-24.04-standard_24.04-1_amd64.tar.zst");
    }
}
