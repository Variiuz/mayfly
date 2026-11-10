//! The Rust slot loops (`engine=rust`, `kind=vm`, `lxc` and `pod`) against the recording fake Proxmox host and a fake
//! GitHub: whole job cycles with the exact commands in their exact order, the snippet and firewall
//! file, the hooks, the log lines and the drain. Nothing here needs Proxmox, a network or root.

use mayfly::cmd_slot::{self, route, Route, Stop};
use mayfly::config::{self, Config, Paths};
use mayfly::github::{jit_body, AppSlot, Jit, SlotApi};
use mayfly::pve::FakePve;
use mayfly::Ctx;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ------------------------------------------------------------------ helpers

const T0: i64 = 1_700_000_000;
const RUNNER: &str = "mf-a-1-1700000000";
const SLOT_LINE: &str = "a-1 9002 linux 8 16 60 no current\n";
const POOL: &str = "pvesh get /nodes/pve1/storage/local-lvm/status --output-format json";
const PVESM: &str = "pvesm path local:snippets/x";
const STOP: &str = "qm stop 9002 --skiplock 1 --timeout 30";
const DESTROY: &str = "qm destroy 9002 --purge 1 --destroy-unreferenced-disks 1 --skiplock 1";
const CLONE: &str = "qm clone 9098 9002 --name ci-a-1";
const START: &str = "qm start 9002";
const CONFIG: &str = "qm config 9002";
const CONFIG_TPL: &str = "qm config 9098";
const PGREP: &str = "timeout 15 qm guest exec 9002 --timeout 10 -- pgrep -f bin/Runner\\.Worker";
/// What the guest runs to learn how long its job has run (the exit code 3 means no job).
const SCRIPT: &str = "p=$(pgrep -of \"bin/Runner\\.Worker\") || exit 3; ps -o etimes= -p \"$p\"";
const TEMPLATE_CFG: &str =
    "name: ci-template\ntags: mayfly\ndescription: profile default\ntemplate: 1\n";
const VM_CFG: &str = "name: ci-a-1\ntags: mayfly\nscsi0: local-lvm:vm-9002-disk-0,size=32G\n";
const USED_10: &str = "{\"total\":1000,\"used\":100}";
const BUSY: &str = "{\"exitcode\":0,\"out-data\":\"42\\n\"}";
const NO_JOB: &str = "{\"exitcode\":3}";

macro_rules! lines {
    ($($e:expr),* $(,)?) => { vec![$(String::from($e)),*] };
}

fn exec_line() -> String {
    format!("timeout 15 qm guest exec 9002 --timeout 10 -- sh -c {SCRIPT}")
}

fn set_line(cores: &str, memory: &str, cpu: &str, net0: &str, runner: &str) -> String {
    let cpu = if cpu.is_empty() {
        String::new()
    } else {
        format!(" --cpu {cpu}")
    };
    format!(
        "qm set 9002 --cores {cores} --memory {memory} --balloon 0 --onboot 0{cpu} --tags mayfly --description runner {runner}, template 9098 --net0 {net0} --ipconfig0 ip=dhcp --cicustom user=local:snippets/mayfly-9002.yaml"
    )
}

fn default_set() -> String {
    set_line("8", "16384", "", "virtio,bridge=vmbr0,firewall=1", RUNNER)
}

/// What the fake `qm start` saw on disk: the snippet and the firewall file of the new VM.
#[derive(Default)]
struct Seen {
    snippet: Option<String>,
    snippet_mode: Option<u32>,
    firewall: Option<String>,
}

struct Rig {
    root: PathBuf,
    paths: Paths,
    meminfo: String,
    clock: Rc<Cell<i64>>,
    terms: Arc<AtomicU32>,
    seen_terms: Arc<AtomicU32>,
    started: Rc<RefCell<Seen>>,
    /// The destroy of the fake VM asks the loop to stop (so a test ends after its first VM).
    stop_after_destroy: Rc<Cell<bool>>,
}

struct Outcome {
    code: Result<i32, String>,
    out: String,
    err: String,
}

impl Rig {
    fn new(pool_extra: &str, slots: &str) -> Rig {
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "mayfly-slot-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        let r = root.display().to_string();
        for d in ["conf/env", "pve/qemu-server", "pve/lxc", "fw", "snip"] {
            fs::create_dir_all(format!("{r}/{d}")).unwrap();
        }
        let paths = Paths::new(
            &format!("{r}/conf"),
            &format!("{r}/share"),
            &format!("{r}/pve"),
            &format!("{r}/fw"),
        );
        fs::write(
            &paths.pool_env,
            format!("ORG=acme\nRUNNER_PREFIX=mf\nTEMPLATE_VMID=9098\n{pool_extra}"),
        )
        .unwrap();
        fs::write(&paths.slots_conf, slots).unwrap();
        // The template.
        fs::write(format!("{r}/pve/qemu-server/9098.conf"), "").unwrap();
        let meminfo = format!("{r}/meminfo");
        fs::write(
            &meminfo,
            "MemTotal:       65536000 kB\nMemAvailable:   33554432 kB\n",
        )
        .unwrap();
        Rig {
            root,
            paths,
            meminfo,
            clock: Rc::new(Cell::new(T0)),
            terms: Arc::new(AtomicU32::new(0)),
            seen_terms: Arc::new(AtomicU32::new(0)),
            started: Rc::new(RefCell::new(Seen::default())),
            stop_after_destroy: Rc::new(Cell::new(true)),
        }
    }

    fn dir(&self, sub: &str) -> String {
        format!("{}/{sub}", self.root.display())
    }

    fn snippet(&self) -> String {
        self.dir("snip/mayfly-9002.yaml")
    }

    fn hook_dir(&self) -> String {
        format!("{}/hooks.d", self.paths.conf_dir)
    }

