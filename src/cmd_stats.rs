//! `mayfly stats [hours]`: what each slot did, read back from the slot journal.
//!
//! Per slot: guests it destroyed, of those how many ran a job, how many were replaced idle or because
//! their runner never came online, jobs that were cut off (MAX_JOB_HOURS, the backstop or a full pool),
//! errors (clone, start, GitHub registration), and the median life of the guests that ran a job. Pods show
//! their VMs and errors only: one pod VM runs many jobs.

use crate::out::{die, Out, R};
use crate::Ctx;
use std::collections::BTreeMap;
use std::io::Write;

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Tally {
    pub pod: bool,
    pub guests: u32,
    pub idle: u32,
    pub stuck: u32,
    pub cut: u32,
    pub errors: u32,
    /// Minutes of each guest that ran a job.
    pub minutes: Vec<u32>,
    /// The next "gone" line belongs to a guest that was replaced without a job.
    pending_no_job: bool,
}

impl Tally {
    pub fn jobs(&self) -> Option<u32> {
        (!self.pod).then(|| self.guests.saturating_sub(self.idle + self.stuck))
    }

    /// Median minutes of the guests that ran a job (the lower one of the middle two).
    pub fn median(&self) -> Option<u32> {
        let mut m = self.minutes.clone();
        m.sort_unstable();
        (!m.is_empty()).then(|| m[(m.len() - 1) / 2])
    }
}

/// Counts the slot lines of a journal (`journalctl -o cat`), by slot name.
pub fn tally(journal: &str) -> BTreeMap<String, Tally> {
    let mut slots: BTreeMap<String, Tally> = BTreeMap::new();
    for line in journal.lines() {
        let (pod, rest) = if let Some(r) = line.strip_prefix("slot ") {
            (false, r)
        } else if let Some(r) = line.strip_prefix("pod ") {
            (true, r)
        } else {
            continue;
        };
        let Some((name, msg)) = rest.split_once(": ") else {
            continue;
        };
        if name.is_empty() || name.contains(' ') {
            continue;
        }
        let t = slots.entry(name.to_string()).or_default();
        t.pod |= pod;
        if let Some(min) = gone_minutes(msg) {
            t.guests += 1;
            if std::mem::take(&mut t.pending_no_job) {
                continue;
            }
            if !pod {
                t.minutes.push(min);
            }
        } else if msg.contains(" had no job for ") {
            t.idle += 1;
            t.pending_no_job = true;
        } else if msg.contains(" not online after ") {
            t.stuck += 1;
            t.pending_no_job = true;
        } else if msg.contains("(its job is cancelled)") || msg.contains("(its jobs are cancelled)")
        {
            t.cut += 1;
        } else if msg.starts_with("could not create ")
            || msg.contains(" did not start")
            || msg.starts_with("GitHub refused the runner registration")
            || msg.contains(" never got ready ")
        {
            t.errors += 1;
        }
    }
    slots
}

/// `VM 9002 gone after 12 min` -> 12.
fn gone_minutes(msg: &str) -> Option<u32> {
    let after = msg.split_once(" gone after ")?.1;
    after.strip_suffix(" min")?.parse().ok()
}

fn parse_hours(args: &[String]) -> R<u32> {
    match args {
        [] => Ok(24),
        [h] => match h.parse::<u32>() {
            Ok(n) if (1..=24 * 90).contains(&n) => Ok(n),
            _ => die(format!(
                "hours: a number from 1 to 2160, not {h}; usage: mayfly stats [hours]"
            )),
        },
        _ => die("usage: mayfly stats [hours]"),
    }
}

