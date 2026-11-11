//! `mayfly stats [hours]`: what each slot did, read back from the slot journal.
//!
//! Per slot: guests it destroyed, of those how many ran a job, how many were replaced idle or because
//! their runner never came online, jobs that were cut off (MAX_JOB_HOURS, the backstop or a full pool),
//! errors (clone, start, GitHub registration), the minutes of the guests that ran a job (total and
//! median) and of all guests. Pods show their VMs, VM minutes and errors only: one pod VM runs many
//! jobs. `--json` and `--csv` print the same numbers for scripts, with each slot's labels.

use crate::config::{slot_opt, Slots};
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
    /// Minutes of every guest, with or without a job (pod VMs too).
    pub guest_minutes: u64,
    /// The next "gone" line belongs to a guest that was replaced without a job.
    pending_no_job: bool,
}

impl Tally {
    pub fn jobs(&self) -> Option<u32> {
        (!self.pod).then(|| self.guests.saturating_sub(self.idle + self.stuck))
    }

    /// Minutes of the guests that ran a job, from clone to destroy (boot included).
    pub fn job_minutes(&self) -> Option<u64> {
        (!self.pod).then(|| self.minutes.iter().map(|&m| u64::from(m)).sum())
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
            t.guest_minutes += u64::from(min);
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

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Format {
    Table,
    Json,
    Csv,
}

const USAGE: &str = "usage: mayfly stats [hours] [--json | --csv]";

fn parse_args(args: &[String]) -> R<(u32, Format)> {
    let mut hours = None;
    let mut format = Format::Table;
    for a in args {
        match a.as_str() {
            "--json" | "--csv" if format != Format::Table => return die(USAGE),
            "--json" => format = Format::Json,
            "--csv" => format = Format::Csv,
            h if hours.is_none() => match h.parse::<u32>() {
                Ok(n) if (1..=24 * 90).contains(&n) => hours = Some(n),
                _ => return die(format!("hours: a number from 1 to 2160, not {h}; {USAGE}")),
            },
            _ => return die(USAGE),
        }
    }
    Ok((hours.unwrap_or(24), format))
}

/// One row of the output: a slot, or `all`.
struct Row<'a> {
    name: &'a str,
    labels: String,
    kind: String,
    t: &'a Tally,
    jobs: Option<u32>,
    job_minutes: Option<u64>,
}

fn json_row(r: &Row) -> serde_json::Value {
    serde_json::json!({
        "slot": r.name,
        "labels": r.labels,
        "kind": r.kind,
        "guests": r.t.guests,
        "jobs": r.jobs,
        "idle": r.t.idle,
        "stuck": r.t.stuck,
        "cut": r.t.cut,
        "errors": r.t.errors,
        "job_minutes": r.job_minutes,
        "guest_minutes": r.t.guest_minutes,
        "median_minutes": r.t.median(),
    })
}

/// A CSV field: quoted when it holds a comma, a quote or a line break.
fn csv(v: &str) -> String {
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map_or(String::new(), |v| v.to_string())
}

pub fn run(cx: &Ctx, args: &[String], out: &Out, w: &mut dyn Write) -> R<i32> {
    let (hours, format) = parse_args(args)?;
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
    let conf = Slots::read(&cx.paths.slots_conf);
    let mut all = Tally::default();
    let mut jobs_all = 0;
    let mut rows = Vec::new();
    for (name, t) in &slots {
        // A slot no longer in slots.conf has no labels; its kind follows from its log lines.
        let (labels, kind) = match conf.get(name) {
            Some(s) => (s.labels.clone(), slot_opt("kind", &s.extra, "vm")),
            None if t.pod => (String::new(), "pod".to_string()),
            None => (String::new(), String::new()),
        };
        rows.push(Row {
            name,
            labels,
            kind,
            t,
            jobs: t.jobs(),
            job_minutes: t.job_minutes(),
        });
        all.guests += t.guests;
        all.idle += t.idle;
        all.stuck += t.stuck;
        all.cut += t.cut;
        all.errors += t.errors;
        all.guest_minutes += t.guest_minutes;
        all.minutes.extend(&t.minutes);
        jobs_all += t.jobs().unwrap_or(0);
    }
    let total = Row {
        name: "all",
        labels: String::new(),
        kind: String::new(),
        job_minutes: Some(all.minutes.iter().map(|&m| u64::from(m)).sum()),
        t: &all,
        jobs: Some(jobs_all),
    };
    match format {
        Format::Json => {
            let v = serde_json::json!({
                "hours": hours,
                "slots": rows.iter().map(json_row).collect::<Vec<_>>(),
                "all": json_row(&total),
            });
            let _ = writeln!(w, "{v}");
            return Ok(0);
        }
        Format::Csv => {
            let _ = writeln!(
                w,
                "slot,labels,kind,guests,jobs,idle,stuck,cut,errors,job_minutes,guest_minutes,median_minutes"
            );
            for r in rows.iter().chain([&total]) {
                let _ = writeln!(
                    w,
                    "{},{},{},{},{},{},{},{},{},{},{},{}",
                    csv(r.name),
                    csv(&r.labels),
                    csv(&r.kind),
                    r.t.guests,
                    opt(r.jobs),
                    r.t.idle,
                    r.t.stuck,
                    r.t.cut,
                    r.t.errors,
                    opt(r.job_minutes),
                    r.t.guest_minutes,
                    opt(r.t.median())
                );
            }
            return Ok(0);
        }
        Format::Table => {}
    }
    let (b, d, z) = (out.bold(), out.dim(), out.reset());
    if slots.is_empty() {
        let _ = writeln!(w, "no slot activity in the last {hours} h");
        return Ok(0);
    }
    let _ = writeln!(
        w,
        "{b}{:<14} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8}{z}",
        "SLOT", "GUESTS", "JOBS", "IDLE", "STUCK", "CUT", "ERRORS", "JOB MIN", "MEDIAN"
    );
    for r in rows.iter().chain([&total]) {
        let (b, z) = if r.name == "all" { (b, z) } else { ("", "") };
        let _ = writeln!(
            w,
            "{b}{:<14} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8}{z}",
            r.name,
            r.t.guests,
            r.jobs.map_or("-".to_string(), |j| j.to_string()),
            r.t.idle,
            r.t.stuck,
            r.t.cut,
            r.t.errors,
            r.job_minutes.map_or("-".to_string(), |m| m.to_string()),
            r.t.median().map_or("-".to_string(), |m| format!("{m} min"))
        );
    }
    let _ = writeln!(
        w,
        "{d}last {hours} h of mayfly-slot@* in the journal; JOBS, JOB MIN and MEDIAN count VM and container slots, not pods; minutes run from clone to destroy{z}"
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
    fn minutes() {
        let s = tally(J);
        assert_eq!(s["a-1"].job_minutes(), Some(377));
        assert_eq!(s["a-1"].guest_minutes, 12 + 360 + 10 + 4 + 361);
        assert_eq!((s["p-1"].job_minutes(), s["p-1"].guest_minutes), (None, 31));
    }

    #[test]
    fn args() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(&a(&[])).unwrap(), (24, Format::Table));
        assert_eq!(parse_args(&a(&["168"])).unwrap(), (168, Format::Table));
        assert_eq!(parse_args(&a(&["--json"])).unwrap(), (24, Format::Json));
        assert_eq!(parse_args(&a(&["--csv", "6"])).unwrap(), (6, Format::Csv));
        assert_eq!(parse_args(&a(&["6", "--json"])).unwrap(), (6, Format::Json));
        for bad in ["0", "x", "-1", "9999", "--yaml"] {
            assert!(parse_args(&a(&[bad])).is_err(), "{bad}");
        }
        assert!(parse_args(&a(&["1", "2"])).is_err());
        assert!(parse_args(&a(&["--json", "--csv"])).is_err());
    }

    #[test]
    fn csv_quotes_only_when_needed() {
        assert_eq!(csv("linux"), "linux");
        assert_eq!(csv("a,b"), "\"a,b\"");
        assert_eq!(csv("say \"hi\""), "\"say \"\"hi\"\"\"");
    }
}