    /// An executable (or not) file in the hook directory.
    fn hook(&self, name: &str, mode: u32) {
        fs::create_dir_all(self.hook_dir()).unwrap();
        let p = format!("{}/{name}", self.hook_dir());
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn hook_line(&self, name: &str, args: &str) -> String {
        format!("timeout 30 {}/{name} {args}", self.hook_dir())
    }

    /// A signal arrives: TERM or INT, once.
    fn signal(&self) -> impl Fn() + 'static {
        let t = Arc::clone(&self.terms);
        move || {
            if t.load(Ordering::SeqCst) == 0 {
                t.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn advance(&self, by: i64) -> impl Fn() + 'static {
        let c = Rc::clone(&self.clock);
        move || c.set(c.get() + by)
    }

    /// The host as a slot sees it: the template, a pool at 10%, `qm clone` that makes the VM's config
    /// file and `qm destroy` that removes it, and a VM whose qemu process answers `states` (one answer
    /// per question, the last one repeating). Every `sleep 30` moves the clock 30 s.
    fn pve(&self, states: &[bool]) -> FakePve {
        let conf = self.dir("pve/qemu-server/9002.conf");
        let conf2 = conf.clone();
        let (snip, fw) = (self.snippet(), self.dir("fw/9002.fw"));
        let seen = Rc::clone(&self.started);
        let stop = Rc::clone(&self.stop_after_destroy);
        let signal = self.signal();
        FakePve::new()
            .answer(PVESM, &format!("{}/x\n", self.dir("snip")))
            .answer(CONFIG_TPL, TEMPLATE_CFG)
            .answer(POOL, USED_10)
            .answer(CONFIG, VM_CFG)
            .answer(CLONE, "")
            .on_call(CLONE, move || fs::write(&conf, "").unwrap())
            .answer(&default_set(), "")
            .answer("qm resize 9002 scsi0 60G", "")
            .answer(START, "")
            .on_call(START, move || {
                let mut s = seen.borrow_mut();
                s.snippet = fs::read_to_string(&snip).ok();
                s.snippet_mode = fs::metadata(&snip).ok().map(|m| m.mode() & 0o777);
                s.firewall = fs::read_to_string(&fw).ok();
            })
            .answer(STOP, "")
            .answer(DESTROY, "")
            .on_call(DESTROY, move || {
                let _ = fs::remove_file(&conf2);
                if stop.get() {
                    signal();
                }
            })
            .with_qemu_states("9002", states)
            .on_call("sleep 30", self.advance(30))
    }

    fn run(&self, pve: &FakePve, gh: &FakeGh) -> Outcome {
        self.run_slot(pve, gh, "a-1")
    }

    fn run_slot(&self, pve: &FakePve, gh: &FakeGh, name: &str) -> Outcome {
        let cx = Ctx {
            paths: &self.paths,
            pve,
            node: "pve1",
            meminfo: &self.meminfo,
        };
        let stop = Stop {
            terms: &self.terms,
            seen: &self.seen_terms,
        };
        let clock = Rc::clone(&self.clock);
        let now = move || clock.get();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code =
            cmd_slot::run_with(&cx, name, gh, &stop, &now, &mut out, &mut err).map_err(|d| d.0);
        Outcome {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A GitHub that hands out one token, one runner (id 77) and records what it is asked.
struct FakeGh {
    calls: RefCell<Vec<String>>,
    token_fails: Cell<u32>,
    jit_fails: Cell<u32>,
    online: Cell<bool>,
}

impl FakeGh {
    fn new() -> FakeGh {
        FakeGh {
            calls: RefCell::new(vec![]),
            token_fails: Cell::new(0),
            jit_fails: Cell::new(0),
            online: Cell::new(false),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl SlotApi for FakeGh {
    fn install_token(&self, _c: &Config) -> Result<String, String> {
        self.calls.borrow_mut().push("token".to_string());
        if self.token_fails.get() > 0 {
            self.token_fails.set(self.token_fails.get() - 1);
            return Err("HTTP 401".to_string());
        }
        Ok("tok".to_string())
    }

    fn jit_config(
        &self,
        _c: &Config,
        token: &str,
        name: &str,
        labels: &str,
    ) -> Result<Jit, String> {
        self.calls
            .borrow_mut()
            .push(format!("jit {token} {name} {labels}"));
        if self.jit_fails.get() > 0 {
            self.jit_fails.set(self.jit_fails.get() - 1);
            return Err("HTTP 422".to_string());
        }
        Ok(Jit {
            encoded: "JITSECRET".to_string(),
            runner_id: "77".to_string(),
        })
    }

    fn runner_online(&self, _c: &Config, token: &str, id: &str) -> bool {
        self.calls.borrow_mut().push(format!("online {id} {token}"));
        self.online.get()
    }

    fn delete_runner(&self, _c: &Config, token: &str, id: &str) {
        self.calls.borrow_mut().push(format!("delete {id} {token}"));
    }
}

const SNIPPET: &str = "\
#cloud-config
hostname: ci-a-1
write_files:
  - path: /etc/mayfly/jit
    permissions: '0600'
    content: JITSECRET
runcmd:
  - [systemctl, start, --no-block, mayfly-job.service]
";

fn no_secret(o: &Outcome, pve: &FakePve, gh: &FakeGh) {
    assert!(!o.out.contains("JITSECRET"), "{}", o.out);
    assert!(!o.err.contains("JITSECRET"), "{}", o.err);
    for c in pve.calls().iter().chain(gh.calls().iter()) {
        assert!(!c.contains("JITSECRET"), "{c}");
    }
}

// ------------------------------------------------------------------ a whole job cycle

#[test]
fn a_job_cycle_from_clone_to_runner_delete() {
    let rig = Rig::new("", SLOT_LINE);
    rig.hook("10-notify", 0o755);
    let pve = rig
        .pve(&[true, true, false])
        .answer(
            &rig.hook_line("10-notify", &format!("vm-up a-1 9002 {RUNNER}")),
            "",
        )
        .answer(&rig.hook_line("10-notify", "vm-gone a-1 9002 1"), "");
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        format!(
            "slot a-1: vm 9002 up as {RUNNER} (template 9098)\nslot a-1: VM 9002 gone after 1 min\n"
        )
    );
    assert_eq!(o.err, "");
    assert_eq!(
        pve.calls(),
        lines![
            // load_env, then the loop's own load_env.
            PVESM,
            PVESM,
            CONFIG_TPL,
            POOL,
            // template_layout.
            CONFIG_TPL,
            CLONE,
            default_set(),
            // grow_disk: the template's disk is 32G, the slot wants 60G.
            CONFIG,
            "qm resize 9002 scsi0 60G",
            START,
            rig.hook_line("10-notify", &format!("vm-up a-1 9002 {RUNNER}")),
            "sleep 30",
            // The pool is looked at on every second pass.
            POOL,
            "sleep 30",
            // The VM powered itself off: destroy (ours, not a template).
            CONFIG,
            CONFIG,
            STOP,
            DESTROY,
            rig.hook_line("10-notify", "vm-gone a-1 9002 1"),
        ]
    );
    assert_eq!(
        gh.calls(),
        lines![
            "token",
            format!("jit tok {RUNNER} linux"),
            "token",
            "delete 77 tok"
        ]
    );
    // The snippet existed with the JIT config and mode 0600 when the VM started; it is gone now.
    let seen = rig.started.borrow();
    assert_eq!(seen.snippet.as_deref(), Some(SNIPPET));
    assert_eq!(seen.snippet_mode, Some(0o600));
    assert!(!Path::new(&rig.snippet()).exists());
    assert_eq!(
        seen.firewall.as_deref(),
        Some(
            "[OPTIONS]\nenable: 1\ndhcp: 1\npolicy_in: DROP\npolicy_out: ACCEPT\n\n[RULES]\n\
             OUT DROP -dest 10.0.0.0/8\nOUT DROP -dest 172.16.0.0/12\nOUT DROP -dest 192.168.0.0/16\n\
             OUT DROP -dest 100.64.0.0/10\nOUT DROP -dest 169.254.0.0/16\n"
        )
    );
    assert!(!Path::new(&rig.dir("fw/9002.fw")).exists());
    no_secret(&o, &pve, &gh);
}

#[test]
fn the_snippet_carries_the_job_env_and_the_polara_ci_layout() {
    let rig = Rig::new("", "a-1 9002 linux 8 16 60 cache,missing current\n");
    fs::write(
        format!("{}/cache.env", rig.paths.env_dir),
        "A=1\n# comment\nnot a line\nB=two words\n1X=no\n",
    )
    .unwrap();
    let pve = rig
        .pve(&[true, false])
        .answer(CONFIG_TPL, "name: ci-template\ntemplate: 1\n");
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        rig.started.borrow().snippet.as_deref(),
        Some(
            "#cloud-config\nhostname: ci-a-1\nwrite_files:\n  - path: /etc/polara-ci/jit\n    permissions: '0600'\n    content: JITSECRET\n  - path: /etc/polara-ci/job.env\n    permissions: '0600'\n    content: |\n      A=1\n      B=two words\nruncmd:\n  - [systemctl, start, --no-block, polara-ci-job.service]\n"
        )
    );
    assert_eq!(
        o.err,
        format!(
            "warning: env missing: {}/missing.env missing or empty; jobs run without it\n",
            rig.paths.env_dir
        )
    );
}

#[test]
fn a_bad_env_name_ends_the_slot() {
    let rig = Rig::new("", "a-1 9002 linux 8 16 60 ../x current\n");
    let pve = rig.pve(&[true, false]);
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(
        o.code,
        Err("env name ../x: letters, digits, dash and underscore only".to_string())
    );
    assert!(!pve.calls().contains(&CLONE.to_string()));
}

#[test]
fn the_firewall_lists_gateway_cache_allowed_and_blocked() {
    let rig = Rig::new(
        "GATEWAY=10.0.0.1\nCACHE_IP=10.0.0.5\nALLOW_OUT=\"10.1.0.0/24 10.2.0.1\"\nBLOCK_NETS=\"10.0.0.0/8 192.168.0.0/16\"\n",
        SLOT_LINE,
    );
    let pve = rig.pve(&[true, false]);
    rig.run(&pve, &FakeGh::new());
    assert_eq!(
        rig.started.borrow().firewall.as_deref(),
        Some(
            "[OPTIONS]\nenable: 1\ndhcp: 1\npolicy_in: DROP\npolicy_out: ACCEPT\n\n[RULES]\n\
             OUT ACCEPT -dest 10.0.0.1 -p udp -dport 53\nOUT ACCEPT -dest 10.0.0.1 -p tcp -dport 53\n\
             OUT ACCEPT -dest 10.0.0.5\nOUT ACCEPT -dest 10.1.0.0/24\nOUT ACCEPT -dest 10.2.0.1\n\
             OUT DROP -dest 10.0.0.0/8\nOUT DROP -dest 192.168.0.0/16\n"
        )
    );
}

#[test]
fn firewall_no_writes_no_file_and_net0_says_so() {
    let rig = Rig::new("FIREWALL=no\n", SLOT_LINE);
    fs::write(rig.dir("fw/9002.fw"), "stale").unwrap();
    let net0 = "virtio,bridge=vmbr0,firewall=0";
    let pve = rig
        .pve(&[true, false])
        .answer(&set_line("8", "16384", "", net0, RUNNER), "");
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert_eq!(rig.started.borrow().firewall, None);
    assert!(pve
        .calls()
        .contains(&set_line("8", "16384", "", net0, RUNNER)));
}

#[test]
fn slot_settings_reach_qm_set_and_the_disk() {
    // bridge, vlan and cpu of the slot over pool.env; DISK_CACHE on the clone; no grow for a 32G slot.
    let rig = Rig::new(
        "BRIDGE=vmbr0\nCPU_TYPE=host\nDISK_CACHE=unsafe\n",
        "a-1 9002 linux 4 8 32 no current bridge=vmbr2 vlan=42 cpu=x86-64-v3\n",
    );
    let net0 = "virtio,bridge=vmbr2,tag=42,firewall=1";
    let set = set_line("4", "8192", "x86-64-v3", net0, RUNNER);
    let pve = rig.pve(&[true, false]).answer(&set, "").answer(
        "qm set 9002 --scsi0 local-lvm:vm-9002-disk-0,size=32G,cache=unsafe",
        "",
    );
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == CLONE).unwrap();
    assert_eq!(
        calls[at..at + 6].to_vec(),
        lines![
            CLONE,
            set,
            // disk_gb: 32G is enough, no resize. set_disk_cache reads the config again.
            CONFIG,
            CONFIG,
            "qm set 9002 --scsi0 local-lvm:vm-9002-disk-0,size=32G,cache=unsafe",
            START
        ]
    );
}

#[test]
fn a_cache_option_in_the_template_disk_is_replaced() {
    let rig = Rig::new(
        "DISK_CACHE=writeback\n",
        "a-1 9002 linux 8 16 32 no current\n",
    );
    let pve = rig
        .pve(&[true, false])
        .answer(
            CONFIG,
            "name: ci-a-1\ntags: mayfly\nscsi0: local-lvm:vm-9002-disk-0,cache=none,size=32G\n",
        )
        .answer(
            "qm set 9002 --scsi0 local-lvm:vm-9002-disk-0,size=32G,cache=writeback",
            "",
        );
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert!(pve.calls().contains(
        &"qm set 9002 --scsi0 local-lvm:vm-9002-disk-0,size=32G,cache=writeback".to_string()
    ));
}

#[test]
fn a_larger_disk_in_tib_or_mib_is_read_as_gb() {
    for (size, resized) in [("1T", false), ("512M", true)] {
        let rig = Rig::new("", "a-1 9002 linux 8 16 60 no current\n");
        let pve = rig.pve(&[true, false]).answer(
            CONFIG,
            &format!("name: ci-a-1\ntags: mayfly\nscsi0: local-lvm:vm-9002-disk-0,size={size}\n"),
        );
        rig.run(&pve, &FakeGh::new());
        assert_eq!(
            pve.calls()
                .contains(&"qm resize 9002 scsi0 60G".to_string()),
            resized,
            "{size}"
        );
    }
}

// ------------------------------------------------------------------ nothing to do yet

#[test]
fn no_template_waits_five_minutes() {
    let rig = Rig::new("TEMPLATE_VMID=\n", SLOT_LINE);
    let pve = rig.pve(&[]).on_call("sleep 300", rig.signal());
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        format!(
            "slot a-1: no template (TEMPLATE_VMID in {} or template= in {}); run mayfly template build\n",
            rig.paths.pool_env, rig.paths.slots_conf
        )
    );
    assert_eq!(pve.calls(), lines![PVESM, PVESM, "sleep 300"]);
    assert!(gh.calls().is_empty());
}

#[test]
fn a_template_that_is_not_one_is_named() {
    let rig = Rig::new("", "a-1 9002 linux 8 16 60 no current template=9097\n");
    let pve = rig.pve(&[]).on_call("sleep 300", rig.signal());
    let o = rig.run(&pve, &FakeGh::new());
    assert!(o
        .out
        .starts_with("slot a-1: no template 9097 (TEMPLATE_VMID in "));
    // 9097 has no config file here, so nothing is asked of qm.
    assert_eq!(pve.calls(), lines![PVESM, PVESM, "sleep 300"]);
}

#[test]
fn a_canary_slot_clones_the_next_template() {
    let rig = Rig::new(
        "NEXT_TEMPLATE_VMID=9099\n",
        "a-1 9002 linux 8 16 60 no next\n",
    );
    // 9099 is no template: the slot says so, which shows which one it looked at.
    let pve = rig.pve(&[]).on_call("sleep 300", rig.signal());
    let o = rig.run(&pve, &FakeGh::new());
    assert!(
        o.out.starts_with("slot a-1: no template 9099 ("),
        "{}",
        o.out
    );
}

#[test]
fn a_full_pool_waits_a_minute() {
    let rig = Rig::new("", SLOT_LINE);
    let pve = rig
        .pve(&[])
        .answer(POOL, "{\"total\":1000,\"used\":800}")
        .on_call("sleep 60", rig.signal());
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.out, "waiting: local-lvm is 80% full (cap 70%)\n");
    assert_eq!(
        pve.calls(),
        lines![PVESM, PVESM, CONFIG_TPL, POOL, "sleep 60"]
    );
    assert!(gh.calls().is_empty());
}

#[test]
fn too_little_ram_waits_a_minute() {
    let rig = Rig::new("", SLOT_LINE);
    fs::write(&rig.meminfo, "MemAvailable:   20971520 kB\n").unwrap();
    let pve = rig.pve(&[]).on_call("sleep 60", rig.signal());
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(
        o.out,
        "waiting: 20 GB RAM available, a 16 GB VM would leave less than 8 GB\n"
    );
    assert!(!pve.calls().contains(&CLONE.to_string()));
}

#[test]
fn an_unreadable_pool_waits_without_a_word() {
    let rig = Rig::new("", SLOT_LINE);
    let pve = rig.pve(&[]).fail(POOL, 1).on_call("sleep 60", rig.signal());
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.out, "");
    assert_eq!(
        pve.calls(),
        lines![PVESM, PVESM, CONFIG_TPL, POOL, "sleep 60"]
    );
}

#[test]
fn github_refusing_the_registration_is_retried_after_two_minutes() {
    let rig = Rig::new("", SLOT_LINE);
    let naps = Rc::new(Cell::new(0));
    let n = Rc::clone(&naps);
    let pve = rig
        .pve(&[true, true, false])
        .on_call("sleep 120", move || n.set(n.get() + 1));
    let gh = FakeGh::new();
    gh.token_fails.set(1);
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        format!(
            "slot a-1: GitHub refused the runner registration; retrying in 2 min\n\
             slot a-1: vm 9002 up as {RUNNER} (template 9098)\n\
             slot a-1: VM 9002 gone after 1 min\n"
        )
    );
    assert_eq!(naps.get(), 1);
    assert_eq!(
        gh.calls(),
        lines![
            "token",
            "token",
            format!("jit tok {RUNNER} linux"),
            "token",
            "delete 77 tok"
        ]
    );
    // Nothing was cloned for the refused round.
    let clones = pve.calls().iter().filter(|c| *c == CLONE).count();
    assert_eq!(clones, 1);
}

#[test]
fn a_refused_jit_config_is_the_same_retry() {
    let rig = Rig::new("", SLOT_LINE);
    let pve = rig.pve(&[]).on_call("sleep 120", rig.signal());
    let gh = FakeGh::new();
    gh.jit_fails.set(1);
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.out,
        "slot a-1: GitHub refused the runner registration; retrying in 2 min\n"
    );
    assert_eq!(
        gh.calls(),
        lines!["token", format!("jit tok {RUNNER} linux")]
    );
    assert!(!pve.calls().contains(&CLONE.to_string()));
    assert!(!Path::new(&rig.snippet()).exists());
}

