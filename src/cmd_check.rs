//! `mayfly check`: App key, storage, network, template; changes nothing.

use crate::config::{self, slot_opt, Config, Slots};
use crate::github::{self, GitHub};
use crate::out::{die, Kind, Out, R};
use crate::pve::{guest_kind, vmid_node, GuestKind};
use crate::Ctx;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

struct Tally<'a> {
    out: &'a Out,
    warn: u32,
    bad: u32,
}

impl Tally<'_> {
    fn item(&mut self, w: &mut dyn Write, kind: Kind, label: &str, text: &str) {
        match kind {
            Kind::Ok => {}
            Kind::Warn => self.warn += 1,
            Kind::Bad => self.bad += 1,
        }
        self.out.item(w, kind, label, text);
    }
}

/// `is_template`: the guest's config has a line starting `template: 1`.
fn is_template(cx: &Ctx, id: &str) -> bool {
    guest_kind(&cx.paths.pve_etc, id)
        .and_then(|k| cx.pve.guest_config(k, id))
        .is_some_and(|cfg| cfg.lines().any(|l| l.starts_with("template: 1")))
}

/// The job files of a VM template: `mayfly` for templates Mayfly built (description `profile <name>`),
/// else `polara-ci`.
pub fn template_layout(cx: &Ctx, id: &str) -> &'static str {
    let built_here = guest_kind(&cx.paths.pve_etc, id)
        .and_then(|k| cx.pve.guest_config(k, id))
        .is_some_and(|cfg| cfg.lines().any(|l| l.starts_with("description: profile")));
    if built_here {
        "mayfly"
    } else {
        "polara-ci"
    }
}

/// The folder of a template profile: /etc/mayfly/profiles/<name> first, then the shipped ones.
fn find_profile(cx: &Ctx, name: &str) -> bool {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    valid
        && [
            format!("{}/profiles/{name}", cx.paths.conf_dir),
            format!("{}/profiles/{name}", cx.paths.share),
        ]
        .iter()
        .any(|d| Path::new(d).is_dir())
}

fn used_elsewhere(cx: &Ctx, vmid: &str) -> Option<String> {
    vmid_node(&cx.paths.pve_etc, vmid).filter(|n| n != cx.node)
}

fn github_items(cx: &Ctx, c: &Config, t: &mut Tally, w: &mut dyn Write) -> R<()> {
    let (app_id, key_path, scope) = (c.get("APP_ID"), c.get("KEY_PATH"), c.scope.as_str());
    if app_id.is_empty() {
        return die(format!("APP_ID is empty in {}", cx.paths.pool_env));
    }
    if fs::File::open(key_path).is_err() {
        return die(format!(
            "{key_path} missing: put the GitHub App's private key there"
        ));
    }
    let gh = GitHub::new(c.get("API_URL"));
    let jwt = github::jwt_from_file(app_id, key_path)?;
    let Ok(app) = gh.app_slug(&jwt) else {
        return die(format!(
            "GitHub refused the App key; check APP_ID and {key_path}"
        ));
    };
    let Ok(token) = gh.install_token(&jwt, c.get("INSTALLATION_ID"), scope) else {
        return die(format!(
            "App {app} has no installation token for {scope}; is it installed there?"
        ));
    };
    let Ok(count) = gh.runner_count(&token, scope) else {
        return die(format!(
            "App {app} cannot list the runners of {scope}; check its permissions"
        ));
    };
    let target = scope.split_once('/').map(|x| x.1).unwrap_or(scope);
    t.item(
        w,
        Kind::Ok,
        "GitHub",
        &format!("App {app}, {count} runners in {target}"),
    );
    Ok(())
}

