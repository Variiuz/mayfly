//! `mayfly slot <name>` for `kind=vm` slots: the loop behind `mayfly-slot@<name>.service`, which runs one
//! throwaway VM per job. It does what the `cmd_slot` of `mayfly.sh` does, step for step: the same `qm`
//! and `pvesh` calls with the same flags in the same order, the same cloud-init snippet and firewall
//! file, the same hooks, log lines, waits and limits.
//!
//! Which loop runs is chosen per slot (`route`): `engine=rust` in `slots.conf`, else `SLOT_ENGINE` in
//! `pool.env` (default `bash`). A slot of another kind always runs `mayfly.sh`.
//!
//! The JIT runner config reaches only the 0600 cloud-init snippet: not a command line, not a log line.

use crate::cmd_check::template_layout;
use crate::cmd_status::free_ram_gb;
use crate::config::{self, parse_pool_env, slot_opt, Config, Slots};
use crate::github::{AppSlot, SlotApi};
use crate::guest;
use crate::out::{die, R};
use crate::pve::{guest_kind, vmid_node, GuestKind};
use crate::Ctx;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::atomic::{AtomicU32, Ordering};

/// A guest's runner has to be online on GitHub within this many minutes.
const REGISTER_MINUTES: i64 = 10;

// ------------------------------------------------------------------ which loop runs a slot

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Engine {
    Rust,
    Bash,
}

/// `rust` or `bash`; anything else is not an engine.
pub fn parse_engine(v: &str) -> Option<Engine> {
    match v {
        "rust" => Some(Engine::Rust),
        "bash" => Some(Engine::Bash),
        _ => None,
    }
}

/// The engine a slot asks for: its `engine=` key, else `pool` (`SLOT_ENGINE`; empty = bash). An empty
/// `engine=` counts as not set. The second value is the text that is not an engine, when there is one
/// (the slot then runs on bash).
pub fn engine_setting(extra: &str, pool: &str) -> (Engine, Option<String>) {
    let own = slot_opt("engine", extra, "");
    let v = if own.is_empty() { pool } else { own.as_str() };
    if v.is_empty() {
        return (Engine::Bash, None);
    }
    match parse_engine(v) {
        Some(e) => (e, None),
        None => (Engine::Bash, Some(v.to_string())),
    }
}

/// The engine that really runs a slot of this kind: only `kind=vm` has a Rust loop so far.
pub fn engine_for_kind(kind: &str, extra: &str, pool: &str) -> Engine {
    match engine_setting(extra, pool) {
        (Engine::Rust, _) if kind == "vm" => Engine::Rust,
        _ => Engine::Bash,
    }
}

/// What `mayfly slot <name>` does.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// The loop of this binary.
    Rust,
    /// `exec mayfly.sh`, after printing `note` (when there is one).
    Bash { note: Option<String> },
}

/// `SLOT_ENGINE` of pool.env, read on its own like `update_mode`: the last assignment wins, "" when unset.
fn pool_engine(paths: &config::Paths) -> String {
    let text = fs::read(&paths.pool_env)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let (vars, _) = parse_pool_env(&text);
    vars.iter()
        .rev()
        .find(|(k, _)| k == "SLOT_ENGINE")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// Decides between the Rust loop and `mayfly.sh` for a slot. Anything unclear (no such slot, an
/// unreadable file) goes to `mayfly.sh`, which has the messages for it.
pub fn route(paths: &config::Paths, name: &str) -> Route {
    let slots = Slots::read(&paths.slots_conf);
    let Some(slot) = slots.line_for_name(name) else {
        return Route::Bash { note: None };
    };
    let kind = slot_opt("kind", &slot.extra, "vm");
    let pool = pool_engine(paths);
    let (engine, unknown) = engine_setting(&slot.extra, &pool);
    if let Some(v) = unknown {
        return Route::Bash {
            note: Some(format!(
                "slot {name}: engine {v} is not rust or bash; running it on bash"
            )),
        };
    }
    match (engine, kind.as_str()) {
        (Engine::Rust, "vm") => Route::Rust,
        (Engine::Rust, k @ ("pod" | "lxc")) => Route::Bash {
            note: Some(format!(
                "slot {name}: engine=rust does not cover kind={k} yet; running it on bash"
            )),
        },
        _ => Route::Bash { note: None },
    }
}

// ------------------------------------------------------------------ TERM and INT

/// Signals received (counted by the handler) and signals the loop has dealt with.
pub static TERMS: AtomicU32 = AtomicU32::new(0);
pub static TERMS_SEEN: AtomicU32 = AtomicU32::new(0);

/// A signal arrived that the loop has not handled yet. `Pve::nap` ends a wait early on it, as bash `wait`
/// returns when a trapped signal arrives.
pub fn term_pending() -> bool {
    TERMS.load(Ordering::SeqCst) > TERMS_SEEN.load(Ordering::SeqCst)
}

extern "C" fn on_signal(_sig: libc::c_int) {
    TERMS.fetch_add(1, Ordering::SeqCst);
}

/// TERM and INT only count: the loop does the draining at the next step, like the bash trap.
fn install_handlers() {
    // SAFETY: the handler only touches an atomic; `sigaction` gets a zeroed struct with the fields set.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGTERM, libc::SIGINT] {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Where the loop learns that it was asked to stop: a count of signals received and a count handled.
pub struct Stop<'a> {
    pub terms: &'a AtomicU32,
    pub seen: &'a AtomicU32,
}

impl Stop<'static> {
    /// The signals of this process.
    pub fn process() -> Stop<'static> {
        Stop {
            terms: &TERMS,
            seen: &TERMS_SEEN,
        }
    }
}

