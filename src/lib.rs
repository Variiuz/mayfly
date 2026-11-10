//! Mayfly: throwaway GitHub Actions runners on Proxmox VE.
//!
//! This crate owns `version`, `help`, `status`, `check`, `update`, `install`, `gc`, `enable`, `disable` and
//! `template`, and `slot` for slots that run `engine=rust` (`kind=vm`). Every other slot is forwarded to
//! `mayfly.sh` (see `forward`).

pub mod cmd_check;
pub mod cmd_gc;
pub mod cmd_help;
pub mod cmd_install;
pub mod cmd_slot;
pub mod cmd_slots;
pub mod cmd_status;
pub mod cmd_template;
pub mod cmd_update;
pub mod config;
pub mod forward;
pub mod github;
pub mod guest;
pub mod out;
pub mod pve;

use config::Paths;
use pve::Pve;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a command needs from its surroundings; tests build one around a fake `Pve`.
pub struct Ctx<'a> {
    pub paths: &'a Paths,
    pub pve: &'a dyn Pve,
    /// `hostname -s`.
    pub node: &'a str,
    pub meminfo: &'a str,
}
