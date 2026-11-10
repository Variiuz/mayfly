//! `mayfly enable [slot ...]` and `mayfly disable [slot ...]`: start or stop slots (all in slots.conf when
//! none is named) and keep the gc timer in step. `disable` lets a running job finish (the slot drains
//! on stop); the gc timer goes with the last slot.

use crate::config::{self, Slots};
use crate::out::{die, Out, R};
use crate::Ctx;
use std::io::Write;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verb {
    Enable,
    Disable,
}

fn unit(slot: &str) -> String {
    format!("mayfly-slot@{slot}")
}

pub fn run(cx: &Ctx, verb: Verb, args: &[String], out: &Out, w: &mut dyn Write) -> R<i32> {
    config::load(cx.paths, cx.pve, cx.node, true)?;
    let slots = Slots::read(&cx.paths.slots_conf);
    let names: Vec<String> = if args.is_empty() {
        slots.names().iter().map(|n| n.to_string()).collect()
    } else {
        args.to_vec()
    };
    if names.is_empty() {
        return die(format!("no slots in {}", cx.paths.slots_conf));
    }
    for s in &names {
        if slots.line_for_name(s).is_none() {
            return die(format!("slot {s} is not in {}", cx.paths.slots_conf));
        }
    }
    match verb {
        Verb::Enable => {
            for s in &names {
                let code = cx
                    .pve
                    .run_loud("systemctl", &["enable", "-q", "--now", &unit(s)]);
                if code != 0 {
                    return Ok(code);
                }
            }
            out.done(w, &format!("on: {}", names.join(" ")));
            Ok(cx
                .pve
                .run_loud("systemctl", &["enable", "-q", "--now", "mayfly-gc.timer"]))
        }
        Verb::Disable => {
            for s in &names {
                let code = cx.pve.run_loud("systemctl", &["disable", "-q", &unit(s)]);
                if code != 0 {
                    return Ok(code);
                }
            }
            // The stops run side by side: each waits for its slot's job. A failed stop is ignored.
            let stops: Vec<Vec<String>> = names
                .iter()
                .map(|s| vec!["stop".to_string(), unit(s)])
                .collect();
            cx.pve.run_loud_parallel("systemctl", &stops);
            out.done(w, &format!("off: {}", names.join(" ")));
            let listed = cx.pve.run(
                "systemctl",
                &[
                    "list-units",
                    "--plain",
                    "--no-legend",
                    "--state=active",
                    "mayfly-slot@*",
                ],
            );
            let any_active = listed.code == 0 && listed.stdout.split('\n').any(|l| !l.is_empty());
            if any_active {
                return Ok(0);
            }
            Ok(cx
                .pve
                .run_loud("systemctl", &["disable", "-q", "--now", "mayfly-gc.timer"]))
        }
    }
}