// ------------------------------------------------------------------ the loop

/// `mayfly slot <name>`: installs the signal handlers and runs the loop against the real GitHub.
pub fn run(cx: &Ctx, args: &[String], w: &mut dyn Write) -> R<i32> {
    let Some(name) = args.first() else {
        return die("usage: mayfly slot <name>");
    };
    install_handlers();
    let now = || crate::github::now() as i64;
    let mut ew = std::io::stderr();
    run_with(cx, name, &AppSlot, &Stop::process(), &now, w, &mut ew)
}

/// The loop with GitHub, the signals and the clock given (`now` is epoch seconds). `w` gets the log lines
/// (stdout in the journal), `ew` the warnings that bash writes to stderr.
pub fn run_with(
    cx: &Ctx,
    name: &str,
    gh: &dyn SlotApi,
    stop: &Stop,
    now: &dyn Fn() -> i64,
    w: &mut dyn Write,
    ew: &mut dyn Write,
) -> R<i32> {
    Loop {
        cx,
        gh,
        stop,
        now,
        w,
        ew,
        name: name.to_string(),
        slot: config::Slot::default(),
        stopping: false,
        stuck: 0,
        ticks: 0,
    }
    .run()
}

enum Flow {
    /// Start the next job.
    Next,
    /// The slot is stopping: leave the loop.
    Done,
    /// A command failed that ends bash (`set -e`): exit with its code.
    Exit(i32),
}

/// Seconds the oldest job in a guest has run, as `job_seconds` of bash.
enum Secs {
    Busy(i64),
    Idle,
    NoAnswer,
}

struct Loop<'a> {
    cx: &'a Ctx<'a>,
    gh: &'a dyn SlotApi,
    stop: &'a Stop<'a>,
    now: &'a dyn Fn() -> i64,
    w: &'a mut dyn Write,
    ew: &'a mut dyn Write,
    name: String,
    slot: config::Slot,
    /// `stopping` of bash: set by the first TERM or INT.
    stopping: bool,
    /// Guests in a row whose runner never came online.
    stuck: i64,
    /// Passes of the wait loop, over all VMs of this run (the pool is checked on every second one).
    ticks: i64,
}

/// Per-slot settings on top of pool.env, as bash `apply_slot`.
struct Applied {
    template: String,
    net0: String,
}