#[test]
fn a_clone_that_fails_is_cleaned_up_and_retried() {
    let rig = Rig::new("", SLOT_LINE);
    let pve = rig.pve(&[]).fail(&default_set(), 1);
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        "slot a-1: could not create VM 9002 from template 9098\n"
    );
    assert_eq!(
        pve.calls(),
        lines![
            PVESM,
            PVESM,
            CONFIG_TPL,
            POOL,
            CONFIG_TPL,
            CLONE,
            default_set(),
            // The clone exists, so it is destroyed; its runner goes.
            CONFIG,
            CONFIG,
            STOP,
            DESTROY,
            "sleep 120"
        ]
    );
    assert_eq!(
        gh.calls(),
        lines!["token", format!("jit tok {RUNNER} linux"), "delete 77 tok"]
    );
    assert!(!Path::new(&rig.snippet()).exists());
}

#[test]
fn a_vm_that_does_not_start_is_destroyed() {
    let rig = Rig::new("", SLOT_LINE);
    let pve = rig.pve(&[]).fail(START, 1);
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.out, "slot a-1: VM 9002 did not start\n");
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == START).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![START, CONFIG, CONFIG, STOP, DESTROY, "sleep 120"]
    );
    assert_eq!(gh.calls().last().map(String::as_str), Some("delete 77 tok"));
    // destroy removes the snippet and the firewall file.
    assert!(!Path::new(&rig.snippet()).exists());
    assert!(!Path::new(&rig.dir("fw/9002.fw")).exists());
}

// ------------------------------------------------------------------ draining

#[test]
fn a_stop_while_the_vm_is_idle_stops_it_at_once() {
    let rig = Rig::new("", SLOT_LINE);
    rig.stop_after_destroy.set(false);
    let pve = rig
        .pve(&[true, true, false])
        .answer(&exec_line(), NO_JOB)
        .on_call("sleep 30", rig.signal());
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        format!(
            "slot a-1: vm 9002 up as {RUNNER} (template 9098)\nslot a-1: VM 9002 gone after 0 min\n"
        )
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == START).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            START,
            "sleep 30",
            // The signal: is a job running? No: stop the VM.
            exec_line(),
            STOP,
            // Then the VM is gone: destroy, which stops it once more.
            CONFIG,
            CONFIG,
            STOP,
            DESTROY
        ]
    );
    // And the slot does not start another VM.
    assert_eq!(pve.calls().iter().filter(|c| *c == CLONE).count(), 1);
    assert_eq!(gh.calls().last().map(String::as_str), Some("delete 77 tok"));
}

#[test]
fn a_stop_while_a_job_runs_lets_the_job_finish() {
    let rig = Rig::new("", SLOT_LINE);
    rig.stop_after_destroy.set(false);
    let pve = rig
        .pve(&[true, true, true, false])
        .answer(&exec_line(), BUSY)
        .on_call("sleep 30", rig.signal());
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == START).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            START,
            "sleep 30",
            // The signal: a job runs, nothing is stopped. The loop looks again while stopping.
            exec_line(),
            exec_line(),
            POOL,
            "sleep 30",
            // The job ended and the VM powered off.
            CONFIG,
            CONFIG,
            STOP,
            DESTROY
        ]
    );
    assert_eq!(
        o.out,
        format!(
            "slot a-1: vm 9002 up as {RUNNER} (template 9098)\nslot a-1: VM 9002 gone after 0 min\n"
        )
    );
    assert_eq!(pve.calls().iter().filter(|c| *c == CLONE).count(), 1);
}

#[test]
fn a_guest_that_does_not_answer_counts_as_busy_when_stopping() {
    let rig = Rig::new("", SLOT_LINE);
    rig.stop_after_destroy.set(false);
    // The exec call is not answered: 127, which is "no answer", not "no job".
    let pve = rig
        .pve(&[true, true, false])
        .on_call("sleep 30", rig.signal());
    rig.run(&pve, &FakeGh::new());
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == START).unwrap();
    // Between the first sleep and the destroy there is no `qm stop` outside destroy's own.
    let stops = calls[at..].iter().filter(|c| *c == STOP).count();
    assert_eq!(stops, 1);
}

#[test]
fn a_signal_before_the_first_vm_starts_nothing() {
    let rig = Rig::new("", SLOT_LINE);
    rig.terms.store(1, Ordering::SeqCst);
    let pve = rig.pve(&[]);
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(o.out, "");
    assert_eq!(pve.calls(), lines![PVESM]);
    assert!(gh.calls().is_empty());
}

#[test]
fn a_signal_while_registering_leaves_no_vm_and_no_runner() {
    let rig = Rig::new("", SLOT_LINE);
    // It arrives while the VM is cloned: the loop sees it before `qm start`.
    let conf = rig.dir("pve/qemu-server/9002.conf");
    let signal = rig.signal();
    let pve = rig.pve(&[]).on_call(CLONE, move || {
        fs::write(&conf, "").unwrap();
        signal();
    });
    rig.stop_after_destroy.set(false);
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    let calls = pve.calls();
    assert!(!calls.contains(&START.to_string()));
    assert_eq!(calls.last().map(String::as_str), Some(DESTROY), "{calls:?}");
    assert_eq!(gh.calls().last().map(String::as_str), Some("delete 77 tok"));
    assert!(!Path::new(&rig.snippet()).exists());
}

// ------------------------------------------------------------------ limits

#[test]
fn a_job_over_max_job_hours_is_stopped() {
    let rig = Rig::new(
        "",
        "a-1 9002 linux 8 16 60 no current max_job_hours=1 max_idle_hours=0\n",
    );
    let pve = rig
        .pve(&[true, true, true, false])
        .answer(&exec_line(), "{\"exitcode\":0,\"out-data\":\"100\\n\"}")
        .on_call("sleep 30", rig.advance(1800));
    let gh = FakeGh::new();
    gh.online.set(true);
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        format!(
            "slot a-1: vm 9002 up as {RUNNER} (template 9098)\n\
             slot a-1: job in VM 9002 ran 1 h; stopping it (its job is cancelled)\n\
             slot a-1: VM 9002 gone after 60 min\n"
        )
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == START).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            START,
            // max_idle_hours=0: the guest is asked at once how long its job has run.
            exec_line(),
            "sleep 30",
            POOL,
            "sleep 30",
            CONFIG,
            CONFIG,
            STOP,
            DESTROY
        ]
    );
}

#[test]
fn a_guest_that_never_answers_is_stopped_after_idle_plus_job_hours() {
    let rig = Rig::new(
        "",
        "a-1 9002 linux 8 16 60 no current max_job_hours=1 max_idle_hours=1\n",
    );
    // The exec call is not answered at all.
    let pve = rig
        .pve(&[true, true, true, false])
        .on_call("sleep 30", rig.advance(3600));
    let gh = FakeGh::new();
    gh.online.set(true);
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.out,
        format!(
            "slot a-1: vm 9002 up as {RUNNER} (template 9098)\n\
             slot a-1: VM 9002 ran 2 h; stopping it (its job is cancelled)\n\
             slot a-1: VM 9002 gone after 120 min\n"
        )
    );
}

#[test]
fn a_pool_over_the_panic_limit_stops_the_vm() {
    let rig = Rig::new("", SLOT_LINE);
    let pve = rig
        .pve(&[true, true, false])
        .sequence(POOL, &[(0, USED_10), (0, "{\"total\":1000,\"used\":900}")]);
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        format!(
            "slot a-1: vm 9002 up as {RUNNER} (template 9098)\n\
             slot a-1: local-lvm is 90% full (limit 85%); stopping VM 9002 (its job is cancelled)\n\
             slot a-1: VM 9002 gone after 0 min\n"
        )
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == START).unwrap();
    // The first pass only sleeps; the second looks at the pool and stops.
    assert_eq!(
        calls[at..].to_vec(),
        lines![START, "sleep 30", POOL, CONFIG, CONFIG, STOP, DESTROY]
    );
}

#[test]
fn a_vm_with_no_job_for_max_idle_hours_is_replaced() {
    let rig = Rig::new("", "a-1 9002 linux 8 16 60 no current max_idle_hours=1\n");
    let pve = rig
        .pve(&[true, true, false])
        .answer(&exec_line(), NO_JOB)
        .on_call("sleep 30", rig.advance(3600));
    let gh = FakeGh::new();
    gh.online.set(true);
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.out,
        format!(
            "slot a-1: vm 9002 up as {RUNNER} (template 9098)\n\
             slot a-1: VM 9002 had no job for 1 h; replacing it\n\
             slot a-1: VM 9002 gone after 60 min\n"
        )
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == START).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            START,
            "sleep 30",
            exec_line(),
            CONFIG,
            CONFIG,
            STOP,
            DESTROY
        ]
    );
}

// ------------------------------------------------------------------ the runner-online watchdog

