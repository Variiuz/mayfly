//! `mayfly install [--no-timer] [--from <dir>]`: puts a release (or a checkout with a built binary) in place.
//! `install.sh` only finds or downloads the release, checks it and runs this.
//!
//! Installs the command (this binary), the units, the template files and the shipped profiles. Creates
//! `pool.env` and `slots.conf` only when missing. Starts no slot and touches no guest. The gc timer is
//! switched on by the first install only; `--no-timer` leaves it off. The update timer is switched on the
//! first time its units are installed, unless pool.env says `UPDATE=off`.
//!
//! Releases up to 0.9.0 came with `mayfly.sh`, which ran the slot loops. Over such an install the script is
//! removed and the running slots are restarted (a restart drains: a job in progress finishes first), because
//! their loops cannot switch to a new command on their own. Loops of this version on can (`cmd_slot::RENEW`).
//! `MAYFLY_SBIN`, `MAYFLY_SHARE`, `MAYFLY_SYSTEMD_DIR` and `MAYFLY_CONF_DIR` move the files (`Paths`).

use crate::cmd_update::UNITS;
use crate::config::Paths;
use crate::out::{die, Die, R};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "usage: mayfly install [--no-timer] [--from <dir>]";
/// Copied to the share directory, mode 0644.
const TEMPLATE_FILES: [&str; 5] = [
    "provision.sh",
    "ci-job.sh",
    "pod-vm.sh",
    "mayfly-job.service",
    "template.env",
];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Opts {
    pub no_timer: bool,
    pub from: Option<PathBuf>,
}

pub fn parse_args(args: &[String]) -> R<Opts> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-timer" => o.no_timer = true,
            "--from" => match it.next() {
                Some(d) if !d.is_empty() => o.from = Some(PathBuf::from(d)),
                _ => return die(format!("--from needs a directory\n{USAGE}")),
            },
            s if s.starts_with("--from=") && s.len() > 7 => {
                o.from = Some(PathBuf::from(&s[7..]));
            }
            other => return die(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    Ok(o)
}

/// Why the install stopped: a message (exit 1), or the exit code of a command that failed (bash `set -e`).
enum Stop {
    Die(Die),
    Exit(i32),
}

impl From<Die> for Stop {
    fn from(d: Die) -> Stop {
        Stop::Die(d)
    }
}

type Res<T> = Result<T, Stop>;

fn io_err<T>(r: io::Result<T>, what: &Path) -> Res<T> {
    r.map_err(|e| Stop::Die(Die(format!("{}: {e}", what.display()))))
}

/// The lines of install.sh: `›`, `✓` and `!` are printed with or without colour (`tty`), never dropped.
struct Say<'a> {
    w: &'a mut dyn Write,
    tty: bool,
}

impl Say<'_> {
    fn c(&self, code: &'static str) -> &'static str {
        if self.tty {
            code
        } else {
            ""
        }
    }
    fn line(&mut self, colour: &'static str, sym: &str, msg: &str) {
        let (c, z) = (self.c(colour), self.c("\x1b[0m"));
        let _ = writeln!(self.w, "{c}{sym}{z} {msg}");
    }
    fn step(&mut self, msg: &str) {
        self.line("\x1b[36m", "›", msg);
    }
    fn done(&mut self, msg: &str) {
        self.line("\x1b[32m", "✓", msg);
    }
    fn warn(&mut self, msg: &str) {
        self.line("\x1b[33m", "!", msg);
    }
    /// Output of a child must not overtake ours.
    fn flush(&mut self) {
        let _ = self.w.flush();
    }
}

/// `id -u` is asked like install.sh does, so a stub on PATH can stand in for root in tests.
fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_end_matches('\n') == "0")
        .unwrap_or(false)
}

