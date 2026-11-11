//! `template build`, `canary`, `promote` and `list` against the recording fake Proxmox host: the exact
//! commands, their order, the waits and the exact output. Nothing here needs Proxmox, a network or root,
//! and nothing waits (the fake records `sleep N` instead of sleeping).

use mayfly::cmd_template::{self, set_pool_value, Opts};
use mayfly::config::{self, Config, Paths};
use mayfly::out::Out;
use mayfly::pve::FakePve;
use mayfly::Ctx;
use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

const IMAGE: &str = "IMAGE-BYTES";
const SUM: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const STAMP: &str = "20231114";

struct Host {
    root: PathBuf,
    paths: Paths,
    config: Config,
    /// The snippet as `qm create` saw it: its text and its mode.
    snippet: Rc<RefCell<Option<(String, u32)>>>,
}

struct Res {
    r: Result<i32, String>,
    out: String,
    err: String,
}

fn lines(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

impl Host {
    fn new(pool_extra: &str) -> Host {
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "mayfly-template-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        let r = root.display().to_string();
        for d in [
            "conf",
            "pve/qemu-server",
            "pve/lxc",
            "fw",
            "snip",
            "share/profiles/default",
            "lock",
        ] {
            fs::create_dir_all(format!("{r}/{d}")).unwrap();
        }
        for (f, text) in [
            ("provision.sh", "P\n"),
            ("ci-job.sh", "J\n"),
            ("mayfly-job.service", "S\n"),
            ("template.env", "E\n"),
            ("profiles/default/provision.sh", "profile\n"),
        ] {
            fs::write(format!("{r}/share/{f}"), text).unwrap();
        }
        let mut paths = Paths::new(
            &format!("{r}/conf"),
            &format!("{r}/share"),
            &format!("{r}/pve"),
            &format!("{r}/fw"),
        );
        paths.state_dir = format!("{r}/state");
        paths.build_lock = format!("{r}/lock/build.lock");
        fs::write(
            &paths.pool_env,
            format!(
                "ORG=acme\nIMAGE_URL=https://img.test/x/disk.img\nTEMPLATE_VMID=9098\nNEXT_TEMPLATE_VMID=9099\n{pool_extra}"
            ),
        )
        .unwrap();
        let (config, _) = config::load(&paths, &Host::base(&r), "pve1", true).unwrap();
        Host {
            root,
            paths,
            config,
            snippet: Rc::new(RefCell::new(None)),
        }
    }

    fn base(root: &str) -> FakePve {
        FakePve::new().answer("pvesm path local:snippets/x", &format!("{root}/snip/x\n"))
    }

    fn d(&self, sub: &str) -> String {
        format!("{}/{sub}", self.root.display())
    }

    fn opts(&self) -> Opts {
        Opts {
            now: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            images_dir: self.d("images"),
            in_unit: true,
            unit_env: vec![],
            self_exe: "/usr/local/sbin/mayfly".to_string(),
            tmp_dir: self.d("tmp"),
            tgz: Some(self.d("tmp/profile.tgz")),
        }
    }

    fn conf(&self, kind: &str, id: &str) {
        fs::write(self.d(&format!("pve/{kind}/{id}.conf")), "").unwrap();
    }

    fn run_with(&self, pve: &FakePve, opts: &Opts, args: &[&str]) -> Res {
        let cx = Ctx {
            paths: &self.paths,
            pve,
            node: "pve1",
            meminfo: "",
        };
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let r = cmd_template::run_with(
            &cx,
            &self.config,
            &args,
            opts,
            &Out::plain(),
            &mut out,
            &mut err,
        )
        .map_err(|d| d.0);
        Res {
            r,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    fn run(&self, pve: &FakePve, args: &[&str]) -> Res {
        self.run_with(pve, &self.opts(), args)
    }

    fn pool_env(&self) -> String {
        fs::read_to_string(&self.paths.pool_env).unwrap()
    }

    // ---- a VM build

    fn img(&self) -> String {
        self.d("images/disk.img")
    }

    /// The fake host up to `qm start`: what a VM build asks for and how the host answers.
    fn vm_host(&self, id: &str) -> FakePve {
        self.vm_host_net(id, "virtio,bridge=vmbr0,firewall=1")
    }

    /// Like `vm_host`, with the net0 the settings of this host make.
    fn vm_host_net(&self, id: &str, net0: &str) -> FakePve {
        let part = format!("{}.part", self.img());
        let part2 = part.clone();
        let snip_path = self.d(&format!("snip/mayfly-{id}.yaml"));
        let seen = self.snippet.clone();
        let conf = self.d(&format!("pve/qemu-server/{id}.conf"));
        let create = format!(
            "qm create {id} --name ci-template-build --tags mayfly --ostype l26 --cpu host --cores 8 --memory 16384 --description profile default --balloon 0 --scsihw virtio-scsi-single --agent enabled=1 --serial0 socket --vga serial0 --net0 {net0} --ipconfig0 ip=dhcp --cicustom user=local:snippets/mayfly-{id}.yaml"
        );
        Host::base(&self.root.display().to_string())
            .answer(
                "curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 https://img.test/x/SHA256SUMS",
                &format!("{SUM} *disk.img\n0000  other.img\n"),
            )
            .answer(
                &format!("curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 -o {part} https://img.test/x/disk.img"),
                "",
            )
            .on_call(
                &format!("curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 -o {part2} https://img.test/x/disk.img"),
                move || fs::write(&part2, IMAGE).unwrap(),
            )
            .answer(&format!("tar -C {}/share/profiles/default -czf - .", self.root.display()), "TARDATA")
            .answer(&create, "")
            .on_call(&create, move || {
                let mode = fs::metadata(&snip_path).unwrap().permissions().mode() & 0o777;
                *seen.borrow_mut() = Some((fs::read_to_string(&snip_path).unwrap(), mode));
                fs::write(&conf, "").unwrap();
            })
            .answer("sha256sum -c -", "")
            .answer("pvesh get /storage/local-lvm --output-format json", "{\"type\":\"lvmthin\"}")
            .answer(&format!("qm set {id} --scsi0 local-lvm:0,import-from={},discard=on,iothread=1,ssd=1", self.img()), "")
            .answer(&format!("qm set {id} --ide2 local-lvm:cloudinit --boot order=scsi0"), "")
            .answer(&format!("qm resize {id} scsi0 80G"), "")
            .answer(&format!("qm start {id}"), "")
    }

    /// The commands of a VM build up to and including `qm start`.
    fn vm_head(&self, id: &str) -> Vec<String> {
        vec![
            "curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 https://img.test/x/SHA256SUMS".to_string(),
            format!("curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 -o {}.part https://img.test/x/disk.img", self.img()),
            "sha256sum -c -".to_string(),
            format!("tar -C {}/share/profiles/default -czf - .", self.root.display()),
            format!("qm create {id} --name ci-template-build --tags mayfly --ostype l26 --cpu host --cores 8 --memory 16384 --description profile default --balloon 0 --scsihw virtio-scsi-single --agent enabled=1 --serial0 socket --vga serial0 --net0 virtio,bridge=vmbr0,firewall=1 --ipconfig0 ip=dhcp --cicustom user=local:snippets/mayfly-{id}.yaml"),
            "pvesh get /storage/local-lvm --output-format json".to_string(),
            format!("qm set {id} --scsi0 local-lvm:0,import-from={},discard=on,iothread=1,ssd=1", self.img()),
            format!("qm set {id} --ide2 local-lvm:cloudinit --boot order=scsi0"),
            format!("qm resize {id} scsi0 80G"),
            format!("qm start {id}"),
        ]
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn exec(id: &str, test: &str) -> String {
    format!("qm guest exec {id} --timeout 20 -- test -e /var/log/mayfly-provision.{test}")
}

const NOT_YET: (i32, &str) = (0, "{\"exitcode\":1,\"exited\":1}");
const YES: (i32, &str) = (0, "{\"exitcode\":0,\"exited\":1}");

// ------------------------------------------------------------------ VM build

#[test]
fn a_vm_build_runs_every_step_in_order() {
    let h = Host::new("");
    let id = "9097";
    let pve = h
        .vm_host(id)
        .sequence(&exec(id, "done"), &[NOT_YET, YES])
        .answer(&exec(id, "failed"), "{\"exitcode\":1}")
        .with_qemu_states(id, &[true, true, false])
        .answer(
            &format!("qm guest exec {id} --timeout 20 -- sh -c rm -f /var/log/mayfly-provision.done; sync"),
            "",
        )
        .answer(&format!("qm shutdown {id} --timeout 300"), "")
        .answer(
            &format!("qm set {id} --delete cicustom --name ci-template-default-{STAMP}"),
            "",
        )
        .answer(&format!("qm template {id}"), "")
        .answer(&format!("qm config {id}"), "name: ci-template-default\ntemplate: 1\n");
    let res = h.run(&pve, &["build"]);
    assert_eq!(res.r, Ok(0));
    assert_eq!(
        res.out,
        format!(
            "downloading disk.img\nbuilding template VM 9097, profile default (20 to 40 min)\nprovisioned; shutting down VM 9097\ntemplate 9097 ready (ci-template-default-{STAMP})\n  next: mayfly template canary 9097, watch a job, then mayfly template promote 9097\n"
        )
    );
    assert_eq!(res.err, "");
    let mut want = h.vm_head(id);
    want.extend(lines(&[
        "sleep 30",
        &exec(id, "done"),
        &exec(id, "failed"),
        "sleep 30",
        &exec(id, "done"),
        "qm guest exec 9097 --timeout 20 -- sh -c rm -f /var/log/mayfly-provision.done; sync",
        "qm shutdown 9097 --timeout 300",
        &format!("qm set 9097 --delete cicustom --name ci-template-default-{STAMP}"),
        "qm template 9097",
        "qm config 9097",
    ]));
    assert_eq!(pve.calls(), want);
    // sha256sum is fed the listed checksum and the downloaded file, as bash's echo does.
    assert_eq!(pve.stdins(), [format!("{SUM}  {}.part\n", h.img())]);
    // The image is in place, checked, and the part file is gone.
    assert_eq!(fs::read_to_string(h.img()).unwrap(), IMAGE);
    assert!(!std::path::Path::new(&format!("{}.part", h.img())).exists());
    // The cloud-init snippet was private and complete while the VM was built, and is removed after.
    let (text, mode) = h.snippet.borrow().clone().unwrap();
    assert_eq!(mode, 0o600);
    assert_eq!(
        text,
        "#cloud-config
hostname: ci-template
write_files:
  - path: /root/provision.sh
    permissions: '0700'
    encoding: b64
    content: UAo=
  - path: /root/ci-job.sh
    encoding: b64
    content: Sgo=
  - path: /root/mayfly-job.service
    encoding: b64
    content: Uwo=
  - path: /root/template.env
    encoding: b64
    content: RQo=
  - path: /root/profile.tgz
    encoding: b64
    content: VEFSREFUQQ==
runcmd:
  - [bash, -c, 'CACHE_IP= /root/provision.sh > /var/log/mayfly-provision.log 2>&1 && touch /var/log/mayfly-provision.done || touch /var/log/mayfly-provision.failed']
"
    );
    assert!(!std::path::Path::new(&h.d("snip/mayfly-9097.yaml")).exists());
    // The firewall file: nothing in, nothing to the private networks.
    assert_eq!(
        fs::read_to_string(h.d("fw/9097.fw")).unwrap(),
        "[OPTIONS]\nenable: 1\ndhcp: 1\npolicy_in: DROP\npolicy_out: ACCEPT\n\n[RULES]\nOUT DROP -dest 10.0.0.0/8\nOUT DROP -dest 172.16.0.0/12\nOUT DROP -dest 192.168.0.0/16\nOUT DROP -dest 100.64.0.0/10\nOUT DROP -dest 169.254.0.0/16\n"
    );
}

#[test]
fn the_firewall_file_and_the_net_follow_the_settings() {
    let h = Host::new(
        "GATEWAY=10.1.1.1\nCACHE_IP=10.1.1.9\nALLOW_OUT=\"10.2.0.0/16 10.3.0.5\"\nBLOCK_NETS=10.0.0.0/8\nVLAN=30\nTEMPLATE_DISK=100\n",
    );
    let id = "9097";
    let pve = h
        .vm_host_net(id, "virtio,bridge=vmbr0,tag=30,firewall=1")
        .answer("qm resize 9097 scsi0 100G", "");
    // Not run to the end: the first poll finds the VM stopped.
    let res = h.run(&pve, &["build"]);
    assert!(res.r.is_err());
    assert_eq!(
        fs::read_to_string(h.d("fw/9097.fw")).unwrap(),
        "[OPTIONS]\nenable: 1\ndhcp: 1\npolicy_in: DROP\npolicy_out: ACCEPT\n\n[RULES]\nOUT ACCEPT -dest 10.1.1.1 -p udp -dport 53\nOUT ACCEPT -dest 10.1.1.1 -p tcp -dport 53\nOUT ACCEPT -dest 10.1.1.9\nOUT ACCEPT -dest 10.2.0.0/16\nOUT ACCEPT -dest 10.3.0.5\nOUT DROP -dest 10.0.0.0/8\n"
    );
    // The other settings reach the commands.
    let calls = pve.calls();
    assert!(calls
        .iter()
        .any(|c| c.contains("--net0 virtio,bridge=vmbr0,tag=30,firewall=1")));
    // The snippet's runcmd carries the cache address.
    assert!(h
        .snippet
        .borrow()
        .clone()
        .unwrap()
        .0
        .contains("'CACHE_IP=10.1.1.9 /root/provision.sh"));
    assert!(calls.iter().any(|c| c == "qm resize 9097 scsi0 100G"));
}

#[test]
fn firewall_no_writes_no_file_and_removes_a_stale_one() {
    let h = Host::new("FIREWALL=no\n");
    fs::write(h.d("fw/9097.fw"), "old").unwrap();
    let pve = h.vm_host_net("9097", "virtio,bridge=vmbr0,firewall=0");
    let res = h.run(&pve, &["build"]);
    assert!(res.r.is_err());
    assert!(!std::path::Path::new(&h.d("fw/9097.fw")).exists());
    assert!(pve
        .calls()
        .iter()
        .any(|c| c.contains("--net0 virtio,bridge=vmbr0,firewall=0")));
}

#[test]
fn file_based_storage_gets_qcow2() {
    for (ty, qcow) in [
        ("dir", true),
        ("nfs", true),
        ("cifs", true),
        ("zfspool", false),
    ] {
        let h = Host::new("");
        let pve = h
            .vm_host("9097")
            .answer(
                "pvesh get /storage/local-lvm --output-format json",
                &format!("{{\"type\":\"{ty}\"}}"),
            )
            .answer(
                &format!(
                    "qm set 9097 --scsi0 local-lvm:0,import-from={},discard=on,iothread=1,ssd=1,format=qcow2",
                    h.img()
                ),
                "",
            );
        let _ = h.run(&pve, &["build"]);
        let want = format!(
            "qm set 9097 --scsi0 local-lvm:0,import-from={},discard=on,iothread=1,ssd=1{}",
            h.img(),
            if qcow { ",format=qcow2" } else { "" }
        );
        assert!(pve.calls().contains(&want), "{ty}: {:?}", pve.calls());
    }
}

#[test]
fn a_storage_type_that_cannot_be_read_is_raw() {
    let h = Host::new("");
    let pve = h
        .vm_host("9097")
        .fail("pvesh get /storage/local-lvm --output-format json", 1);
    let _ = h.run(&pve, &["build"]);
    assert!(pve.calls().contains(&format!(
        "qm set 9097 --scsi0 local-lvm:0,import-from={},discard=on,iothread=1,ssd=1",
        h.img()
    )));
}

#[test]
fn provisioning_failure_shows_the_log_and_leaves_the_vm() {
    let h = Host::new("");
    let id = "9097";
    let tail =
        format!("qm guest exec {id} --timeout 20 -- tail -n 30 /var/log/mayfly-provision.log");
    let pve = h
        .vm_host(id)
        .answer(&exec(id, "done"), "{\"exitcode\":1}")
        .answer(&exec(id, "failed"), "{\"exitcode\":0}")
        .answer(
            &tail,
            "{\"exitcode\":0,\"out-data\":\"apt failed\\nbye\\n\"}",
        )
        .with_qemu_states(id, &[true]);
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("provisioning failed in template VM 9097 (log above; VM left running for a look, 'mayfly template build' again replaces it)".to_string())
    );
    // jq -r prints the log and a newline of its own.
    assert_eq!(res.err, "apt failed\nbye\n\n");
    let mut want = h.vm_head(id);
    want.extend(lines(&[
        "sleep 30",
        &exec(id, "done"),
        &exec(id, "failed"),
        &tail,
    ]));
    assert_eq!(pve.calls(), want);
    // The VM and its snippet stay for a look.
    assert!(std::path::Path::new(&h.d("snip/mayfly-9097.yaml")).exists());
}

#[test]
fn a_vm_that_stops_before_the_marker_ends_the_build() {
    let h = Host::new("");
    let id = "9097";
    let pve = h
        .vm_host(id)
        .answer(&exec(id, "done"), "{\"exitcode\":1}")
        .answer(&exec(id, "failed"), "{\"exitcode\":1}")
        .with_qemu_states(id, &[true, false]);
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("template VM 9097 stopped before provisioning finished; nothing was built (see /var/log/mayfly-provision.log in its disk, or build again)".to_string())
    );
    let mut want = h.vm_head(id);
    want.extend(lines(&["sleep 30", &exec(id, "done"), &exec(id, "failed")]));
    assert_eq!(pve.calls(), want);
}

#[test]
fn a_vm_destroyed_from_outside_ends_the_build() {
    let h = Host::new("");
    let id = "9097";
    let conf = h.d("pve/qemu-server/9097.conf");
    let pve = h
        .vm_host(id)
        .answer(&exec(id, "done"), "{\"exitcode\":1}")
        .on_call(&exec(id, "done"), move || {
            let _ = fs::remove_file(&conf);
        })
        .answer(&exec(id, "failed"), "{\"exitcode\":1}")
        .with_qemu_states(id, &[true]);
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("template VM 9097 is gone (destroyed from outside?); nothing was built".to_string())
    );
    let mut want = h.vm_head(id);
    want.extend(lines(&["sleep 30", &exec(id, "done"), &exec(id, "failed")]));
    assert_eq!(pve.calls(), want);
}

#[test]
fn a_build_that_never_finishes_gives_up_after_90_minutes() {
    let h = Host::new("");
    let id = "9097";
    let pve = h
        .vm_host(id)
        .answer(&exec(id, "done"), "{\"exitcode\":1}")
        .answer(&exec(id, "failed"), "{\"exitcode\":1}")
        .with_qemu_states(id, &[true]);
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("template VM 9097 still provisioning after 90 min; see 'qm terminal 9097' or /var/log/mayfly-provision.log inside".to_string())
    );
    let calls = pve.calls();
    let head = h.vm_head(id).len();
    let polls = &calls[head..];
    // 5400 s at 30 s: 180 rounds of a wait and the two marker checks, then the die.
    assert_eq!(polls.len(), 180 * 3);
    for round in polls.chunks(3) {
        assert_eq!(round, ["sleep 30", &exec(id, "done"), &exec(id, "failed")]);
    }
}

#[test]
fn a_marker_check_that_gets_no_answer_counts_as_no() {
    let h = Host::new("");
    let id = "9097";
    // qm itself fails (agent not up yet), then answers garbage, then says yes.
    let pve = h
        .vm_host(id)
        .sequence(
            &exec(id, "done"),
            &[(255, ""), (0, "not json"), (1, "{\"exitcode\":0}"), YES],
        )
        .answer(&exec(id, "failed"), "{\"exitcode\":1}")
        .with_qemu_states(id, &[true, true, true, true, false])
        .answer(&format!("qm shutdown {id} --timeout 300"), "");
    let _ = h.run(&pve, &["build"]);
    let calls = pve.calls();
    assert_eq!(calls.iter().filter(|c| **c == "sleep 30").count(), 4);
}

#[test]
fn a_shutdown_that_fails_falls_back_to_stop_and_then_gives_up() {
    let h = Host::new("");
    let id = "9097";
    let base = |pve: FakePve| {
        pve.answer(&exec(id, "done"), "{\"exitcode\":0}")
            .with_qemu_states(id, &[true, false])
    };
    let sh = format!("qm shutdown {id} --timeout 300");
    let stop = format!("qm stop {id}");
    let pve = base(h.vm_host(id)).fail(&sh, 255).answer(&stop, "");
    let res = h.run(&pve, &["build"]);
    // `qm stop` worked, so the build goes on (and stops at the rename, which has no answer).
    assert_eq!(res.r, Err("could not rename template VM 9097".to_string()));
    assert!(pve.calls().contains(&stop));

    let h = Host::new("");
    let pve = base(h.vm_host(id)).fail(&sh, 255).fail(&stop, 1);
    let res = h.run(&pve, &["build"]);
    assert_eq!(res.r, Err("could not stop template VM 9097".to_string()));
}

#[test]
fn a_vm_still_running_after_shutdown_is_no_template() {
    let h = Host::new("");
    let id = "9097";
    let pve = h
        .vm_host(id)
        .answer(&exec(id, "done"), "{\"exitcode\":0}")
        .with_qemu_states(id, &[true, true])
        .answer(&format!("qm shutdown {id} --timeout 300"), "");
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("template VM 9097 did not stop cleanly; nothing was built".to_string())
    );
}

