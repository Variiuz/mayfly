//! `mayfly update`: self-update from the latest GitHub release, with a backup, a health check and a
//! rollback. The files are put in place by the release's own `install.sh`.

use crate::config::{env_or, update_mode, Paths};
use crate::github::now;
use crate::out::{die, print_die, Die, Out, R};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Output};
use std::time::Duration;

/// The systemd units Mayfly installs; they are saved and restored with the rest.
const UNITS: [&str; 5] = [
    "mayfly-slot@.service",
    "mayfly-gc.service",
    "mayfly-gc.timer",
    "mayfly-update.service",
    "mayfly-update.timer",
];
const USAGE: &str = "usage: mayfly update [--version x.y.z] [--check] [--rollback]";

// ------------------------------------------------------------------ arguments

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Opts {
    pub check: bool,
    pub rollback: bool,
    /// Run by the timer: obeys UPDATE and the skipped version, logs plain lines.
    pub auto: bool,
    pub version: Option<String>,
}

pub fn parse_args(args: &[String]) -> R<Opts> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => o.check = true,
            "--rollback" => o.rollback = true,
            "--auto" => o.auto = true,
            "--version" => match it.next() {
                Some(v) => o.version = Some(v.clone()),
                None => return die(format!("--version needs a version; {USAGE}")),
            },
            s if s.starts_with("--version=") => {
                o.version = Some(s["--version=".len()..].to_string())
            }
            _ => return die(format!("unknown argument {a}; {USAGE}")),
        }
    }
    if let Some(v) = &o.version {
        let v = v.strip_prefix('v').unwrap_or(v);
        if parse_version(v).is_none() {
            return die(format!("--version {v}: use x.y.z"));
        }
        o.version = Some(v.to_string());
    }
    if o.rollback && (o.check || o.auto || o.version.is_some()) {
        return die("--rollback takes no other option");
    }
    if o.auto && o.version.is_some() {
        return die("--auto and --version do not go together");
    }
    Ok(o)
}

/// `x.y.z` as numbers; None for anything else (no pre-release tags).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut p = s.split('.');
    let mut num = || {
        let t = p.next()?;
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        t.parse::<u64>().ok()
    };
    let v = (num()?, num()?, num()?);
    if p.next().is_some() {
        return None;
    }
    Some(v)
}

// ------------------------------------------------------------------ state

/// `update.state`: plain KEY=value lines.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct State {
    pub last_attempt: String,
    pub last_result: String,
    /// A version that failed `mayfly check` and was rolled back; the timer does not try it again.
    pub skipped: String,
    /// The release the timer left alone because it is breaking (with `last_result=breaking`).
    pub breaking: String,
}

pub fn state_path(paths: &Paths) -> String {
    format!("{}/update.state", paths.state_dir)
}

impl State {
    pub fn read(paths: &Paths) -> State {
        let text = fs::read_to_string(state_path(paths)).unwrap_or_default();
        let mut s = State::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "last_attempt" => s.last_attempt = v.trim().to_string(),
                "last_result" => s.last_result = v.trim().to_string(),
                "skipped" => s.skipped = v.trim().to_string(),
                "breaking" => s.breaking = v.trim().to_string(),
                _ => {}
            }
        }
        s
    }

    fn write(&self, paths: &Paths) -> io::Result<()> {
        fs::create_dir_all(&paths.state_dir)?;
        let mut text = format!(
            "last_attempt={}\nlast_result={}\n",
            self.last_attempt, self.last_result
        );
        if !self.skipped.is_empty() {
            text.push_str(&format!("skipped={}\n", self.skipped));
        }
        if !self.breaking.is_empty() {
            text.push_str(&format!("breaking={}\n", self.breaking));
        }
        let path = state_path(paths);
        let tmp = format!("{path}.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &path)
    }
}

enum Skip {
    Keep,
    Clear,
    Set(String),
}

/// Notes an attempt in the state file. A state file that cannot be written is a warning, not a failure.
fn record(paths: &Paths, result: &str, skip: Skip) {
    save(paths, result, skip, "");
}

/// The timer found `version` breaking and left it alone.
fn record_breaking(paths: &Paths, version: &str) {
    save(paths, "breaking", Skip::Keep, version);
}

