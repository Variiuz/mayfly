//! `mayfly gc` (the timer runs it every 15 minutes): removes stale runner registrations, orphan guests
//! and old snippets. The rules, thresholds and log lines are those of bash `cmd_gc`.

use crate::config::{self, Config, Slots};
use crate::github::{AppRunners, RunnerApi};
use crate::guest;
use crate::out::{die, Out, R};
use crate::pve::guest_kind;
use crate::Ctx;
use std::fs;
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A registration younger than this is left alone: a slot registers its runner before `qm set` writes the
/// description onto the new clone, so a fresh registration has no matching guest yet.
const FRESH_SECONDS: i128 = 600;
/// A snippet older than this, with no running guest, is removed.
const SNIPPET_MAX_AGE: Duration = Duration::from_secs(30 * 60);

pub fn run(cx: &Ctx, out: &Out, w: &mut dyn Write) -> R<i32> {
    let (c, _) = config::load(cx.paths, cx.pve, cx.node, true)?;
    let gh = AppRunners::new(
        c.get("API_URL"),
        c.get("APP_ID"),
        c.get("KEY_PATH"),
        c.get("INSTALLATION_ID"),
        &c.scope,
    );
    run_with(cx, &c, &gh, out, w, SystemTime::now())
}

/// The VMIDs `seq VMID_MIN VMID_MAX` prints (none when either is not a number).
fn vmids(c: &Config) -> Vec<String> {
    match (c.number("VMID_MIN"), c.number("VMID_MAX")) {
        (Some(min), Some(max)) => (min..=max).map(|i| i.to_string()).collect(),
        _ => vec![],
    }
}

/// `read -r a b c d`: whitespace separates the fields, the last one takes the rest of the line.
fn read_fields(line: &str, n: usize) -> Vec<&str> {
    let blank = |c: char| matches!(c, ' ' | '\t' | '\n');
    let mut out = Vec::new();
    let mut rest = line.trim_start_matches(blank);
    while !rest.is_empty() {
        if out.len() + 1 == n {
            out.push(rest.trim_end_matches(blank));
            break;
        }
        match rest.find(blank) {
            Some(i) => {
                out.push(&rest[..i]);
                rest = rest[i..].trim_start_matches(blank);
            }
            None => {
                out.push(rest);
                break;
            }
        }
    }
    out
}

/// A runner name `<prefix>-<slot>-<epoch>` registered less than `FRESH_SECONDS` ago.
fn too_young(name: &str, now_secs: u64) -> bool {
    let born = name.rsplit('-').next().unwrap_or(name);
    if born.is_empty() || !born.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    // A number too big to read is far in the future: young.
    let born = born.parse::<i128>().unwrap_or(i128::MAX);
    (now_secs as i128) - born < FRESH_SECONDS
}

/// Names the guests of this node's pool claim for their runner: `description: runner <name>` in each config.
fn live_runners(cx: &Ctx, ids: &[String]) -> Vec<String> {
    let mut live = Vec::new();
    for id in ids {
        let Some(kind) = guest_kind(&cx.paths.pve_etc, id) else {
            continue;
        };
        let Some(cfg) = cx.pve.guest_config(kind, id) else {
            continue;
        };
        for line in cfg.split('\n') {
            if let Some(rest) = line.strip_prefix("description: runner ") {
                live.push(rest.split([' ', ',']).next().unwrap_or("").to_string());
            }
        }
    }
    live
}

/// Stale registrations: offline, not busy, named `<RUNNER_PREFIX>-...`, no live guest names them and not
/// registered in the last 10 minutes. Each page is handled before the next one is asked for.
fn stale_registrations(
    c: &Config,
    gh: &dyn RunnerApi,
    live: &[String],
    now: SystemTime,
    w: &mut dyn Write,
) -> R<()> {
    let Ok(token) = gh.install_token() else {
        return die("could not mint an installation token");
    };
    let prefix = format!("{}-", c.get("RUNNER_PREFIX"));
    for page in 1.. {
        let runners = gh.runners_page(&token, page);
        if runners.is_empty() {
            break;
        }
        for r in runners {
            let line = format!("{} {} {} {}", r.id, r.name, r.status, r.busy);
            let f = read_fields(&line, 4);
            let at = |i: usize| f.get(i).copied().unwrap_or("");
            let (id, name, status, busy) = (at(0), at(1), at(2), at(3));
            if !name.starts_with(&prefix) {
                continue;
            }
            if status != "offline" || busy != "false" {
                continue;
            }
            if live.iter().any(|l| l == name) {
                continue;
            }
            let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            if too_young(name, secs) {
                continue;
            }
            let _ = writeln!(w, "gc: removing stale registration {name}");
            if !id.is_empty() {
                gh.delete_runner(&token, id);
            }
        }
    }
    Ok(())
}