#[test]
fn the_last_steps_name_what_failed() {
    let id = "9097";
    let sealed = |h: &Host| {
        h.vm_host(id)
            .answer(&exec(id, "done"), "{\"exitcode\":0}")
            .with_qemu_states(id, &[true, false])
            .answer(&format!("qm shutdown {id} --timeout 300"), "")
    };
    let rename = format!("qm set {id} --delete cicustom --name ci-template-default-{STAMP}");
    let h = Host::new("");
    let res = h.run(&sealed(&h), &["build"]);
    assert_eq!(res.r, Err("could not rename template VM 9097".to_string()));
    let h = Host::new("");
    let res = h.run(&sealed(&h).answer(&rename, ""), &["build"]);
    assert_eq!(
        res.r,
        Err("qm template 9097 failed; VM 9097 is not a template".to_string())
    );
    let h = Host::new("");
    let res = h.run(
        &sealed(&h)
            .answer(&rename, "")
            .answer("qm template 9097", "")
            .answer("qm config 9097", "name: x\n"),
        &["build"],
    );
    assert_eq!(
        res.r,
        Err("VM 9097 is not a template after qm template".to_string())
    );
}

#[test]
fn a_failing_command_ends_the_build_with_its_exit_code() {
    // bash runs under `set -e`: qm create failing is the script's exit code.
    let h = Host::new("");
    let create = h.vm_head("9097")[4].clone();
    let pve = h.vm_host("9097").fail(&create, 17);
    assert_eq!(h.run(&pve, &["build"]).r, Ok(17));
    // So does the image download.
    let h = Host::new("");
    let dl = h.vm_head("9097")[1].clone();
    let pve = h.vm_host("9097").fail(&dl, 22);
    let res = h.run(&pve, &["build"]);
    assert_eq!(res.r, Ok(22));
    assert_eq!(res.out, "downloading disk.img\n");
}