impl Loop<'_> {
    fn log(&mut self, msg: &str) {
        let _ = writeln!(self.w, "{msg}");
        let _ = self.w.flush();
    }

    fn nap(&self, secs: u64) {
        self.cx.pve.nap(secs);
    }

    fn load(&self) -> R<Config> {
        Ok(config::load(self.cx.paths, self.cx.pve, self.cx.node, true)?.0)
    }

    fn kind(&self, id: &str) -> Option<GuestKind> {
        guest_kind(&self.cx.paths.pve_etc, id)
    }

    // ---- TERM and INT

    /// Deals with signals that arrived since the last look, once each, like the bash trap.
    fn poll(&mut self) {
        let t = self.stop.terms.load(Ordering::SeqCst);
        let s = self.stop.seen.load(Ordering::SeqCst);
        if t > s {
            self.stop.seen.store(t, Ordering::SeqCst);
            for _ in s..t {
                self.on_term();
            }
        }
    }

    fn stopping(&mut self) -> bool {
        self.poll();
        self.stopping
    }

    /// An idle VM goes at once; a VM in a job finishes it (systemd waits, TimeoutStopSec).
    fn on_term(&mut self) {
        self.stopping = true;
        let id = self.slot.vmid.clone();
        if self.running(&id) && self.job_idle(&id) {
            if self.kind(&id) == Some(GuestKind::Ct) {
                self.cx.pve.run("pct", &["stop", &id, "--skiplock", "1"]);
            } else {
                self.cx
                    .pve
                    .run("qm", &["stop", &id, "--skiplock", "1", "--timeout", "30"]);
            }
        }
    }

    // ---- the guest

    /// bash `vm_running`.
    fn running(&self, id: &str) -> bool {
        self.cx.pve.guest_running(self.kind(id), id)
    }

    /// bash `guest_exec`: runs a command in the guest, `(exit code, stdout)`; 124 = no answer in time.
    fn guest_exec(&self, id: &str, t: u64, cmd: &[&str]) -> (i32, String) {
        let pve = self.cx.pve;
        let secs = t.to_string();
        if self.kind(id) == Some(GuestKind::Ct) {
            let mut args = vec![secs.as_str(), "lxc-attach", "-n", id, "--"];
            args.extend_from_slice(cmd);
            let r = pve.run("timeout", &args);
            return (r.code, r.stdout);
        }
        let outer = (t + 5).to_string();
        let mut args = vec![
            outer.as_str(),
            "qm",
            "guest",
            "exec",
            id,
            "--timeout",
            secs.as_str(),
            "--",
        ];
        args.extend_from_slice(cmd);
        let r = pve.run("timeout", &args);
        if r.code != 0 {
            return (124, String::new());
        }
        let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap_or_default();
        let code = v
            .get("exitcode")
            .and_then(|c| c.as_i64())
            .map_or(124, |c| c as i32);
        let out = v
            .get("out-data")
            .and_then(|o| o.as_str())
            .unwrap_or("")
            .to_string();
        (code, out)
    }

    /// bash `job_seconds`.
    fn job_seconds(&self, id: &str) -> Secs {
        // 3, not 1, for "no job": lxc-attach itself fails with 1, which must count as no answer.
        let script = "p=$(pgrep -of \"bin/Runner\\.Worker\") || exit 3; ps -o etimes= -p \"$p\"";
        let (code, out) = self.guest_exec(id, 10, &["sh", "-c", script]);
        if code == 3 {
            return Secs::Idle;
        }
        if code != 0 {
            return Secs::NoAnswer;
        }
        let digits: String = out.chars().filter(char::is_ascii_digit).collect();
        match digits.parse::<i64>() {
            Ok(s) => Secs::Busy(s),
            Err(_) => Secs::NoAnswer,
        }
    }

    /// True only when the guest answered and runs no job. A guest that is slow under load counts as busy,
    /// so a stop never cancels a running job.
    fn job_idle(&self, id: &str) -> bool {
        matches!(self.job_seconds(id), Secs::Idle)
    }

    /// bash `job_active`.
    fn job_active(&self, id: &str) -> bool {
        self.cx
            .pve
            .job_active(self.kind(id).unwrap_or(GuestKind::Vm), id)
    }

    /// `destroy_vm`; `Ok(code)` is the exit code of a failing `qm destroy` (bash exits with it).
    fn destroy(&self, c: &Config, id: &str) -> R<i32> {
        guest::destroy(self.cx, c, id)
    }

    fn delete_runner(&self, c: &Config, token: &str, id: &str) {
        if !id.is_empty() {
            self.gh.delete_runner(c, token, id);
        }
    }

    fn remove_snippet(&self, c: &Config, id: &str) {
        let _ = fs::remove_file(format!("{}/mayfly-{id}.yaml", c.snippet_dir));
    }

    // ---- settings

    /// bash `apply_slot`: the slot's own settings over pool.env.
    fn apply_slot(&self, c: &mut Config) -> R<Applied> {
        let x = &self.slot.extra;
        let has = |k: &str| format!(" {x} ").contains(&format!(" {k}="));
        let template = slot_opt("template", x, "");
        let bridge = slot_opt("bridge", x, c.get("BRIDGE"));
        c.set("BRIDGE", &bridge);
        if has("vlan") {
            c.set("VLAN", &slot_opt("vlan", x, ""));
        }
        if has("disk_cache") {
            c.set("DISK_CACHE", &slot_opt("disk_cache", x, ""));
        }
        let cpu = slot_opt("cpu", x, c.get("CPU_TYPE"));
        c.set("CPU_TYPE", &cpu);
        for v in ["max_job_hours", "max_idle_hours"] {
            if has(v) {
                let n = slot_opt(v, x, "");
                if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                    return die(format!(
                        "slot {}: {v}={n} must be a whole number of hours",
                        self.name
                    ));
                }
                c.set(&v.to_ascii_uppercase(), &n);
            }
        }
        let fw = if c.get("FIREWALL") == "yes" { 1 } else { 0 };
        let tag = match c.get("VLAN") {
            "" => String::new(),
            v => format!(",tag={v}"),
        };
        let net0 = format!("virtio,bridge={}{tag},firewall={fw}", c.get("BRIDGE"));
        Ok(Applied { template, net0 })
    }

    /// bash `slot_template`: the slot's `template=`, else current (or next for a canary slot).
    fn slot_template(&self, c: &Config, own: &str) -> String {
        if !own.is_empty() {
            return own.to_string();
        }
        if self.slot.which == "next" {
            let next = c.get("NEXT_TEMPLATE_VMID");
            if next.is_empty() {
                c.get("TEMPLATE_VMID").to_string()
            } else {
                next.to_string()
            }
        } else {
            c.get("TEMPLATE_VMID").to_string()
        }
    }

    /// A whole number of hours from pool.env or the slot.
    fn hours(&self, c: &Config, key: &str) -> R<i64> {
        match c.number(key) {
            Some(h) => Ok(h),
            None => die(format!(
                "{key}={}: must be a whole number of hours",
                c.get(key)
            )),
        }
    }

    // ---- capacity

    /// bash `has_capacity`: room for one more VM of `ram` GB. A pool that cannot be read is no room, silently.
    fn has_capacity(&mut self, c: &Config, ram: i64) -> bool {
        let Some(pct) = self.cx.pve.pool_percent(self.cx.node, c.get("STORAGE")) else {
            return false;
        };
        let free_text = free_ram_gb(self.cx.meminfo);
        let free: i64 = free_text.parse().unwrap_or(0);
        if c.number("MAX_POOL_PERCENT").is_some_and(|m| pct >= m) {
            let msg = format!(
                "waiting: {} is {pct}% full (cap {}%)",
                c.get("STORAGE"),
                c.get("MAX_POOL_PERCENT")
            );
            self.log(&msg);
            return false;
        }
        if c.number("MIN_FREE_RAM_GB").is_some_and(|m| free - ram < m) {
            let msg = format!(
                "waiting: {free_text} GB RAM available, a {} GB VM would leave less than {} GB",
                self.slot.ram,
                c.get("MIN_FREE_RAM_GB")
            );
            self.log(&msg);
            return false;
        }
        true
    }

    // ---- files

    /// The job environment of a slot's env column as KEY=value lines: `no`, `-` or nothing is none, else
    /// the comma-separated names of `<conf>/env/<name>.env`.
    fn job_env(&mut self, spec: &str) -> R<String> {
        if matches!(spec, "no" | "-" | "") {
            return Ok(String::new());
        }
        let mut lines: Vec<String> = Vec::new();
        for n in spec.replace(',', " ").split_whitespace() {
            if !n
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return die(format!(
                    "env name {n}: letters, digits, dash and underscore only"
                ));
            }
            let f = format!("{}/{n}.env", self.cx.paths.env_dir);
            if fs::metadata(&f).map(|m| m.len() > 0).unwrap_or(false) {
                let text = fs::read(&f)
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                for l in text.split('\n') {
                    if is_assignment(l) {
                        lines.push(l.to_string());
                    }
                }
            } else {
                let _ = writeln!(
                    self.ew,
                    "warning: env {n}: {f} missing or empty; jobs run without it"
                );
            }
        }
        Ok(lines.join("\n"))
    }

    /// cloud-init user-data for one job VM. It holds the one-use JIT config, so it is 0600 and goes as soon
    /// as the VM has started.
    fn write_snippet(&mut self, c: &Config, id: &str, host: &str, jit: &str, d: &str) -> R<()> {
        let lines = self.job_env(&self.slot.envspec.clone())?;
        let mut t = String::new();
        t += "#cloud-config\n";
        t += &format!("hostname: {host}\n");
        t += "write_files:\n";
        t += &format!("  - path: /etc/{d}/jit\n");
        t += "    permissions: '0600'\n";
        t += &format!("    content: {jit}\n");
        if !lines.is_empty() {
            t += &format!("  - path: /etc/{d}/job.env\n");
            t += "    permissions: '0600'\n";
            t += "    content: |\n";
            for l in lines.split('\n') {
                t += &format!("      {l}\n");
            }
        }
        // Also start the job from cloud-final, so a template whose unit did not start at boot still works.
        t += "runcmd:\n";
        t += &format!("  - [systemctl, start, --no-block, {d}-job.service]\n");
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

    /// Per-VM firewall: nothing in, DNS to the gateway, CACHE_IP and ALLOW_OUT, nothing to BLOCK_NETS,
    /// the rest out. FIREWALL=no writes none.
    fn write_firewall(&self, c: &Config, id: &str) -> R<()> {
        let path = format!("{}/{id}.fw", self.cx.paths.pve_fw_dir);
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
            Err(e) => die(format!("cannot write {path}: {e}")),
        }
    }

    /// Executables in HOOK_DIR run on events: `vm-up <slot> <vmid> <runner>`, `vm-gone <slot> <vmid>
    /// <minutes>`, `vm-stuck <slot> <vmid> <runner>`. 30 s each, failures only noted.
    fn run_hook(&mut self, c: &Config, args: &[&str]) {
        let dir = c.get("HOOK_DIR");
        let Ok(rd) = fs::read_dir(dir) else {
            return;
        };
        let mut names: Vec<String> = rd
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        for n in names {
            let h = format!("{dir}/{n}");
            let runs = fs::metadata(&h)
                .map(|m| m.is_file() && m.mode() & 0o111 != 0)
                .unwrap_or(false);
            if !runs {
                continue;
            }
            let mut argv = vec!["30", h.as_str()];
            argv.extend_from_slice(args);
            if self.cx.pve.run_null("timeout", &argv) != 0 {
                let msg = format!("warning: hook {n} {} failed", args[0]);
                self.log(&msg);
            }
        }
    }

    // ---- a VM

    /// Disk size in whole GB of a VM's scsi0 (None when unknown), as bash `disk_gb`.
    fn disk_gb(&self, id: &str) -> Option<i64> {
        let cfg = self.cx.pve.run("qm", &["config", id]).stdout;
        for line in cfg.split('\n') {
            let Some(rest) = line.strip_prefix("scsi0:") else {
                continue;
            };
            // `.*[,:]size=\([0-9]*[MGT]\).*`: the last `,size=` or `:size=` followed by digits and a unit.
            let b = rest.as_bytes();
            let mut found = None;
            for at in 0..b.len() {
                if !matches!(b[at], b',' | b':') || !rest[at + 1..].starts_with("size=") {
                    continue;
                }
                let tail = &rest[at + 6..];
                let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
                if matches!(tail.as_bytes().get(digits), Some(b'M' | b'G' | b'T')) {
                    found = Some((&tail[..digits], tail.as_bytes()[digits]));
                }
            }
            let Some((digits, unit)) = found else {
                continue;
            };
            let n: i64 = digits.parse().ok()?;
            return Some(match unit {
                b'G' => n,
                b'T' => n * 1024,
                _ => (n + 1023) / 1024,
            });
        }
        None
    }

    /// bash `grow_disk`: only when the slot wants more than the template has.
    fn grow_disk(&self, id: &str, want: &str) -> bool {
        if let (Some(have), Ok(w)) = (self.disk_gb(id), want.trim().parse::<i64>()) {
            if w <= have {
                return true;
            }
        }
        self.cx
            .pve
            .run_stderr("qm", &["resize", id, "scsi0", &format!("{want}G")])
            == 0
    }

    /// bash `set_disk_cache`: DISK_CACHE on the clone's scsi0 (empty keeps the template's setting).
    fn set_disk_cache(&self, c: &Config, id: &str) -> bool {
        let mode = c.get("DISK_CACHE");
        if mode.is_empty() {
            return true;
        }
        let cfg = self.cx.pve.run_keep_err("qm", &["config", id]).stdout;
        let spec: Vec<String> = cfg
            .split('\n')
            .filter_map(|l| l.strip_prefix("scsi0: "))
            .map(drop_first_cache)
            .collect();
        let spec = spec.join("\n");
        let spec = spec.trim_end_matches('\n');
        if spec.is_empty() {
            return false;
        }
        self.cx.pve.run_stderr(
            "qm",
            &["set", id, "--scsi0", &format!("{spec},cache={mode}")],
        ) == 0
    }

    /// Clone, size and configure the job VM; false at the first step that fails.
    fn create_vm(&self, c: &Config, a: &Applied, tpl: &str, host: &str, runner: &str) -> bool {
        let s = &self.slot;
        let pve = self.cx.pve;
        let id = s.vmid.as_str();
        if pve.run_stderr("qm", &["clone", tpl, id, "--name", host]) != 0 {
            return false;
        }
        let Ok(ram) = s.ram.trim().parse::<i64>() else {
            return false;
        };
        let memory = (ram * 1024).to_string();
        let desc = format!("runner {runner}, template {tpl}");
        let cicustom = format!(
            "user={}:snippets/mayfly-{id}.yaml",
            c.get("SNIPPET_STORAGE")
        );
        let mut set = vec![
            "set",
            id,
            "--cores",
            s.vcpu.as_str(),
            "--memory",
            memory.as_str(),
            "--balloon",
            "0",
            "--onboot",
            "0",
        ];
        if !c.get("CPU_TYPE").is_empty() {
            set.extend(["--cpu", c.get("CPU_TYPE")]);
        }
        set.extend([
            "--tags",
            c.get("TAG"),
            "--description",
            desc.as_str(),
            "--net0",
            a.net0.as_str(),
            "--ipconfig0",
            "ip=dhcp",
            "--cicustom",
            cicustom.as_str(),
        ]);
        pve.run_stderr("qm", &set) == 0 && self.grow_disk(id, &s.disk) && self.set_disk_cache(c, id)
    }

    // ---- the loop

    fn run(&mut self) -> R<i32> {
        let name = self.name.clone();
        let paths = self.cx.paths;
        let mut c = self.load()?;
        let slots = Slots::read(&paths.slots_conf);
        let Some(line) = slots.line_for_name(&name) else {
            return die(format!("no slot {name} in {}", paths.slots_conf));
        };
        self.slot = line.clone();
        if self.slot.which.is_empty() {
            self.slot.which = "current".to_string();
        }
        self.apply_slot(&mut c)?;
        let kind = slot_opt("kind", &self.slot.extra, "vm");
        if kind != "vm" {
            return match kind.as_str() {
                "pod" | "lxc" => die(format!(
                    "slot {name}: kind={kind} runs on the bash loop (mayfly.sh), not here"
                )),
                _ => die(format!("slot {name}: kind={kind} (vm, pod or lxc)")),
            };
        }
        let vmid = self.slot.vmid.clone();
        let (min, max) = (c.number("VMID_MIN"), c.number("VMID_MAX"));
        let in_range = match (vmid.trim().parse::<i64>(), min, max) {
            (Ok(v), Some(lo), Some(hi)) => v >= lo && v <= hi,
            _ => false,
        };
        if !in_range {
            return die(format!(
                "slot {name}: VMID {vmid} outside {}..{}",
                c.get("VMID_MIN"),
                c.get("VMID_MAX")
            ));
        }
        for t in c.get("TEMPLATE_VMIDS").split_whitespace() {
            if vmid == t {
                return die(format!("slot {name}: VMID {vmid} is a template VMID"));
            }
        }
        if let Some(node) = vmid_node(&paths.pve_etc, &vmid).filter(|n| n != self.cx.node) {
            self.log(&format!(
                "slot {name}: VMID {vmid} belongs to node {node}; pick a free VMID in {}",
                paths.slots_conf
            ));
            self.nap(600);
            return Ok(1);
        }
        while !self.stopping() {
            match self.iteration(&mut c)? {
                Flow::Next => {}
                Flow::Done => break,
                Flow::Exit(code) => return Ok(code),
            }
        }
        Ok(0)
    }

    /// One VM, from clone to destroy.
    fn iteration(&mut self, c: &mut Config) -> R<Flow> {
        let name = self.name.clone();
        let vmid = self.slot.vmid.clone();
        let labels = self.slot.labels.clone();
        let code = self.destroy(c, &vmid)?;
        if code != 0 {
            return Ok(Flow::Exit(code));
        }
        *c = self.load()?;
        let applied = self.apply_slot(c)?;
        let tpl = self.slot_template(c, &applied.template);
        if tpl.is_empty() || !guest::is_template(self.cx, &tpl) {
            let shown = if tpl.is_empty() {
                String::new()
            } else {
                format!("{tpl} ")
            };
            self.log(&format!(
                "slot {name}: no template {shown}(TEMPLATE_VMID in {} or template= in {}); run mayfly template build",
                self.cx.paths.pool_env, self.cx.paths.slots_conf
            ));
            self.nap(300);
            return Ok(Flow::Next);
        }
        let ram: i64 = match self.slot.ram.trim().parse() {
            Ok(r) => r,
            Err(_) => {
                return die(format!(
                    "slot {name}: ram {} is not a whole number of GB",
                    self.slot.ram
                ))
            }
        };
        if !self.has_capacity(c, ram) {
            self.nap(60);
            return Ok(Flow::Next);
        }

        // Stopped while waiting for a template or capacity: register nothing, clone nothing.
        if self.stopping() {
            return Ok(Flow::Done);
        }
        let host = format!("{}{name}", c.get("NAME_PREFIX"));
        let runner_name = format!("{}-{name}-{}", c.get("RUNNER_PREFIX"), (self.now)());
        let registered = self.gh.install_token(c).and_then(|token| {
            self.gh
                .jit_config(c, &token, &runner_name, &labels)
                .map(|jit| (token, jit))
        });
        let Ok((token, jit)) = registered else {
            self.log(&format!(
                "slot {name}: GitHub refused the runner registration; retrying in 2 min"
            ));
            self.nap(120);
            return Ok(Flow::Next);
        };
        let runner_id = jit.runner_id;

        let layout = template_layout(self.cx, &tpl);
        self.write_snippet(c, &vmid, &host, &jit.encoded, layout)?;
        drop(jit.encoded);
        if !self.create_vm(c, &applied, &tpl, &host, &runner_name) {
            self.log(&format!(
                "slot {name}: could not create VM {vmid} from template {tpl}"
            ));
            self.remove_snippet(c, &vmid);
            if self.kind(&vmid).is_some() {
                let code = self.destroy(c, &vmid)?;
                if code != 0 {
                    return Ok(Flow::Exit(code));
                }
            }
            self.delete_runner(c, &token, &runner_id);
            self.nap(120);
            return Ok(Flow::Next);
        }
        self.write_firewall(c, &vmid)?;
        if self.stopping() {
            self.remove_snippet(c, &vmid);
            let code = self.destroy(c, &vmid)?;
            if code != 0 {
                return Ok(Flow::Exit(code));
            }
            self.delete_runner(c, &token, &runner_id);
            return Ok(Flow::Done);
        }
        if self.cx.pve.run_stderr("qm", &["start", &vmid]) != 0 {
            self.log(&format!("slot {name}: VM {vmid} did not start"));
            let code = self.destroy(c, &vmid)?;
            if code != 0 {
                return Ok(Flow::Exit(code));
            }
            self.delete_runner(c, &token, &runner_id);
            self.nap(120);
            return Ok(Flow::Next);
        }
        self.remove_snippet(c, &vmid);
        let started = (self.now)();
        self.log(&format!(
            "slot {name}: vm {vmid} up as {runner_name} (template {tpl})"
        ));
        self.run_hook(c, &["vm-up", &name, &vmid, &runner_name]);

        // The VM powers itself off after its one job. Replace it after MAX_IDLE_HOURS without a job, so no
        // job lands on an old VM; stop it once its job has run MAX_JOB_HOURS; and stop it when the slot is
        // stopping and the VM has no job. The guest is only asked once it is MAX_IDLE_HOURS old.
        let idle_h = self.hours(c, "MAX_IDLE_HOURS")?;
        let job_h = self.hours(c, "MAX_JOB_HOURS")?;
        let mut job_since: Option<i64> = None;
        let mut online = false;
        loop {
            // A signal since the last pass drains the VM now (the bash trap), before the guest is looked at.
            self.poll();
            if !self.running(&vmid) {
                break;
            }
            let now = (self.now)();
            // A guest whose runner never comes online (a template whose job unit waits for another file, no
            // network, a broken runner) would hold the slot for MAX_IDLE_HOURS: replace it instead.
            if !online && now - started >= 60 && (now - started) % 60 < 30 {
                let up = match self.gh.install_token(c) {
                    Ok(t) => self.gh.runner_online(c, &t, &runner_id),
                    Err(_) => false,
                };
                if up {
                    online = true;
                    self.stuck = 0;
                } else if now - started >= REGISTER_MINUTES * 60 && !self.job_active(&vmid) {
                    self.stuck += 1;
                    self.log(&format!(
                        "slot {name}: runner {runner_name} not online after {REGISTER_MINUTES} min; replacing vm {vmid} (check its template: mayfly check)"
                    ));
                    self.run_hook(c, &["vm-stuck", &name, &vmid, &runner_name]);
                    break;
                }
            }
            if job_since.is_none() && now - started >= idle_h * 3600 {
                match self.job_seconds(&vmid) {
                    Secs::Busy(secs) => job_since = Some(now - secs),
                    Secs::Idle => {
                        self.log(&format!(
                            "slot {name}: VM {vmid} had no job for {} h; replacing it",
                            c.get("MAX_IDLE_HOURS")
                        ));
                        break;
                    }
                    Secs::NoAnswer => {}
                }
            }
            if job_since.is_some_and(|since| now - since >= job_h * 3600) {
                self.log(&format!(
                    "slot {name}: job in VM {vmid} ran {} h; stopping it (its job is cancelled)",
                    c.get("MAX_JOB_HOURS")
                ));
                break;
            }
            // Backstop for a guest that never answers.
            if now - started >= (idle_h + job_h) * 3600 {
                self.log(&format!(
                    "slot {name}: VM {vmid} ran {} h; stopping it (its job is cancelled)",
                    idle_h + job_h
                ));
                break;
            }
            if self.stopping() && self.job_idle(&vmid) {
                break;
            }
            // The pool is shared with other VMs: a full thin pool would break them too, so job VMs go first.
            self.ticks += 1;
            if self.ticks % 2 == 0 {
                if let Some(pct) = self.cx.pve.pool_percent(self.cx.node, c.get("STORAGE")) {
                    if c.number("POOL_PANIC_PERCENT").is_some_and(|p| pct >= p) {
                        self.log(&format!(
                            "slot {name}: {} is {pct}% full (limit {}%); stopping VM {vmid} (its job is cancelled)",
                            c.get("STORAGE"),
                            c.get("POOL_PANIC_PERCENT")
                        ));
                        break;
                    }
                }
            }
            self.poll();
            self.nap(30);
        }
        let code = self.destroy(c, &vmid)?;
        if code != 0 {
            return Ok(Flow::Exit(code));
        }
        // An ephemeral runner removes itself after its job; this catches one that never got one.
        if let Ok(t) = self.gh.install_token(c) {
            self.delete_runner(c, &t, &runner_id);
        }
        let minutes = ((self.now)() - started) / 60;
        self.log(&format!("slot {name}: VM {vmid} gone after {minutes} min"));
        let minutes = ((self.now)() - started) / 60;
        self.run_hook(c, &["vm-gone", &name, &vmid, &minutes.to_string()]);
        // Guests that never got online again and again: back off instead of cloning every few minutes.
        if self.stuck > 0 && !online && !self.stopping() {
            self.nap((self.stuck * 120).min(1800) as u64);
        }
        Ok(Flow::Next)
    }
}