#[test]
fn a_runner_that_never_comes_online_is_replaced_with_a_backoff() {
    let rig = Rig::new("", SLOT_LINE);
    rig.hook("10-notify", 0o755);
    rig.stop_after_destroy.set(false);
    let up = rig.hook_line("10-notify", &format!("vm-up a-1 9002 {RUNNER}"));
    let stuck = rig.hook_line("10-notify", &format!("vm-stuck a-1 9002 {RUNNER}"));
    let gone = rig.hook_line("10-notify", "vm-gone a-1 9002 10");
    let pve = rig
        .pve(&[true, true, true, false])
        .answer(&up, "")
        .answer(&stuck, "")
        .answer(&gone, "")
        .on_call("sleep 30", rig.advance(300))
        .on_call("sleep 120", rig.signal());
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        format!(
            "slot a-1: vm 9002 up as {RUNNER} (template 9098)\n\
             slot a-1: runner {RUNNER} not online after 10 min; replacing vm 9002 (check its template: mayfly check)\n\
             slot a-1: VM 9002 gone after 10 min\n"
        )
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == START).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            START,
            up,
            "sleep 30",
            POOL,
            "sleep 30",
            // 10 minutes in, still offline: is a job running? (The guest does not answer: no.)
            PGREP,
            stuck,
            CONFIG,
            CONFIG,
            STOP,
            DESTROY,
            gone,
            // One stuck guest: 2 minutes of backoff.
            "sleep 120"
        ]
    );
    assert_eq!(
        gh.calls(),
        lines![
            "token",
            format!("jit tok {RUNNER} linux"),
            // Checked at 5 and at 10 minutes.
            "token",
            "online 77 tok",
            "token",
            "online 77 tok",
            "token",
            "delete 77 tok"
        ]
    );
}

#[test]
fn a_runner_that_is_online_is_asked_once() {
    let rig = Rig::new("", SLOT_LINE);
    let pve = rig
        .pve(&[true, true, false])
        .on_call("sleep 30", rig.advance(60));
    let gh = FakeGh::new();
    gh.online.set(true);
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.out,
        format!(
            "slot a-1: vm 9002 up as {RUNNER} (template 9098)\nslot a-1: VM 9002 gone after 2 min\n"
        )
    );
    assert_eq!(
        gh.calls(),
        lines![
            "token",
            format!("jit tok {RUNNER} linux"),
            "token",
            "online 77 tok",
            "token",
            "delete 77 tok"
        ]
    );
}

#[test]
fn a_stuck_guest_whose_job_runs_is_left_alone() {
    // 10 minutes without a runner online, but the guest runs a job: not replaced (and no stuck count).
    let rig = Rig::new("", SLOT_LINE);
    let pve = rig
        .pve(&[true, true, true, false])
        .answer(PGREP, "{\"exitcode\":0,\"out-data\":\"1\\n\"}")
        .on_call("sleep 30", rig.advance(300));
    let o = rig.run(&pve, &FakeGh::new());
    assert!(!o.out.contains("not online"), "{}", o.out);
    assert!(pve.calls().contains(&PGREP.to_string()));
}

// ------------------------------------------------------------------ hooks

#[test]
fn hooks_run_in_order_with_their_args_and_a_failure_is_noted() {
    let rig = Rig::new("", SLOT_LINE);
    rig.hook("10-a", 0o755);
    rig.hook("20-fail", 0o755);
    rig.hook("30-not-executable", 0o644);
    rig.hook(".hidden", 0o755);
    fs::create_dir_all(format!("{}/40-dir", rig.hook_dir())).unwrap();
    let pve = rig
        .pve(&[true, true, false])
        .answer(
            &rig.hook_line("10-a", &format!("vm-up a-1 9002 {RUNNER}")),
            "",
        )
        .answer(&rig.hook_line("10-a", "vm-gone a-1 9002 1"), "")
        .fail(
            &rig.hook_line("20-fail", &format!("vm-up a-1 9002 {RUNNER}")),
            1,
        )
        .fail(&rig.hook_line("20-fail", "vm-gone a-1 9002 1"), 124);
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(
        o.out,
        format!(
            "slot a-1: vm 9002 up as {RUNNER} (template 9098)\n\
             warning: hook 20-fail vm-up failed\n\
             slot a-1: VM 9002 gone after 1 min\n\
             warning: hook 20-fail vm-gone failed\n"
        )
    );
    let hooks: Vec<String> = pve
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("timeout 30 "))
        .collect();
    assert_eq!(
        hooks,
        lines![
            rig.hook_line("10-a", &format!("vm-up a-1 9002 {RUNNER}")),
            rig.hook_line("20-fail", &format!("vm-up a-1 9002 {RUNNER}")),
            rig.hook_line("10-a", "vm-gone a-1 9002 1"),
            rig.hook_line("20-fail", "vm-gone a-1 9002 1"),
        ]
    );
}

#[test]
fn hooks_come_from_hook_dir() {
    let rig = Rig::new("", SLOT_LINE);
    let other = rig.dir("elsewhere");
    fs::create_dir_all(&other).unwrap();
    fs::write(format!("{other}/up"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(format!("{other}/up"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        &rig.paths.pool_env,
        format!("ORG=acme\nRUNNER_PREFIX=mf\nTEMPLATE_VMID=9098\nHOOK_DIR={other}\n"),
    )
    .unwrap();
    let pve = rig.pve(&[true, false]);
    rig.run(&pve, &FakeGh::new());
    assert!(pve
        .calls()
        .contains(&format!("timeout 30 {other}/up vm-up a-1 9002 {RUNNER}")));
}

// ------------------------------------------------------------------ what a slot refuses

#[test]
fn a_vmid_that_is_not_ours_is_refused() {
    let rig = Rig::new("", SLOT_LINE);
    fs::write(rig.dir("pve/qemu-server/9002.conf"), "").unwrap();
    let pve = rig.pve(&[]).answer(CONFIG, "name: web\ntags: other\n");
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.code,
        Err("VMID 9002 exists but is not a mayfly guest; refusing to touch it".to_string())
    );
    assert_eq!(pve.calls(), lines![PVESM, CONFIG]);
    assert!(gh.calls().is_empty());
    assert_eq!(o.out, "");
}

#[test]
fn a_template_in_the_slots_vmid_is_refused() {
    let rig = Rig::new("", SLOT_LINE);
    fs::write(rig.dir("pve/qemu-server/9002.conf"), "").unwrap();
    let pve = rig
        .pve(&[])
        .answer(CONFIG, "name: ci-a-1\ntags: mayfly\ntemplate: 1\n");
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(
        o.code,
        Err("VMID 9002 is a template; refusing to destroy it from a slot".to_string())
    );
    assert_eq!(pve.calls(), lines![PVESM, CONFIG, CONFIG]);
}

#[test]
fn a_failing_destroy_ends_the_slot_with_its_exit_code() {
    let rig = Rig::new("", SLOT_LINE);
    fs::write(rig.dir("pve/qemu-server/9002.conf"), "").unwrap();
    let pve = rig
        .pve(&[])
        .answer(CONFIG, "name: ci-a-1\ntags: mayfly\n")
        .fail(DESTROY, 5);
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(5));
    assert_eq!(pve.calls(), lines![PVESM, CONFIG, CONFIG, STOP, DESTROY]);
    assert!(gh.calls().is_empty());
}

#[test]
fn a_vmid_of_another_node_waits_ten_minutes_and_exits() {
    let rig = Rig::new("", SLOT_LINE);
    fs::write(
        format!("{}/.vmlist", rig.paths.pve_etc),
        "{\"ids\":{\"9002\":{\"node\":\"pve2\"}}}",
    )
    .unwrap();
    let pve = rig.pve(&[]);
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(1));
    assert_eq!(
        o.out,
        format!(
            "slot a-1: VMID 9002 belongs to node pve2; pick a free VMID in {}\n",
            rig.paths.slots_conf
        )
    );
    assert_eq!(pve.calls(), lines![PVESM, "sleep 600"]);
}

#[test]
fn a_slot_is_checked_before_anything_runs() {
    let cases = [
        (
            "a-1 8000 linux 8 16 60 no current\n",
            "a-1",
            "slot a-1: VMID 8000 outside 9000..9099",
        ),
        (
            "a-1 9002 linux 8 16 60 no current kind=zzz\n",
            "a-1",
            "slot a-1: kind=zzz (vm, pod or lxc)",
        ),
        (
            "a-1 9002 linux 8 16 60 no current max_job_hours=abc\n",
            "a-1",
            "slot a-1: max_job_hours=abc must be a whole number of hours",
        ),
        (
            "a-1 9002 linux 8 16 60 no current max_idle_hours=\n",
            "a-1",
            "slot a-1: max_idle_hours= must be a whole number of hours",
        ),
        (
            "a-1 9002 linux 8 16 60 no current kind=pod containers=0\n",
            "a-1",
            "slot a-1: containers=0 must be a number from 1 to ram_gb",
        ),
        (
            "a-1 9002 linux 8 16 60 no current kind=pod containers=17\n",
            "a-1",
            "slot a-1: containers=17 must be a number from 1 to ram_gb",
        ),
        (
            "a-1 9002 linux 8 16 60 no current kind=pod containers=x2\n",
            "a-1",
            "slot a-1: containers=x2 must be a number from 1 to ram_gb",
        ),
    ];
    for (slots, name, want) in cases {
        let rig = Rig::new("", slots);
        let pve = rig.pve(&[]);
        let o = rig.run_slot(&pve, &FakeGh::new(), name);
        assert_eq!(o.code, Err(want.to_string()), "{slots}");
        assert_eq!(pve.calls(), lines![PVESM], "{slots}");
    }
    let rig = Rig::new("", SLOT_LINE);
    let o = rig.run_slot(&rig.pve(&[]), &FakeGh::new(), "zz");
    assert_eq!(
        o.code,
        Err(format!("no slot zz in {}", rig.paths.slots_conf))
    );
    let rig = Rig::new("TEMPLATE_VMIDS=\"9002 9003\"\n", SLOT_LINE);
    let o = rig.run(&rig.pve(&[]), &FakeGh::new());
    assert_eq!(
        o.code,
        Err("slot a-1: VMID 9002 is a template VMID".to_string())
    );
    // A slot's own hours override pool.env, and slot names are found like awk finds them.
    let rig = Rig::new("", "01 9002 linux 8 16 60 no current\n");
    rig.terms.store(1, Ordering::SeqCst);
    let o = rig.run_slot(&rig.pve(&[]), &FakeGh::new(), "1");
    assert_eq!(o.code, Ok(0));
}

// ------------------------------------------------------------------ the registration body

#[test]
fn the_jit_body_is_what_jq_builds() {
    assert_eq!(
        jit_body("mf-a-1-5", "1", "self-hosted,Linux,X64", "linux,small").unwrap(),
        "{\"name\":\"mf-a-1-5\",\"runner_group_id\":1,\"work_folder\":\"_work\",\"labels\":[\"Linux\",\"X64\",\"linux\",\"self-hosted\",\"small\"]}"
    );
    // Empty labels dropped, duplicates removed, sorted.
    assert_eq!(
        jit_body("n", "7", "b,,a", "a,,c,").unwrap(),
        "{\"name\":\"n\",\"runner_group_id\":7,\"work_folder\":\"_work\",\"labels\":[\"a\",\"b\",\"c\"]}"
    );
    assert_eq!(
        jit_body("n", "1", "", "").unwrap(),
        "{\"name\":\"n\",\"runner_group_id\":1,\"work_folder\":\"_work\",\"labels\":[]}"
    );
    assert!(jit_body("n", "one", "a", "b").is_err());
    // The name is JSON-escaped.
    assert!(jit_body("a\"b", "1", "a", "b")
        .unwrap()
        .starts_with("{\"name\":\"a\\\"b\","));
}

struct Request {
    method: String,
    path: String,
    auth: String,
    body: String,
}