pub fn run(cx: &Ctx, args: &[String], out: &Out, w: &mut dyn Write) -> R<i32> {
    let hours = parse_hours(args)?;
    let since = format!("{hours} hours ago");
    let j = cx.pve.run(
        "journalctl",
        &[
            "--no-pager",
            "-q",
            "-o",
            "cat",
            "-u",
            "mayfly-slot@*",
            "--since",
            &since,
        ],
    );
    if j.code != 0 {
        return die("cannot read the journal (journalctl -u 'mayfly-slot@*'); run as root");
    }
    let slots = tally(&j.stdout);
    let (b, d, z) = (out.bold(), out.dim(), out.reset());
    if slots.is_empty() {
        let _ = writeln!(w, "no slot activity in the last {hours} h");
        return Ok(0);
    }
    let _ = writeln!(
        w,
        "{b}{:<14} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8}{z}",
        "SLOT", "GUESTS", "JOBS", "IDLE", "STUCK", "CUT", "ERRORS", "MEDIAN"
    );
    let mut all = Tally::default();
    let mut jobs_all = 0;
    for (name, t) in &slots {
        let jobs = t.jobs().map_or("-".to_string(), |j| j.to_string());
        let med = t.median().map_or("-".to_string(), |m| format!("{m} min"));
        let _ = writeln!(
            w,
            "{:<14} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8}",
            name, t.guests, jobs, t.idle, t.stuck, t.cut, t.errors, med
        );
        all.guests += t.guests;
        all.idle += t.idle;
        all.stuck += t.stuck;
        all.cut += t.cut;
        all.errors += t.errors;
        all.minutes.extend(&t.minutes);
        jobs_all += t.jobs().unwrap_or(0);
    }
    let med = all.median().map_or("-".to_string(), |m| format!("{m} min"));
    let _ = writeln!(
        w,
        "{b}{:<14} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8}{z}",
        "all", all.guests, jobs_all, all.idle, all.stuck, all.cut, all.errors, med
    );
    let _ = writeln!(
        w,
        "{d}last {hours} h of mayfly-slot@* in the journal; JOBS and MEDIAN count VM and container slots, not pods{z}"
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const J: &str = "\
slot a-1: vm 9002 up as mf-a-1-1 (template 9098)
slot a-1: VM 9002 gone after 12 min
slot a-1: vm 9002 up as mf-a-1-2 (template 9098)
slot a-1: VM 9002 had no job for 6 h; replacing it
slot a-1: VM 9002 gone after 360 min
slot a-1: runner mf-a-1-3 not online after 10 min; replacing vm 9002 (check its template: mayfly check)
slot a-1: VM 9002 gone after 10 min
slot a-1: VM 9002 gone after 4 min
slot a-1: job in VM 9002 ran 6 h; stopping it (its job is cancelled)
slot a-1: VM 9002 gone after 361 min
slot b-1: could not create VM 9003 from template 9098
slot b-1: GitHub refused the runner registration; retrying in 2 min
slot b-1: VM 9003 gone after 7 min
pod p-1: VM 9041 booting (4 containers, template 9098)
pod p-1: VM 9041 never got ready (see /var/log/mayfly-pod-image.log inside); replacing it
pod p-1: VM 9041 gone after 31 min
warning: env missing: /etc/mayfly/env/x.env missing or empty; jobs run without it
slot weird name: VM 1 gone after 1 min
";

    #[test]
    fn counts_by_slot() {
        let s = tally(J);
        assert_eq!(s.len(), 3, "{s:?}");
        let a = &s["a-1"];
        assert_eq!(
            (a.guests, a.idle, a.stuck, a.cut, a.errors),
            (5, 1, 1, 1, 0)
        );
        assert_eq!(a.jobs(), Some(3));
        assert_eq!(a.minutes, vec![12, 4, 361]);
        assert_eq!(a.median(), Some(12));
        let b = &s["b-1"];
        assert_eq!((b.guests, b.errors, b.jobs()), (1, 2, Some(1)));
        let p = &s["p-1"];
        assert!(p.pod);
        assert_eq!(
            (p.guests, p.errors, p.jobs(), p.median()),
            (1, 1, None, None)
        );
    }

    #[test]
    fn median_of_an_even_count_is_the_lower_middle() {
        let t = Tally {
            minutes: vec![9, 1, 5, 3],
            ..Default::default()
        };
        assert_eq!(t.median(), Some(3));
        assert_eq!(Tally::default().median(), None);
    }

    #[test]
    fn hours() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_hours(&a(&[])).unwrap(), 24);
        assert_eq!(parse_hours(&a(&["168"])).unwrap(), 168);
        for bad in ["0", "x", "-1", "9999"] {
            assert!(parse_hours(&a(&[bad])).is_err(), "{bad}");
        }
        assert!(parse_hours(&a(&["1", "2"])).is_err());
    }
}
