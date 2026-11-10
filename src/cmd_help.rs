//! `mayfly help` and `mayfly version`.

use crate::config::Paths;
use crate::out::Out;
use std::io::Write;

pub fn version(w: &mut dyn Write) {
    let _ = writeln!(w, "mayfly {}", crate::VERSION);
}

pub fn help(paths: &Paths, out: &Out, w: &mut dyn Write) {
    let (b, d, z) = (out.bold(), out.dim(), out.reset());
    let _ = write!(
        w,
        "{b}mayfly{z} {version}: throwaway GitHub Actions runners on Proxmox, one VM or container per job.

{b}Usage{z}  mayfly <command>

{d}Set up{z}
  check                             App key, storage, network, template; changes nothing
  template build [profile] [--lxc]  build a template (VM, or LXC container with --lxc); runs in
                                    unit mayfly-template-build, --foreground to wait for it
  template canary <vmid>            slots marked \"next\" clone it
  template promote <vmid>           every slot clones it from its next job on
  template list

{d}Run{z}
  enable [slot ...]                 start slots (all when none named) and the gc timer
  disable [slot ...]                stop slots; running jobs finish first
  status                            slots, guests, jobs, pool and RAM

{d}Other{z}
  gc                                stale runners, orphan guests, old snippets (the timer runs it)
  slot <name>                       one slot's loop (mayfly-slot@<name>.service runs it)
  update [--check|--rollback]       update Mayfly from the latest release (the update timer runs it)
  version

Config in {conf}: pool.env, slots.conf. Docs: https://github.com/variiuz/mayfly
",
        version = crate::VERSION,
        conf = paths.conf_dir,
    );
}