/// A GitHub on a local port: installation, token, jit config, runner status and delete.
fn github(runner_status: &'static str) -> (u16, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).unwrap_or(0) == 0 {
                continue;
            }
            let mut headers = HashMap::new();
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                    break;
                }
                if let Some((k, v)) = l.trim_end().split_once(':') {
                    headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let len: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let (code, answer) = match (method.as_str(), path.as_str()) {
                ("GET", "/orgs/acme/installation") => (200, "{\"id\":42}".to_string()),
                ("POST", "/app/installations/42/access_tokens") => {
                    (201, "{\"token\":\"ghs_faketoken\"}".to_string())
                }
                ("POST", "/orgs/acme/actions/runners/generate-jitconfig") => (
                    201,
                    "{\"runner\":{\"id\":77,\"name\":\"x\"},\"encoded_jit_config\":\"JITSECRET\"}"
                        .to_string(),
                ),
                ("GET", "/orgs/acme/actions/runners/77") => {
                    (200, format!("{{\"id\":77,\"status\":\"{runner_status}\"}}"))
                }
                ("DELETE", "/orgs/acme/actions/runners/77") => (204, String::new()),
                _ => (404, "{}".to_string()),
            };
            log.lock().unwrap().push(Request {
                method,
                path,
                auth: headers.get("authorization").cloned().unwrap_or_default(),
                body: String::from_utf8_lossy(&body).into_owned(),
            });
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
            let _ = stream.flush();
        }
    });
    (port, seen)
}

fn make_key(dir: &Path) -> PathBuf {
    let key = dir.join("app.pem");
    let key_s = key.to_str().unwrap();
    let ok = |c: &mut Command| c.output().map(|o| o.status.success()).unwrap_or(false);
    assert!(
        ok(Command::new("openssl").args(["genrsa", "-traditional", "-out", key_s, "2048"]))
            || ok(Command::new("openssl").args(["genrsa", "-out", key_s, "2048"])),
        "openssl could not make a key"
    );
    key
}

#[test]
fn the_real_github_registers_a_single_job_runner() {
    let rig = Rig::new("", SLOT_LINE);
    let (port, seen) = github("online");
    let key = make_key(&rig.root);
    fs::write(
        &rig.paths.pool_env,
        format!(
            "APP_ID=12345\nKEY_PATH={}\nAPI_URL=http://127.0.0.1:{port}\nORG=acme\nRUNNER_GROUP_ID=3\nRUNNER_LABELS_BASE=self-hosted,Linux\n",
            key.display()
        ),
    )
    .unwrap();
    let pve = rig.pve(&[]);
    let (c, _) = config::load(&rig.paths, &pve, "pve1", true).unwrap();
    let gh = AppSlot;
    let token = gh.install_token(&c).unwrap();
    assert_eq!(token, "ghs_faketoken");
    let jit = gh
        .jit_config(&c, &token, "mf-a-1-5", "linux,small")
        .unwrap();
    assert_eq!(jit.encoded, "JITSECRET");
    assert_eq!(jit.runner_id, "77");
    assert!(gh.runner_online(&c, &token, "77"));
    assert!(!gh.runner_online(&c, &token, ""));
    gh.delete_runner(&c, &token, "77");

    let seen = seen.lock().unwrap();
    let calls: Vec<String> = seen
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    assert_eq!(
        calls,
        [
            "GET /orgs/acme/installation",
            "POST /app/installations/42/access_tokens",
            "POST /orgs/acme/actions/runners/generate-jitconfig",
            "GET /orgs/acme/actions/runners/77",
            "DELETE /orgs/acme/actions/runners/77"
        ]
    );
    // The body of the registration is the one `jit_body` builds; the token is only in a header.
    assert_eq!(
        seen[2].body,
        jit_body("mf-a-1-5", "3", "self-hosted,Linux", "linux,small").unwrap()
    );
    assert_eq!(seen[2].auth, "Bearer ghs_faketoken");
    assert_eq!(seen[3].auth, "Bearer ghs_faketoken");
    assert!(seen[2..].iter().all(|r| !r.path.contains("ghs_")));
}

#[test]
fn a_runner_that_is_offline_or_unknown_is_not_online() {
    let rig = Rig::new("", SLOT_LINE);
    let (port, _) = github("offline");
    let key = make_key(&rig.root);
    fs::write(
        &rig.paths.pool_env,
        format!(
            "APP_ID=12345\nKEY_PATH={}\nAPI_URL=http://127.0.0.1:{port}\nORG=acme\n",
            key.display()
        ),
    )
    .unwrap();
    let (c, _) = config::load(&rig.paths, &rig.pve(&[]), "pve1", true).unwrap();
    assert!(!AppSlot.runner_online(&c, "tok", "77"));
    assert!(!AppSlot.runner_online(&c, "tok", "78"));
}

// ------------------------------------------------------------------ which engine runs a slot

fn routed(pool: &str, slots: &str, name: &str) -> Route {
    let rig = Rig::new(pool, slots);
    route(&rig.paths, name)
}

#[test]
fn engine_routing() {
    let bash = Route::Bash { note: None };
    // The default is bash; a vm slot asks for rust with engine= or SLOT_ENGINE.
    assert_eq!(routed("", SLOT_LINE, "a-1"), bash);
    assert_eq!(routed("SLOT_ENGINE=bash\n", SLOT_LINE, "a-1"), bash);
    assert_eq!(routed("SLOT_ENGINE=rust\n", SLOT_LINE, "a-1"), Route::Rust);
    assert_eq!(
        routed("", "a-1 9002 l 8 16 60 no current engine=rust\n", "a-1"),
        Route::Rust
    );
    assert_eq!(
        routed(
            "",
            "a-1 9002 l 8 16 60 no current kind=vm engine=rust\n",
            "a-1"
        ),
        Route::Rust
    );
    // The slot's own key wins over pool.env, both ways.
    assert_eq!(
        routed(
            "SLOT_ENGINE=rust\n",
            "a-1 9002 l 8 16 60 no current engine=bash\n",
            "a-1"
        ),
        bash
    );
    assert_eq!(
        routed(
            "SLOT_ENGINE=bash\n",
            "a-1 9002 l 8 16 60 no current engine=rust\n",
            "a-1"
        ),
        Route::Rust
    );
    // The last assignment in pool.env wins.
    assert_eq!(
        routed("SLOT_ENGINE=bash\nSLOT_ENGINE=rust\n", SLOT_LINE, "a-1"),
        Route::Rust
    );
    // Pods and containers run on rust too, by the slot's key or by pool.env.
    for kind in ["pod", "lxc"] {
        let slots = format!("p-1 9041 l 8 16 60 no current kind={kind} engine=rust\n");
        assert_eq!(routed("", &slots, "p-1"), Route::Rust);
        let slots = format!("p-1 9041 l 8 16 60 no current kind={kind}\n");
        assert_eq!(routed("SLOT_ENGINE=rust\n", &slots, "p-1"), Route::Rust);
        // On bash anyway: nothing to say.
        assert_eq!(routed("", &slots, "p-1"), bash);
        let slots = format!("p-1 9041 l 8 16 60 no current kind={kind} engine=bash\n");
        assert_eq!(routed("SLOT_ENGINE=rust\n", &slots, "p-1"), bash);
    }
    // Not an engine: bash, and a line about it.
    assert_eq!(
        routed("", "a-1 9002 l 8 16 60 no current engine=fast\n", "a-1"),
        Route::Bash {
            note: Some("slot a-1: engine fast is not rust or bash; running it on bash".to_string())
        }
    );
    // Unknown slot, unknown kind: bash has the messages.
    assert_eq!(routed("SLOT_ENGINE=rust\n", SLOT_LINE, "zz"), bash);
    assert_eq!(
        routed(
            "",
            "a-1 9002 l 8 16 60 no current kind=zzz engine=rust\n",
            "a-1"
        ),
        bash
    );
}