/// `gc` with GitHub and the clock given. `Ok(code)` is the exit code of a failed `destroy`.
pub fn run_with(
    cx: &Ctx,
    c: &Config,
    gh: &dyn RunnerApi,
    _out: &Out,
    w: &mut dyn Write,
    now: SystemTime,
) -> R<i32> {
    let ids = vmids(c);
    // Runner names of the guests that exist right now.
    let live = live_runners(cx, &ids);
    stale_registrations(c, gh, &live, now, w)?;

    // Guests of slots that no longer exist or whose service is not running.
    let template_vmids = format!(" {} ", c.get("TEMPLATE_VMIDS"));
    for id in &ids {
        if guest_kind(&cx.paths.pve_etc, id).is_none()
            || !guest::is_ours(cx, c, id)
            || guest::is_template(cx, id)
        {
            continue;
        }
        // Template VMIDs belong to `template build`; a build in progress is not a template yet.
        if template_vmids.contains(&format!(" {id} ")) {
            continue;
        }
        let slots = match fs::read(&cx.paths.slots_conf) {
            Ok(b) => Slots::parse(&String::from_utf8_lossy(&b)),
            Err(_) => {
                // bash stops here too (its awk cannot read the file); not knowing the slots, nothing
                // is destroyed.
                return die(format!("cannot read {}", cx.paths.slots_conf));
            }
        };
        let active = slots
            .name_for_vmid(id)
            .is_some_and(|name| cx.pve.unit_active(&format!("mayfly-slot@{name}")));
        if !active {
            let _ = writeln!(w, "gc: destroying orphan VM {id}");
            let code = guest::destroy(cx, c, id)?;
            if code != 0 {
                return Ok(code);
            }
        }
    }

    old_snippets(cx, c, now);
    Ok(0)
}

/// Snippets left by a crash between write and start: `mayfly-<vmid>.yaml` of a guest that is not
/// running and older than 30 minutes.
fn old_snippets(cx: &Ctx, c: &Config, now: SystemTime) {
    let Ok(entries) = fs::read_dir(&c.snippet_dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("mayfly-") && n.ends_with(".yaml"))
        .collect();
    names.sort();
    for name in names {
        let path = format!("{}/{name}", c.snippet_dir);
        if fs::metadata(&path).is_err() {
            continue;
        }
        let tail = &name[name.rfind("mayfly-").map_or(0, |i| i + "mayfly-".len())..];
        let vmid = tail.strip_suffix(".yaml").unwrap_or(tail);
        if vmid.is_empty() || !vmid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if cx
            .pve
            .guest_running(guest_kind(&cx.paths.pve_etc, vmid), vmid)
        {
            continue;
        }
        let old = fs::symlink_metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > SNIPPET_MAX_AGE);
        if old {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_read_like_read_r() {
        assert_eq!(
            read_fields("1 mf-a-1 offline false", 4),
            ["1", "mf-a-1", "offline", "false"]
        );
        assert_eq!(
            read_fields("  1   mf-a-1\toffline  false  ", 4),
            ["1", "mf-a-1", "offline", "false"]
        );
        // The last field takes the rest of the line.
        assert_eq!(
            read_fields("1 mf-a offline false extra  words ", 4),
            ["1", "mf-a", "offline", "false extra  words"]
        );
        assert_eq!(read_fields("1 a b", 4), ["1", "a", "b"]);
        assert!(read_fields("   ", 4).is_empty());
    }

    #[test]
    fn registrations_younger_than_ten_minutes_are_fresh() {
        let now = 1_700_000_000;
        assert!(too_young("mf-a-1-1700000000", now));
        assert!(too_young("mf-a-1-1699999401", now));
        assert!(!too_young("mf-a-1-1699999400", now));
        assert!(!too_young("mf-a-1-111", now));
        // From the future: young.
        assert!(too_young("mf-a-1-1700009999", now));
        // No epoch at the end.
        assert!(!too_young("mf-a-1-x", now));
        assert!(!too_young("mf-a-1-", now));
        assert!(!too_young("mf-a-1-12a", now));
        assert!(!too_young("mf-a-1--5", now));
        assert!(too_young(
            "mf-a-1-99999999999999999999999999999999999999999",
            now
        ));
    }
}
