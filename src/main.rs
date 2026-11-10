use mayfly::cmd_slots::Verb;
use mayfly::config::Paths;
use mayfly::out::{print_die, Out};
use mayfly::pve::RealPve;
use mayfly::{
    cmd_check, cmd_gc, cmd_help, cmd_install, cmd_slots, cmd_status, cmd_update, forward, Ctx,
};
use std::io::{IsTerminal, Write};
use std::process::{Command, ExitCode};

/// `hostname -s`.
fn node() -> String {
    let short = |s: &str| s.trim().split('.').next().unwrap_or("").to_string();
    if let Ok(o) = Command::new("hostname").arg("-s").output() {
        if o.status.success() {
            return short(&String::from_utf8_lossy(&o.stdout));
        }
    }
    short(&std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = Paths::from_env();
    let out = Out::detect();
    let mut stdout = std::io::stdout().lock();
    let result = match args.first().map(String::as_str) {
        Some("version" | "--version" | "-v") => {
            cmd_help::version(&mut stdout);
            Ok(0)
        }
        Some("help" | "--help" | "-h") => {
            cmd_help::help(&paths, &out, &mut stdout);
            Ok(0)
        }
        Some("install") => {
            // install.sh coloured on a terminal unless NO_COLOR is set, whatever TERM says.
            let tty = std::io::stdout().is_terminal()
                && std::env::var("NO_COLOR").map_or(true, |v| v.is_empty());
            cmd_install::run(&paths, &args[1..], tty, &mut stdout)
        }
        Some("update") => cmd_update::run(&paths, &args[1..], &out, &mut stdout),
        Some(cmd @ ("status" | "check" | "gc" | "enable" | "disable")) => {
            let node = node();
            let cx = Ctx {
                paths: &paths,
                pve: &RealPve,
                node: &node,
                meminfo: "/proc/meminfo",
            };
            match cmd {
                "status" => cmd_status::run(&cx, &out, &mut stdout),
                "check" => cmd_check::run(&cx, &out, &mut stdout),
                "gc" => cmd_gc::run(&cx, &out, &mut stdout),
                "enable" => cmd_slots::run(&cx, Verb::Enable, &args[1..], &out, &mut stdout),
                _ => cmd_slots::run(&cx, Verb::Disable, &args[1..], &out, &mut stdout),
            }
        }
        _ => {
            let _ = stdout.flush();
            forward::exec(&paths, &args)
        }
    };
    let _ = stdout.flush();
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(d) => {
            print_die(&d);
            ExitCode::from(1)
        }
    }
}