fn tmp(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "mayfly-slot-bin-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

/// Runs the binary as `mayfly slot <name>` with a stand-in `mayfly.sh` (exit 7) and a pool.env with no
/// ORG: the Rust loop stops at that with a message, the stand-in prints its arguments.
fn slot_binary(pool: &str, slots: &str, name: &str) -> std::process::Output {
    let root = tmp("route");
    fs::create_dir_all(root.join("share")).unwrap();
    fs::create_dir_all(root.join("conf")).unwrap();
    let script = root.join("share/mayfly.sh");
    fs::write(&script, "#!/bin/sh\necho \"bash: $*\"\nexit 7\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("conf/pool.env"), format!("APP_ID=1\n{pool}")).unwrap();
    fs::write(root.join("conf/slots.conf"), slots).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mayfly"));
    cmd.args(["slot", name])
        .env("MAYFLY_SHARE", root.join("share"))
        .env("MAYFLY_CONF_DIR", root.join("conf"))
        .env_remove("NO_COLOR");
    // A script written a moment ago can be "Text file busy" while another test forks.
    for _ in 0..20 {
        let out = cmd.output().unwrap();
        if !String::from_utf8_lossy(&out.stderr).contains("Text file busy") {
            let _ = fs::remove_dir_all(&root);
            return out;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    cmd.output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn the_binary_runs_the_rust_loop_for_slots_on_rust() {
    // Rust: the loop starts and stops at the missing ORG.
    for (pool, slots) in [
        ("", "a-1 9002 l 8 16 60 no current engine=rust\n"),
        ("SLOT_ENGINE=rust\n", "a-1 9002 l 8 16 60 no current\n"),
    ] {
        let o = slot_binary(pool, slots, "a-1");
        assert_eq!(o.status.code(), Some(1), "{slots}");
        assert!(text(&o.stderr).contains("set ORG"), "{}", text(&o.stderr));
        assert_eq!(text(&o.stdout), "");
    }
    // Bash: the stand-in runs with the arguments, exit code and all.
    for (pool, slots) in [
        ("", "a-1 9002 l 8 16 60 no current\n"),
        ("", "a-1 9002 l 8 16 60 no current engine=bash\n"),
        (
            "SLOT_ENGINE=rust\n",
            "a-1 9002 l 8 16 60 no current engine=bash\n",
        ),
        ("SLOT_ENGINE=bash\n", "a-1 9002 l 8 16 60 no current\n"),
    ] {
        let o = slot_binary(pool, slots, "a-1");
        assert_eq!(o.status.code(), Some(7), "{slots}");
        assert_eq!(text(&o.stdout), "bash: slot a-1\n", "{slots}");
    }
    // Pods and containers: the Rust loop too.
    for kind in ["pod", "lxc"] {
        let slots = format!("p-1 9041 l 8 16 60 no current kind={kind} engine=rust\n");
        let o = slot_binary("", &slots, "p-1");
        assert_eq!(o.status.code(), Some(1), "{kind}");
        assert!(text(&o.stderr).contains("set ORG"), "{}", text(&o.stderr));
        assert_eq!(text(&o.stdout), "");
    }
    // No such slot: bash says so.
    let o = slot_binary(
        "SLOT_ENGINE=rust\n",
        "a-1 9002 l 8 16 60 no current\n",
        "nope",
    );
    assert_eq!(o.status.code(), Some(7));
    assert_eq!(text(&o.stdout), "bash: slot nope\n");
}

// ------------------------------------------------------------------ kind=lxc

const CT_SLOT: &str = "a-1 9002 linux 2 4 20 no current kind=lxc\n";
const LXC_POOL: &str = "LXC_TEMPLATE_VMID=9099\n";
const CT_RUNNER_NET0: &str = "name=eth0,bridge=vmbr0,ip=dhcp,firewall=1";
const CT_CLONE: &str = "pct clone 9099 9002 --hostname ci-a-1";
const CT_START: &str = "pct start 9002";
const CT_CONFIG: &str = "pct config 9002";
const CT_CONFIG_TPL: &str = "pct config 9099";
const CT_STOP: &str = "pct stop 9002 --skiplock 1";
const CT_DESTROY: &str = "pct destroy 9002 --purge 1 --destroy-unreferenced-disks 1 --force 1";
const CT_INFO: &str = "lxc-info -n 9002 -sH";
const CT_BOOT: &str = "timeout 10 lxc-attach -n 9002 -- systemctl is-system-running";
const CT_JIT: &str =
    "timeout 30 lxc-attach -n 9002 -- sh -c umask 077; mkdir -p /etc/mayfly; cat >/etc/mayfly/jit";
const CT_ENV: &str = "timeout 30 lxc-attach -n 9002 -- sh -c umask 077; cat >/etc/mayfly/job.env";
const CT_JOB: &str =
    "timeout 30 lxc-attach -n 9002 -- systemctl start --no-block mayfly-job.service";
const CT_PGREP: &str = "timeout 10 lxc-attach -n 9002 -- pgrep -f bin/Runner\\.Worker";
const CT_CFG: &str = "hostname: ci-a-1\ntags: mayfly\nrootfs: local-lvm:vm-9002-disk-0,size=8G\n";
const CT_TEMPLATE_CFG: &str = "hostname: ci-ct-template\ntags: mayfly\ntemplate: 1\n";

fn ct_exec_line() -> String {
    format!("timeout 10 lxc-attach -n 9002 -- sh -c {SCRIPT}")
}

fn ct_set(net0: &str) -> String {
    format!(
        "pct set 9002 --cores 2 --memory 4096 --swap 0 --onboot 0 --tags mayfly --description runner {RUNNER}, template 9099 --net0 {net0}"
    )
}

impl Rig {
    /// The host as an lxc slot sees it: the container template, a pool at 10%, `pct clone` that makes the
    /// container's config file and `pct destroy` that removes it, and a container that `lxc-info` calls
    /// RUNNING or STOPPED per question (the last answer repeating). Every `sleep 30` moves the clock 30 s.
    fn ct_pve(&self, states: &[bool]) -> FakePve {
        fs::write(self.dir("pve/lxc/9099.conf"), "").unwrap();
        let conf = self.dir("pve/lxc/9002.conf");
        let conf2 = conf.clone();
        let (snip, fw) = (self.snippet(), self.dir("fw/9002.fw"));
        let seen = Rc::clone(&self.started);
        let stop = Rc::clone(&self.stop_after_destroy);
        let signal = self.signal();
        let answers: Vec<(i32, &str)> = states
            .iter()
            .map(|s| (0, if *s { "RUNNING\n" } else { "STOPPED\n" }))
            .collect();
        FakePve::new()
            .answer(PVESM, &format!("{}/x\n", self.dir("snip")))
            .answer(CT_CONFIG_TPL, CT_TEMPLATE_CFG)
            .answer(POOL, USED_10)
            .answer(CT_CONFIG, CT_CFG)
            .answer(CT_CLONE, "")
            .on_call(CT_CLONE, move || fs::write(&conf, "").unwrap())
            .answer(&ct_set(CT_RUNNER_NET0), "")
            .answer("pct resize 9002 rootfs 20G", "")
            .answer(CT_START, "")
            .on_call(CT_START, move || {
                let mut s = seen.borrow_mut();
                s.snippet = fs::read_to_string(&snip).ok();
                s.snippet_mode = fs::metadata(&snip).ok().map(|m| m.mode() & 0o777);
                s.firewall = fs::read_to_string(&fw).ok();
            })
            .answer(CT_BOOT, "running\n")
            .answer(CT_JIT, "")
            .answer(CT_ENV, "")
            .answer(CT_JOB, "")
            .answer(CT_STOP, "")
            .answer(CT_DESTROY, "")
            .on_call(CT_DESTROY, move || {
                let _ = fs::remove_file(&conf2);
                if stop.get() {
                    signal();
                }
            })
            .sequence(CT_INFO, &answers)
            .on_call("sleep 30", self.advance(30))
    }
}

#[test]
fn an_lxc_job_cycle_from_clone_to_runner_delete() {
    let rig = Rig::new(
        LXC_POOL,
        "a-1 9002 linux 2 4 20 cache,missing current kind=lxc\n",
    );
    fs::write(
        format!("{}/cache.env", rig.paths.env_dir),
        "A=1\n# comment\nB=two\n",
    )
    .unwrap();
    rig.hook("10-notify", 0o755);
    let up = rig.hook_line("10-notify", &format!("vm-up a-1 9002 {RUNNER}"));
    let gone = rig.hook_line("10-notify", "vm-gone a-1 9002 1");
    let pve = rig
        .ct_pve(&[true, true, false])
        .answer(&up, "")
        .answer(&gone, "");
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        format!(
            "slot a-1: lxc 9002 up as {RUNNER} (template 9099)\nslot a-1: VM 9002 gone after 1 min\n"
        )
    );
    assert_eq!(
        o.err,
        format!(
            "warning: env missing: {}/missing.env missing or empty; jobs run without it\n",
            rig.paths.env_dir
        )
    );
    assert_eq!(
        pve.calls(),
        lines![
            PVESM,
            PVESM,
            CT_CONFIG_TPL,
            POOL,
            CT_CLONE,
            ct_set(CT_RUNNER_NET0),
            // ct_grow: the template's rootfs is 8G, the slot wants 20G.
            CT_CONFIG,
            "pct resize 9002 rootfs 20G",
            CT_START,
            // deliver: booted, the JIT config and the job env over stdin, then the job service.
            CT_BOOT,
            CT_JIT,
            CT_ENV,
            CT_JOB,
            up,
            CT_INFO,
            "sleep 30",
            CT_INFO,
            POOL,
            "sleep 30",
            // The container powered itself off.
            CT_INFO,
            CT_CONFIG,
            CT_CONFIG,
            CT_STOP,
            CT_DESTROY,
            gone,
        ]
    );
    // The secret and the environment travel on stdin only.
    assert_eq!(pve.stdins(), lines!["JITSECRET", "A=1\nB=two\n"]);
    assert_eq!(
        gh.calls(),
        lines![
            "token",
            format!("jit tok {RUNNER} linux"),
            "token",
            "delete 77 tok"
        ]
    );
    // No cloud-init snippet for a container; the firewall file is the VM's.
    let seen = rig.started.borrow();
    assert_eq!(seen.snippet, None);
    assert!(seen
        .firewall
        .as_deref()
        .unwrap()
        .starts_with("[OPTIONS]\nenable: 1\ndhcp: 1\npolicy_in: DROP\n"));
    assert!(!Path::new(&rig.dir("fw/9002.fw")).exists());
    no_secret(&o, &pve, &gh);
}

#[test]
fn an_lxc_slot_uses_its_own_net_and_keeps_a_big_enough_rootfs() {
    let rig = Rig::new(
        LXC_POOL,
        "a-1 9002 linux 2 4 8 no current kind=lxc bridge=vmbr2 vlan=42\n",
    );
    let net0 = "name=eth0,bridge=vmbr2,tag=42,ip=dhcp,firewall=1";
    let pve = rig.ct_pve(&[true, false]).answer(&ct_set(net0), "");
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert!(pve.calls().contains(&ct_set(net0)));
    // 8G wanted, 8G there: no resize.
    assert!(!pve.calls().iter().any(|c| c.starts_with("pct resize")));
    // An empty env column delivers no job.env.
    assert_eq!(pve.stdins(), lines!["JITSECRET"]);
}

#[test]
fn an_lxc_slot_without_a_container_template_waits() {
    let rig = Rig::new("", CT_SLOT);
    let pve = rig.ct_pve(&[]).on_call("sleep 300", rig.signal());
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.out,
        format!(
            "slot a-1: no template (TEMPLATE_VMID in {} or template= in {}); run mayfly template build\n",
            rig.paths.pool_env, rig.paths.slots_conf
        )
    );
    assert_eq!(pve.calls(), lines![PVESM, PVESM, "sleep 300"]);
    assert!(gh.calls().is_empty());
}

#[test]
fn an_lxc_canary_slot_clones_the_next_container_template() {
    let rig = Rig::new(
        "LXC_TEMPLATE_VMID=9099\nNEXT_LXC_TEMPLATE_VMID=9097\n",
        "a-1 9002 linux 2 4 20 no next kind=lxc\n",
    );
    fs::write(rig.dir("pve/lxc/9097.conf"), "").unwrap();
    let pve = rig
        .ct_pve(&[true, false])
        .answer("pct config 9097", CT_TEMPLATE_CFG)
        .on_call("sleep 120", rig.signal());
    rig.run(&pve, &FakeGh::new());
    assert!(pve
        .calls()
        .contains(&"pct clone 9097 9002 --hostname ci-a-1".to_string()));
}

#[test]
fn an_lxc_set_that_fails_is_cleaned_up_and_retried() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    let pve = rig.ct_pve(&[]).fail(&ct_set(CT_RUNNER_NET0), 1);
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        "slot a-1: could not create container 9002 from template 9099\n"
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == CT_CLONE).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            CT_CLONE,
            ct_set(CT_RUNNER_NET0),
            // The clone exists, so it is destroyed; its runner goes.
            CT_CONFIG,
            CT_CONFIG,
            CT_STOP,
            CT_DESTROY,
            "sleep 120"
        ]
    );
    assert_eq!(
        gh.calls(),
        lines!["token", format!("jit tok {RUNNER} linux"), "delete 77 tok"]
    );
    assert!(pve.stdins().is_empty());
}

#[test]
fn an_lxc_clone_that_fails_leaves_nothing_to_destroy() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    // The failed clone leaves no config file behind.
    let pve = rig
        .ct_pve(&[])
        .fail(CT_CLONE, 1)
        .on_call(CT_CLONE, || {})
        .on_call("sleep 120", rig.signal());
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.out,
        "slot a-1: could not create container 9002 from template 9099\n"
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == CT_CLONE).unwrap();
    assert_eq!(calls[at..].to_vec(), lines![CT_CLONE, "sleep 120"]);
    assert_eq!(gh.calls().last().map(String::as_str), Some("delete 77 tok"));
}

#[test]
fn an_lxc_container_that_does_not_start_is_destroyed() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    let pve = rig.ct_pve(&[]).fail(CT_START, 1);
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.out,
        "slot a-1: container 9002 did not start or did not take the runner config\n"
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == CT_START).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            CT_START,
            CT_CONFIG,
            CT_CONFIG,
            CT_STOP,
            CT_DESTROY,
            "sleep 120"
        ]
    );
    assert_eq!(gh.calls().last().map(String::as_str), Some("delete 77 tok"));
    assert!(pve.stdins().is_empty());
    assert!(!Path::new(&rig.dir("fw/9002.fw")).exists());
}

#[test]
fn an_lxc_container_that_does_not_take_the_runner_config_is_destroyed() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    let pve = rig.ct_pve(&[]).fail(CT_JIT, 1);
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.out,
        "slot a-1: container 9002 did not start or did not take the runner config\n"
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == CT_BOOT).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            CT_BOOT,
            CT_JIT,
            CT_CONFIG,
            CT_CONFIG,
            CT_STOP,
            CT_DESTROY,
            "sleep 120"
        ]
    );
    assert_eq!(gh.calls().last().map(String::as_str), Some("delete 77 tok"));
    no_secret(&o, &pve, &gh);
}

