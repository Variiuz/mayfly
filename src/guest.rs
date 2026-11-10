//! Which guests are Mayfly's. `is_ours` is the safety rail of everything that stops or destroys a
//! guest: a guest is ours only with a VMID inside `VMID_MIN..VMID_MAX`, a tag (`TAG`, or one of
//! `LEGACY_TAGS` on a guest named `ci-*`) and a name starting with `NAME_PREFIX`. It is the bash
//! `is_ours`, rule for rule.

use crate::config::Config;
use crate::out::{die, R};
use crate::pve::{guest_kind, vmid_node};
use crate::Ctx;
use std::fs;

/// A line `tags: a;b` of the config lists `tag`: a separator (space, `;` or `,`) before it, a separator
/// or the end of the line after it, and nothing but `tags:` ahead of that separator.
fn has_tag(cfg: &str, tag: &str) -> bool {
    if tag.is_empty() {
        return false;
    }
    let t = tag.as_bytes();
    cfg.split('\n').any(|line| {
        let Some(rest) = line.strip_prefix("tags:") else {
            return false;
        };
        let b = rest.as_bytes();
        let sep = |c: u8| matches!(c, b' ' | b';' | b',');
        (1..b.len()).any(|at| {
            let end = at + t.len();
            sep(b[at - 1]) && b[at..].starts_with(t) && (end == b.len() || sep(b[end]))
        })
    })
}

/// Some line of the config starts with `prefix`.
fn has_line(cfg: &str, prefix: &str) -> bool {
    cfg.split('\n').any(|l| l.starts_with(prefix))
}

/// The tag and name rules on a guest's config (`qm config` / `pct config`).
///
/// - Tagged `tag`: the name (a container's hostname) starts with `name_prefix`.
/// - Else tagged with the first of `legacy_tags` it carries: the name is `ci-*` (a VM made by an
///   earlier tool; its hostname does not count).
/// - Else not ours.
pub fn ours_by_config(cfg: &str, tag: &str, legacy_tags: &str, name_prefix: &str) -> bool {
    if has_tag(cfg, tag) {
        return has_line(cfg, &format!("name: {name_prefix}"))
            || has_line(cfg, &format!("hostname: {name_prefix}"));
    }
    for t in legacy_tags.split_whitespace() {
        if has_tag(cfg, t) {
            return has_line(cfg, "name: ci-");
        }
    }
    false
}

/// Some line of the config starts with `template: 1`.
pub fn config_is_template(cfg: &str) -> bool {
    has_line(cfg, "template: 1")
}

/// A guest's config, if it exists on this node and `qm`/`pct` can print it.
fn config_of(cx: &Ctx, id: &str) -> Option<String> {
    guest_kind(&cx.paths.pve_etc, id).and_then(|k| cx.pve.guest_config(k, id))
}

/// True only for guests this tool made. Reads the config with `qm config` / `pct config`.
pub fn is_ours(cx: &Ctx, c: &Config, id: &str) -> bool {
    let (Ok(n), Some(min), Some(max)) = (
        id.parse::<i64>(),
        c.number("VMID_MIN"),
        c.number("VMID_MAX"),
    ) else {
        return false;
    };
    if n < min || n > max {
        return false;
    }
    // A VMID of another node of the cluster is not ours to touch.
    if vmid_node(&cx.paths.pve_etc, id).is_some_and(|node| node != cx.node) {
        return false;
    }
    match config_of(cx, id) {
        Some(cfg) => ours_by_config(
            &cfg,
            c.get("TAG"),
            c.get("LEGACY_TAGS"),
            c.get("NAME_PREFIX"),
        ),
        None => false,
    }
}

pub fn is_template(cx: &Ctx, id: &str) -> bool {
    config_of(cx, id).is_some_and(|cfg| config_is_template(&cfg))
}