pub fn run(cx: &Ctx, out: &Out, w: &mut dyn Write) -> R<i32> {
    let (c, issues) = config::load(cx.paths, cx.pve, cx.node, false)?;
    let mut t = Tally {
        out,
        warn: 0,
        bad: 0,
    };
    github_items(cx, &c, &mut t, w)?;
    for i in issues.iter().take(5) {
        t.item(
            w,
            Kind::Warn,
            "pool.env",
            &format!(
                "line {}: {}; the next Mayfly reads pool.env as plain KEY=value",
                i.line, i.reason
            ),
        );
    }
    if issues.len() > 5 {
        t.item(
            w,
            Kind::Warn,
            "pool.env",
            &format!(
                "and {} more lines that are not plain KEY=value",
                issues.len() - 5
            ),
        );
    }

    let (pool_env, storage) = (cx.paths.pool_env.as_str(), c.get("STORAGE"));
    if cx.pve.storage_exists(storage) {
        // A failing pvesh ends the Bash version silently; this one says so.
        let Some(p) = cx.pve.pool_percent(cx.node, storage) else {
            return die(format!("could not read the usage of {storage} (pvesh)"));
        };
        let max = c.get("MAX_POOL_PERCENT");
        if c.number("MAX_POOL_PERCENT").is_some_and(|m| p >= m) {
            t.item(
                w,
                Kind::Warn,
                "storage",
                &format!("{storage} {p}% used, over the {max}% limit: no new VMs"),
            );
        } else {
            t.item(w, Kind::Ok, "storage", &format!("{storage} {p}% used"));
        }
    } else {
        t.item(
            w,
            Kind::Bad,
            "storage",
            &format!("{storage} not found (STORAGE in {pool_env})"),
        );
    }

    let (bridge, vlan, gateway) = (c.get("BRIDGE"), c.get("VLAN"), c.get("GATEWAY"));
    if cx.pve.bridge_exists(bridge) {
        let vlan_part = if vlan.is_empty() {
            String::new()
        } else {
            format!(", VLAN {vlan}")
        };
        let gw_part = if gateway.is_empty() {
            String::new()
        } else {
            format!(", gateway {gateway}")
        };
        t.item(
            w,
            Kind::Ok,
            "network",
            &format!("{bridge}{vlan_part}{gw_part}"),
        );
    } else {
        t.item(
            w,
            Kind::Bad,
            "network",
            &format!("bridge {bridge} not found (BRIDGE in {pool_env})"),
        );
    }

    if c.get("FIREWALL") != "yes" {
        t.item(
            w,
            Kind::Ok,
            "firewall",
            "off (FIREWALL=no): isolate job VMs by VLAN or on the host",
        );
    } else if cx.pve.firewall_status().contains("enabled") {
        t.item(w, Kind::Ok, "firewall", "job VMs reach the internet only");
    } else {
        let isolates = if vlan.is_empty() {
            String::new()
        } else {
            format!(" (VLAN {vlan} still isolates)")
        };
        t.item(
            w,
            Kind::Warn,
            "firewall",
            &format!("cluster firewall is off, so the per-VM rules do nothing{isolates}"),
        );
    }

    let template = c.get("TEMPLATE_VMID");
    if !template.is_empty() && is_template(cx, template) {
        t.item(w, Kind::Ok, "template", template);
    } else {
        t.item(
            w,
            Kind::Warn,
            "template",
            "none yet: mayfly template build, then mayfly template promote <vmid>",
        );
    }

    for id in template
        .split_whitespace()
        .chain(c.get("NEXT_TEMPLATE_VMID").split_whitespace())
    {
        if is_template(cx, id)
            && guest_kind(&cx.paths.pve_etc, id) == Some(GuestKind::Vm)
            && template_layout(cx, id) != "mayfly"
        {
            t.item(
                w,
                Kind::Warn,
                "template",
                &format!(
                    "{id} was built by polara-ci; jobs get its /etc/polara-ci paths. Rebuild: mayfly template build"
                ),
            );
        }
    }

    let profile = c.get("PROFILE");
    if find_profile(cx, profile) {
        t.item(w, Kind::Ok, "profile", profile);
    } else {
        t.item(
            w,
            Kind::Bad,
            "profile",
            &format!(
                "{profile} not found in {}/profiles or {}/profiles",
                cx.paths.conf_dir, cx.paths.share
            ),
        );
    }

    let slots = Slots::read(&cx.paths.slots_conf);
    let mut n = 0;
    for name in slots.names() {
        n += 1;
        let Some(slot) = slots.get(name) else {
            continue;
        };
        if let Some(node) = used_elsewhere(cx, &slot.vmid) {
            t.item(
                w,
                Kind::Bad,
                name,
                &format!("VMID {} is taken on node {node}", slot.vmid),
            );
        }
        let kind = slot_opt("kind", &slot.extra, "vm");
        match kind.as_str() {
            "vm" | "pod" => {}
            "lxc" => {
                if !cx.pve.has_command("lxc-attach") {
                    t.item(w, Kind::Bad, name, "kind=lxc, but lxc-attach is missing");
                }
                if slot_opt("template", &slot.extra, "").is_empty()
                    && c.get("LXC_TEMPLATE_VMID").is_empty()
                {
                    t.item(
                        w,
                        Kind::Warn,
                        name,
                        "kind=lxc, no container template yet: mayfly template build --lxc",
                    );
                }
            }
            _ => t.item(
                w,
                Kind::Bad,
                name,
                &format!("unknown kind={kind} (vm, pod or lxc)"),
            ),
        }
        let pinned = slot_opt("template", &slot.extra, "");
        if !pinned.is_empty() && !is_template(cx, &pinned) {
            t.item(
                w,
                Kind::Bad,
                name,
                &format!("template={pinned} is not a template"),
            );
        }
        for e in slot.envspec.replace(',', " ").split_whitespace() {
            if e == "no" || e == "-" {
                continue;
            }
            let file = format!("{}/{e}.env", cx.paths.env_dir);
            if !fs::metadata(&file).map(|m| m.len() > 0).unwrap_or(false) {
                t.item(w, Kind::Warn, name, &format!("env {e}: {file} is missing"));
            }
        }
    }
    if n == 0 {
        t.item(
            w,
            Kind::Warn,
            "slots",
            &format!("none in {}", cx.paths.slots_conf),
        );
    } else {
        t.item(
            w,
            Kind::Ok,
            "slots",
            &format!("{n} in {}", cx.paths.slots_conf),
        );
    }

    let hook_dir = c.get("HOOK_DIR");
    let hooks = fs::read_dir(hook_dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| fs::symlink_metadata(e.path()).ok())
                .filter(|m| m.is_file() && m.permissions().mode() & 0o100 != 0)
                .count()
        })
        .unwrap_or(0);
    if hooks > 0 {
        t.item(w, Kind::Ok, "hooks", &format!("{hooks} in {hook_dir}"));
    }

    for id in cx.pve.backup_jobs_all() {
        if !id.is_empty() {
            t.item(
                w,
                Kind::Warn,
                "backup",
                &format!(
                    "job {id} backs up every VM: exclude {}-{} there",
                    c.get("VMID_MIN"),
                    c.get("VMID_MAX")
                ),
            );
        }
    }

    let _ = writeln!(w);
    let z = out.reset();
    if t.bad > 0 {
        let _ = writeln!(w, "{}{} problem(s) to fix.{z}", out.red(), t.bad);
        Ok(1)
    } else if t.warn > 0 {
        let _ = writeln!(w, "{}Ready, with {} warning(s).{z}", out.yel(), t.warn);
        Ok(0)
    } else {
        let _ = writeln!(w, "{}All good.{z}", out.grn());
        Ok(0)
    }
}