fn save(paths: &Paths, result: &str, skip: Skip, breaking: &str) {
    let mut s = State::read(paths);
    s.last_attempt = utc(now());
    s.last_result = result.to_string();
    s.breaking = breaking.to_string();
    match skip {
        Skip::Keep => {}
        Skip::Clear => s.skipped.clear(),
        Skip::Set(v) => s.skipped = v,
    }
    if let Err(e) = s.write(paths) {
        eprintln!("warning: cannot write {}: {e}", state_path(paths));
    }
}

/// The text of the `update` line of `mayfly status`.
pub fn status_text(paths: &Paths) -> String {
    let s = State::read(paths);
    if s.last_attempt.is_empty() {
        format!("{}, no update run yet", crate::VERSION)
    } else if s.last_result == "breaking" && !s.breaking.is_empty() {
        format!(
            "{}, last {} breaking {} waits",
            crate::VERSION,
            s.last_attempt,
            s.breaking
        )
    } else {
        format!(
            "{}, last {} {}",
            crate::VERSION,
            s.last_attempt,
            s.last_result
        )
    }
}

/// Seconds since the epoch as `2026-11-10T04:12:09Z`.
pub fn utc(t: u64) -> String {
    let (days, secs) = ((t / 86400) as i64, t % 86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

// ------------------------------------------------------------------ small helpers

fn say(w: &mut dyn Write, auto: bool, msg: &str) {
    let _ = if auto {
        writeln!(w, "update: {msg}")
    } else {
        writeln!(w, "{msg}")
    };
}

fn io_err<T>(r: io::Result<T>, what: &str) -> R<T> {
    r.map_err(|e| Die(format!("{what}: {e}")))
}

/// Runs a command; a script written a moment ago can be "Text file busy" while another process forks.
fn output(c: &mut Command) -> io::Result<Output> {
    for _ in 0..30 {
        match c.output() {
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(Duration::from_millis(100))
            }
            r => return r,
        }
    }
    c.output()
}

fn status(c: &mut Command) -> io::Result<ExitStatus> {
    for _ in 0..30 {
        match c.status() {
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(Duration::from_millis(100))
            }
            r => return r,
        }
    }
    c.status()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(base: &str) -> R<TempDir> {
        io_err(fs::create_dir_all(base), base)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        for i in 0..100 {
            let p =
                Path::new(base).join(format!("mayfly-update-{}-{nanos}-{i}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&p) {
                Ok(()) => return Ok(TempDir(p)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return die(format!("{}: {e}", p.display())),
            }
        }
        die(format!("cannot make a temporary directory in {base}"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ------------------------------------------------------------------ release lookup and download

fn agent(secs: u64) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(secs)))
        .user_agent(concat!("mayfly/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

/// One anonymous GET, retried twice on a transport error, 408, 429 or 5xx.
fn fetch(url: &str, secs: u64, json: bool) -> Result<ureq::Body, String> {
    let agent = agent(secs);
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        let mut req = agent.get(url);
        if json {
            req = req
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
        }
        match req.call() {
            Ok(resp) => return Ok(resp.into_body()),
            Err(ureq::Error::StatusCode(code)) => {
                last = format!("HTTP {code}");
                if !(code == 408 || code == 429 || code >= 500) {
                    break;
                }
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// The version of the latest release, or of release `v<version>` when one is asked for.
fn lookup(api: &str, version: Option<&str>) -> R<String> {
    let url = match version {
        Some(v) => format!("{api}/releases/tags/v{v}"),
        None => format!("{api}/releases/latest"),
    };
    let fail = |e: String| Die(format!("cannot look up the release at {url}: {e}"));
    let mut body = fetch(&url, 30, true).map_err(fail)?;
    let json: serde_json::Value = body
        .read_json()
        .map_err(|e| Die(format!("unreadable answer from {url}: {e}")))?;
    let tag = json.get("tag_name").and_then(|t| t.as_str()).unwrap_or("");
    let v = tag.strip_prefix('v').unwrap_or(tag);
    if parse_version(v).is_none() {
        return die(format!("{url}: the release tag \"{tag}\" is not v<x.y.z>"));
    }
    Ok(v.to_string())
}

/// A release whose notes have a release-please `BREAKING CHANGES` heading.
struct Breaking {
    version: String,
    url: String,
}

/// True when a line of the release body is a markdown heading naming BREAKING CHANGES.
fn has_breaking_section(body: &str) -> bool {
    body.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with('#') && l.contains("BREAKING CHANGES")
    })
}

/// The oldest breaking release in `(current, target]`, from the release list (newest first, paged).
/// Drafts and pre-releases do not count. Err when the list cannot be read.
fn first_breaking(
    api: &str,
    repo: &str,
    current: (u64, u64, u64),
    target: (u64, u64, u64),
) -> Result<Option<Breaking>, String> {
    let mut found: Vec<((u64, u64, u64), Breaking)> = Vec::new();
    for page in 1..=20 {
        let url = if page == 1 {
            format!("{api}/releases?per_page=100")
        } else {
            format!("{api}/releases?per_page=100&page={page}")
        };
        let mut body = fetch(&url, 30, true).map_err(|e| format!("{url}: {e}"))?;
        let json: serde_json::Value = body
            .read_json()
            .map_err(|e| format!("unreadable answer from {url}: {e}"))?;
        let list = json
            .as_array()
            .ok_or_else(|| format!("{url}: not a list of releases"))?;
        let mut past = list.is_empty() || list.len() < 100;
        for r in list {
            let flag = |k: &str| r.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            if flag("draft") || flag("prerelease") {
                continue;
            }
            let tag = r.get("tag_name").and_then(|t| t.as_str()).unwrap_or("");
            let version = tag.strip_prefix('v').unwrap_or(tag);
            let Some(v) = parse_version(version) else {
                continue;
            };
            if v <= current {
                past = true;
            } else if v <= target
                && r.get("body")
                    .and_then(|b| b.as_str())
                    .is_some_and(has_breaking_section)
            {
                let link = r.get("html_url").and_then(|u| u.as_str()).unwrap_or("");
                let link = if link.is_empty() {
                    format!("{repo}/releases/tag/v{version}")
                } else {
                    link.to_string()
                };
                found.push((
                    v,
                    Breaking {
                        version: version.to_string(),
                        url: link,
                    },
                ));
            }
        }
        if past {
            break;
        }
    }
    Ok(found.into_iter().min_by_key(|f| f.0).map(|f| f.1))
}

fn download(url: &str, dest: &Path) -> R<()> {
    let body = fetch(url, 600, false).map_err(|e| Die(format!("cannot download {url}: {e}")))?;
    let mut file = io_err(fs::File::create(dest), &dest.display().to_string())?;
    io_err(io::copy(&mut body.into_reader(), &mut file), url)?;
    Ok(())
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut h = Sha256::new();
    io::copy(&mut fs::File::open(path)?, &mut h)?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The checksum `SHA256SUMS` lists for `name` (lines are `<hex>  <name>` or `<hex> *<name>`).
pub fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let (hash, file) = (f.next()?, f.next()?);
        (file.strip_prefix('*').unwrap_or(file) == name).then(|| hash.to_ascii_lowercase())
    })
}

/// Unpacks the release tarball. Absolute paths, `..` and anything but files and directories are refused.
fn unpack(tgz: &Path, dest: &Path) -> R<()> {
    let file = io_err(fs::File::open(tgz), &tgz.display().to_string())?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let bad = |e: io::Error| Die(format!("cannot unpack {}: {e}", tgz.display()));
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let path = entry.path().map_err(bad)?.into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return die(format!("unsafe path in the archive: {}", path.display()));
        }
        let kind = entry.header().entry_type();
        if kind.is_file() || kind.is_dir() {
            if !entry.unpack_in(dest).map_err(bad)? {
                return die(format!("unsafe path in the archive: {}", path.display()));
            }
        } else if kind.is_symlink()
            || kind.is_hard_link()
            || kind.is_character_special()
            || kind.is_block_special()
            || kind.is_fifo()
        {
            return die(format!(
                "{} is a link or device; refusing the archive",
                path.display()
            ));
        }
        // Anything else is archive metadata (the pax header of git archive).
    }
    Ok(())
}

// ------------------------------------------------------------------ backup and restore

fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    let tmp = PathBuf::from(format!("{}.mayfly-tmp", dst.display()));
    fs::copy(src, &tmp)?;
    fs::rename(&tmp, dst)
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let _ = fs::remove_file(dst);
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)
    } else if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        fs::set_permissions(dst, meta.permissions())
    } else {
        copy_file(src, dst)
    }
}

