//! `mayfly status`: slots, guests, jobs, pool fill, free RAM and templates.

use crate::cmd_slot::{engine_for_kind, Engine};
use crate::cmd_update::status_text;
use crate::config::{self, slot_opt, Config, Slots};
use crate::github::{self, GitHub, Runner};
use crate::out::{Colour, Out, R};
use crate::pve::guest_kind;
use crate::Ctx;
use std::io::Write;

/// Free RAM in whole GB (`MemAvailable`), "" when the file has no such line.
pub fn free_ram_gb(meminfo: &str) -> String {
    let text = std::fs::read_to_string(meminfo).unwrap_or_default();
    text.lines()
        .find(|l| l.starts_with("MemAvailable:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| (kb / 1_048_576).to_string())
        .unwrap_or_default()
}

/// Our runners on GitHub; None when GitHub cannot be asked (no key, no token, an error).
fn github_runners(c: &Config) -> Option<Vec<Runner>> {
    let jwt = github::jwt_from_file(c.get("APP_ID"), c.get("KEY_PATH")).ok()?;
    let gh = GitHub::new(c.get("API_URL"));
    let token = gh
        .install_token(&jwt, c.get("INSTALLATION_ID"), &c.scope)
        .ok()?;
    gh.runners(&token, &c.scope, c.get("RUNNER_PREFIX")).ok()
}

/// GitHub's view of a slot: for a pod `<online+busy>/<total>` of its runners (or none); for a VM or
/// container the runner named in the guest's description: busy, online, offline or none; `-` when
/// the description names no runner.
fn github_state(
    cx: &Ctx,
    c: &Config,
    runners: &[Runner],
    name: &str,
    vmid: &str,
    kind: &str,
) -> String {
    if kind == "pod" {
        let own = format!("{}-{name}-", c.get("RUNNER_PREFIX"));
        let (mut on, mut off, mut busy) = (0, 0, 0);
        for r in runners.iter().filter(|r| r.name.starts_with(&own)) {
            if r.status == "online" && r.busy {
                busy += 1;
            } else if r.status == "online" {
                on += 1;
            } else {
                off += 1;
            }
        }
        if on + off + busy == 0 {
            return "none".to_string();
        }
        return format!("{}/{}", on + busy, on + off + busy);
    }
    let described = guest_kind(&cx.paths.pve_etc, vmid)
        .and_then(|k| cx.pve.guest_config(k, vmid))
        .and_then(|cfg| {
            cfg.lines().find_map(|l| {
                let rest = l.strip_prefix("description: runner ")?;
                Some(rest.split([' ', ',']).next().unwrap_or("").to_string())
            })
        })
        .unwrap_or_default();
    if described.is_empty() {
        return "-".to_string();
    }
    match runners.iter().find(|r| r.name == described) {
        Some(r) if r.status == "online" && r.busy => "busy",
        Some(r) if r.status == "online" => "online",
        Some(r) if r.status == "offline" => "offline",
        _ => "none",
    }
    .to_string()
}

pub fn run(cx: &Ctx, out: &Out, w: &mut dyn Write) -> R<i32> {
    let (c, _) = config::load(cx.paths, cx.pve, cx.node, true)?;
    let slots = Slots::read(&cx.paths.slots_conf);
    let has_content = std::fs::metadata(&cx.paths.slots_conf)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if !has_content || slots.names().is_empty() {
        out.warn(w, &format!("no slots in {}", cx.paths.slots_conf));
        return Ok(0);
    }
    let runners = github_runners(&c);
    // The engine column only shows while some slot runs the Rust loop (SLOT_ENGINE / engine=); without
    // one the output is that of the bash status.
    let engine_of =
        |extra: &str| engine_for_kind(&slot_opt("kind", extra, "vm"), extra, c.get("SLOT_ENGINE"));
    let any_rust = slots
        .names()
        .iter()
        .filter_map(|n| slots.get(n))
        .any(|s| engine_of(&s.extra) == Engine::Rust);
    let engine_col = |text: &str| {
        if any_rust {
            format!("{text:<6} ")
        } else {
            String::new()
        }
    };
    let _ = writeln!(
        w,
        "{}{:<10} {:<6} {:<4} {}{:<22} {:<7} {:<9} {:<8} {:<6} GITHUB{}",
        out.dim(),
        "SLOT",
        "VMID",
        "KIND",
        engine_col("ENGINE"),
        "LABELS",
        "SIZE",
        "SERVICE",
        "GUEST",
        "JOB",
        out.reset()
    );
    let mut stuck = 0;
    for name in slots.names() {
        let Some(slot) = slots.get(name) else {
            continue;
        };
        let kind = slot_opt("kind", &slot.extra, "vm");
        let gk = guest_kind(&cx.paths.pve_etc, &slot.vmid);
        let state = match gk {
            Some(k) => cx.pve.guest_state(k, &slot.vmid),
            None => String::new(),
        };
        let mut job = "-".to_string();
        if state == "running" && kind == "pod" {
            job = format!("{} containers", slot_opt("containers", &slot.extra, "?"));
        } else if state == "running" {
            // A running guest that is neither a VM nor a container cannot happen: it has a config file.
            let active = gk.is_some_and(|k| cx.pve.job_active(k, &slot.vmid));
            job = if active { "busy" } else { "idle" }.to_string();
        }
        let svc = cx.pve.service_state(&format!("mayfly-slot@{name}"));
        let svc = out.paint(
            &svc,
            9,
            &[
                ("active", Colour::Grn),
                ("failed", Colour::Red),
                ("activating", Colour::Blu),
                ("deactivating", Colour::Yel),
            ],
        );
        let guest = out.paint(
            if state.is_empty() { "none" } else { &state },
            8,
            &[("running", Colour::Grn)],
        );
        let job = out.paint(&job, 6, &[("busy", Colour::Blu), ("idle", Colour::Dim)]);
        let gh = match &runners {
            None => "?".to_string(),
            Some(list) if state == "running" => github_state(cx, &c, list, name, &slot.vmid, &kind),
            Some(_) => "-".to_string(),
        };
        if gh == "offline" || gh == "none" || gh.starts_with("0/") {
            stuck += 1;
        }
        let gh = out.paint(
            &gh,
            0,
            &[
                ("online", Colour::Grn),
                ("busy", Colour::Blu),
                ("offline", Colour::Red),
                ("none", Colour::Red),
            ],
        );
        let _ = writeln!(
            w,
            "{:<10} {:<6} {:<4} {}{:<22} {:<7} {} {} {} {}",
            name,
            slot.vmid,
            kind,
            engine_col(match engine_of(&slot.extra) {
                Engine::Rust => "rust",
                Engine::Bash => "bash",
            }),
            slot.labels,
            format!("{}c/{}G", slot.vcpu, slot.ram),
            svc,
            guest,
            job,
            gh
        );
    }
    let _ = writeln!(w);
    if runners.is_none() {
        out.warn(w, "could not list the runners on GitHub (mayfly check)");
    }
    if stuck > 0 {
        out.warn(
            w,
            &format!("{stuck} running guest(s) have no online runner on GitHub; a job VM that boots but never registers points at the template (mayfly check)"),
        );
    }
    let pct = cx
        .pve
        .pool_percent(cx.node, c.get("STORAGE"))
        .map(|p| p.to_string())
        .unwrap_or_default();
    let (d, z) = (out.dim(), out.reset());
    let _ = writeln!(
        w,
        "{d}{:<10}{z} {} {}% used, limit {}%",
        "storage",
        c.get("STORAGE"),
        pct,
        c.get("MAX_POOL_PERCENT")
    );
    let _ = writeln!(
        w,
        "{d}{:<10}{z} {} GB free, {} GB kept for the host",
        "ram",
        free_ram_gb(cx.meminfo),
        c.get("MIN_FREE_RAM_GB")
    );
    let or_none = |s: &str| {
        if s.is_empty() {
            "none".to_string()
        } else {
            s.to_string()
        }
    };
    let next = |s: &str| {
        if s.is_empty() {
            String::new()
        } else {
            format!(", next {s}")
        }
    };
    let _ = writeln!(
        w,
        "{d}{:<10}{z} {}{}, profile {}",
        "template",
        or_none(c.get("TEMPLATE_VMID")),
        next(c.get("NEXT_TEMPLATE_VMID")),
        c.get("PROFILE")
    );
    let (lxc, lxc_next) = (c.get("LXC_TEMPLATE_VMID"), c.get("NEXT_LXC_TEMPLATE_VMID"));
    if !lxc.is_empty() || !lxc_next.is_empty() {
        let _ = writeln!(w, "{d}{:<10}{z} {}{}", "lxc", or_none(lxc), next(lxc_next));
    }
    // Only the Rust status has this line; bash status does not (tests/parity.sh ignores it).
    let _ = writeln!(w, "{d}{:<10}{z} {}", "update", status_text(cx.paths));
    Ok(0)
}