#[test]
fn the_image_must_match_sha256sums() {
    let h = Host::new("");
    let pve = h.vm_host("9097").fail("sha256sum -c -", 1);
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("checksum of the cloud image does not match SHA256SUMS".to_string())
    );
    // Not moved into place, nothing built.
    assert!(!std::path::Path::new(&h.img()).exists());
    assert_eq!(pve.calls().len(), 3);
}

#[test]
fn an_image_without_a_listed_checksum_is_refused() {
    // Nothing lists disk.img: sha256sum gets a line with no checksum in it, and refuses it.
    let h = Host::new("");
    let pve = h
        .vm_host("9097")
        .answer(
            "curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 https://img.test/x/SHA256SUMS",
            "abc  other.img\nabc  disk.img.sig\n",
        )
        .fail("sha256sum -c -", 1);
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("checksum of the cloud image does not match SHA256SUMS".to_string())
    );
    assert_eq!(pve.stdins(), [format!("  {}.part\n", h.img())]);
    // Two lines for the image: both go in, on lines of their own.
    let h = Host::new("");
    let pve = h
        .vm_host("9097")
        .answer(
            "curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 https://img.test/x/SHA256SUMS",
            "aa  disk.img\nbb *disk.img\n",
        )
        .fail("sha256sum -c -", 1);
    let _ = h.run(&pve, &["build"]);
    assert_eq!(pve.stdins(), [format!("aa\nbb  {}.part\n", h.img())]);
    // No SHA256SUMS at all.
    let h = Host::new("");
    let pve = h.vm_host("9097").fail(
        "curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 https://img.test/x/SHA256SUMS",
        22,
    );
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err(
            "no SHA256SUMS next to https://img.test/x/disk.img; refusing an unchecked image"
                .to_string()
        )
    );
    assert_eq!(pve.calls().len(), 1);
}