/// A line of an env file the job gets: `NAME=...` with a shell-style name.
fn is_assignment(l: &str) -> bool {
    let Some(eq) = l.find('=') else {
        return false;
    };
    let name = &l[..eq];
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `sed 's/,cache=[^,]*//'`: the first `,cache=<value>` of the line goes.
fn drop_first_cache(spec: &str) -> String {
    match spec.find(",cache=") {
        Some(at) => {
            let after = &spec[at + 7..];
            let end = after.find(',').unwrap_or(after.len());
            format!("{}{}", &spec[..at], &after[end..])
        }
        None => spec.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_comes_from_the_slot_then_pool_env() {
        assert_eq!(engine_setting("", ""), (Engine::Bash, None));
        assert_eq!(engine_setting("", "rust"), (Engine::Rust, None));
        assert_eq!(engine_setting("engine=rust", ""), (Engine::Rust, None));
        assert_eq!(engine_setting("engine=bash", "rust"), (Engine::Bash, None));
        assert_eq!(
            engine_setting("engine= kind=vm", "rust"),
            (Engine::Rust, None)
        );
        assert_eq!(
            engine_setting("kind=vm engine=rust", "bash"),
            (Engine::Rust, None)
        );
        assert_eq!(
            engine_setting("engine=fast", "rust"),
            (Engine::Bash, Some("fast".to_string()))
        );
        assert_eq!(
            engine_setting("", "Rust"),
            (Engine::Bash, Some("Rust".to_string()))
        );
    }

    #[test]
    fn only_vm_slots_run_on_rust() {
        assert_eq!(engine_for_kind("vm", "engine=rust", ""), Engine::Rust);
        assert_eq!(engine_for_kind("pod", "engine=rust", "rust"), Engine::Bash);
        assert_eq!(engine_for_kind("lxc", "", "rust"), Engine::Bash);
        assert_eq!(engine_for_kind("vm", "", ""), Engine::Bash);
        assert_eq!(engine_for_kind("vm", "engine=x", "rust"), Engine::Bash);
    }

    #[test]
    fn the_first_cache_option_goes() {
        assert_eq!(
            drop_first_cache("local:vm-1-disk-0,cache=none,size=32G"),
            "local:vm-1-disk-0,size=32G"
        );
        assert_eq!(
            drop_first_cache("local:vm-1-disk-0,size=32G,cache=writeback"),
            "local:vm-1-disk-0,size=32G"
        );
        assert_eq!(drop_first_cache("a,size=1G"), "a,size=1G");
    }

    #[test]
    fn env_lines_need_a_shell_name() {
        assert!(is_assignment("A=1"));
        assert!(is_assignment("_a1="));
        assert!(!is_assignment("1A=1"));
        assert!(!is_assignment("# A=1"));
        assert!(!is_assignment("A B=1"));
        assert!(!is_assignment("=1"));
        assert!(!is_assignment("none"));
    }
}
