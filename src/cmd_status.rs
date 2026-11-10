//! `mayfly status`: slots, guests, jobs, pool fill, free RAM and templates.

use crate::config::{self, slot_opt, Slots};
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
    let _ = writeln!(
        w,
        "{}{:<10} {:<6} {:<4} {:<22} {:<7} {:<9} {:<8} JOB{}",
        out.dim(),
        "SLOT",
        "VMID",
        "KIND",
        "LABELS",
        "SIZE",
        "SERVICE",
        "GUEST",
        out.reset()
    );
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
        let job = out.paint(&job, 0, &[("busy", Colour::Blu), ("idle", Colour::Dim)]);
        let _ = writeln!(
            w,
            "{:<10} {:<6} {:<4} {:<22} {:<7} {} {} {}",
            name,
            slot.vmid,
            kind,
            slot.labels,
            format!("{}c/{}G", slot.vcpu, slot.ram),
            svc,
            guest,
            job
        );
    }
    let _ = writeln!(w);
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
    Ok(0)
}
