//! Every command this binary does not own goes to `mayfly.sh`, which is exec'd in place: same
//! arguments, same environment, same signals and exit code.

use crate::config::Paths;
use crate::out::{die, R};
use std::os::unix::process::CommandExt;
use std::process::Command;

pub fn script_path(paths: &Paths) -> String {
    format!("{}/mayfly.sh", paths.share)
}

/// Replaces this process with `mayfly.sh <args>`. Returns only on failure.
pub fn exec(paths: &Paths, args: &[String]) -> R<i32> {
    let script = script_path(paths);
    if !std::path::Path::new(&script).is_file() {
        return die(format!(
            "{script} is missing; run install.sh again (or set MAYFLY_SHARE to where mayfly.sh is)"
        ));
    }
    let err = Command::new(&script).args(args).exec();
    die(format!("cannot run {script}: {err}"))
}