#[test]
fn an_unknown_profile_is_refused_before_anything_runs() {
    let h = Host::new("");
    let pve = FakePve::new();
    let res = h.run(&pve, &["build", "nope"]);
    assert_eq!(
        res.r,
        Err(format!(
            "profile nope not found in {}/profiles or {}/profiles",
            h.paths.conf_dir, h.paths.share
        ))
    );
    let res = h.run(&pve, &["build", "../x"]);
    assert!(res.r.is_err());
    assert!(pve.calls().is_empty());
}

#[test]
fn a_profile_in_the_config_dir_wins() {
    let h = Host::new("PROFILE=custom\n");
    let pve = FakePve::new();
    assert!(h.run(&pve, &["build"]).r.is_err());
    fs::create_dir_all(h.d("share/profiles/custom")).unwrap();
    fs::create_dir_all(h.d("conf/profiles/custom")).unwrap();
    let pve = h.vm_host("9097");
    let _ = h.run(&pve, &["build"]);
    // The default profile (PROFILE in pool.env) is read from the config dir first.
    assert!(pve
        .calls()
        .iter()
        .any(|c| c == &format!("tar -C {}/profiles/custom -czf - .", h.paths.conf_dir)));
}

#[test]
fn build_options_are_checked() {
    let h = Host::new("");
    let pve = FakePve::new();
    for bad in ["--lxcc", "-x", "-"] {
        let res = h.run(&pve, &["build", bad]);
        assert_eq!(
            res.r,
            Err("usage: mayfly template build [profile] [--lxc] [--foreground]".to_string()),
            "{bad}"
        );
    }
    assert!(pve.calls().is_empty());
}

// ------------------------------------------------------------------ the lock and the detached build

#[test]
fn a_build_while_another_holds_the_lock_is_refused() {
    let h = Host::new("");
    let held = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&h.paths.build_lock)
        .unwrap();
    held.try_lock().unwrap();
    let pve = h.vm_host("9097");
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err(format!(
            "another template build is running (lock {}); follow it: journalctl -fu mayfly-template-build",
            h.paths.build_lock
        ))
    );
    assert!(pve.calls().is_empty());
    assert_eq!(res.out, "");
    // Also in front of the detached start, and for a container build.
    let res = h.run(&pve, &["build", "--lxc", "--foreground"]);
    assert!(res
        .r
        .unwrap_err()
        .starts_with("another template build is running"));
    drop(held);
    // Once it lets go, the next build gets the lock (and fails on the first command here).
    let res = h.run(&FakePve::new(), &["build"]);
    assert!(!res.r.unwrap_err().starts_with("another template build"));
}

#[test]
fn the_lock_directory_is_made() {
    let mut h = Host::new("");
    h.paths.build_lock = h.d("run/lock/deeper/build.lock");
    let _ = h.run(&FakePve::new(), &["build"]);
    assert!(std::path::Path::new(&h.paths.build_lock).exists());
}

#[test]
fn a_build_outside_a_unit_starts_one_and_returns() {
    let h = Host::new("");
    let mut o = h.opts();
    o.in_unit = false;
    o.unit_env = vec![
        "--setenv=MAYFLY_CONF_DIR=/etc/mayfly".to_string(),
        "--setenv=NO_COLOR=1".to_string(),
    ];
    let pve = FakePve::new()
        .with_command("systemd-run")
        .fail("systemctl is-active -q mayfly-template-build", 3)
        .answer("systemctl reset-failed mayfly-template-build", "")
        .answer("systemd-run -q --unit=mayfly-template-build --description=mayfly template build --collect --setenv=MAYFLY_CONF_DIR=/etc/mayfly --setenv=NO_COLOR=1 /usr/local/sbin/mayfly template build ci --lxc --foreground", "");
    let res = h.run_with(&pve, &o, &["build", "ci", "--lxc"]);
    assert_eq!(res.r, Ok(0));
    assert_eq!(
        res.out,
        "template build started in mayfly-template-build\n  follow it: journalctl -fu mayfly-template-build (done when it prints 'ready'; mayfly template list)\n"
    );
    assert_eq!(
        pve.calls(),
        [
            "systemctl is-active -q mayfly-template-build",
            "systemctl reset-failed mayfly-template-build",
            "systemd-run -q --unit=mayfly-template-build --description=mayfly template build --collect --setenv=MAYFLY_CONF_DIR=/etc/mayfly --setenv=NO_COLOR=1 /usr/local/sbin/mayfly template build ci --lxc --foreground"
        ]
    );
    // The lock is let go before the unit starts, so that the unit can take it.
    let f = fs::OpenOptions::new()
        .append(true)
        .open(&h.paths.build_lock)
        .unwrap();
    assert!(f.try_lock().is_ok());
}

#[test]
fn foreground_and_a_missing_systemd_run_build_in_place() {
    let h = Host::new("");
    let mut o = h.opts();
    o.in_unit = false;
    // No systemd-run on this host: build right here.
    let pve = FakePve::new();
    let res = h.run_with(&pve, &o, &["build"]);
    assert!(res.r.is_err());
    assert!(pve.calls()[0].starts_with("curl "));
    // --foreground with systemd-run present.
    let pve = FakePve::new().with_command("systemd-run");
    let res = h.run_with(&pve, &o, &["build", "--foreground"]);
    assert!(res.r.is_err());
    assert!(pve.calls()[0].starts_with("curl "));
    // And inside a unit.
    let pve = FakePve::new().with_command("systemd-run");
    let res = h.run(&pve, &["build"]);
    assert!(res.r.is_err());
    assert!(pve.calls()[0].starts_with("curl "));
}

#[test]
fn a_unit_that_is_already_running_or_cannot_start_is_an_error() {
    let h = Host::new("");
    let mut o = h.opts();
    o.in_unit = false;
    let pve = FakePve::new()
        .with_command("systemd-run")
        .answer("systemctl is-active -q mayfly-template-build", "");
    let res = h.run_with(&pve, &o, &["build"]);
    assert_eq!(
        res.r,
        Err("mayfly-template-build is already running; follow it: journalctl -fu mayfly-template-build".to_string())
    );
    assert_eq!(
        pve.calls(),
        ["systemctl is-active -q mayfly-template-build"]
    );

    let pve = FakePve::new().with_command("systemd-run").fail(
        "systemd-run -q --unit=mayfly-template-build --description=mayfly template build --collect /usr/local/sbin/mayfly template build --foreground",
        1,
    );
    let res = h.run_with(&pve, &o, &["build"]);
    assert_eq!(
        res.r,
        Err("could not start mayfly-template-build".to_string())
    );
}

// ------------------------------------------------------------------ the VMID a build uses

#[test]
fn the_first_vmid_that_is_not_current_or_next_is_used() {
    // 9098 is current and 9099 next: 9097 it is.
    let h = Host::new("");
    let pve = h.vm_host("9097");
    let _ = h.run(&pve, &["build"]);
    assert!(pve.calls().iter().any(|c| c.starts_with("qm create 9097 ")));

    // A container template counts as taken too.
    let h = Host::new("LXC_TEMPLATE_VMID=9097\nNEXT_LXC_TEMPLATE_VMID=9099\nTEMPLATE_VMID=\nNEXT_TEMPLATE_VMID=\n");
    let pve = h.vm_host("9098");
    let _ = h.run(&pve, &["build"]);
    assert!(pve.calls().iter().any(|c| c.starts_with("qm create 9098 ")));
}