/// `command -v name` for a program on PATH.
fn on_path(name: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|d| {
        fs::metadata(d.join(name)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// A file every release and checkout has next to the binary or `install.sh`.
const MARKER: &str = "mayfly-slot@.service";

/// The release directory: `--from`, or the directory of the running binary when the units are next to it.
fn source_dir(from: Option<PathBuf>) -> R<PathBuf> {
    let dir = match from {
        Some(d) => d,
        None => {
            let exe = std::env::current_exe()
                .map_err(|e| Die(format!("cannot tell where mayfly is: {e}")))?;
            let d = exe.parent().map(Path::to_path_buf).unwrap_or_default();
            if !d.join(MARKER).is_file() {
                return die(format!(
                    "no release files next to {}; run mayfly install --from <release or checkout directory>",
                    exe.display()
                ));
            }
            d
        }
    };
    if !dir.join(MARKER).is_file() {
        return die(format!(
            "{} is not a release or checkout directory (no {MARKER})",
            dir.display()
        ));
    }
    Ok(dir)
}

/// `install -d [-m mode] dir`: missing parents get the default mode; the directory itself the given one.
fn install_d(dir: &Path, mode: Option<u32>) -> Res<()> {
    io_err(fs::create_dir_all(dir), dir)?;
    if let Some(m) = mode {
        io_err(fs::set_permissions(dir, fs::Permissions::from_mode(m)), dir)?;
    }
    Ok(())
}

/// `install -m mode src dst`, atomically: written next to `dst`, then renamed over it, so a running slot
/// or a running `mayfly` never sees half a file.
fn put(src: &Path, dst: &Path, mode: u32) -> Res<()> {
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dst.with_file_name(format!(".{name}.new.{}", std::process::id()));
    let r = fs::copy(src, &tmp)
        .and_then(|_| fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)))
        .and_then(|_| fs::rename(&tmp, dst));
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r.map_err(|e| {
        Stop::Die(Die(format!(
            "cannot install {} as {}: {e}",
            src.display(),
            dst.display()
        )))
    })
}

/// `cp -r src dst; chmod -R u=rwX,go=rX dst`: directories 0755, files 0755 when they were executable, else 0644.
fn copy_profiles(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)
    } else if meta.is_dir() {
        fs::create_dir(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_profiles(&e.path(), &dst.join(e.file_name()))?;
        }
        fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
    } else {
        fs::copy(src, dst)?;
        let m = if meta.permissions().mode() & 0o111 != 0 {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(dst, fs::Permissions::from_mode(m))
    }
}

/// The version an installed command reports. Builds without a version command print their help instead;
/// only a real `mayfly <version>` first line counts.
fn installed_version(bin: &Path) -> Option<String> {
    let o = Command::new(bin)
        .arg("version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let out = String::from_utf8_lossy(&o.stdout);
    let first = out.split('\n').next()?;
    let digit = first
        .strip_prefix("mayfly ")?
        .chars()
        .next()?
        .is_ascii_digit();
    if !digit {
        return None;
    }
    first
        .split([' ', '\t'])
        .filter(|f| !f.is_empty())
        .nth(1)
        .map(str::to_string)
}

/// `sed -n 's/^KEY=//p' pool.env | tail -n1`: the text after the last `KEY=`, quotes and all.
fn last_value(pool_env: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    pool_env
        .split('\n')
        .filter_map(|l| l.strip_prefix(&prefix))
        .next_back()
        .map(str::to_string)
}

/// Runs a command whose failure stops the install with its exit code (bash `set -e`).
fn must(say: &mut Say, c: &mut Command) -> Res<()> {
    say.flush();
    match c.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(Stop::Exit(s.code().unwrap_or(1))),
        Err(e) => Err(Stop::Die(Die(format!(
            "cannot run {}: {e}",
            c.get_program().to_string_lossy()
        )))),
    }
}

/// The content types of a storage, as `jq -r .content` prints them.
fn storage_content(say: &mut Say, storage: &str) -> Res<String> {
    say.flush();
    let o = Command::new("pvesh")
        .args([
            "get",
            &format!("/storage/{storage}"),
            "--output-format",
            "json",
        ])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| Stop::Die(Die(format!("cannot run pvesh: {e}"))))?;
    if !o.status.success() {
        return Err(Stop::Exit(o.status.code().unwrap_or(1)));
    }
    let v: serde_json::Value = serde_json::from_slice(&o.stdout)
        .map_err(|e| Stop::Die(Die(format!("pvesh get /storage/{storage}: {e}"))))?;
    Ok(match v.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "null".to_string(),
    })
}