#[test]
fn an_lxc_container_that_is_slow_to_boot_is_waited_for() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    let pve = rig
        .ct_pve(&[true, false])
        .sequence(CT_BOOT, &[(1, ""), (0, "starting\n"), (0, "degraded\n")]);
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == CT_START).unwrap();
    assert_eq!(
        calls[at..at + 7].to_vec(),
        lines![CT_START, CT_BOOT, "sleep 2", CT_BOOT, "sleep 2", CT_BOOT, CT_JIT]
    );
}

#[test]
fn an_lxc_container_that_never_reports_running_is_given_the_config_anyway() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    let pve = rig.ct_pve(&[true, false]).fail(CT_BOOT, 1);
    rig.run(&pve, &FakeGh::new());
    let calls = pve.calls();
    assert_eq!(calls.iter().filter(|c| *c == CT_BOOT).count(), 60);
    // 59 waits of 2 s: none after the last look.
    assert_eq!(calls.iter().filter(|c| *c == "sleep 2").count(), 59);
    assert!(calls.contains(&CT_JIT.to_string()));
}

#[test]
fn a_stop_while_the_container_is_idle_stops_it_at_once() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    rig.stop_after_destroy.set(false);
    let pve = rig
        .ct_pve(&[true, true, false])
        .fail(&ct_exec_line(), 3)
        .on_call("sleep 30", rig.signal());
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == CT_JOB).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            CT_JOB,
            CT_INFO,
            "sleep 30",
            // The signal: is a job running? No (exit 3): stop the container.
            CT_INFO,
            ct_exec_line(),
            CT_STOP,
            CT_INFO,
            // Then it is gone: destroy.
            CT_CONFIG,
            CT_CONFIG,
            CT_STOP,
            CT_DESTROY
        ]
    );
    assert_eq!(pve.calls().iter().filter(|c| *c == CT_CLONE).count(), 1);
    assert_eq!(gh.calls().last().map(String::as_str), Some("delete 77 tok"));
}

#[test]
fn a_stop_while_a_container_runs_a_job_lets_the_job_finish() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    rig.stop_after_destroy.set(false);
    let pve = rig
        .ct_pve(&[true, true, true, false])
        .answer(&ct_exec_line(), "42\n")
        .on_call("sleep 30", rig.signal());
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == CT_JOB).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            CT_JOB,
            CT_INFO,
            "sleep 30",
            // The signal: a job runs, nothing is stopped. The loop looks again while stopping.
            CT_INFO,
            ct_exec_line(),
            CT_INFO,
            ct_exec_line(),
            POOL,
            "sleep 30",
            // The job ended and the container powered off.
            CT_INFO,
            CT_CONFIG,
            CT_CONFIG,
            CT_STOP,
            CT_DESTROY
        ]
    );
    assert_eq!(pve.calls().iter().filter(|c| *c == CT_CLONE).count(), 1);
}

#[test]
fn an_lxc_runner_that_never_comes_online_is_replaced() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    rig.stop_after_destroy.set(false);
    let pve = rig
        .ct_pve(&[true, true, true, false])
        .on_call("sleep 30", rig.advance(300))
        .on_call("sleep 120", rig.signal());
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        format!(
            "slot a-1: lxc 9002 up as {RUNNER} (template 9099)\n\
             slot a-1: runner {RUNNER} not online after 10 min; replacing lxc 9002 (check its template: mayfly check)\n\
             slot a-1: VM 9002 gone after 10 min\n"
        )
    );
    // Asked once the container is 10 minutes old: is a job running?
    assert!(pve.calls().contains(&CT_PGREP.to_string()));
}

#[test]
fn an_lxc_vmid_that_is_not_ours_is_refused() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    fs::write(rig.dir("pve/lxc/9002.conf"), "").unwrap();
    let pve = rig
        .ct_pve(&[])
        .answer(CT_CONFIG, "hostname: web\ntags: other\n");
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.code,
        Err("VMID 9002 exists but is not a mayfly guest; refusing to touch it".to_string())
    );
    assert_eq!(pve.calls(), lines![PVESM, CT_CONFIG]);
    assert!(gh.calls().is_empty());
}

#[test]
fn an_lxc_template_in_the_slots_vmid_is_never_destroyed() {
    let rig = Rig::new(LXC_POOL, CT_SLOT);
    fs::write(rig.dir("pve/lxc/9002.conf"), "").unwrap();
    let pve = rig
        .ct_pve(&[])
        .answer(CT_CONFIG, "hostname: ci-a-1\ntags: mayfly\ntemplate: 1\n");
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(
        o.code,
        Err("VMID 9002 is a template; refusing to destroy it from a slot".to_string())
    );
    assert_eq!(pve.calls(), lines![PVESM, CT_CONFIG, CT_CONFIG]);
}

// ------------------------------------------------------------------ kind=pod

const POD_SLOT: &str = "a-1 9002 linux 8 16 60 no current kind=pod containers=4\n";
const POD_SET: &str = "qm set 9002 --cores 8 --memory 16384 --balloon 0 --onboot 0 --tags mayfly --description pod a-1, template 9098 --net0 virtio,bridge=vmbr0,firewall=1 --ipconfig0 ip=dhcp --cicustom user=local:snippets/mayfly-9002.yaml";
const POD_READY: &str =
    "timeout 40 qm guest exec 9002 --timeout 30 -- /usr/local/sbin/mayfly-pod ready";
const POD_WANT: &str =
    "timeout 40 qm guest exec 9002 --timeout 30 -- /usr/local/sbin/mayfly-pod want";
const POD_DRAIN: &str =
    "timeout 40 qm guest exec 9002 --timeout 30 -- /usr/local/sbin/mayfly-pod drain";
const POD_BUSY: &str =
    "timeout 40 qm guest exec 9002 --timeout 30 -- /usr/local/sbin/mayfly-pod busy";
const OK: &str = "{\"exitcode\":0}";

fn out_data(s: &str) -> String {
    format!("{{\"exitcode\":0,\"out-data\":\"{s}\\n\"}}")
}

fn put_line(i: u32, secret: &str) -> String {
    format!(
        "timeout 40 qm guest exec 9002 --timeout 30 -- /usr/local/sbin/mayfly-pod put mf-a-1-{i}-1700000000 {secret}"
    )
}

impl Rig {
    /// The VM host of a pod slot, with the pod script on disk and `mayfly-pod` answering `ready`, `drain`
    /// and `busy` (idle). `want` is for the test to answer.
    fn pod_pve(&self, states: &[bool]) -> FakePve {
        fs::create_dir_all(&self.paths.share).unwrap();
        fs::write(format!("{}/pod-vm.sh", self.paths.share), "echo pod\n").unwrap();
        self.pve(states)
            .answer(POD_SET, "")
            .answer(POD_READY, OK)
            .answer(POD_DRAIN, OK)
            .answer(POD_BUSY, &out_data("0"))
    }
}

const POD_SNIPPET: &str = "\
#cloud-config
hostname: ci-a-1
write_files:
  - path: /usr/local/sbin/mayfly-pod
    permissions: '0755'
    encoding: b64
    content: ZWNobyBwb2QK
  - path: /etc/mayfly/pod.env
    permissions: '0600'
    content: |
      CONTAINERS=4
      CPUS=2.00
      MEMORY=4g
      MAX_JOB_SECS=10800
      MAX_IDLE_SECS=7200
runcmd:
  - [bash, -c, '/usr/local/sbin/mayfly-pod image > /var/log/mayfly-pod-image.log 2>&1']
";

#[test]
fn a_pod_cycle_feeds_idle_containers_and_drains_on_a_signal() {
    let rig = Rig::new("", POD_SLOT);
    rig.stop_after_destroy.set(false);
    rig.hook("10-notify", 0o755);
    let up = rig.hook_line("10-notify", "pod-up a-1 9002 4");
    let gone = rig.hook_line("10-notify", "pod-gone a-1 9002 0");
    let (clock, signal) = (rig.advance(15), rig.signal());
    let sleeps = Cell::new(0);
    let pve = rig
        .pod_pve(&[true])
        .answer(&up, "")
        .answer(&gone, "")
        .sequence(POD_WANT, &[(0, &out_data("2")), (0, &out_data("0"))])
        // Two idle containers: the first config is taken, the second is refused (its runner goes).
        .answer(&put_line(1, "JITSECRET"), OK)
        .fail(&put_line(2, "JITSECRET"), 1)
        .on_call("sleep 15", move || {
            sleeps.set(sleeps.get() + 1);
            if sleeps.get() == 1 {
                clock();
            } else {
                signal();
            }
        });
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        "pod a-1: VM 9002 booting (4 containers, template 9098)\n\
         pod a-1: ready after 0 min\n\
         pod a-1: VM 9002 gone after 0 min\n"
    );
    assert_eq!(
        pve.calls(),
        lines![
            PVESM,
            PVESM,
            CONFIG_TPL,
            POOL,
            CLONE,
            POD_SET,
            CONFIG,
            "qm resize 9002 scsi0 60G",
            START,
            up,
            POD_READY,
            POD_WANT,
            put_line(1, "JITSECRET"),
            put_line(2, "JITSECRET"),
            "sleep 15",
            POD_WANT,
            "sleep 15",
            // The signal: no stop of the pod VM, only a drain; no running job, so on to the destroy.
            POD_DRAIN,
            POD_BUSY,
            CONFIG,
            CONFIG,
            STOP,
            DESTROY,
            gone,
        ]
    );
    assert_eq!(
        gh.calls(),
        lines![
            "token",
            "jit tok mf-a-1-1-1700000000 linux",
            "jit tok mf-a-1-2-1700000000 linux",
            "delete 77 tok",
        ]
    );
    // The snippet existed with the pod script and its limits, mode 0600, and is gone.
    let seen = rig.started.borrow();
    assert_eq!(seen.snippet.as_deref(), Some(POD_SNIPPET));
    assert_eq!(seen.snippet_mode, Some(0o600));
    assert!(!Path::new(&rig.snippet()).exists());
    assert!(seen.firewall.is_some());
    // The pod VM stops only in destroy.
    assert_eq!(pve.calls().iter().filter(|c| *c == STOP).count(), 1);
}

#[test]
fn a_pod_takes_at_most_eight_configs_a_pass_and_stops_at_a_refusal() {
    let rig = Rig::new("", POD_SLOT);
    let signal = rig.signal();
    let pve = rig
        .pod_pve(&[true])
        .answer(POD_WANT, &out_data("12"))
        .on_call("sleep 15", signal);
    let gh = FakeGh::new();
    gh.jit_fails.set(0);
    rig.stop_after_destroy.set(false);
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    let puts = pve
        .calls()
        .iter()
        .filter(|c| c.contains("mayfly-pod put"))
        .count();
    // Nothing answers `put`, so every config is refused: 8 tries, 8 runners deleted.
    assert_eq!(puts, 8);
    assert_eq!(
        gh.calls().iter().filter(|c| *c == "delete 77 tok").count(),
        8
    );
}