/// Stops and destroys a guest of ours (never a template) with its disks, its firewall file and its
/// snippet. Nothing happens when the VMID is free. A guest that is not ours, or is a template, is an
/// error. `Ok(code)` is the exit code of the failing `qm`/`pct destroy` (0 when it worked).
pub fn destroy(cx: &Ctx, c: &Config, id: &str) -> R<i32> {
    let Some(kind) = guest_kind(&cx.paths.pve_etc, id) else {
        return Ok(0);
    };
    if !is_ours(cx, c, id) {
        return die(format!(
            "VMID {id} exists but is not a mayfly guest; refusing to touch it"
        ));
    }
    if is_template(cx, id) {
        return die(format!(
            "VMID {id} is a template; refusing to destroy it from a slot"
        ));
    }
    let code = cx.pve.destroy_guest(kind, id);
    if code != 0 {
        return Ok(code);
    }
    let _ = fs::remove_file(format!("{}/{id}.fw", cx.paths.pve_fw_dir));
    let _ = fs::remove_file(format!("{}/mayfly-{id}.yaml", c.snippet_dir));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Paths};
    use crate::pve::FakePve;
    use std::path::{Path, PathBuf};

    const TAG: &str = "mayfly";
    const PREFIX: &str = "ci-";

    fn ours(cfg: &str) -> bool {
        ours_by_config(cfg, TAG, "", PREFIX)
    }

    // ---- the tag and name rules

    #[test]
    fn our_tag_and_our_name_prefix() {
        assert!(ours("name: ci-build\ntags: mayfly\n"));
        assert!(ours("tags: mayfly\nname: ci-build\n"));
        assert!(ours("name: ci-build\ntags: mayfly;other\n"));
        assert!(ours("name: ci-build\ntags: other;mayfly\n"));
        assert!(ours("name: ci-build\ntags: other,mayfly,more\n"));
        assert!(ours("name: ci-build\ntags: other mayfly\n"));
        assert!(ours("name: ci-build\ntags: mayfly\ntemplate: 0\n"));
    }

    #[test]
    fn a_container_is_named_by_its_hostname() {
        assert!(ours("hostname: ci-ct\ntags: mayfly\n"));
        assert!(!ours("hostname: web\ntags: mayfly\n"));
        // Not at the start of a line.
        assert!(!ours("description: hostname: ci-ct\ntags: mayfly\n"));
        assert!(!ours("description: name: ci-x\ntags: mayfly\n"));
    }

    #[test]
    fn the_name_prefix_is_the_configured_one() {
        assert!(ours_by_config(
            "name: job-1\ntags: mayfly\n",
            TAG,
            "",
            "job-"
        ));
        assert!(!ours_by_config(
            "name: ci-1\ntags: mayfly\n",
            TAG,
            "",
            "job-"
        ));
        assert!(ours_by_config(
            "name: ci-1\ntags: x-ci\n",
            "x-ci",
            "",
            "ci-"
        ));
    }

    #[test]
    fn our_tag_with_the_wrong_name_is_not_ours() {
        assert!(!ours("name: web-1\ntags: mayfly\n"));
        assert!(!ours("name: cix\ntags: mayfly\n"));
        assert!(!ours("name: CI-build\ntags: mayfly\n"));
        assert!(!ours("name:ci-build\ntags: mayfly\n"));
        assert!(!ours("name:  ci-build\ntags: mayfly\n"));
        assert!(!ours("tags: mayfly\n"));
        assert!(!ours("name: \ntags: mayfly\n"));
    }

    #[test]
    fn the_wrong_tag_is_not_ours() {
        assert!(!ours("name: ci-build\ntags: other\n"));
        assert!(!ours("name: ci-build\ntags: mayfly2\n"));
        assert!(!ours("name: ci-build\ntags: xmayfly\n"));
        assert!(!ours("name: ci-build\ntags: may\n"));
        assert!(!ours("name: ci-build\ntags: mayfly-extra\n"));
        assert!(!ours("name: ci-build\ntags: my_mayfly\n"));
        assert!(!ours("name: ci-build\ntags: Mayfly\n"));
        assert!(!ours("name: ci-build\ntags:mayfly\n"));
        assert!(!ours("name: ci-build\ntags:\n"));
        assert!(!ours("name: ci-build\n"));
        assert!(!ours("name: ci-build\ndescription: tags: mayfly\n"));
        assert!(!ours("name: ci-build\nnotes: mayfly\n"));
        assert!(!ours(""));
    }

    #[test]
    fn a_legacy_tag_needs_a_ci_name() {
        let legacy = |cfg: &str| ours_by_config(cfg, TAG, "oldci older", PREFIX);
        assert!(legacy("name: ci-build\ntags: oldci\n"));
        assert!(legacy("name: ci-build\ntags: x;older\n"));
        assert!(legacy("name: ci-build\ntags: oldci;older\n"));
        assert!(!legacy("name: web-1\ntags: oldci\n"));
        assert!(!legacy("name: build-ci-1\ntags: oldci\n"));
        assert!(!legacy("name: ci-build\ntags: other\n"));
        assert!(!legacy("name: ci-build\ntags: oldcix\n"));
        assert!(!legacy("tags: oldci\n"));
        // The legacy rule names `ci-` whatever NAME_PREFIX is, and does not look at a hostname.
        assert!(ours_by_config(
            "name: ci-b\ntags: oldci\n",
            TAG,
            "oldci",
            "job-"
        ));
        assert!(!ours_by_config(
            "hostname: ci-b\ntags: oldci\n",
            TAG,
            "oldci",
            "ci-"
        ));
        // No legacy tags set: the legacy tag means nothing.
        assert!(!ours_by_config(
            "name: ci-build\ntags: oldci\n",
            TAG,
            "",
            PREFIX
        ));
    }

    #[test]
    fn our_tag_decides_before_the_legacy_tags() {
        // Tagged mayfly and oldci with a name that fails NAME_PREFIX: the legacy rule is not tried.
        assert!(!ours_by_config(
            "name: ci-b\ntags: mayfly;oldci\n",
            TAG,
            "oldci",
            "job-"
        ));
        assert!(ours_by_config(
            "name: job-b\ntags: mayfly;oldci\n",
            TAG,
            "oldci",
            "job-"
        ));
    }

    #[test]
    fn a_legacy_tag_is_taken_literally() {
        assert!(!ours_by_config(
            "name: ci-b\ntags: oldXci\n",
            TAG,
            "old.ci",
            PREFIX
        ));
        assert!(ours_by_config(
            "name: ci-b\ntags: old.ci\n",
            TAG,
            "old.ci",
            PREFIX
        ));
        assert!(!ours_by_config(
            "name: ci-b\ntags: anything\n",
            TAG,
            "*",
            PREFIX
        ));
        assert!(!ours_by_config(
            "name: ci-b\ntags: anything\n",
            TAG,
            "  ",
            PREFIX
        ));
    }

    #[test]
    fn a_template_line_is_template_colon_1() {
        assert!(config_is_template("name: ci-t\ntemplate: 1\n"));
        assert!(config_is_template("template: 1"));
        // As bash `grep '^template: 1'`: a longer number matches too.
        assert!(config_is_template("template: 10\n"));
        assert!(!config_is_template("template: 0\n"));
        assert!(!config_is_template("name: ci-t\n"));
        assert!(!config_is_template("description: template: 1\n"));
        assert!(!config_is_template(" template: 1\n"));
    }

    // ---- is_ours: range, cluster, config

    struct Host {
        root: PathBuf,
        paths: Paths,
        config: Config,
    }

    impl Host {
        fn new(name: &str, pool_extra: &str) -> Host {
            let root =
                std::env::temp_dir().join(format!("mayfly-guest-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let r = root.display().to_string();
            fs::create_dir_all(format!("{r}/conf")).unwrap();
            fs::create_dir_all(format!("{r}/pve/qemu-server")).unwrap();
            fs::create_dir_all(format!("{r}/pve/lxc")).unwrap();
            fs::create_dir_all(format!("{r}/fw")).unwrap();
            fs::create_dir_all(format!("{r}/snip")).unwrap();
            let paths = Paths::new(
                &format!("{r}/conf"),
                &format!("{r}/share"),
                &format!("{r}/pve"),
                &format!("{r}/fw"),
            );
            fs::write(&paths.pool_env, format!("ORG=acme\n{pool_extra}")).unwrap();
            let pve =
                FakePve::new().answer("pvesm path local:snippets/x", &format!("{r}/snip/x\n"));
            let (config, _) = config::load(&paths, &pve, "pve1", true).unwrap();
            Host {
                root,
                paths,
                config,
            }
        }

        fn guest(&self, kind: &str, id: &str) {
            fs::write(format!("{}/pve/{kind}/{id}.conf", self.root.display()), "").unwrap();
        }

        fn is_ours(&self, pve: &FakePve, id: &str) -> bool {
            let cx = Ctx {
                paths: &self.paths,
                pve,
                node: "pve1",
                meminfo: "",
            };
            is_ours(&cx, &self.config, id)
        }
    }

    impl Drop for Host {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const OURS: &str = "name: ci-a\ntags: mayfly\n";

    #[test]
    fn the_vmid_range_is_inclusive() {
        let h = Host::new("range", "");
        let mut pve = FakePve::new();
        for id in ["8999", "9000", "9050", "9099", "9100"] {
            h.guest("qemu-server", id);
            pve = pve.answer(&format!("qm config {id}"), OURS);
        }
        assert!(!h.is_ours(&pve, "8999"));
        assert!(h.is_ours(&pve, "9000"));
        assert!(h.is_ours(&pve, "9050"));
        assert!(h.is_ours(&pve, "9099"));
        assert!(!h.is_ours(&pve, "9100"));
        // Outside the range nothing is asked of Proxmox.
        assert_eq!(
            pve.calls(),
            ["qm config 9000", "qm config 9050", "qm config 9099"]
        );
    }

    #[test]
    fn the_vmid_range_is_configurable() {
        let h = Host::new("range2", "VMID_MIN=100\nVMID_MAX=199\n");
        h.guest("qemu-server", "150");
        h.guest("qemu-server", "9002");
        let pve = FakePve::new()
            .answer("qm config 150", OURS)
            .answer("qm config 9002", OURS);
        assert!(h.is_ours(&pve, "150"));
        assert!(!h.is_ours(&pve, "9002"));
    }

    #[test]
    fn a_vmid_that_is_not_a_number_is_not_ours() {
        let h = Host::new("nan", "");
        h.guest("qemu-server", "x");
        let pve = FakePve::new().answer("qm config x", OURS);
        assert!(!h.is_ours(&pve, "x"));
        assert!(!h.is_ours(&pve, ""));
        assert!(pve.calls().is_empty());
        let bad = Host::new("nan2", "VMID_MIN=low\n");
        bad.guest("qemu-server", "9002");
        assert!(!bad.is_ours(&FakePve::new().answer("qm config 9002", OURS), "9002"));
    }

    #[test]
    fn a_vmid_with_no_config_file_is_not_ours() {
        let h = Host::new("nofile", "");
        let pve = FakePve::new().answer("qm config 9002", OURS);
        assert!(!h.is_ours(&pve, "9002"));
        assert!(pve.calls().is_empty());
    }

    #[test]
    fn a_config_that_cannot_be_read_is_not_ours() {
        let h = Host::new("nocfg", "");
        h.guest("qemu-server", "9002");
        assert!(!h.is_ours(&FakePve::new().fail("qm config 9002", 2), "9002"));
        assert!(!h.is_ours(&FakePve::new(), "9002"));
    }

    #[test]
    fn a_container_is_read_with_pct() {
        let h = Host::new("ct", "");
        h.guest("lxc", "9051");
        let pve = FakePve::new().answer("pct config 9051", "hostname: ci-ct\ntags: mayfly\n");
        assert!(h.is_ours(&pve, "9051"));
        assert_eq!(pve.calls(), ["pct config 9051"]);
    }

    #[test]
    fn a_vmid_of_another_node_is_not_ours() {
        let h = Host::new("elsewhere", "");
        h.guest("qemu-server", "9002");
        h.guest("qemu-server", "9003");
        h.guest("qemu-server", "9004");
        fs::write(
            format!("{}/pve/.vmlist", h.root.display()),
            r#"{"ids":{"9002":{"node":"pve2"},"9003":{"node":"pve1"}}}"#,
        )
        .unwrap();
        let pve = FakePve::new()
            .answer("qm config 9002", OURS)
            .answer("qm config 9003", OURS)
            .answer("qm config 9004", OURS);
        assert!(!h.is_ours(&pve, "9002"));
        assert!(h.is_ours(&pve, "9003"));
        assert!(h.is_ours(&pve, "9004"));
        assert_eq!(pve.calls(), ["qm config 9003", "qm config 9004"]);
    }

    #[test]
    fn the_configured_tag_and_legacy_tags_apply() {
        let h = Host::new(
            "tags",
            "TAG=ci_pool\nLEGACY_TAGS=\"oldci older\"\nNAME_PREFIX=job-\n",
        );
        for id in ["9002", "9003", "9004", "9005"] {
            h.guest("qemu-server", id);
        }
        let pve = FakePve::new()
            .answer("qm config 9002", "name: job-a\ntags: ci_pool\n")
            .answer("qm config 9003", "name: ci-a\ntags: oldci\n")
            .answer("qm config 9004", "name: job-a\ntags: mayfly\n")
            .answer("qm config 9005", "name: ci-a\ntags: ci_pool\n");
        assert!(h.is_ours(&pve, "9002"));
        assert!(h.is_ours(&pve, "9003"));
        assert!(!h.is_ours(&pve, "9004"));
        assert!(!h.is_ours(&pve, "9005"));
    }

    // ---- destroy

    fn cx<'a>(h: &'a Host, pve: &'a FakePve) -> Ctx<'a> {
        Ctx {
            paths: &h.paths,
            pve,
            node: "pve1",
            meminfo: "",
        }
    }

    const VM_DESTROY: [&str; 2] = [
        "qm stop 9002 --skiplock 1 --timeout 30",
        "qm destroy 9002 --purge 1 --destroy-unreferenced-disks 1 --skiplock 1",
    ];

    #[test]
    fn a_free_vmid_is_left_alone() {
        let h = Host::new("free", "");
        let pve = FakePve::new();
        assert_eq!(destroy(&cx(&h, &pve), &h.config, "9002").unwrap(), 0);
        assert!(pve.calls().is_empty());
    }

    #[test]
    fn a_vm_is_stopped_and_destroyed_with_its_files() {
        let h = Host::new("vm", "");
        h.guest("qemu-server", "9002");
        let fw = format!("{}/fw/9002.fw", h.root.display());
        let snip = format!("{}/snip/mayfly-9002.yaml", h.root.display());
        let keep = format!("{}/snip/mayfly-9003.yaml", h.root.display());
        for f in [&fw, &snip, &keep] {
            fs::write(f, "x").unwrap();
        }
        let pve = FakePve::new()
            .answer("qm config 9002", OURS)
            .answer(VM_DESTROY[0], "")
            .answer(VM_DESTROY[1], "");
        assert_eq!(destroy(&cx(&h, &pve), &h.config, "9002").unwrap(), 0);
        // is_ours and is_template each read the config, then stop, then destroy.
        assert_eq!(
            pve.calls(),
            [
                "qm config 9002",
                "qm config 9002",
                VM_DESTROY[0],
                VM_DESTROY[1]
            ]
        );
        assert!(!Path::new(&fw).exists());
        assert!(!Path::new(&snip).exists());
        assert!(Path::new(&keep).exists());
    }

    #[test]
    fn a_container_is_stopped_and_destroyed_with_pct() {
        let h = Host::new("ctd", "");
        h.guest("lxc", "9051");
        let pve = FakePve::new()
            .answer("pct config 9051", "hostname: ci-c\ntags: mayfly\n")
            .answer("pct stop 9051 --skiplock 1", "")
            .answer(
                "pct destroy 9051 --purge 1 --destroy-unreferenced-disks 1 --force 1",
                "",
            );
        assert_eq!(destroy(&cx(&h, &pve), &h.config, "9051").unwrap(), 0);
        assert_eq!(
            pve.calls(),
            [
                "pct config 9051",
                "pct config 9051",
                "pct stop 9051 --skiplock 1",
                "pct destroy 9051 --purge 1 --destroy-unreferenced-disks 1 --force 1"
            ]
        );
    }

    #[test]
    fn a_stop_that_fails_does_not_stop_the_destroy() {
        let h = Host::new("stopfail", "");
        h.guest("qemu-server", "9002");
        let pve = FakePve::new()
            .answer("qm config 9002", OURS)
            .fail(VM_DESTROY[0], 255)
            .answer(VM_DESTROY[1], "");
        assert_eq!(destroy(&cx(&h, &pve), &h.config, "9002").unwrap(), 0);
        assert_eq!(pve.calls().len(), 4);
    }

    #[test]
    fn a_failed_destroy_keeps_the_files_and_returns_its_code() {
        let h = Host::new("destroyfail", "");
        h.guest("qemu-server", "9002");
        let fw = format!("{}/fw/9002.fw", h.root.display());
        fs::write(&fw, "x").unwrap();
        let pve = FakePve::new()
            .answer("qm config 9002", OURS)
            .answer(VM_DESTROY[0], "")
            .fail(VM_DESTROY[1], 5);
        assert_eq!(destroy(&cx(&h, &pve), &h.config, "9002").unwrap(), 5);
        assert!(Path::new(&fw).exists());
    }

    #[test]
    fn a_guest_that_is_not_ours_is_never_touched() {
        let h = Host::new("notours", "");
        let cases = [
            ("9002", "name: web\ntags: mayfly\n"),
            ("9003", "name: ci-a\ntags: other\n"),
            ("9004", "name: ci-a\n"),
            ("9100", "name: ci-a\ntags: mayfly\n"),
            ("8999", "name: ci-a\ntags: mayfly\n"),
        ];
        for (id, cfg) in cases {
            h.guest("qemu-server", id);
            let pve = FakePve::new().answer(&format!("qm config {id}"), cfg);
            let e = destroy(&cx(&h, &pve), &h.config, id).err().unwrap();
            assert_eq!(
                e.0,
                format!("VMID {id} exists but is not a mayfly guest; refusing to touch it")
            );
            assert!(
                pve.calls().iter().all(|c| c.starts_with("qm config")),
                "{id}: {:?}",
                pve.calls()
            );
        }
    }

    #[test]
    fn a_template_is_never_destroyed() {
        let h = Host::new("tmpl", "");
        h.guest("qemu-server", "9002");
        let pve = FakePve::new().answer(
            "qm config 9002",
            "name: ci-template-default\ntags: mayfly\ntemplate: 1\n",
        );
        let e = destroy(&cx(&h, &pve), &h.config, "9002").err().unwrap();
        assert_eq!(
            e.0,
            "VMID 9002 is a template; refusing to destroy it from a slot"
        );
        assert_eq!(pve.calls(), ["qm config 9002", "qm config 9002"]);
    }

    #[test]
    fn a_container_template_is_never_destroyed() {
        let h = Host::new("ctmpl", "");
        h.guest("lxc", "9096");
        let pve = FakePve::new().answer(
            "pct config 9096",
            "hostname: ci-template\ntags: mayfly\ntemplate: 1\n",
        );
        assert!(destroy(&cx(&h, &pve), &h.config, "9096").is_err());
        assert!(pve.calls().iter().all(|c| c.starts_with("pct config")));
    }

    #[test]
    fn a_guest_whose_config_cannot_be_read_is_never_destroyed() {
        let h = Host::new("unreadable", "");
        h.guest("qemu-server", "9002");
        let pve = FakePve::new().fail("qm config 9002", 1);
        assert!(destroy(&cx(&h, &pve), &h.config, "9002").is_err());
        assert_eq!(pve.calls(), ["qm config 9002"]);
    }

    #[test]
    fn running_is_lxc_info_for_a_container_and_the_process_for_a_vm() {
        use crate::pve::{GuestKind, Pve};
        let pve = FakePve::new()
            .answer("lxc-info -n 9051 -sH", "RUNNING\n")
            .answer("lxc-info -n 9052 -sH", "STOPPED\n")
            .with_running_qemu("9002");
        assert!(pve.guest_running(Some(GuestKind::Ct), "9051"));
        assert!(!pve.guest_running(Some(GuestKind::Ct), "9052"));
        assert!(!pve.guest_running(Some(GuestKind::Ct), "9053"));
        assert!(pve.guest_running(Some(GuestKind::Vm), "9002"));
        assert!(!pve.guest_running(Some(GuestKind::Vm), "9003"));
        assert!(pve.guest_running(None, "9002"));
        assert_eq!(
            pve.calls(),
            [
                "lxc-info -n 9051 -sH",
                "lxc-info -n 9052 -sH",
                "lxc-info -n 9053 -sH"
            ]
        );
    }
}
