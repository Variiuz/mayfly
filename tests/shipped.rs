//! The files a release ships as they are: the example configuration and the template's provisioning script.

use mayfly::config::{parse_pool_env, Slots};
use std::fs;
use std::path::Path;
use std::process::Command;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn pool_env_example_is_plain_key_value() {
    let text = fs::read_to_string(root().join("pool.env.example")).unwrap();
    let (vars, issues) = parse_pool_env(&text);
    assert!(
        issues.is_empty(),
        "lines {:?}",
        issues.iter().map(|i| i.line).collect::<Vec<_>>()
    );
    // The settings a new host has to fill in are there, empty.
    for key in ["APP_ID", "ORG", "REPO"] {
        assert!(vars.iter().any(|(k, v)| k == key && v.is_empty()), "{key}");
    }
    // Settings that are gone are not offered.
    assert!(!vars.iter().any(|(k, _)| k == "SLOT_ENGINE"));
}

#[test]
fn slots_conf_example_has_slots_without_removed_keys() {
    let text = fs::read_to_string(root().join("slots.conf.example")).unwrap();
    let slots = Slots::parse(&text);
    assert_eq!(slots.names(), ["build-1", "build-2", "small-1"]);
    assert!(!text.contains("engine="));
}

/// `template.env` of the core and of the profile reach the profile's `provision.sh`, which is a child
/// bash: the block between `set -a` and `set +a` of `template/provision.sh` exports them.
#[test]
fn provision_exports_core_and_profile_template_env() {
    let dir = std::env::temp_dir().join(format!("mayfly-shipped-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("profile")).unwrap();
    fs::write(dir.join("template.env"), "CORE_X=core\n").unwrap();
    fs::write(dir.join("profile/template.env"), "PROFILE_X=profile\n").unwrap();
    let script = fs::read_to_string(root().join("template/provision.sh")).unwrap();
    let from = script.find("\nset -a\n").expect("set -a") + 1;
    let to = script.find("\nset +a\n").expect("set +a") + "\nset +a\n".len();
    let d = dir.display().to_string();
    let block = script[from..to]
        .replace("/opt/mayfly-profile", &format!("{d}/profile"))
        .replace("/root/", &format!("{d}/"));
    fs::write(dir.join("env.sh"), block).unwrap();
    let out = Command::new("bash")
        .arg("-c")
        .arg(format!(
            ". '{d}/env.sh'; bash -c 'echo \"$CORE_X-$PROFILE_X\"'"
        ))
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "core-profile\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