/// Slots started by 0.9.0 or earlier run a loop that never looks for a new command: restart them once.
/// `systemctl restart` drains (a guest in a job finishes it first) and `--no-block` does not wait for that.
/// A slot that cannot be restarted is named and keeps its old loop until it is.
fn restart_old_loops(say: &mut Say) {
    say.flush();
    let listed = Command::new("systemctl")
        .args([
            "list-units",
            "--plain",
            "--no-legend",
            "--state=active",
            "mayfly-slot@*",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let units: Vec<String> = listed
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|u| u.starts_with("mayfly-slot@"))
        .map(str::to_string)
        .collect();
    if units.is_empty() {
        return;
    }
    say.step(&format!(
        "restarting {} running slot{} so that they run this version (jobs in progress finish first)",
        units.len(),
        if units.len() == 1 { "" } else { "s" }
    ));
    for u in units {
        say.flush();
        let ok = Command::new("systemctl")
            .args(["restart", "--no-block", &u])
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            say.warn(&format!(
                "could not restart {u}; it runs the old loop until: systemctl restart {u}"
            ));
        }
    }
}

pub fn run(paths: &Paths, args: &[String], tty: bool, w: &mut dyn Write) -> R<i32> {
    let mut say = Say { w, tty };
    match install(paths, args, &mut say) {
        Ok(()) => Ok(0),
        Err(Stop::Exit(code)) => Ok(code),
        Err(Stop::Die(d)) => Err(d),
    }
}