#[test]
fn a_refused_jit_config_in_a_pod_waits_a_minute() {
    let rig = Rig::new("", POD_SLOT);
    let pve = rig
        .pod_pve(&[true])
        .answer(POD_WANT, &out_data("2"))
        .on_call("sleep 60", rig.signal());
    rig.stop_after_destroy.set(false);
    let gh = FakeGh::new();
    gh.jit_fails.set(5);
    let o = rig.run(&pve, &gh);
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        "pod a-1: VM 9002 booting (4 containers, template 9098)\n\
         pod a-1: ready after 0 min\n\
         pod a-1: GitHub refused the runner registration; retrying in 1 min\n\
         pod a-1: VM 9002 gone after 0 min\n"
    );
    // The first refusal ends the pass: one try, no `put`.
    assert_eq!(
        gh.calls(),
        lines!["token", format!("jit tok mf-a-1-1-1700000000 linux")]
    );
}

#[test]
fn a_pod_drain_waits_for_running_jobs() {
    let rig = Rig::new("", POD_SLOT);
    rig.stop_after_destroy.set(false);
    let pve = rig
        .pod_pve(&[true])
        .answer(POD_WANT, &out_data("0"))
        .sequence(
            POD_BUSY,
            &[
                (0, &out_data("2")),
                (0, &out_data("1")),
                (0, &out_data("0")),
            ],
        )
        .on_call("sleep 15", rig.signal());
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == POD_DRAIN).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![
            POD_DRAIN, POD_BUSY, "sleep 30", POD_BUSY, "sleep 30", POD_BUSY, CONFIG, CONFIG, STOP,
            DESTROY
        ]
    );
}

#[test]
fn a_pod_whose_template_changed_is_recycled() {
    let rig = Rig::new("", POD_SLOT);
    let pool_env = rig.paths.pool_env.clone();
    let pve = rig
        .pod_pve(&[true])
        .answer(POD_WANT, &out_data("0"))
        .on_call("sleep 15", move || {
            fs::write(
                &pool_env,
                "ORG=acme\nRUNNER_PREFIX=mf\nTEMPLATE_VMID=9099\n",
            )
            .unwrap()
        });
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        "pod a-1: VM 9002 booting (4 containers, template 9098)\n\
         pod a-1: ready after 0 min\n\
         pod a-1: recycling VM 9002 (template 9099 replaces 9098)\n\
         pod a-1: VM 9002 gone after 0 min\n"
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == POD_WANT).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![POD_WANT, "sleep 15", POD_DRAIN, POD_BUSY, CONFIG, CONFIG, STOP, DESTROY]
    );
}

#[test]
fn a_slot_template_pins_the_pod_to_it() {
    let rig = Rig::new(
        "",
        "a-1 9002 linux 8 16 60 no current kind=pod containers=4 template=9098\n",
    );
    rig.stop_after_destroy.set(false);
    let pool_env = rig.paths.pool_env.clone();
    let signal = rig.signal();
    let sleeps = Cell::new(0);
    // After the first pass pool.env moves on to another template; the slot's own template= does not.
    let pve = rig
        .pod_pve(&[true])
        .answer(POD_WANT, &out_data("0"))
        .on_call("sleep 15", move || {
            sleeps.set(sleeps.get() + 1);
            if sleeps.get() == 1 {
                fs::write(&pool_env, "ORG=acme\nTEMPLATE_VMID=9099\n").unwrap();
            } else {
                signal();
            }
        });
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert!(!o.out.contains("recycling"), "{}", o.out);
    assert_eq!(pve.calls().iter().filter(|c| *c == POD_WANT).count(), 2);
}

#[test]
fn a_pod_older_than_pod_max_age_hours_is_recycled() {
    // 1 h, plus (9002 % 8) * 30 min = 1 h.
    let rig = Rig::new("POD_MAX_AGE_HOURS=1\n", POD_SLOT);
    let pve = rig
        .pod_pve(&[true])
        .answer(POD_WANT, &out_data("0"))
        .on_call("sleep 15", rig.advance(7200));
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        "pod a-1: VM 9002 booting (4 containers, template 9098)\n\
         pod a-1: ready after 0 min\n\
         pod a-1: recycling VM 9002 (older than 1 h)\n\
         pod a-1: VM 9002 gone after 120 min\n"
    );
}

#[test]
fn a_pod_whose_guest_stops_answering_is_replaced_after_twenty_misses() {
    let rig = Rig::new("", POD_SLOT);
    let pve = rig.pod_pve(&[true]);
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert!(
        o.out
            .contains("pod a-1: recycling VM 9002 (the guest has not answered for 5 min)\n"),
        "{}",
        o.out
    );
    let calls = pve.calls();
    assert_eq!(calls.iter().filter(|c| *c == POD_WANT).count(), 20);
    assert_eq!(calls.iter().filter(|c| *c == "sleep 15").count(), 19);
}

#[test]
fn a_pod_that_never_gets_ready_is_replaced_with_a_backoff() {
    let rig = Rig::new("", POD_SLOT);
    rig.stop_after_destroy.set(false);
    let pve = rig
        .pod_pve(&[true])
        .fail(POD_READY, 1)
        .on_call("sleep 120", rig.signal());
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        "pod a-1: VM 9002 booting (4 containers, template 9098)\n\
         pod a-1: VM 9002 never got ready (see /var/log/mayfly-pod-image.log inside); replacing it\n\
         pod a-1: VM 9002 gone after 0 min\n"
    );
    let calls = pve.calls();
    assert_eq!(calls.iter().filter(|c| *c == POD_READY).count(), 90);
    assert_eq!(calls.iter().filter(|c| *c == "sleep 20").count(), 90);
    // Not ready: no drain, and one failure waits two minutes.
    assert!(!calls.contains(&POD_DRAIN.to_string()));
    assert_eq!(calls.last().map(String::as_str), Some("sleep 120"));
}

#[test]
fn a_pool_over_the_panic_limit_stops_the_pod() {
    let rig = Rig::new("POOL_PANIC_PERCENT=50\n", POD_SLOT);
    rig.stop_after_destroy.set(false);
    let pve = rig
        .pod_pve(&[true])
        .on_call("sleep 120", rig.signal())
        .answer(POD_WANT, &out_data("0"))
        .sequence(POOL, &[(0, USED_10), (0, "{\"total\":1000,\"used\":900}")]);
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert_eq!(
        o.out,
        "pod a-1: VM 9002 booting (4 containers, template 9098)\n\
         pod a-1: ready after 0 min\n\
         pod a-1: local-lvm is 90% full (limit 50%); stopping VM 9002 (its jobs are cancelled)\n\
         pod a-1: VM 9002 gone after 0 min\n"
    );
    // Cancelled jobs are not waited for, and the pod is not "never ready": one nap of two minutes.
    let calls = pve.calls();
    assert!(!calls.contains(&POD_DRAIN.to_string()));
    assert_eq!(calls.last().map(String::as_str), Some("sleep 120"));
    assert_eq!(calls.iter().filter(|c| *c == POD_WANT).count(), 4);
}

#[test]
fn the_container_count_sets_the_pod_limits() {
    let rig = Rig::new(
        "MAX_JOB_HOURS=2\n",
        "a-1 9002 linux 8 16 60 no current kind=pod containers=3 max_idle_hours=1\n",
    );
    let pve = rig.pod_pve(&[false]).answer(POD_SET, "");
    rig.run(&pve, &FakeGh::new());
    let snippet = rig.started.borrow().snippet.clone().unwrap();
    assert!(
        snippet.contains(
            "      CONTAINERS=3\n      CPUS=2.67\n      MEMORY=5g\n      MAX_JOB_SECS=7200\n      MAX_IDLE_SECS=3600\n"
        ),
        "{snippet}"
    );
}

#[test]
fn a_pod_snippet_carries_the_job_env() {
    let rig = Rig::new(
        "",
        "a-1 9002 linux 8 16 60 cache current kind=pod containers=4\n",
    );
    fs::write(format!("{}/cache.env", rig.paths.env_dir), "A=1\nB=two\n").unwrap();
    let pve = rig.pod_pve(&[false]);
    rig.run(&pve, &FakeGh::new());
    let snippet = rig.started.borrow().snippet.clone().unwrap();
    assert!(
        snippet.contains(
            "      MAX_IDLE_SECS=7200\n  - path: /etc/mayfly/job.env\n    permissions: '0600'\n    content: |\n      A=1\n      B=two\nruncmd:\n"
        ),
        "{snippet}"
    );
}

#[test]
fn a_pod_without_a_template_waits_five_minutes() {
    let rig = Rig::new("TEMPLATE_VMID=\n", POD_SLOT);
    let pve = rig.pod_pve(&[]).on_call("sleep 300", rig.signal());
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(
        o.out,
        format!(
            "pod a-1: no template (TEMPLATE_VMID in {} or template= in {}); run mayfly template build\n",
            rig.paths.pool_env, rig.paths.slots_conf
        )
    );
    assert_eq!(pve.calls(), lines![PVESM, PVESM, "sleep 300"]);
}

#[test]
fn a_pod_clone_that_fails_is_cleaned_up_and_retried() {
    let rig = Rig::new("", POD_SLOT);
    let pve = rig.pod_pve(&[]).fail(POD_SET, 1);
    let gh = FakeGh::new();
    let o = rig.run(&pve, &gh);
    assert_eq!(
        o.out,
        "pod a-1: could not create VM 9002 from template 9098\n"
    );
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == CLONE).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![CLONE, POD_SET, CONFIG, CONFIG, STOP, DESTROY, "sleep 120"]
    );
    // A pod registers no runner of its own.
    assert!(gh.calls().is_empty());
    assert!(!Path::new(&rig.snippet()).exists());
}

#[test]
fn a_pod_vm_that_does_not_start_is_destroyed() {
    let rig = Rig::new("", POD_SLOT);
    let pve = rig.pod_pve(&[]).fail(START, 1);
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.out, "pod a-1: VM 9002 did not start\n");
    let calls = pve.calls();
    let at = calls.iter().position(|c| c == START).unwrap();
    assert_eq!(
        calls[at..].to_vec(),
        lines![START, CONFIG, CONFIG, STOP, DESTROY, "sleep 120"]
    );
    assert!(!Path::new(&rig.snippet()).exists());
    assert!(!Path::new(&rig.dir("fw/9002.fw")).exists());
}

#[test]
fn a_signal_before_the_pod_starts_nothing() {
    let rig = Rig::new("", POD_SLOT);
    rig.terms.store(1, Ordering::SeqCst);
    let pve = rig.pod_pve(&[]);
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(0));
    assert_eq!(o.out, "");
    assert_eq!(pve.calls(), lines![PVESM]);
}

#[test]
fn a_pod_vmid_that_is_not_ours_is_refused() {
    let rig = Rig::new("", POD_SLOT);
    fs::write(rig.dir("pve/qemu-server/9002.conf"), "").unwrap();
    let pve = rig.pod_pve(&[]).answer(CONFIG, "name: web\ntags: other\n");
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(
        o.code,
        Err("VMID 9002 exists but is not a mayfly guest; refusing to touch it".to_string())
    );
    assert_eq!(pve.calls(), lines![PVESM, CONFIG]);
}

#[test]
fn a_failing_pod_destroy_ends_the_slot_with_its_exit_code() {
    let rig = Rig::new("", POD_SLOT);
    fs::write(rig.dir("pve/qemu-server/9002.conf"), "").unwrap();
    let pve = rig
        .pod_pve(&[])
        .answer(CONFIG, "name: ci-a-1\ntags: mayfly\n")
        .fail(DESTROY, 5);
    let o = rig.run(&pve, &FakeGh::new());
    assert_eq!(o.code, Ok(5));
}