#[test]
fn no_free_template_vmid_is_an_error() {
    let h = Host::new("TEMPLATE_VMIDS=\"9098 9099\"\n");
    let pve = FakePve::new();
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("no free template VMID in TEMPLATE_VMIDS (9098 9099); add one".to_string())
    );
    assert!(pve.calls().is_empty());
}

#[test]
fn a_vmid_of_another_node_is_skipped() {
    let h2 = Host::new("TEMPLATE_VMIDS=\"9097 9096 9098 9099\"\n");
    fs::write(h2.d("pve/.vmlist"), r#"{"ids":{"9097":{"node":"pve2"}}}"#).unwrap();
    let pve = h2.vm_host("9096");
    let _ = h2.run(&pve, &["build"]);
    assert!(pve.calls().iter().any(|c| c.starts_with("qm create 9096 ")));
    // On this node: used.
    fs::write(h2.d("pve/.vmlist"), r#"{"ids":{"9097":{"node":"pve1"}}}"#).unwrap();
    let pve = h2.vm_host("9097");
    let _ = h2.run(&pve, &["build"]);
    assert!(pve.calls().iter().any(|c| c.starts_with("qm create 9097 ")));
}

#[test]
fn a_guest_that_is_not_ours_in_a_template_slot_is_never_touched() {
    let h = Host::new("");
    h.conf("qemu-server", "9097");
    let pve = FakePve::new().answer("qm config 9097", "name: web\ntags: other\n");
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("VMID 9097 exists and is not a mayfly guest".to_string())
    );
    assert_eq!(pve.calls(), ["qm config 9097"]);
}

#[test]
fn a_running_vm_that_is_no_template_yet_is_never_replaced() {
    let h = Host::new("");
    h.conf("qemu-server", "9097");
    let pve = FakePve::new()
        .answer("qm config 9097", "name: ci-template-build\ntags: mayfly\n")
        .with_running_qemu("9097");
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("VMID 9097 is running and not a template yet (a build in progress?); stop it first if it is stale".to_string())
    );
    assert!(pve.calls().iter().all(|c| c.starts_with("qm config")));
}

#[test]
fn a_template_with_clones_is_never_replaced() {
    let h = Host::new("");
    h.conf("qemu-server", "9097");
    h.conf("qemu-server", "9010");
    h.conf("lxc", "9011");
    fs::write(
        h.d("pve/qemu-server/9010.conf"),
        "name: ci-job\nscsi0: local-lvm:base-9097-disk-0/vm-9010-disk-0,size=80G\n",
    )
    .unwrap();
    let tpl = "name: ci-template\ntags: mayfly\ntemplate: 1\n";
    let pve = FakePve::new().answer("qm config 9097", tpl);
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("template 9097 still has clones (9010)".to_string())
    );
    assert!(pve.calls().iter().all(|c| c.starts_with("qm config")));
    // A container clone counts as well.
    fs::write(h.d("pve/qemu-server/9010.conf"), "name: ci-job\n").unwrap();
    fs::write(
        h.d("pve/lxc/9011.conf"),
        "rootfs: local-lvm:base-9097-disk-0/vm-9011-disk-0\n",
    )
    .unwrap();
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.r,
        Err("template 9097 still has clones (9011)".to_string())
    );
    // The template's own config does not count, and neither does another base.
    fs::write(
        h.d("pve/lxc/9011.conf"),
        "rootfs: local-lvm:base-9098-disk-0\n",
    )
    .unwrap();
    fs::write(
        h.d("pve/qemu-server/9097.conf"),
        "scsi0: base-9097-disk-0\n",
    )
    .unwrap();
    let res = h.run(&pve, &["build"]);
    assert!(!res.r.unwrap_err().starts_with("template 9097 still has"));
}

#[test]
fn a_failed_old_template_removal_warns_and_the_build_goes_on() {
    let h = Host::new("");
    h.conf("qemu-server", "9097");
    let tpl = "name: ci-template-old\ntags: mayfly\ntemplate: 1\n";
    let pve = h.vm_host("9097").answer("qm config 9097", tpl).fail(
        "qm destroy 9097 --purge 1 --destroy-unreferenced-disks 1",
        5,
    );
    let res = h.run(&pve, &["build"]);
    assert_eq!(
        res.err,
        "removing old template 9097\nwarning: could not remove old template 9097 (exit 5); remove it by hand: qm destroy 9097 --purge 1 --destroy-unreferenced-disks 1\n"
    );
    assert!(pve.calls().iter().any(|c| c.starts_with("qm create 9097 ")));
}

#[test]
fn an_old_template_is_destroyed_before_the_build() {
    let h = Host::new("");
    h.conf("qemu-server", "9097");
    let tpl = "name: ci-template-old\ntags: mayfly\ntemplate: 1\n";
    let pve = h.vm_host("9097").answer("qm config 9097", tpl).answer(
        "qm destroy 9097 --purge 1 --destroy-unreferenced-disks 1",
        "",
    );
    let res = h.run(&pve, &["build"]);
    // The progress line goes to stderr: the function's stdout is the VMID.
    assert_eq!(res.err, "removing old template 9097\n");
    assert!(res.out.starts_with("downloading disk.img\n"));
    let calls = pve.calls();
    assert_eq!(
        &calls[..4],
        [
            "qm config 9097",
            "qm config 9097",
            "qm destroy 9097 --purge 1 --destroy-unreferenced-disks 1",
            "curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 https://img.test/x/SHA256SUMS"
        ]
    );
}

#[test]
fn a_container_template_is_destroyed_with_pct() {
    let h = Host::new("");
    h.conf("lxc", "9097");
    let pve = FakePve::new()
        .answer(
            "pct config 9097",
            "hostname: ci-template-old\ntags: mayfly\ntemplate: 1\n",
        )
        .answer(
            "pct destroy 9097 --purge 1 --destroy-unreferenced-disks 1",
            "",
        );
    let _ = h.run(&pve, &["build"]);
    assert!(pve
        .calls()
        .contains(&"pct destroy 9097 --purge 1 --destroy-unreferenced-disks 1".to_string()));
}

#[test]
fn a_destroy_that_fails_does_not_stop_the_build() {
    // bash runs free_template_vmid in a command substitution, where `set -e` is off: the build goes on
    // and fails later, at `qm create`.
    let h = Host::new("");
    h.conf("qemu-server", "9097");
    let pve = h
        .vm_host("9097")
        .answer("qm config 9097", "name: ci-t\ntags: mayfly\ntemplate: 1\n")
        .fail(
            "qm destroy 9097 --purge 1 --destroy-unreferenced-disks 1",
            2,
        );
    let _ = h.run(&pve, &["build"]);
    assert!(pve.calls().iter().any(|c| c.starts_with("qm create 9097 ")));
}

// ------------------------------------------------------------------ LXC build

const CT_FILE: &str = "ubuntu-24.04-standard_24.04-2_amd64.tar.zst";

impl Host {
    fn ct_host(&self, id: &str, listed: &str) -> FakePve {
        let conf = self.d(&format!("pve/lxc/{id}.conf"));
        let create = format!(
            "pct create {id} local:vztmpl/{CT_FILE} --hostname ci-template-build --unprivileged 1 --features nesting=1,keyctl=1 --ostype ubuntu --onboot 0 --cores 8 --memory 16384 --swap 0 --rootfs local-lvm:80 --net0 name=eth0,bridge=vmbr0,ip=dhcp,firewall=1 --tags mayfly --description profile default"
        );
        let attach = |rest: &str| format!("timeout 10 lxc-attach -n {id} -- {rest}");
        FakePve::new()
            .with_command("pveam")
            .with_command("lxc-attach")
            .answer("pveam update", "")
            .answer(
                "pveam available --section system",
                &format!(
                    "system          debian-12-standard_12.7-1_amd64.tar.zst\nsystem          ubuntu-24.04-standard_24.04-1_amd64.tar.zst\nsystem          {CT_FILE}\nsystem          ubuntu-22.04-standard_22.04-1_amd64.tar.zst\n"
                ),
            )
            .answer("pveam list local", listed)
            .answer("pveam download local ubuntu-24.04-standard_24.04-2_amd64.tar.zst", "")
            .answer(&create, "")
            .on_call(&create, move || fs::write(&conf, "").unwrap())
            .answer(&format!("pct start {id}"), "")
            .sequence(
                &attach("systemctl is-system-running"),
                &[(1, "starting\n"), (1, "degraded\n")],
            )
            .sequence(&attach("getent hosts github.com"), &[(2, ""), (0, "")])
    }

