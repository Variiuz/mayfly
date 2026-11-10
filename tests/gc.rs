//! `gc`, `enable` and `disable` against the recording fake Proxmox host and a fake GitHub: the exact
//! commands, their order and the exact output. Nothing here needs Proxmox, a network or root.

use mayfly::cmd_gc;
use mayfly::cmd_slots::{self, Verb};
use mayfly::config::{self, Config, Paths};
use mayfly::github::{RunnerApi, RunnerRec};
use mayfly::out::Out;
use mayfly::pve::FakePve;
use mayfly::Ctx;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ------------------------------------------------------------------ helpers

const NOW: u64 = 1_700_000_000;

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW)
}

struct Host {
    root: PathBuf,
    paths: Paths,
    config: Config,
}

impl Host {
    fn new(pool_extra: &str, slots: &str) -> Host {
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "mayfly-gc-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        let r = root.display().to_string();
        for d in ["conf", "pve/qemu-server", "pve/lxc", "fw", "snip"] {
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
            format!("ORG=acme\nRUNNER_PREFIX=mf\n{pool_extra}"),
        )
        .unwrap();
        fs::write(&paths.slots_conf, slots).unwrap();
        let (config, _) = config::load(&paths, &Host::pve_base(&r), "pve1", true).unwrap();
        Host {
            root,
            paths,
            config,
        }
    }

    fn pve_base(root: &str) -> FakePve {
        FakePve::new().answer("pvesm path local:snippets/x", &format!("{root}/snip/x\n"))
    }

    /// A fake host that answers the snippet storage.
    fn pve(&self) -> FakePve {
        Host::pve_base(&self.root.display().to_string())
    }

    fn dir(&self, sub: &str) -> String {
        format!("{}/{sub}", self.root.display())
    }

    /// A guest with its config file and what `qm config` / `pct config` answers.
    fn guest(&self, pve: FakePve, kind: &str, id: &str, cfg: &str) -> FakePve {
        fs::write(self.dir(&format!("pve/{kind}/{id}.conf")), "").unwrap();
        let tool = if kind == "lxc" { "pct" } else { "qm" };
        pve.answer(&format!("{tool} config {id}"), cfg)
    }

    fn ctx<'a>(&'a self, pve: &'a FakePve) -> Ctx<'a> {
        Ctx {
            paths: &self.paths,
            pve,
            node: "pve1",
            meminfo: "",
        }
    }

    fn gc(&self, pve: &FakePve, gh: &FakeGh) -> (Result<i32, String>, String) {
        let mut buf = Vec::new();
        let r = cmd_gc::run_with(
            &self.ctx(pve),
            &self.config,
            gh,
            &Out::plain(),
            &mut buf,
            now(),
        )
        .map_err(|d| d.0);
        (r, String::from_utf8(buf).unwrap())
    }

    fn slots(&self, pve: &FakePve, verb: Verb, args: &[&str]) -> (Result<i32, String>, String) {
        self.slots_out(pve, verb, args, &Out::plain())
    }