/// What lives in the share directory besides the backup itself.
fn share_entries(share: &Path) -> io::Result<Vec<PathBuf>> {
    let mut v = Vec::new();
    for e in fs::read_dir(share)? {
        let e = e?;
        let name = e.file_name();
        if name != "previous" && name != "previous.new" {
            v.push(e.path());
        }
    }
    Ok(v)
}

/// Saves the installed command, share directory and units in `<share>/previous`, replacing an older one.
fn backup(paths: &Paths) -> R<()> {
    let share = Path::new(&paths.share);
    let sbin = Path::new(&paths.sbin).join("mayfly");
    if !sbin.is_file() {
        return die(format!(
            "{} is missing, so there is nothing to back up; run install.sh",
            sbin.display()
        ));
    }
    let new = share.join("previous.new");
    let _ = fs::remove_dir_all(&new);
    for d in ["sbin", "share", "systemd"] {
        io_err(fs::create_dir_all(new.join(d)), "backup")?;
    }
    io_err(
        copy_file(&sbin, &new.join("sbin/mayfly")),
        "backup of mayfly",
    )?;
    for p in io_err(share_entries(share), &paths.share)? {
        let name = p.file_name().unwrap_or_default();
        io_err(copy_tree(&p, &new.join("share").join(name)), "backup")?;
    }
    for u in UNITS {
        let src = Path::new(&paths.systemd_dir).join(u);
        if src.is_file() {
            io_err(copy_file(&src, &new.join("systemd").join(u)), "backup")?;
        }
    }
    io_err(
        fs::write(new.join("VERSION"), format!("{}\n", crate::VERSION)),
        "backup",
    )?;
    let prev = share.join("previous");
    let _ = fs::remove_dir_all(&prev);
    io_err(fs::rename(&new, &prev), "backup")
}