    fn ct_head(&self, id: &str, download: bool) -> Vec<String> {
        let share = self.d("share");
        let attach = |rest: &str| format!("timeout 10 lxc-attach -n {id} -- {rest}");
        let mut v = lines(&[
            "pveam update",
            "pveam available --section system",
            "pveam list local",
        ]);
        if download {
            v.push(format!("pveam download local {CT_FILE}"));
        }
        v.push(format!("pct create {id} local:vztmpl/{CT_FILE} --hostname ci-template-build --unprivileged 1 --features nesting=1,keyctl=1 --ostype ubuntu --onboot 0 --cores 8 --memory 16384 --swap 0 --rootfs local-lvm:80 --net0 name=eth0,bridge=vmbr0,ip=dhcp,firewall=1 --tags mayfly --description profile default"));
        v.push(format!("pct start {id}"));
        v.push(attach("systemctl is-system-running"));
        v.push("sleep 2".into());
        v.push(attach("systemctl is-system-running"));
        v.push(attach("getent hosts github.com"));
        v.push("sleep 2".into());
        v.push(attach("getent hosts github.com"));
        for f in [
            "provision.sh",
            "ci-job.sh",
            "mayfly-job.service",
            "template.env",
        ] {
            v.push(format!("pct push {id} {share}/{f} /root/{f}"));
        }
        v.push(format!(
            "tar -C {share}/profiles/default -czf {} .",
            self.d("tmp/profile.tgz")
        ));
        v.push(format!(
            "pct push {id} {} /root/profile.tgz",
            self.d("tmp/profile.tgz")
        ));
        v.push(format!("timeout 5400 pct exec {id} -- env MAYFLY_GUEST=lxc CACHE_IP= bash -c bash /root/provision.sh > /var/log/mayfly-provision.log 2>&1"));
        v
    }

    fn ct_pushes(&self, pve: FakePve, id: &str) -> FakePve {
        let share = self.d("share");
        let mut pve = pve;
        for f in [
            "provision.sh",
            "ci-job.sh",
            "mayfly-job.service",
            "template.env",
        ] {
            pve = pve.answer(&format!("pct push {id} {share}/{f} /root/{f}"), "");
        }
        pve.answer(
            &format!(
                "tar -C {share}/profiles/default -czf {} .",
                self.d("tmp/profile.tgz")
            ),
            "",
        )
        .answer(
            &format!(
                "pct push {id} {} /root/profile.tgz",
                self.d("tmp/profile.tgz")
            ),
            "",
        )
    }
}

fn ct_provision(id: &str) -> String {
    format!("timeout 5400 pct exec {id} -- env MAYFLY_GUEST=lxc CACHE_IP= bash -c bash /root/provision.sh > /var/log/mayfly-provision.log 2>&1")
}

#[test]
fn an_lxc_build_runs_every_step_in_order() {
    let h = Host::new("");
    fs::create_dir_all(h.d("tmp")).unwrap();
    let id = "9097";
    let pve = h
        .ct_pushes(
            h.ct_host(
                id,
                "local:vztmpl/ubuntu-22.04-standard_22.04-1_amd64.tar.zst\n",
            ),
            id,
        )
        .answer(&ct_provision(id), "")
        .answer(&format!("pct stop {id}"), "")
        .answer(
            &format!("pct set {id} --hostname ci-template-default-{STAMP}"),
            "",
        )
        .answer(&format!("pct template {id}"), "")
        .answer(&format!("pct config {id}"), "hostname: x\ntemplate: 1\n");
    let res = h.run(&pve, &["build", "--lxc"]);
    assert_eq!(res.r, Ok(0));
    assert_eq!(
        res.out,
        format!(
            "downloading {CT_FILE}\nbuilding container template 9097, profile default (15 to 30 min)\ncontainer template 9097 ready (ci-template-default-{STAMP})\n  next: mayfly template canary 9097, watch a job, then mayfly template promote 9097\n"
        )
    );
    assert_eq!(res.err, "");
    let mut want = h.ct_head(id, true);
    want.extend(lines(&[
        "pct stop 9097",
        &format!("pct set 9097 --hostname ci-template-default-{STAMP}"),
        "pct template 9097",
        "pct config 9097",
    ]));
    assert_eq!(pve.calls(), want);
    // The tarball is removed again, and the firewall file is there.
    assert!(!std::path::Path::new(&h.d("tmp/profile.tgz")).exists());
    assert!(std::path::Path::new(&h.d("fw/9097.fw")).exists());
}

#[test]
fn an_image_that_is_already_downloaded_is_not_fetched_again() {
    let h = Host::new("");
    fs::create_dir_all(h.d("tmp")).unwrap();
    let id = "9097";
    let pve = h
        .ct_pushes(
            h.ct_host(id, &format!("local:vztmpl/{CT_FILE}  123MB\n")),
            id,
        )
        .answer(&ct_provision(id), "")
        .answer(&format!("pct stop {id}"), "");
    let res = h.run(&pve, &["build", "--lxc"]);
    assert!(res.out.starts_with("building container template 9097"));
    let want = h.ct_head(id, false);
    assert_eq!(&pve.calls()[..want.len()], &want[..]);
}

#[test]
fn a_failed_pveam_update_only_warns() {
    let h = Host::new("");
    fs::create_dir_all(h.d("tmp")).unwrap();
    let pve = h
        .ct_host("9097", &format!("local:vztmpl/{CT_FILE}\n"))
        .fail("pveam update", 1);
    let res = h.run(&pve, &["build", "--lxc"]);
    assert!(res.out.starts_with(
        "warning: pveam update failed; using the cached image index\nbuilding container template 9097"
    ));
}

#[test]
fn the_newest_container_image_is_chosen_by_version() {
    let h = Host::new("LXC_OSTEMPLATE=ubuntu-24.04-standard\n");
    fs::create_dir_all(h.d("tmp")).unwrap();
    let pve = h
        .ct_host("9097", "")
        .answer(
            "pveam available --section system",
            "system ubuntu-24.04-standard_24.04-2_amd64.tar.zst\nsystem ubuntu-24.04-standard_24.04-10_amd64.tar.zst\nsystem ubuntu-24.04-standard_24.04-9_amd64.tar.zst\n",
        )
        .answer("pveam download local ubuntu-24.04-standard_24.04-10_amd64.tar.zst", "");
    let _ = h.run(&pve, &["build", "--lxc"]);
    let calls = pve.calls();
    assert!(
        calls.contains(
            &"pveam download local ubuntu-24.04-standard_24.04-10_amd64.tar.zst".to_string()
        ),
        "{calls:?}"
    );
}

#[test]
fn no_matching_container_image_dies_with_a_message() {
    let h = Host::new("LXC_OSTEMPLATE=nixos\n");
    let pve = h.ct_host("9097", "");
    let res = h.run(&pve, &["build", "--lxc"]);
    assert_eq!(
        res.r,
        Err("no container image matching nixos in pveam available (pveam update, or check LXC_OSTEMPLATE)".to_string())
    );
    assert_eq!(
        pve.calls(),
        ["pveam update", "pveam available --section system"]
    );
}

#[test]
fn a_failed_pveam_available_names_the_exit_code() {
    let h = Host::new("");
    let pve = h
        .ct_host("9097", "")
        .fail("pveam available --section system", 3);
    let res = h.run(&pve, &["build", "--lxc"]);
    let err = res.r.unwrap_err();
    assert!(err.starts_with("pveam available failed (exit 3)"), "{err}");
    assert!(!pve.calls().iter().any(|c| c.starts_with("pveam download")));
}