fn install(paths: &Paths, args: &[String], say: &mut Say) -> Res<()> {
    if !is_root() {
        return Err(Die("run as root".into()).into());
    }
    if !on_path("qm") {
        return Err(Die("not a Proxmox host (qm is missing)".into()).into());
    }
    let opts = parse_args(args)?;
    let src = source_dir(opts.from)?;
    let exe =
        std::env::current_exe().map_err(|e| Die(format!("cannot tell where mayfly is: {e}")))?;

    let conf = Path::new(&paths.conf_dir);
    let share = Path::new(&paths.share);
    let sbin = Path::new(&paths.sbin);
    let units = Path::new(&paths.systemd_dir);
    let installed = sbin.join("mayfly");
    let first = !installed.exists();
    // The gc timer: off with --no-timer, on for a first install, otherwise left as it is.
    let timer = if opts.no_timer {
        "no"
    } else if first {
        "yes"
    } else {
        "keep"
    };
    let mut update_timer = !units.join("mayfly-update.timer").exists();
    let old = if first {
        None
    } else {
        installed_version(&installed)
    };
    // This binary is the new one and is running, so there is no "does not run on this host" case.
    let new = crate::VERSION;
    let was = old.map(|o| format!(" (was {o})")).unwrap_or_default();
    say.step(&format!("installing Mayfly {new}{was}"));

    // `template build` downloads the cloud image with curl.
    must(
        say,
        Command::new("apt-get")
            .args(["install", "-y", "-q", "curl"])
            .stdout(Stdio::null()),
    )?;
    install_d(conf, Some(0o700))?;
    install_d(&conf.join("env"), Some(0o700))?;
    install_d(&conf.join("hooks.d"), Some(0o755))?;
    install_d(&conf.join("profiles"), Some(0o755))?;
    install_d(sbin, None)?;
    put(&exe, &installed, 0o755)?;
    install_d(share, None)?;
    // The script of 0.9.0 and earlier. Its slot loops are still running: see `restart_old_loops`.
    let script = share.join("mayfly.sh");
    let had_script = script.exists();
    if had_script {
        io_err(fs::remove_file(&script), &script)?;
    }
    for f in TEMPLATE_FILES {
        put(&src.join("template").join(f), &share.join(f), 0o644)?;
    }
    // Shipped profiles are replaced on every install; your own go in /etc/mayfly/profiles/<name>/.
    let shipped = share.join("profiles");
    if let Ok(m) = fs::symlink_metadata(&shipped) {
        let gone = if m.is_dir() {
            fs::remove_dir_all(&shipped)
        } else {
            fs::remove_file(&shipped)
        };
        io_err(gone, &shipped)?;
    }
    io_err(copy_profiles(&src.join("profiles"), &shipped), &shipped)?;
    install_d(units, None)?;
    for u in UNITS {
        put(&src.join(u), &units.join(u), 0o644)?;
    }
    let mut created: Vec<&str> = Vec::new();
    if !conf.join("pool.env").exists() {
        put(&src.join("pool.env.example"), &conf.join("pool.env"), 0o600)?;
        created.push("pool.env");
    }
    if !conf.join("slots.conf").exists() {
        put(
            &src.join("slots.conf.example"),
            &conf.join("slots.conf"),
            0o644,
        )?;
        created.push("slots.conf");
    }

    // Cloud-init snippets need a storage with "snippets" content. Add it to the snippet storage,
    // keeping whatever content types it already has.
    let pool_env =
        String::from_utf8_lossy(&fs::read(conf.join("pool.env")).unwrap_or_default()).into_owned();
    let storage = last_value(&pool_env, "SNIPPET_STORAGE")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".to_string());
    let content = storage_content(say, &storage)?;
    if !format!(",{content},").contains(",snippets,") {
        must(
            say,
            Command::new("pvesm")
                .args(["set", &storage, "--content"])
                .arg(format!("{content},snippets")),
        )?;
        say.done(&format!("storage {storage} now holds snippets"));
    }

    // `awk '{ print $1 }'` of the value without quotes.
    let update = last_value(&pool_env, "UPDATE")
        .map(|v| v.replace(['"', '\''], ""))
        .and_then(|v| {
            v.split([' ', '\t', '\n'])
                .find(|f| !f.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if update == "off" {
        update_timer = false;
    }

    must(say, Command::new("systemctl").arg("daemon-reload"))?;
    if update_timer {
        must(
            say,
            Command::new("systemctl").args(["enable", "-q", "--now", "mayfly-update.timer"]),
        )?;
    }
    match timer {
        "yes" => must(
            say,
            Command::new("systemctl").args(["enable", "-q", "--now", "mayfly-gc.timer"]),
        )?,
        "no" => {
            say.flush();
            let _ = Command::new("systemctl")
                .args(["disable", "-q", "--now", "mayfly-gc.timer"])
                .stderr(Stdio::null())
                .status();
        }
        _ => {}
    }
    // Running slots switch to the new command by themselves between two guests (`cmd_slot::RENEW`).
    if had_script {
        restart_old_loops(say);
    }
    let made = if created.is_empty() {
        String::new()
    } else {
        format!(", created {} in {}", created.join(" "), conf.display())
    };
    say.done(&format!("Mayfly {new} installed{made}"));

    if first || !created.is_empty() {
        let (b, d, z) = (say.c("\x1b[1m"), say.c("\x1b[2m"), say.c("\x1b[0m"));
        let conf_dir = conf.display();
        let _ = writeln!(say.w);
        let _ = writeln!(say.w, "{b}Next{z}");
        let mut n = 0;
        let mut next = |w: &mut dyn Write, text: String| {
            n += 1;
            let _ = writeln!(w, "  {n}. {text}");
        };
        if !conf.join("app.pem").exists() {
            next(
                say.w,
                format!("GitHub App private key to {conf_dir}/app.pem (chmod 600)"),
            );
        }
        let app_id_set = pool_env
            .split('\n')
            .any(|l| l.strip_prefix("APP_ID=").is_some_and(|v| !v.is_empty()));
        if !app_id_set {
            next(
                say.w,
                format!("APP_ID and ORG (or REPO) in {conf_dir}/pool.env"),
            );
        }
        next(say.w, format!("slots in {conf_dir}/slots.conf"));
        next(say.w, "mayfly check".to_string());
        next(
            say.w,
            "mayfly template build, then mayfly template promote <vmid>".to_string(),
        );
        next(say.w, "mayfly enable".to_string());
        let repo = std::env::var("MAYFLY_REPO_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "https://github.com/variiuz/mayfly".to_string());
        let _ = writeln!(say.w, "  {d}Getting started: {repo}#get-started{z}");
    }
    if timer == "no" {
        say.warn("gc timer is off (--no-timer); mayfly enable starts it");
    }
    say.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arguments() {
        assert_eq!(parse_args(&[]).unwrap(), Opts::default());
        let o = parse_args(&args(&["--no-timer", "--from", "/x"])).unwrap();
        assert_eq!((o.no_timer, o.from), (true, Some(PathBuf::from("/x"))));
        assert_eq!(
            parse_args(&args(&["--from=/y"])).unwrap().from,
            Some(PathBuf::from("/y"))
        );
        for bad in [vec!["--from"], vec!["--now"], vec!["x"]] {
            assert!(parse_args(&args(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_last_value_wins_and_keeps_its_quotes() {
        let t = "A=1\nUPDATE=patch\nUPDATE=\"off\" # x\nXUPDATE=y\n";
        assert_eq!(last_value(t, "UPDATE").as_deref(), Some("\"off\" # x"));
        assert_eq!(last_value(t, "B"), None);
    }
}