fn daemon_reload() {
    match status(Command::new("systemctl").arg("daemon-reload")) {
        Ok(s) if s.success() => {}
        _ => eprintln!("warning: systemctl daemon-reload failed; run it by hand"),
    }
}

/// Puts the files of `<share>/previous` back (units the backup has not are removed) and reloads systemd.
/// Returns the restored version.
fn restore(paths: &Paths) -> R<String> {
    let share = Path::new(&paths.share);
    let prev = share.join("previous");
    let version = fs::read_to_string(prev.join("VERSION")).map(|v| v.trim().to_string());
    let Ok(version) = version else {
        return die(format!(
            "no previous version saved ({}/VERSION is missing)",
            prev.display()
        ));
    };
    if !prev.join("sbin/mayfly").is_file() || !prev.join("share").is_dir() {
        return die(format!("{} is incomplete; not restoring", prev.display()));
    }
    io_err(
        copy_file(
            &prev.join("sbin/mayfly"),
            &Path::new(&paths.sbin).join("mayfly"),
        ),
        "restoring mayfly",
    )?;
    for p in io_err(share_entries(share), &paths.share)? {
        let gone = if p.is_dir() && !p.is_symlink() {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        };
        io_err(gone, &p.display().to_string())?;
    }
    for e in io_err(fs::read_dir(prev.join("share")), "restoring")? {
        let e = io_err(e, "restoring")?;
        io_err(
            copy_tree(&e.path(), &share.join(e.file_name())),
            "restoring",
        )?;
    }
    for u in UNITS {
        let saved = prev.join("systemd").join(u);
        let live = Path::new(&paths.systemd_dir).join(u);
        if saved.is_file() {
            io_err(copy_file(&saved, &live), "restoring")?;
        } else if live.exists() {
            if u.ends_with(".timer") {
                let _ = status(Command::new("systemctl").args(["disable", "-q", "--now", u]));
            }
            io_err(fs::remove_file(&live), &live.display().to_string())?;
        }
    }
    daemon_reload();
    Ok(version)
}

// ------------------------------------------------------------------ the command

/// `mayfly check` of the installed command: (passed, its output). None when it cannot be run.
fn health(paths: &Paths) -> Option<(bool, String)> {
    let bin = Path::new(&paths.sbin).join("mayfly");
    let o = output(Command::new(bin).arg("check")).ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    Some((o.status.success(), text.trim_end().to_string()))
}