#[test]
fn an_lxc_build_needs_pveam_and_lxc_attach() {
    let h = Host::new("");
    for missing in ["pveam", "lxc-attach"] {
        let other = if missing == "pveam" {
            "lxc-attach"
        } else {
            "pveam"
        };
        let pve = FakePve::new().with_command(other);
        let res = h.run(&pve, &["build", "--lxc"]);
        assert_eq!(
            res.r,
            Err("pveam/lxc-attach missing: not a Proxmox host?".to_string())
        );
        assert!(pve.calls().is_empty());
    }
    // The profile is looked up first.
    let res = h.run(&FakePve::new(), &["build", "--lxc", "nope"]);
    assert!(res.r.unwrap_err().starts_with("profile nope not found"));
}

#[test]
fn an_lxc_provisioning_failure_shows_the_log_and_leaves_the_container() {
    let h = Host::new("");
    fs::create_dir_all(h.d("tmp")).unwrap();
    let id = "9097";
    let pve = h
        .ct_pushes(h.ct_host(id, &format!("local:vztmpl/{CT_FILE}\n")), id)
        .fail(&ct_provision(id), 1)
        .answer(
            &format!("timeout 20 lxc-attach -n {id} -- tail -n 30 /var/log/mayfly-provision.log"),
            "E: broken\n",
        );
    let res = h.run(&pve, &["build", "--lxc"]);
    assert_eq!(
        res.r,
        Err("provisioning failed in container 9097 (log above; left running for a look, 'mayfly template build --lxc' again replaces it)".to_string())
    );
    assert_eq!(res.err, "E: broken\n");
    let calls = pve.calls();
    assert_eq!(
        calls.last().unwrap(),
        "timeout 20 lxc-attach -n 9097 -- tail -n 30 /var/log/mayfly-provision.log"
    );
    assert!(!calls.iter().any(|c| c.starts_with("pct stop")));
}

#[test]
fn the_lxc_waits_stop_after_60_and_30_tries() {
    let h = Host::new("");
    fs::create_dir_all(h.d("tmp")).unwrap();
    let id = "9097";
    let attach = |rest: &str| format!("timeout 10 lxc-attach -n {id} -- {rest}");
    let pve = h
        .ct_host(id, &format!("local:vztmpl/{CT_FILE}\n"))
        .answer(&attach("systemctl is-system-running"), "starting\n")
        .fail(&attach("getent hosts github.com"), 2)
        .fail(&ct_provision(id), 1);
    let _ = h.run(&pve, &["build", "--lxc"]);
    let calls = pve.calls();
    let first = calls
        .iter()
        .position(|c| c == &attach("systemctl is-system-running"))
        .unwrap();
    let state: Vec<&String> = calls[first..]
        .iter()
        .take_while(|c| !c.contains("getent"))
        .collect();
    // 60 tries with a 2 s wait between them: 59 waits.
    assert_eq!(state.len(), 60 + 59);
    let dns = calls.iter().filter(|c| c.contains("getent")).count();
    assert_eq!(dns, 30);
    assert_eq!(calls.iter().filter(|c| *c == "sleep 2").count(), 59 + 29);
}

#[test]
fn the_lxc_last_steps_name_what_failed() {
    let id = "9097";
    let steps = |h: &Host| {
        h.ct_pushes(h.ct_host(id, &format!("local:vztmpl/{CT_FILE}\n")), id)
            .answer(&ct_provision(id), "")
    };
    let go = |h: &Host, pve: &FakePve| h.run(pve, &["build", "--lxc"]).r;
    let h = Host::new("");
    fs::create_dir_all(h.d("tmp")).unwrap();
    assert_eq!(
        go(&h, &steps(&h)),
        Err("could not stop container 9097".to_string())
    );
    let h = Host::new("");
    fs::create_dir_all(h.d("tmp")).unwrap();
    let rm = h.d("pve/lxc/9097.conf");
    let pve = steps(&h).on_call(&ct_provision(id), move || {
        let _ = fs::remove_file(&rm);
    });
    assert_eq!(
        go(&h, &pve),
        Err("container 9097 is gone (destroyed from outside?); nothing was built".to_string())
    );
    let stop = format!("pct stop {id}");
    let h = Host::new("");
    fs::create_dir_all(h.d("tmp")).unwrap();
    assert_eq!(
        go(&h, &steps(&h).answer(&stop, "")),
        Err("could not rename container 9097".to_string())
    );
    let set = format!("pct set {id} --hostname ci-template-default-{STAMP}");
    let h = Host::new("");
    fs::create_dir_all(h.d("tmp")).unwrap();
    assert_eq!(
        go(&h, &steps(&h).answer(&stop, "").answer(&set, "")),
        Err("pct template 9097 failed; container 9097 is not a template".to_string())
    );
    let h = Host::new("");
    fs::create_dir_all(h.d("tmp")).unwrap();
    assert_eq!(
        go(
            &h,
            &steps(&h)
                .answer(&stop, "")
                .answer(&set, "")
                .answer("pct template 9097", "")
                .answer("pct config 9097", "hostname: x\n")
        ),
        Err("container 9097 is not a template after pct template".to_string())
    );
}

// ------------------------------------------------------------------ canary, promote

const TPL_VM: &str =
    "name: ci-template-default-20231101\ndescription: profile default\ntags: mayfly\ntemplate: 1\n";
const TPL_CT: &str = "hostname: ci-template-default-20231101\ndescription: profile default\ntags: mayfly\ntemplate: 1\n";

#[test]
fn canary_marks_a_vm_template_next() {
    let h = Host::new("");
    h.conf("qemu-server", "9097");
    let pve = FakePve::new().answer("qm config 9097", TPL_VM);
    let res = h.run(&pve, &["canary", "9097"]);
    assert_eq!(res.r, Ok(0));
    assert_eq!(
        res.out,
        "slots marked next now clone 9097\n  when their jobs pass: mayfly template promote 9097\n"
    );
    // is_template, is_ours, then the layout.
    assert_eq!(
        pve.calls(),
        ["qm config 9097", "qm config 9097", "qm config 9097"]
    );
    assert_eq!(
        h.pool_env(),
        "ORG=acme\nIMAGE_URL=https://img.test/x/disk.img\nTEMPLATE_VMID=9098\nNEXT_TEMPLATE_VMID=9097\n"
    );
}

#[test]
fn canary_marks_a_container_template_next_lxc() {
    let h = Host::new("LXC_TEMPLATE_VMID=9096\n");
    h.conf("lxc", "9095");
    let pve = FakePve::new().answer("pct config 9095", TPL_CT);
    let res = h.run(&pve, &["canary", "9095"]);
    assert_eq!(
        res.out,
        "lxc slots marked next now clone 9095\n  when their jobs pass: mayfly template promote 9095\n"
    );
    // Containers are checked for a profile description too.
    assert_eq!(
        pve.calls(),
        ["pct config 9095", "pct config 9095", "pct config 9095"]
    );
    assert_eq!(
        h.pool_env(),
        "ORG=acme\nIMAGE_URL=https://img.test/x/disk.img\nTEMPLATE_VMID=9098\nNEXT_TEMPLATE_VMID=9099\nLXC_TEMPLATE_VMID=9096\nNEXT_LXC_TEMPLATE_VMID=9095\n"
    );
}

#[test]
fn promote_switches_a_vm_template_and_clears_next() {
    let h = Host::new("# kept\nMAX_JOB_HOURS=4   # and this\n");
    h.conf("qemu-server", "9097");
    let pve = FakePve::new().answer("qm config 9097", TPL_VM);
    let res = h.run(&pve, &["promote", "9097"]);
    assert_eq!(res.out, "every slot clones 9097 from its next job on\n");
    assert_eq!(
        h.pool_env(),
        "ORG=acme\nIMAGE_URL=https://img.test/x/disk.img\nTEMPLATE_VMID=9097\nNEXT_TEMPLATE_VMID=\n# kept\nMAX_JOB_HOURS=4   # and this\n"
    );
}

#[test]
fn promote_a_container_template_only_touches_the_lxc_keys() {
    let h = Host::new("NEXT_LXC_TEMPLATE_VMID=9095\n");
    h.conf("lxc", "9095");
    let pve = FakePve::new().answer("pct config 9095", TPL_CT);
    let res = h.run(&pve, &["promote", "9095"]);
    assert_eq!(res.out, "every lxc slot clones 9095 from its next job on\n");
    assert_eq!(
        h.pool_env(),
        "ORG=acme\nIMAGE_URL=https://img.test/x/disk.img\nTEMPLATE_VMID=9098\nNEXT_TEMPLATE_VMID=9099\nNEXT_LXC_TEMPLATE_VMID=\nLXC_TEMPLATE_VMID=9095\n"
    );
}