    fn slots_out(
        &self,
        pve: &FakePve,
        verb: Verb,
        args: &[&str],
        out: &Out,
    ) -> (Result<i32, String>, String) {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let mut buf = Vec::new();
        let r = cmd_slots::run(&self.ctx(pve), verb, &args, out, &mut buf).map_err(|d| d.0);
        (r, String::from_utf8(buf).unwrap())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A GitHub that serves the given pages of runners and records what it is asked.
struct FakeGh {
    token: Result<String, String>,
    pages: Vec<Vec<RunnerRec>>,
    calls: RefCell<Vec<String>>,
}

impl FakeGh {
    fn new(pages: Vec<Vec<RunnerRec>>) -> FakeGh {
        FakeGh {
            token: Ok("tok".to_string()),
            pages,
            calls: RefCell::new(vec![]),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl RunnerApi for FakeGh {
    fn install_token(&self) -> Result<String, String> {
        self.calls.borrow_mut().push("token".to_string());
        self.token.clone()
    }

    fn runners_page(&self, token: &str, page: u32) -> Vec<RunnerRec> {
        self.calls.borrow_mut().push(format!("page {page} {token}"));
        self.pages
            .get(page as usize - 1)
            .cloned()
            .unwrap_or_default()
    }

    fn delete_runner(&self, token: &str, id: &str) {
        self.calls.borrow_mut().push(format!("delete {id} {token}"));
    }
}

fn runner(id: u32, name: &str, status: &str, busy: &str) -> RunnerRec {
    RunnerRec {
        id: id.to_string(),
        name: name.to_string(),
        status: status.to_string(),
        busy: busy.to_string(),
    }
}

fn no_gh() -> FakeGh {
    FakeGh::new(vec![])
}

const OURS: &str = "name: ci-a\ntags: mayfly\n";
const SLOTS: &str = "a-1 9002 linux 8 16 60 no current\n\
                     b-1 9004 linux 8 16 60 no current\n\
                     c-1 9051 small 2 4 20 no current kind=lxc\n";

fn stop(id: &str) -> String {
    format!("qm stop {id} --skiplock 1 --timeout 30")
}

fn destroy(id: &str) -> String {
    format!("qm destroy {id} --purge 1 --destroy-unreferenced-disks 1 --skiplock 1")
}

fn ct_stop(id: &str) -> String {
    format!("pct stop {id} --skiplock 1")
}

fn ct_destroy(id: &str) -> String {
    format!("pct destroy {id} --purge 1 --destroy-unreferenced-disks 1 --force 1")
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// ------------------------------------------------------------------ gc: stale registrations

#[test]
fn stale_registrations_are_removed_page_by_page() {
    let h = Host::new("", SLOTS);
    let pve = h.guest(
        h.pve(),
        "qemu-server",
        "9002",
        "name: ci-a\ntags: mayfly\ndescription: runner mf-a-1-111, template 9098\n",
    );
    let pve = pve.answer("systemctl is-active -q mayfly-slot@a-1", "");
    let fresh = format!("mf-fresh-1-{}", NOW - 60);
    let gh = FakeGh::new(vec![
        vec![
            runner(11, "mf-old-1-5", "offline", "false"),
            // A live guest names it.
            runner(12, "mf-a-1-111", "offline", "false"),
            // Busy, online, someone else's, fresh.
            runner(13, "mf-busy-1-6", "offline", "true"),
            runner(14, "mf-on-1-7", "online", "false"),
            runner(15, "other-9", "offline", "false"),
            runner(16, "mf-evil", "offline", "false"),
            runner(17, &fresh, "offline", "false"),
            runner(18, "mf-nonum-x", "offline", "false"),
            runner(19, "mf-nullbusy-1-8", "offline", "null"),
            runner(20, "mfx-old-1-9", "offline", "false"),
        ],
        vec![runner(21, "mf-p2-1-7", "offline", "false")],
    ]);
    let (r, out) = h.gc(&pve, &gh);
    assert_eq!(r, Ok(0));
    assert_eq!(
        out,
        "gc: removing stale registration mf-old-1-5\n\
         gc: removing stale registration mf-evil\n\
         gc: removing stale registration mf-nonum-x\n\
         gc: removing stale registration mf-p2-1-7\n"
    );
    // A page is handled before the next one is asked for; the last (empty) page ends it.
    assert_eq!(
        gh.calls(),
        strs(&[
            "token",
            "page 1 tok",
            "delete 11 tok",
            "delete 16 tok",
            "delete 18 tok",
            "page 2 tok",
            "delete 21 tok",
            "page 3 tok"
        ])
    );
    // The names of live guests are read first, from the guests that exist; then the guest of a-1 is
    // checked (ours, not a template, its service is active).
    assert_eq!(
        pve.calls(),
        [
            "qm config 9002",
            "qm config 9002",
            "qm config 9002",
            "systemctl is-active -q mayfly-slot@a-1"
        ]
    );
}

#[test]
fn the_prefix_is_runner_prefix_dash() {
    let h = Host::new("RUNNER_PREFIX=ci-pve1\n", "");
    let gh = FakeGh::new(vec![vec![
        runner(1, "ci-pve1-a-1-5", "offline", "false"),
        runner(2, "ci-pve1", "offline", "false"),
        runner(3, "ci-pve10-a-1-5", "offline", "false"),
    ]]);
    let (_, out) = h.gc(&h.pve(), &gh);
    assert_eq!(out, "gc: removing stale registration ci-pve1-a-1-5\n");
}

#[test]
fn a_registration_ten_minutes_old_is_removed_and_a_younger_one_kept() {
    let h = Host::new("", "");
    let gh = FakeGh::new(vec![vec![
        runner(1, &format!("mf-a-1-{}", NOW - 599), "offline", "false"),
        runner(2, &format!("mf-b-1-{}", NOW - 600), "offline", "false"),
        runner(3, &format!("mf-c-1-{}", NOW + 3600), "offline", "false"),
    ]]);
    let (_, out) = h.gc(&h.pve(), &gh);
    assert_eq!(
        out,
        format!("gc: removing stale registration mf-b-1-{}\n", NOW - 600)
    );
}

#[test]
fn a_runner_is_removed_by_its_id() {
    let h = Host::new("", "");
    let gh = FakeGh::new(vec![vec![runner(7, "mf-a-1-5", "offline", "false")]]);
    let (_, out) = h.gc(&h.pve(), &gh);
    assert_eq!(out, "gc: removing stale registration mf-a-1-5\n");
    assert_eq!(
        gh.calls(),
        ["token", "page 1 tok", "delete 7 tok", "page 2 tok"]
    );
}

#[test]
fn an_empty_id_shifts_the_fields_like_read_r_does() {
    // bash reads "<id> <name> <status> <busy>" with `read -r`: with no id the name becomes the id, the
    // status the name, and nothing matches.
    let h = Host::new("", "");
    let gh = FakeGh::new(vec![vec![RunnerRec {
        id: String::new(),
        name: "mf-a-1-5".to_string(),
        status: "offline".to_string(),
        busy: "false".to_string(),
    }]]);
    let (r, out) = h.gc(&h.pve(), &gh);
    assert_eq!((r, out.as_str()), (Ok(0), ""));
    assert_eq!(gh.calls(), ["token", "page 1 tok", "page 2 tok"]);
}

#[test]
fn no_token_stops_before_anything_is_removed() {
    let h = Host::new("", SLOTS);
    let pve = h.guest(h.pve(), "qemu-server", "9003", OURS);
    let mut gh = no_gh();
    gh.token = Err("HTTP 401".to_string());
    let (r, out) = h.gc(&pve, &gh);
    assert_eq!(r, Err("could not mint an installation token".to_string()));
    assert_eq!(out, "");
    assert_eq!(gh.calls(), ["token"]);
    assert_eq!(pve.calls(), ["qm config 9003"]);
}

#[test]
fn a_github_that_cannot_be_asked_still_lets_the_guests_be_cleaned() {
    // An error page reads as an empty one: the registrations are skipped, the rest goes on.
    let h = Host::new("", SLOTS);
    let pve = h.guest(h.pve(), "qemu-server", "9003", OURS);
    let pve = pve.answer(&stop("9003"), "").answer(&destroy("9003"), "");
    let (r, out) = h.gc(&pve, &no_gh());
    assert_eq!(r, Ok(0));
    assert_eq!(out, "gc: destroying orphan VM 9003\n");
}

// ------------------------------------------------------------------ gc: orphan guests

/// 9002 a-1 (active), 9003 no slot, 9004 b-1 (inactive), 9005 wrong name, 9006 wrong tag, 9007 legacy
/// tag, 9008 template, 9009 on another node, 9051 container c-1 (active), 9052 orphan container, 9097 a
/// template build in progress, 9100 outside the range.
fn pool() -> (Host, FakePve) {
    let h = Host::new("LEGACY_TAGS=oldci\n", SLOTS);
    let mut pve = h.pve();
    let vms = [
        (
            "9002",
            "name: ci-a\ntags: mayfly\ndescription: runner mf-a-1-111, template 9098\n",
        ),
        ("9003", OURS),
        ("9004", OURS),
        ("9005", "name: web\ntags: mayfly\n"),
        ("9006", "name: ci-x\ntags: other\n"),
        ("9007", "name: ci-legacy\ntags: oldci\n"),
        ("9008", "name: ci-template\ntags: mayfly\ntemplate: 1\n"),
        ("9009", OURS),
        ("9097", "name: ci-building\ntags: mayfly\n"),
        ("9100", OURS),
    ];
    for (id, cfg) in vms {
        pve = h.guest(pve, "qemu-server", id, cfg);
    }
    pve = h.guest(pve, "lxc", "9051", "hostname: ci-c-1\ntags: mayfly\n");
    pve = h.guest(pve, "lxc", "9052", "hostname: ci-c-2\ntags: mayfly\n");
    fs::write(
        h.dir("pve/.vmlist"),
        r#"{"ids":{"9009":{"node":"pve2"},"9002":{"node":"pve1"}}}"#,
    )
    .unwrap();
    for id in ["9003", "9004", "9007"] {
        pve = pve.answer(&stop(id), "").answer(&destroy(id), "");
    }
    pve = pve
        .answer(&ct_stop("9052"), "")
        .answer(&ct_destroy("9052"), "")
        .answer("systemctl is-active -q mayfly-slot@a-1", "")
        .answer("systemctl is-active -q mayfly-slot@c-1", "")
        .fail("systemctl is-active -q mayfly-slot@b-1", 3);
    (h, pve)
}

#[test]
fn orphan_guests_are_destroyed_and_nothing_else() {
    let (h, pve) = pool();
    let (r, out) = h.gc(&pve, &no_gh());
    assert_eq!(r, Ok(0));
    assert_eq!(
        out,
        "gc: destroying orphan VM 9003\n\
         gc: destroying orphan VM 9004\n\
         gc: destroying orphan VM 9007\n\
         gc: destroying orphan VM 9052\n"
    );
    let mut want: Vec<String> = vec![];
    // The names of live guests, in VMID order (9009 is another node's but has a config file here).
    want.extend(strs(&[
        "qm config 9002",
        "qm config 9003",
        "qm config 9004",
        "qm config 9005",
        "qm config 9006",
        "qm config 9007",
        "qm config 9008",
        "qm config 9009",
        "pct config 9051",
        "pct config 9052",
        "qm config 9097",
    ]));
    // Per guest: is_ours, is_template, then the slot's service.
    want.extend(strs(&["qm config 9002", "qm config 9002"]));
    want.extend(strs(&["systemctl is-active -q mayfly-slot@a-1"]));
    // 9003: no slot, so no service is asked; destroy checks is_ours and is_template again.
    want.extend(strs(&["qm config 9003", "qm config 9003"]));
    want.extend(strs(&["qm config 9003", "qm config 9003"]));
    want.push(stop("9003"));
    want.push(destroy("9003"));
    want.extend(strs(&["qm config 9004", "qm config 9004"]));
    want.extend(strs(&["systemctl is-active -q mayfly-slot@b-1"]));
    want.extend(strs(&["qm config 9004", "qm config 9004"]));
    want.push(stop("9004"));
    want.push(destroy("9004"));
    // 9005 wrong name and 9006 wrong tag: one read each. 9009 is another node's: none.
    want.extend(strs(&["qm config 9005", "qm config 9006"]));
    want.extend(strs(&["qm config 9007", "qm config 9007"]));
    want.extend(strs(&["qm config 9007", "qm config 9007"]));
    want.push(stop("9007"));
    want.push(destroy("9007"));
    // 9008 is a template.
    want.extend(strs(&["qm config 9008", "qm config 9008"]));
    want.extend(strs(&["pct config 9051", "pct config 9051"]));
    want.extend(strs(&["systemctl is-active -q mayfly-slot@c-1"]));
    want.extend(strs(&["pct config 9052", "pct config 9052"]));
    want.extend(strs(&["pct config 9052", "pct config 9052"]));
    want.push(ct_stop("9052"));
    want.push(ct_destroy("9052"));
    // 9097 is a template VMID: ours, not a template yet, never destroyed.
    want.extend(strs(&["qm config 9097", "qm config 9097"]));
    assert_eq!(pve.calls(), want);
}

#[test]
fn a_destroyed_guest_loses_its_firewall_file_and_snippet() {
    let (h, pve) = pool();
    for f in ["fw/9003.fw", "fw/9002.fw", "snip/mayfly-9003.yaml"] {
        fs::write(h.dir(f), "x").unwrap();
    }
    let (_, _) = h.gc(&pve, &no_gh());
    assert!(!Path::new(&h.dir("fw/9003.fw")).exists());
    assert!(!Path::new(&h.dir("snip/mayfly-9003.yaml")).exists());
    assert!(Path::new(&h.dir("fw/9002.fw")).exists());
}

#[test]
fn a_template_is_never_destroyed_even_without_a_slot() {
    let h = Host::new("", SLOTS);
    let pve = h.guest(
        h.pve(),
        "qemu-server",
        "9050",
        "name: ci-template-default\ntags: mayfly\ntemplate: 1\n",
    );
    let pve = h.guest(
        pve,
        "lxc",
        "9051",
        "hostname: ci-template\ntags: mayfly\ntemplate: 1\n",
    );
    let (r, out) = h.gc(&pve, &no_gh());
    assert_eq!(r, Ok(0));
    assert_eq!(out, "");
    assert!(pve
        .calls()
        .iter()
        .all(|c| c.ends_with("config 9050") || c.ends_with("config 9051")));
}

#[test]
fn the_template_vmids_are_taken_from_pool_env() {
    let h = Host::new("TEMPLATE_VMIDS=\"9003 9010\"\n", SLOTS);
    let pve = h.guest(h.pve(), "qemu-server", "9003", OURS);
    let (_, out) = h.gc(&pve, &no_gh());
    assert_eq!(out, "");
    // 9097 is no longer protected.
    let pve = h.guest(h.pve(), "qemu-server", "9097", OURS);
    let pve = pve.answer(&stop("9097"), "").answer(&destroy("9097"), "");
    let (_, out) = h.gc(&pve, &no_gh());
    assert_eq!(out, "gc: destroying orphan VM 9097\n");
}

#[test]
fn a_slot_is_found_by_its_vmid_as_awk_compares_it() {
    let h = Host::new(
        "",
        "a-1 09002 linux 8 16 60 no current\n# off 9003 x 1 1 1 no current\n",
    );
    let pve = h.guest(h.pve(), "qemu-server", "9002", OURS);
    let pve = h.guest(pve, "qemu-server", "9003", OURS);
    let pve = pve
        .answer("systemctl is-active -q mayfly-slot@a-1", "")
        .answer(&stop("9003"), "")
        .answer(&destroy("9003"), "");
    let (_, out) = h.gc(&pve, &no_gh());
    // 09002 is 9002; a commented-out slot is no slot.
    assert_eq!(out, "gc: destroying orphan VM 9003\n");
}

#[test]
fn a_failed_destroy_ends_gc_with_its_exit_code() {
    let h = Host::new("", SLOTS);
    let pve = h.guest(h.pve(), "qemu-server", "9003", OURS);
    let pve = h.guest(pve, "qemu-server", "9005", OURS);
    let pve = pve
        .answer(&stop("9003"), "")
        .fail(&destroy("9003"), 5)
        .answer(&stop("9005"), "")
        .answer(&destroy("9005"), "");
    let snip = h.dir("snip/mayfly-9003.yaml");
    fs::write(&snip, "x").unwrap();
    let (r, out) = h.gc(&pve, &no_gh());
    assert_eq!(r, Ok(5));
    assert_eq!(out, "gc: destroying orphan VM 9003\n");
    assert!(!pve.calls().iter().any(|c| c.contains("9005 --purge")));
    // Its files stay.
    assert!(Path::new(&snip).exists());
}

#[test]
fn without_slots_conf_nothing_is_destroyed() {
    let h = Host::new("", SLOTS);
    fs::remove_file(&h.paths.slots_conf).unwrap();
    let pve = h.guest(h.pve(), "qemu-server", "9003", OURS);
    let (r, out) = h.gc(&pve, &no_gh());
    assert_eq!(r, Err(format!("cannot read {}", h.paths.slots_conf)));
    assert_eq!(out, "");
    assert!(!pve.calls().iter().any(|c| c.starts_with("qm stop")));
}

#[test]
fn a_pool_with_nothing_in_it_does_nothing() {
    let h = Host::new("", "");
    let pve = h.pve();
    let (r, out) = h.gc(&pve, &no_gh());
    assert_eq!((r, out.as_str()), (Ok(0), ""));
    assert_eq!(pve.calls(), Vec::<String>::new());
}

#[test]
fn a_custom_range_is_walked() {
    let h = Host::new("VMID_MIN=100\nVMID_MAX=101\n", "");
    let pve = h.guest(h.pve(), "qemu-server", "100", OURS);
    let pve = h.guest(pve, "qemu-server", "9003", OURS);
    let pve = pve.answer(&stop("100"), "").answer(&destroy("100"), "");
    let (_, out) = h.gc(&pve, &no_gh());
    assert_eq!(out, "gc: destroying orphan VM 100\n");
}

// ------------------------------------------------------------------ gc: snippets

fn snippet(h: &Host, name: &str, age_secs: u64) {
    let p = h.dir(&format!("snip/{name}"));
    fs::write(&p, "x").unwrap();
    let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
    f.set_modified(now() - Duration::from_secs(age_secs))
        .unwrap();
}

#[test]
fn old_snippets_of_guests_that_do_not_run_are_removed() {
    let h = Host::new("", "");
    let pve = h.guest(h.pve(), "lxc", "9503", "hostname: x\n");
    let pve = h.guest(pve, "lxc", "9504", "hostname: x\n");
    let pve = pve
        .answer("lxc-info -n 9503 -sH", "RUNNING\n")
        .answer("lxc-info -n 9504 -sH", "STOPPED\n")
        .with_running_qemu("9502");
    let old = 31 * 60;
    for (name, age) in [
        ("mayfly-9500.yaml", old),
        ("mayfly-9501.yaml", 29 * 60),
        ("mayfly-9502.yaml", old),
        ("mayfly-9503.yaml", old),
        ("mayfly-9504.yaml", old),
        ("mayfly-abc.yaml", old),
        ("mayfly-.yaml", old),
        ("mayfly-9505.yml", old),
        ("other.yaml", old),
        ("mayfly-9506.yaml", 1800),
        ("mayfly-9507.yaml", 1801),
    ] {
        snippet(&h, name, age);
    }
    let (r, out) = h.gc(&pve, &no_gh());
    assert_eq!((r, out.as_str()), (Ok(0), ""));
    let mut left: Vec<String> = fs::read_dir(h.dir("snip"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            "mayfly-.yaml",
            "mayfly-9501.yaml",
            "mayfly-9502.yaml",
            "mayfly-9503.yaml",
            "mayfly-9505.yml",
            "mayfly-9506.yaml",
            "mayfly-abc.yaml",
            "other.yaml"
        ]
    );
    // Only containers are asked (a VM is looked up in /var/run), in file order.
    assert_eq!(
        pve.calls(),
        ["lxc-info -n 9503 -sH", "lxc-info -n 9504 -sH"]
    );
}

#[test]
fn a_snippet_dir_that_is_missing_is_fine() {
    let h = Host::new("", "");
    fs::remove_dir_all(h.dir("snip")).unwrap();
    let (r, _) = h.gc(&h.pve(), &no_gh());
    assert_eq!(r, Ok(0));
}

// ------------------------------------------------------------------ enable and disable

const TWO: &str = "a 9002 linux 8 16 60 no current\n# off 9003 x 1 1 1 no current\nb 9004 linux 8 16 60 no current\n";

fn enable_answers(pve: FakePve, units: &[&str]) -> FakePve {
    units.iter().fold(pve, |p, u| {
        p.answer(&format!("systemctl enable -q --now {u}"), "")
    })
}

#[test]
fn enable_starts_every_slot_then_the_gc_timer() {
    let h = Host::new("", TWO);
    let pve = enable_answers(
        h.pve(),
        &["mayfly-slot@a", "mayfly-slot@b", "mayfly-gc.timer"],
    );
    let (r, out) = h.slots(&pve, Verb::Enable, &[]);
    assert_eq!(r, Ok(0));
    assert_eq!(out, "on: a b\n");
    assert_eq!(
        pve.calls(),
        [
            "pvesm path local:snippets/x",
            "systemctl enable -q --now mayfly-slot@a",
            "systemctl enable -q --now mayfly-slot@b",
            "systemctl enable -q --now mayfly-gc.timer"
        ]
    );
}

#[test]
fn enable_with_names_starts_those_in_that_order() {
    let h = Host::new("", TWO);
    let pve = enable_answers(
        h.pve(),
        &["mayfly-slot@b", "mayfly-slot@a", "mayfly-gc.timer"],
    );
    let (r, out) = h.slots(&pve, Verb::Enable, &["b", "a"]);
    assert_eq!((r, out.as_str()), (Ok(0), "on: b a\n"));
    assert_eq!(
        &pve.calls()[1..],
        [
            "systemctl enable -q --now mayfly-slot@b",
            "systemctl enable -q --now mayfly-slot@a",
            "systemctl enable -q --now mayfly-gc.timer"
        ]
    );
}

#[test]
fn an_unknown_slot_is_refused_before_anything_starts() {
    let h = Host::new("", TWO);
    for verb in [Verb::Enable, Verb::Disable] {
        let pve = h.pve();
        let (r, out) = h.slots(&pve, verb, &["a", "nope"]);
        assert_eq!(
            r,
            Err(format!("slot nope is not in {}", h.paths.slots_conf))
        );
        assert_eq!(out, "");
        assert_eq!(pve.calls(), ["pvesm path local:snippets/x"]);
    }
    // A commented-out slot is not one.
    let (r, _) = h.slots(&h.pve(), Verb::Enable, &["off"]);
    assert!(r.is_err());
}

#[test]
fn slot_names_are_compared_like_awk_does() {
    let h = Host::new("", "01 9002 linux 8 16 60 no current\n");
    let pve = enable_answers(h.pve(), &["mayfly-slot@1", "mayfly-gc.timer"]);
    let (r, out) = h.slots(&pve, Verb::Enable, &["1"]);
    assert_eq!((r, out.as_str()), (Ok(0), "on: 1\n"));
}

#[test]
fn no_slots_is_an_error_unless_named() {
    let h = Host::new("", "# nothing\n");
    let (r, _) = h.slots(&h.pve(), Verb::Enable, &[]);
    assert_eq!(r, Err(format!("no slots in {}", h.paths.slots_conf)));
    let (r, _) = h.slots(&h.pve(), Verb::Disable, &[]);
    assert_eq!(r, Err(format!("no slots in {}", h.paths.slots_conf)));
    let (r, _) = h.slots(&h.pve(), Verb::Enable, &["a"]);
    assert_eq!(r, Err(format!("slot a is not in {}", h.paths.slots_conf)));
}

#[test]
fn a_failing_enable_stops_there_with_its_exit_code() {
    let h = Host::new("", TWO);
    let pve = h.pve().fail("systemctl enable -q --now mayfly-slot@a", 4);
    let (r, out) = h.slots(&pve, Verb::Enable, &[]);
    assert_eq!((r, out.as_str()), (Ok(4), ""));
    assert_eq!(pve.calls().len(), 2);
}

#[test]
fn a_failing_gc_timer_is_the_exit_code() {
    let h = Host::new("", TWO);
    let pve = enable_answers(h.pve(), &["mayfly-slot@a", "mayfly-slot@b"])
        .fail("systemctl enable -q --now mayfly-gc.timer", 1);
    let (r, out) = h.slots(&pve, Verb::Enable, &[]);
    assert_eq!((r, out.as_str()), (Ok(1), "on: a b\n"));
}

const LIST: &str = "systemctl list-units --plain --no-legend --state=active mayfly-slot@*";

fn disable_answers(pve: FakePve, units: &[&str]) -> FakePve {
    units.iter().fold(pve, |p, u| {
        p.answer(&format!("systemctl disable -q {u}"), "")
            .answer(&format!("systemctl stop {u}"), "")
    })
}

#[test]
fn disable_stops_every_slot_and_the_gc_timer_with_the_last() {
    let h = Host::new("", TWO);
    let pve = disable_answers(h.pve(), &["mayfly-slot@a", "mayfly-slot@b"])
        .answer(LIST, "")
        .answer("systemctl disable -q --now mayfly-gc.timer", "");
    let (r, out) = h.slots(&pve, Verb::Disable, &[]);
    assert_eq!(r, Ok(0));
    assert_eq!(out, "off: a b\n");
    assert_eq!(
        &pve.calls()[1..],
        [
            "systemctl disable -q mayfly-slot@a",
            "systemctl disable -q mayfly-slot@b",
            "systemctl stop mayfly-slot@a",
            "systemctl stop mayfly-slot@b",
            LIST,
            "systemctl disable -q --now mayfly-gc.timer"
        ]
    );
}

#[test]
fn disable_keeps_the_gc_timer_while_a_slot_runs() {
    let h = Host::new("", TWO);
    let pve = disable_answers(h.pve(), &["mayfly-slot@a"]).answer(
        LIST,
        "mayfly-slot@b.service loaded active running Mayfly slot b\n",
    );
    let (r, out) = h.slots(&pve, Verb::Disable, &["a"]);
    assert_eq!((r, out.as_str()), (Ok(0), "off: a\n"));
    assert_eq!(
        &pve.calls()[1..],
        [
            "systemctl disable -q mayfly-slot@a",
            "systemctl stop mayfly-slot@a",
            LIST
        ]
    );
}

#[test]
fn disable_takes_the_timer_down_when_the_list_fails() {
    // bash: `list-units | grep -q .` under pipefail.
    let h = Host::new("", TWO);
    let pve = disable_answers(h.pve(), &["mayfly-slot@a"])
        .fail(LIST, 1)
        .answer("systemctl disable -q --now mayfly-gc.timer", "");
    let (r, _) = h.slots(&pve, Verb::Disable, &["a"]);
    assert_eq!(r, Ok(0));
    assert_eq!(
        pve.calls().last().unwrap(),
        "systemctl disable -q --now mayfly-gc.timer"
    );
}

#[test]
fn disable_ignores_a_stop_that_fails_but_not_a_disable_that_fails() {
    let h = Host::new("", TWO);
    let pve = h
        .pve()
        .answer("systemctl disable -q mayfly-slot@a", "")
        .fail("systemctl stop mayfly-slot@a", 1)
        .answer(LIST, "")
        .answer("systemctl disable -q --now mayfly-gc.timer", "");
    let (r, out) = h.slots(&pve, Verb::Disable, &["a"]);
    assert_eq!((r, out.as_str()), (Ok(0), "off: a\n"));
    let pve = h.pve().fail("systemctl disable -q mayfly-slot@a", 2);
    let (r, out) = h.slots(&pve, Verb::Disable, &["a"]);
    assert_eq!((r, out.as_str()), (Ok(2), ""));
    assert_eq!(pve.calls().len(), 2);
}

#[test]
fn the_done_line_has_a_tick_on_a_terminal() {
    let h = Host::new("", TWO);
    let pve = enable_answers(h.pve(), &["mayfly-slot@a", "mayfly-gc.timer"]);
    let (_, out) = h.slots_out(&pve, Verb::Enable, &["a"], &Out { tty: true });
    assert_eq!(out, "\x1b[32m✓\x1b[0m on: a\n");
}