pub fn run(paths: &Paths, args: &[String], out: &Out, w: &mut dyn Write) -> R<i32> {
    let o = parse_args(args)?;
    let result = if o.rollback {
        rollback(paths, w)
    } else {
        update(paths, &o, out, w)
    };
    if result.is_err() && !o.check && !o.rollback {
        record(paths, "failed", Skip::Keep);
    }
    result
}

fn rollback(paths: &Paths, w: &mut dyn Write) -> R<i32> {
    let was = restore(paths)?;
    // The timer must not bring back the version that was just taken out.
    let skip = if was == crate::VERSION {
        Skip::Keep
    } else {
        Skip::Set(crate::VERSION.to_string())
    };
    record(paths, "rolled-back", skip);
    say(
        w,
        false,
        &format!("restored mayfly {was} (was {})", crate::VERSION),
    );
    Ok(0)
}

fn update(paths: &Paths, o: &Opts, out: &Out, w: &mut dyn Write) -> R<i32> {
    let auto = o.auto;
    let mode = if auto {
        update_mode(paths)?
    } else {
        String::new()
    };
    if mode == "off" {
        say(w, auto, "UPDATE=off in pool.env, nothing to do");
        return Ok(0);
    }
    let api = env_or(
        "MAYFLY_RELEASE_API",
        "https://api.github.com/repos/variiuz/mayfly",
    );
    let repo = env_or("MAYFLY_REPO_URL", "https://github.com/variiuz/mayfly");
    let (api, repo) = (api.trim_end_matches('/'), repo.trim_end_matches('/'));
    let current = crate::VERSION;
    let target = lookup(api, o.version.as_deref())?;
    let (cv, tv) = (
        parse_version(current).unwrap_or_default(),
        parse_version(&target).unwrap_or_default(),
    );
    if o.version.is_none() && tv <= cv {
        say(
            w,
            auto,
            &format!("mayfly {current} is up to date (latest release {target})"),
        );
        if !o.check {
            record(paths, "up-to-date", Skip::Keep);
        }
        return Ok(0);
    }
    if o.version.is_some() && tv == cv {
        say(w, auto, &format!("mayfly {current} is already installed"));
        return Ok(0);
    }
    if o.check {
        // Best effort: when the release list cannot be read the line just stays plain.
        let note = match first_breaking(api, repo, cv, tv) {
            Ok(Some(_)) => " (breaking: run mayfly update by hand)",
            _ => "",
        };
        say(
            w,
            auto,
            &format!(
                "mayfly {target} is available (installed {current}); mayfly update installs it{note}"
            ),
        );
        return Ok(0);
    }
    if mode == "patch" && (tv.0, tv.1) != (cv.0, cv.1) {
        say(
            w,
            auto,
            &format!(
                "mayfly {target} is out (installed {current}); UPDATE=patch only takes {}.{}.x",
                cv.0, cv.1
            ),
        );
        return Ok(0);
    }
    if o.version.is_none() && State::read(paths).skipped == target {
        say(
            w,
            auto,
            &format!("mayfly {target} is skipped: it failed mayfly check here and was rolled back; mayfly update --version {target} tries it again"),
        );
        return Ok(0);
    }

    // The timer never takes a breaking release, the target or any one it skips over.
    if auto {
        match first_breaking(api, repo, cv, tv) {
            Ok(None) => {}
            Ok(Some(b)) => {
                say(
                    w,
                    auto,
                    &format!(
                        "mayfly {target} needs a manual mayfly update (breaking: see release notes)"
                    ),
                );
                say(
                    w,
                    auto,
                    &format!("  first breaking release {}: {}", b.version, b.url),
                );
                record_breaking(paths, &target);
                return Ok(0);
            }
            Err(e) => {
                say(
                    w,
                    auto,
                    &format!(
                        "cannot tell whether mayfly {target} is breaking ({e}); not updating, mayfly update takes it by hand"
                    ),
                );
                return Ok(0);
            }
        }
    }

    say(w, auto, &format!("updating mayfly {current} to {target}"));
    let tmp = TempDir::new(&paths.tmp_dir)?;
    let name = format!("mayfly-{target}.tar.gz");
    let base = format!("{repo}/releases/download/v{target}");
    let tgz = tmp.0.join(&name);
    let sums = tmp.0.join("SHA256SUMS");
    download(&format!("{base}/{name}"), &tgz)?;
    download(&format!("{base}/SHA256SUMS"), &sums)?;
    let listed = fs::read_to_string(&sums).unwrap_or_default();
    let got = io_err(sha256_file(&tgz), &name)?;
    if expected_sum(&listed, &name).as_deref() != Some(got.as_str()) {
        return die(format!("checksum mismatch for {name}; nothing was changed"));
    }
    let unpacked = tmp.0.join("unpacked");
    io_err(fs::create_dir(&unpacked), "unpacking")?;
    unpack(&tgz, &unpacked)?;
    let root = unpacked.join(format!("mayfly-{target}"));
    let seen = output(Command::new(root.join("mayfly")).arg("version"))
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .to_string()
        })
        .unwrap_or_default();
    if seen != format!("mayfly {target}") {
        return die(format!(
            "the new mayfly prints \"{seen}\", not \"mayfly {target}\"; nothing was changed"
        ));
    }
    if !root.join("install.sh").is_file() {
        return die(format!("{name} has no install.sh; nothing was changed"));
    }

    // The old command's verdict decides whether a failing new one is the update's fault.
    let old_ok = health(paths).is_some_and(|h| h.0);
    backup(paths)?;
    say(
        w,
        auto,
        &format!("saved mayfly {current} in {}/previous", paths.share),
    );
    let _ = w.flush();
    let installed = io_err(
        status(
            Command::new("bash")
                .arg(root.join("install.sh"))
                .current_dir(&root),
        ),
        "running install.sh",
    )?;
    if !installed.success() {
        let code = installed
            .code()
            .map_or("a signal".to_string(), |c| c.to_string());
        restore(paths)?;
        record(paths, "failed", Skip::Keep);
        print_die(&Die(format!(
            "install.sh of {target} failed (exit {code}); restored mayfly {current}"
        )));
        return Ok(1);
    }
    match health(paths) {
        Some((true, _)) => {}
        failed => {
            let text = failed.map_or_else(|| "cannot run the new mayfly".to_string(), |h| h.1);
            if old_ok {
                for l in text.lines() {
                    say(w, auto, &format!("  {l}"));
                }
                restore(paths)?;
                record(paths, "rolled-back", Skip::Set(target.clone()));
                print_die(&Die(format!(
                    "mayfly {target} failed mayfly check (it passed before); rolled back to {current}. {target} is skipped until mayfly update --version {target}"
                )));
                return Ok(1);
            }
            out.warn(
                w,
                &format!(
                    "mayfly check fails with {target}; it failed with {current} too, so {target} stays"
                ),
            );
        }
    }
    record(paths, "ok", Skip::Clear);
    say(
        w,
        auto,
        &format!(
            "mayfly {target} installed (was {current}); running slots use it from their next job"
        ),
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.12.3"), Some((0, 12, 3)));
        for bad in [
            "",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "v1.2.3",
            "1.2.3-rc1",
            "1..3",
            "+1.2.3",
        ] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
    }

    #[test]
    fn utc_formats_the_epoch() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_794_283_929), "2026-11-10T04:12:09Z");
    }

    #[test]
    fn checksum_lines() {
        let sums = "AB12  other\ncd34 *mayfly-1.0.0.tar.gz\n";
        assert_eq!(
            expected_sum(sums, "mayfly-1.0.0.tar.gz").as_deref(),
            Some("cd34")
        );
        assert_eq!(expected_sum(sums, "other").as_deref(), Some("ab12"));
        assert_eq!(expected_sum(sums, "mayfly"), None);
    }

    #[test]
    fn arguments() {
        let o = parse_args(&args(&["--check", "--version", "v1.2.3"])).unwrap();
        assert_eq!((o.check, o.version.as_deref()), (true, Some("1.2.3")));
        assert_eq!(
            parse_args(&args(&["--version=1.2.3"]))
                .unwrap()
                .version
                .as_deref(),
            Some("1.2.3")
        );
        assert!(parse_args(&args(&["--auto"])).unwrap().auto);
        for bad in [
            vec!["--version"],
            vec!["--version", "1.2"],
            vec!["--rollback", "--check"],
            vec!["--auto", "--version", "1.2.3"],
            vec!["--now"],
        ] {
            assert!(parse_args(&args(&bad)).is_err(), "{bad:?}");
        }
    }
}