#[test]
fn canary_and_promote_refuse_a_template_mayfly_did_not_build() {
    let h = Host::new("");
    h.conf("qemu-server", "9097");
    let pve = FakePve::new().answer(
        "qm config 9097",
        "name: ci-template-old\ntags: mayfly\ntemplate: 1\n",
    );
    for sub in ["canary", "promote"] {
        let res = h.run(&pve, &[sub, "9097"]);
        assert_eq!(
            res.r,
            Err(
                "template 9097 was not built by mayfly template build; its jobs may never start"
                    .to_string()
            ),
            "{sub}"
        );
        assert_eq!(res.out, "", "{sub}");
    }
    assert!(!h.pool_env().contains("9097"));
}

#[test]
fn canary_and_promote_need_a_template_of_ours() {
    let h = Host::new("");
    for id in ["9097", "9096", "8999", "abc"] {
        h.conf("qemu-server", id);
    }
    let pve = FakePve::new()
        .answer("qm config 9097", "name: ci-vm\ntags: mayfly\n")
        .answer("qm config 9096", "name: web\ntags: other\ntemplate: 1\n")
        .answer("qm config 8999", "name: ci-t\ntags: mayfly\ntemplate: 1\n");
    for sub in ["canary", "promote"] {
        for id in ["9097", "9096", "8999", "abc", "7777", ""] {
            let res = h.run(&pve, &[sub, id]);
            assert_eq!(
                res.r,
                Err(format!("usage: mayfly template {sub} <template vmid>")),
                "{sub} {id:?}"
            );
        }
        let res = h.run(&pve, &[sub]);
        assert!(res.r.is_err());
    }
    assert!(h.pool_env().ends_with("NEXT_TEMPLATE_VMID=9099\n"));
}

#[test]
fn pool_env_edits_keep_every_other_byte() {
    let dir = std::env::temp_dir().join(format!("mayfly-poolenv-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("pool.env");
    let path = p.display().to_string();
    let put = |text: &[u8]| fs::write(&p, text).unwrap();
    let get = || fs::read(&p).unwrap();

    // Replaced in place, the neighbours untouched (comments, blanks, odd spacing, no final newline).
    put(b"# top\r\n\nA=1\nTEMPLATE_VMID=9098 # old\n  B = 2\nexport TEMPLATE_VMID=1\nLAST=x");
    set_pool_value(&path, "TEMPLATE_VMID", "9097").unwrap();
    assert_eq!(
        get(),
        b"# top\r\n\nA=1\nTEMPLATE_VMID=9097\n  B = 2\nexport TEMPLATE_VMID=1\nLAST=x"
    );
    // The key must be at the start of the line and followed by `=`: others are not replaced.
    put(b"TEMPLATE_VMID_X=1\nMY_TEMPLATE_VMID=2\n");
    set_pool_value(&path, "TEMPLATE_VMID", "5").unwrap();
    assert_eq!(
        get(),
        b"TEMPLATE_VMID_X=1\nMY_TEMPLATE_VMID=2\nTEMPLATE_VMID=5\n"
    );
    // Every line of that key is replaced, like `sed s/^KEY=.*/.../`.
    put(b"K=1\nA=2\nK=3\n");
    set_pool_value(&path, "K", "").unwrap();
    assert_eq!(get(), b"K=\nA=2\nK=\n");
    // Appended after a file that has no final newline, on a line of its own.
    put(b"A=1");
    set_pool_value(&path, "B", "2").unwrap();
    assert_eq!(get(), b"A=1\nB=2\n");
    put(b"");
    set_pool_value(&path, "B", "2").unwrap();
    assert_eq!(get(), b"B=2\n");
    // Bytes that are not UTF-8 survive.
    put(b"X=\xff\xfe\nK=1\n");
    set_pool_value(&path, "K", "2").unwrap();
    assert_eq!(get(), b"X=\xff\xfe\nK=2\n");
    // Missing file: created.
    fs::remove_file(&p).unwrap();
    set_pool_value(&path, "K", "2").unwrap();
    assert_eq!(get(), b"K=2\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pool_env_keeps_its_mode_and_leaves_no_temp_file() {
    let dir = std::env::temp_dir().join(format!("mayfly-poolenv-mode-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("pool.env");
    for mode in [0o600, 0o640, 0o644] {
        fs::write(&p, "A=1\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        set_pool_value(&p.display().to_string(), "A", "2").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, mode);
        assert_eq!(fs::read_to_string(&p).unwrap(), "A=2\n");
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["pool.env"]);
    }
    // A directory that cannot be written is an error and the file stays as it was.
    let _ = fs::remove_dir_all(&dir);
    assert!(set_pool_value(&p.display().to_string(), "A", "3").is_err());
}

// ------------------------------------------------------------------ list

#[test]
fn list_shows_templates_with_their_role() {
    let h = Host::new("LXC_TEMPLATE_VMID=9095\nNEXT_LXC_TEMPLATE_VMID=9094\nTEMPLATE_VMIDS=\"9099 9098 9097 9096 9095 9094 9093\"\n");
    for (kind, id) in [
        ("qemu-server", "9098"),
        ("qemu-server", "9099"),
        ("qemu-server", "9097"),
        ("lxc", "9095"),
        ("lxc", "9094"),
        ("lxc", "9093"),
    ] {
        h.conf(kind, id);
    }
    let pve = FakePve::new()
        .answer(
            "qm config 9098",
            "name: ci-template-default-20231101\ntemplate: 1\n",
        )
        .answer(
            "qm config 9099",
            "name: ci-template-default-20231110\ntemplate: 1\n",
        )
        .answer("qm config 9097", "name: ci-template-build\n")
        .answer("pct config 9095", "hostname: ci-ct-1\n")
        .answer("pct config 9094", "hostname: ci-ct-2\n")
        .fail("pct config 9093", 2);
    let res = h.run(&pve, &["list"]);
    assert_eq!(res.r, Ok(0));
    // 9096 does not exist. 9093 prints with an empty name (its config cannot be read).
    assert_eq!(
        res.out,
        "9099 vm ci-template-default-20231110 (next)\n9098 vm ci-template-default-20231101 (current)\n9097 vm ci-template-build\n9095 ct ci-ct-1 (current)\n9094 ct ci-ct-2 (next)\n9093 ct \n"
    );
    assert_eq!(res.err, "");
}

#[test]
fn list_with_nothing_prints_nothing() {
    let h = Host::new("");
    let pve = FakePve::new();
    let res = h.run(&pve, &["list"]);
    assert_eq!(res.r, Ok(0));
    assert_eq!(res.out, "");
    assert!(pve.calls().is_empty());
}

// ------------------------------------------------------------------ usage

#[test]
fn an_unknown_subcommand_is_a_usage_error() {
    let h = Host::new("");
    for args in [vec![], vec!["frobnicate"], vec!["Build"]] {
        let res = h.run(&FakePve::new(), &args);
        assert_eq!(
            res.r,
            Err(
                "usage: mayfly template build [profile] [--lxc]|canary <vmid>|promote <vmid>|list"
                    .to_string()
            )
        );
    }
}

// ------------------------------------------------------------------ the binary

#[test]
fn the_binary_runs_template_itself() {
    // 0.5.0 and earlier forwarded `template` to mayfly.sh: with no pool.env the command says so itself, and
    // a script that is still there (it would print "forwarded") is never run.
    let root = std::env::temp_dir().join(format!("mayfly-tbin-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let script = root.join("mayfly.sh");
    fs::write(&script, "#!/bin/sh\necho forwarded\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mayfly"))
        .args(["template", "list"])
        .env("MAYFLY_SHARE", &root)
        .env("MAYFLY_CONF_DIR", root.join("none"))
        .env_remove("NO_COLOR")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "error: {}/none/pool.env missing (run install.sh)\n",
            root.display()
        )
    );
    let _ = fs::remove_dir_all(&root);
}
