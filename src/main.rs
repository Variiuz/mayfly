use mayfly::config::Paths;
use mayfly::out::{print_die, Out};
use mayfly::pve::RealPve;
use mayfly::{cmd_check, cmd_help, cmd_status, forward, Ctx};
use std::io::Write;
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
        Some(cmd @ ("status" | "check")) => {
            let node = node();
            let cx = Ctx {
                paths: &paths,
                pve: &RealPve,
                node: &node,
                meminfo: "/proc/meminfo",
            };
            if cmd == "status" {
                cmd_status::run(&cx, &out, &mut stdout)
            } else {
                cmd_check::run(&cx, &out, &mut stdout)
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
