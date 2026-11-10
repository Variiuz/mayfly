//! `status`, `check`, the config parser and the forwarding, against a fake Proxmox host and a fake
//! GitHub. Nothing here needs Proxmox, a network or root (the key is made with `openssl`).

use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use mayfly::config::{self, Paths};
use mayfly::out::Out;
use mayfly::pve::FakePve;
use mayfly::{cmd_check, cmd_status, Ctx};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ------------------------------------------------------------------ helpers

fn tmp(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "mayfly-test-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst),
        name
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn write(path: impl AsRef<Path>, text: &str) {
    if let Some(dir) = path.as_ref().parent() {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(path, text).unwrap();
}

struct Fixture {
    root: PathBuf,
    paths: Paths,
    meminfo: String,
}

impl Fixture {
    fn new(name: &str, pool_env: &str, slots: &str) -> Fixture {
        let root = tmp(name);
        let r = root.display().to_string();
        let mut paths = Paths::new(
            &format!("{r}/conf"),
            &format!("{r}/share"),
            &format!("{r}/pve"),
            &format!("{r}/fw"),
        );
        paths.state_dir = format!("{r}/state");
        write(&paths.pool_env, pool_env);
        write(&paths.slots_conf, slots);
        fs::create_dir_all(format!("{r}/share/profiles/default")).unwrap();
        fs::create_dir_all(format!("{r}/pve/qemu-server")).unwrap();
        fs::create_dir_all(format!("{r}/pve/lxc")).unwrap();
        let meminfo = format!("{r}/meminfo");
        write(
            &meminfo,
            "MemTotal:       65536000 kB\nMemAvailable:   33554432 kB\n",
        );
        Fixture {
            root,
            paths,
            meminfo,
        }
    }

    fn guest(&self, kind: &str, id: &str) {
        write(format!("{}/pve/{kind}/{id}.conf", self.root.display()), "");
    }

    fn run_status(&self, pve: &FakePve) -> (i32, String) {
        let cx = Ctx {
            paths: &self.paths,
            pve,
            node: "pve1",
            meminfo: &self.meminfo,
        };
        let mut buf = Vec::new();
        let code = cmd_status::run(&cx, &Out::plain(), &mut buf).unwrap();
        (code, String::from_utf8(buf).unwrap())
    }

    fn run_check(&self, pve: &FakePve) -> Result<(i32, String), String> {
        let cx = Ctx {
            paths: &self.paths,
            pve,
            node: "pve1",
            meminfo: &self.meminfo,
        };
        let mut buf = Vec::new();
        cmd_check::run(&cx, &Out::plain(), &mut buf)
            .map(|code| (code, String::from_utf8(buf).unwrap()))
            .map_err(|d| d.0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn base_pve() -> FakePve {
    FakePve::new().answer("pvesm path local:snippets/x", "/var/lib/vz/snippets/x\n")
}

fn load(f: &Fixture, strict: bool) -> Result<config::Config, String> {
    config::load(&f.paths, &base_pve(), "pve1", strict)
        .map(|(c, _)| c)
        .map_err(|d| d.0)
}

// ------------------------------------------------------------------ config

#[test]
fn paths_follow_the_environment() {
    // The only test that touches these variables.
    std::env::set_var("MAYFLY_CONF_DIR", "/c");
    std::env::set_var("MAYFLY_SHARE", "/s");
    std::env::set_var("MAYFLY_PVE_ETC", "/p");
    std::env::remove_var("MAYFLY_PVE_FW_DIR");
    let p = Paths::from_env();
    assert_eq!(
        (
            p.pool_env.as_str(),
            p.slots_conf.as_str(),
            p.env_dir.as_str()
        ),
        ("/c/pool.env", "/c/slots.conf", "/c/env")
    );
    assert_eq!(
        (p.share.as_str(), p.pve_fw_dir.as_str()),
        ("/s", "/p/firewall")
    );
    std::env::set_var("MAYFLY_PVE_FW_DIR", "/fw");
    assert_eq!(Paths::from_env().pve_fw_dir, "/fw");
    for k in [
        "MAYFLY_CONF_DIR",
        "MAYFLY_SHARE",
        "MAYFLY_PVE_ETC",
        "MAYFLY_PVE_FW_DIR",
    ] {
        std::env::remove_var(k);
    }
    let d = Paths::from_env();
    assert_eq!(
        (
            d.conf_dir.as_str(),
            d.share.as_str(),
            d.pve_etc.as_str(),
            d.pve_fw_dir.as_str()
        ),
        (
            "/etc/mayfly",
            "/usr/local/share/mayfly",
            "/etc/pve",
            "/etc/pve/firewall"
        )
    );
}

#[test]
fn no_org_or_repo_is_refused() {
    let f = Fixture::new("noorg", "", "");
    let e = load(&f, true).err().unwrap();
    assert!(
        e.starts_with("set ORG (organization runners) or REPO=owner/name"),
        "{e}"
    );
}

#[test]
fn missing_pool_env_is_refused() {
    let f = Fixture::new("nopool", "ORG=a\n", "");
    fs::remove_file(&f.paths.pool_env).unwrap();
    let e = load(&f, true).err().unwrap();
    assert_eq!(e, format!("{} missing (run install.sh)", f.paths.pool_env));
}

#[test]
fn org_gives_org_scope_and_the_defaults() {
    let f = Fixture::new("org", "ORG=acme\n", "");
    let c = load(&f, true).unwrap();
    assert_eq!(c.scope, "orgs/acme");
    assert_eq!(c.snippet_dir, "/var/lib/vz/snippets");
    assert_eq!(c.get("RUNNER_PREFIX"), "ci-pve1");
    assert_eq!(c.get("API_URL"), "https://api.github.com");
    assert_eq!(c.get("KEY_PATH"), format!("{}/app.pem", f.paths.conf_dir));
    assert_eq!(c.get("HOOK_DIR"), format!("{}/hooks.d", f.paths.conf_dir));
    assert_eq!(c.get("STORAGE"), "local-lvm");
    assert_eq!(c.get("RUNNER_GROUP_ID"), "1");
    assert_eq!(c.get("RUNNER_LABELS_BASE"), "self-hosted,Linux,X64");
    assert_eq!(c.get("VMID_MIN"), "9000");
    assert_eq!(c.get("TEMPLATE_VMIDS"), "9097 9098 9099");
    assert_eq!(c.get("MAX_POOL_PERCENT"), "70");
    assert_eq!(c.number("MIN_FREE_RAM_GB"), Some(8));
    assert_eq!(c.get("PROFILE"), "default");
    assert_eq!(c.get("LXC_OSTEMPLATE"), "ubuntu-24.04-standard");
    assert_eq!(
        c.get("BLOCK_NETS"),
        "10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 100.64.0.0/10 169.254.0.0/16"
    );
}

#[test]
fn repo_gives_repo_scope() {
    let f = Fixture::new("repo", "REPO=acme/app\nORG=ignored\n", "");
    assert_eq!(load(&f, true).unwrap().scope, "repos/acme/app");
}

#[test]
fn repo_without_owner_is_refused() {
    let f = Fixture::new("repo2", "REPO=acme\n", "");
    assert_eq!(load(&f, true).err().unwrap(), "REPO=acme: use owner/name");
}

#[test]
fn bad_name_prefix_and_tag_are_refused() {
    let f = Fixture::new("prefix", "ORG=acme\nNAME_PREFIX=Bad_Prefix\n", "");
    assert_eq!(
        load(&f, true).err().unwrap(),
        "NAME_PREFIX=Bad_Prefix: lowercase letters, digits and dashes only"
    );
    write(&f.paths.pool_env, "ORG=acme\nTAG=-x\n");
    assert_eq!(
        load(&f, true).err().unwrap(),
        "TAG=-x: lowercase letters, digits, dashes and underscores only"
    );
    write(&f.paths.pool_env, "ORG=acme\nTAG=a_b-c\nNAME_PREFIX=ci-\n");
    assert!(load(&f, true).is_ok());
}

#[test]
fn image_url_falls_back_to_ubuntu_image_url() {
    let f = Fixture::new("image", "ORG=acme\n", "");
    let c = load(&f, true).unwrap();
    assert_eq!(c.get("IMAGE_URL"), c.get("UBUNTU_IMAGE_URL"));
    assert!(c
        .get("IMAGE_URL")
        .ends_with("noble-server-cloudimg-amd64.img"));
    write(
        &f.paths.pool_env,
        "ORG=acme\nUBUNTU_IMAGE_URL=https://u/x.img\n",
    );
    assert_eq!(load(&f, true).unwrap().get("IMAGE_URL"), "https://u/x.img");
    write(
        &f.paths.pool_env,
        "ORG=acme\nIMAGE_URL=https://i/y.img\nUBUNTU_IMAGE_URL=https://u/x.img\n",
    );
    assert_eq!(load(&f, true).unwrap().get("IMAGE_URL"), "https://i/y.img");
}

#[test]
fn runner_prefix_defaults_to_name_prefix_and_host() {
    let f = Fixture::new(
        "rprefix",
        "ORG=acme\nNAME_PREFIX=lab-\nRUNNER_PREFIX=\n",
        "",
    );
    assert_eq!(load(&f, true).unwrap().get("RUNNER_PREFIX"), "lab-pve1");
    write(&f.paths.pool_env, "ORG=acme\nRUNNER_PREFIX=mine\n");
    assert_eq!(load(&f, true).unwrap().get("RUNNER_PREFIX"), "mine");
}

#[test]
fn later_assignments_win_and_empty_overrides_a_default() {
    let f = Fixture::new(
        "last",
        "ORG=a\nSTORAGE=one\nSTORAGE=two\nBRIDGE=\nVLAN=\"42\"\n",
        "",
    );
    let c = load(&f, true).unwrap();
    assert_eq!(c.get("STORAGE"), "two");
    assert_eq!(c.get("BRIDGE"), "");
    assert_eq!(c.get("VLAN"), "42");
}

#[test]
fn a_shell_line_is_an_error_naming_the_line() {
    let f = Fixture::new("shell", "ORG=acme\nKEY=$(hostname)\n", "");
    assert_eq!(
        load(&f, true).err().unwrap(),
        "pool.env line 2: variable or command substitution"
    );
    // check reads what it can and reports the rest.
    let (c, issues) = config::load(&f.paths, &base_pve(), "pve1", false).unwrap();
    assert_eq!(c.scope, "orgs/acme");
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].line, 2);
}

#[test]
fn storage_without_snippets_is_refused() {
    let f = Fixture::new("snip", "ORG=acme\nSNIPPET_STORAGE=nope\n", "");
    let e = config::load(&f.paths, &FakePve::new(), "pve1", true)
        .err()
        .unwrap()
        .0;
    assert_eq!(
        e,
        "storage nope has no snippets content (install.sh adds it)"
    );
}

// ------------------------------------------------------------------ status

fn status_fixture(pool_extra: &str) -> (Fixture, FakePve) {
    let f = Fixture::new(
        "status",
        &format!("ORG=acme\nTEMPLATE_VMID=9098\nNEXT_TEMPLATE_VMID=9099\n{pool_extra}"),
        "build-1 9002 linux 8 16 60 no current\n\
         # old 9009 x 1 1 1 no current\n\
         pods-1 9041 small 12 36 120 no current kind=pod containers=6\n\
         ct-1 9051 small 2 4 20 no current kind=lxc\n\
         off-1 9052 small 1 1 1 no current\n",
    );
    f.guest("qemu-server", "9002");
    f.guest("qemu-server", "9041");
    f.guest("lxc", "9051");
    let pve = base_pve()
        .answer("qm status 9002", "status: running\n")
        .answer("qm status 9041", "status: running\n")
        .answer("pct status 9051", "status: stopped\n")
        .answer(
            "timeout 15 qm guest exec 9002 --timeout 10 -- pgrep -f bin/Runner\\.Worker",
            "{\"exitcode\":0,\"out-data\":\"123\\n\"}",
        )
        .answer("systemctl is-active mayfly-slot@build-1", "active\n")
        .answer("systemctl is-active mayfly-slot@pods-1", "activating\n")
        .answer("systemctl is-active mayfly-slot@ct-1", "failed\n")
        .answer("systemctl is-active mayfly-slot@off-1", "inactive\n")
        .answer(
            "pvesh get /nodes/pve1/storage/local-lvm/status --output-format json",
            "{\"total\":1000,\"used\":425}",
        );
    (f, pve)
}

#[test]
fn status_lines() {
    let (f, pve) = status_fixture("LXC_TEMPLATE_VMID=9096\nNEXT_LXC_TEMPLATE_VMID=9095\n");
    let (code, out) = f.run_status(&pve);
    let want = "\
SLOT       VMID   KIND LABELS                 SIZE    SERVICE   GUEST    JOB    GITHUB
build-1    9002   vm   linux                  8c/16G  active    running  busy   ?
pods-1     9041   pod  small                  12c/36G activating running  6 containers ?
ct-1       9051   lxc  small                  2c/4G   failed    stopped  -      ?
off-1      9052   vm   small                  1c/1G   inactive  none     -      ?

warning: could not list the runners on GitHub (mayfly check)
storage    local-lvm 42% used, limit 70%
ram        32 GB free, 8 GB kept for the host
template   9098, next 9099, profile default
lxc        9096, next 9095
update     {version}, no update run yet
";
    let want = want.replace("{version}", env!("CARGO_PKG_VERSION"));
    assert_eq!(out, want);
    assert_eq!(code, 0);
}

#[test]
fn status_without_lxc_templates_has_no_lxc_line() {
    let (f, pve) = status_fixture("");
    let (_, out) = f.run_status(&pve);
    assert!(
        out.ends_with(&format!(
            "template   9098, next 9099, profile default\nupdate     {}, no update run yet\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{out}"
    );
    assert!(!out.contains("\nlxc "));
    write(
        format!("{}/update.state", f.paths.state_dir),
        "last_attempt=2026-11-10T04:12:09Z\nlast_result=rolled-back\nskipped=9.9.9\n",
    );
    let (_, out) = f.run_status(&pve);
    assert!(
        out.ends_with(&format!(
            "\nupdate     {}, last 2026-11-10T04:12:09Z rolled-back\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{out}"
    );
}

#[test]
fn status_asks_a_busy_guest_only_when_it_runs() {
    let (f, pve) = status_fixture("");
    f.run_status(&pve);
    let calls = pve.calls();
    // The pod and the stopped container are not asked for a job.
    assert!(
        calls.iter().filter(|c| c.contains("pgrep")).count() == 1,
        "{calls:?}"
    );
    assert!(calls.iter().any(|c| c == "qm status 9002"));
    assert!(!calls.iter().any(|c| c.starts_with("qm status 9052")));
}

#[test]
fn status_without_slots_warns() {
    let f = Fixture::new("noslots", "ORG=acme\n", "# nothing\n\n");
    let (code, out) = f.run_status(&base_pve());
    assert_eq!(code, 0);
    assert_eq!(
        out,
        format!("warning: no slots in {}\n", f.paths.slots_conf)
    );
    fs::remove_file(&f.paths.slots_conf).unwrap();
    assert_eq!(f.run_status(&base_pve()).1, out);
}

#[test]
fn status_refuses_a_shell_pool_env() {
    let f = Fixture::new(
        "shellstatus",
        "ORG=acme\nsource /etc/x\n",
        "a 9002 x 1 1 1 no current\n",
    );
    let cx = Ctx {
        paths: &f.paths,
        pve: &base_pve(),
        node: "pve1",
        meminfo: &f.meminfo,
    };
    let e = cmd_status::run(&cx, &Out::plain(), &mut Vec::new())
        .err()
        .unwrap();
    assert_eq!(e.0, "pool.env line 2: not a KEY=value assignment");
}

#[test]
fn paint_pads_and_colours() {
    use mayfly::out::Colour;
    let plain = Out::plain();
    assert_eq!(plain.paint("busy", 6, &[("busy", Colour::Blu)]), "busy  ");
    let tty = Out { tty: true };
    assert_eq!(
        tty.paint("busy", 6, &[("busy", Colour::Blu)]),
        "\x1b[36mbusy  \x1b[0m"
    );
    assert_eq!(tty.paint("x", 3, &[("busy", Colour::Blu)]), "x  ");
    let mut buf = Vec::new();
    tty.item(&mut buf, mayfly::out::Kind::Warn, "storage", "70% used");
    assert_eq!(
        String::from_utf8(buf).unwrap(),
        "\x1b[33m!\x1b[0m storage   70% used\n"
    );
}

// ------------------------------------------------------------------ fake GitHub

struct Seen {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

struct FakeGithub {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// Answers the four calls of `check`. A `GET /app` answer of 401 makes the key "refused".
fn fake_github(app_status: u16) -> FakeGithub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).unwrap_or(0) == 0 {
                continue;
            }
            let mut headers = HashMap::new();
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                    break;
                }
                if let Some((k, v)) = l.trim_end().split_once(':') {
                    headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let (code, body) = match (method.as_str(), path.as_str()) {
                ("GET", "/app") if app_status != 200 => (app_status, "{\"message\":\"Bad\"}"),
                ("GET", "/app") => (200, "{\"slug\":\"my-app\",\"id\":12345}"),
                ("GET", "/orgs/acme/installation") => (200, "{\"id\":42}"),
                ("POST", "/app/installations/42/access_tokens") => {
                    (201, "{\"token\":\"ghs_faketoken\"}")
                }
                ("GET", "/orgs/acme/actions/runners?per_page=100&page=1") => (
                    200,
                    "{\"total_count\":6,\"runners\":[\
                     {\"name\":\"ci-pve1-build-1-1700000000\",\"status\":\"online\",\"busy\":true},\
                     {\"name\":\"ci-pve1-pods-1-1-1700000000\",\"status\":\"online\",\"busy\":false},\
                     {\"name\":\"ci-pve1-pods-1-2-1700000000\",\"status\":\"online\",\"busy\":true},\
                     {\"name\":\"ci-pve1-pods-1-3-1700000000\",\"status\":\"offline\",\"busy\":false},\
                     {\"name\":\"ci-pve1-ct-1-1700000001\",\"status\":\"offline\",\"busy\":false},\
                     {\"name\":\"other-x\",\"status\":\"online\",\"busy\":false}]}",
                ),
                ("GET", "/orgs/acme/actions/runners?per_page=100&page=2") => (200, "{\"runners\":[]}"),
                ("GET", "/orgs/acme/actions/runners?per_page=1") => {
                    (200, "{\"total_count\":3,\"runners\":[]}")
                }
                _ => (404, "{}"),
            };
            log.lock().unwrap().push(Seen {
                method,
                path,
                headers,
            });
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
        }
    });
    FakeGithub { port, seen }
}

/// A fresh RSA key: PKCS#1 PEM (what GitHub hands out) or PKCS#8. Returns the public key PEM.
fn make_key(dir: &Path, pkcs8: bool) -> (PathBuf, Vec<u8>) {
    let key = dir.join(if pkcs8 { "app8.pem" } else { "app.pem" });
    let key_s = key.to_str().unwrap();
    let ok = |c: &mut Command| c.output().map(|o| o.status.success()).unwrap_or(false);
    let made = if pkcs8 {
        ok(Command::new("openssl").args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
            "-out",
            key_s,
        ]))
    } else {
        ok(Command::new("openssl").args(["genrsa", "-traditional", "-out", key_s, "2048"]))
            || ok(Command::new("openssl").args(["genrsa", "-out", key_s, "2048"]))
    };
    assert!(made, "openssl could not make a key");
    let out = Command::new("openssl")
        .args(["rsa", "-in", key_s, "-pubout"])
        .output()
        .unwrap();
    assert!(out.status.success());
    (key, out.stdout)
}

fn verify(jwt: &str, public: &[u8]) -> Value {
    let mut v = Validation::new(Algorithm::RS256);
    v.validate_aud = false;
    decode::<Value>(jwt, &DecodingKey::from_rsa_pem(public).unwrap(), &v)
        .unwrap()
        .claims
}

fn check_fixture(gh_port: u16, key: &Path) -> (Fixture, FakePve) {
    let f = Fixture::new(
        "check",
        &format!(
            "APP_ID=12345\nKEY_PATH={}\nAPI_URL=http://127.0.0.1:{gh_port}\nORG=acme\nTEMPLATE_VMID=9098\nKEY=$(x)\n",
            key.display()
        ),
        "s1 9002 linux 8 16 60 cache,missing,no current\n",
    );
    f.guest("qemu-server", "9098");
    write(format!("{}/env/cache.env", f.paths.conf_dir), "A=1\n");
    write(format!("{}/hooks.d/a", f.paths.conf_dir), "#!/bin/sh\n");
    fs::set_permissions(
        format!("{}/hooks.d/a", f.paths.conf_dir),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    write(format!("{}/hooks.d/b", f.paths.conf_dir), "x\n");
    fs::set_permissions(
        format!("{}/hooks.d/b", f.paths.conf_dir),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let pve = base_pve()
        .answer("pvesm status --storage local-lvm", "")
        .answer(
            "pvesh get /nodes/pve1/storage/local-lvm/status --output-format json",
            "{\"total\":1000,\"used\":425}",
        )
        .answer("ip link show vmbr0", "")
        .answer("pve-firewall status", "Status: enabled/running\n")
        .answer(
            "qm config 9098",
            "name: ci-template\ndescription: profile default\ntemplate: 1\n",
        )
        .answer(
            "pvesh get /cluster/backup --output-format json",
            "[{\"id\":\"backup-1\",\"all\":1},{\"id\":\"b2\",\"all\":0},{\"id\":\"b3\"}]",
        );
    (f, pve)
}

#[test]
fn check_lines_and_a_verifiable_jwt() {
    let dir = tmp("key");
    let (key, public) = make_key(&dir, false);
    let gh = fake_github(200);
    let (f, pve) = check_fixture(gh.port, &key);
    let before = mayfly::github::now();
    let (code, out) = f.run_check(&pve).unwrap();
    let after = mayfly::github::now();
    let want = format!(
        "\
ok   GitHub    App my-app, 3 runners in acme
warn pool.env  line 6: variable or command substitution; the next Mayfly reads pool.env as plain KEY=value
ok   storage   local-lvm 42% used
ok   network   vmbr0
ok   firewall  job VMs reach the internet only
ok   template  9098
ok   profile   default
warn s1        env missing: {env}/missing.env is missing
ok   slots     1 in {slots}
ok   hooks     1 in {conf}/hooks.d
warn backup    job backup-1 backs up every VM: exclude 9000-9099 there

Ready, with 3 warning(s).
",
        env = f.paths.env_dir,
        slots = f.paths.slots_conf,
        conf = f.paths.conf_dir,
    );
    assert_eq!(out, want);
    assert_eq!(code, 0);

    let seen = gh.seen.lock().unwrap();
    let calls: Vec<String> = seen
        .iter()
        .map(|s| format!("{} {}", s.method, s.path))
        .collect();
    assert_eq!(
        calls,
        [
            "GET /app",
            "GET /orgs/acme/installation",
            "POST /app/installations/42/access_tokens",
            "GET /orgs/acme/actions/runners?per_page=1"
        ]
    );
    let bearer = |i: usize| {
        seen[i].headers["authorization"]
            .strip_prefix("Bearer ")
            .unwrap()
            .to_string()
    };
    let jwt = bearer(0);
    assert_eq!(bearer(1), jwt);
    assert_eq!(bearer(2), jwt);
    assert_eq!(bearer(3), "ghs_faketoken");
    assert_eq!(seen[0].headers["accept"], "application/vnd.github+json");
    assert_eq!(seen[0].headers["x-github-api-version"], "2022-11-28");

    let claims = verify(&jwt, &public);
    assert_eq!(claims["iss"], 12345);
    let (iat, exp) = (
        claims["iat"].as_u64().unwrap(),
        claims["exp"].as_u64().unwrap(),
    );
    assert_eq!(exp - iat, 600);
    assert!(
        iat + 60 >= before && iat + 60 <= after,
        "iat {iat} not 60 s before now"
    );
}

#[test]
fn jwt_iss_is_a_number_or_a_string_and_pkcs8_keys_work() {
    let dir = tmp("jwt");
    let (key1, public1) = make_key(&dir, false);
    let (key8, public8) = make_key(&dir, true);
    let jwt = mayfly::github::make_jwt("987", &fs::read(&key1).unwrap(), 1_000_000).unwrap();
    let mut v = Validation::new(Algorithm::RS256);
    v.validate_aud = false;
    v.validate_exp = false;
    let claims = decode::<Value>(&jwt, &DecodingKey::from_rsa_pem(&public1).unwrap(), &v)
        .unwrap()
        .claims;
    assert_eq!(claims["iss"], 987);
    assert_eq!(claims["iat"], 1_000_000 - 60);
    assert_eq!(claims["exp"], 1_000_000 + 540);
    let jwt = mayfly::github::make_jwt("Iv1.abc123", &fs::read(&key8).unwrap(), 1_000_000).unwrap();
    let claims = decode::<Value>(&jwt, &DecodingKey::from_rsa_pem(&public8).unwrap(), &v)
        .unwrap()
        .claims;
    assert_eq!(claims["iss"], "Iv1.abc123");
    assert!(mayfly::github::make_jwt("1", b"not a key", 1).is_err());
}

#[test]
fn check_refused_key_stops_with_a_message() {
    let dir = tmp("key2");
    let (key, _) = make_key(&dir, false);
    let gh = fake_github(401);
    let (f, pve) = check_fixture(gh.port, &key);
    let e = f.run_check(&pve).err().unwrap();
    assert_eq!(
        e,
        format!(
            "GitHub refused the App key; check APP_ID and {}",
            key.display()
        )
    );
}

#[test]
fn check_needs_app_id_and_key() {
    let f = Fixture::new("noapp", "ORG=acme\n", "");
    assert_eq!(
        f.run_check(&base_pve()).err().unwrap(),
        format!("APP_ID is empty in {}", f.paths.pool_env)
    );
    write(&f.paths.pool_env, "ORG=acme\nAPP_ID=1\n");
    assert_eq!(
        f.run_check(&base_pve()).err().unwrap(),
        format!(
            "{}/app.pem missing: put the GitHub App's private key there",
            f.paths.conf_dir
        )
    );
}

#[test]
fn check_reports_problems_and_exits_1() {
    let dir = tmp("key3");
    let (key, _) = make_key(&dir, false);
    let gh = fake_github(200);
    let (f, _) = check_fixture(gh.port, &key);
    write(
        &f.paths.slots_conf,
        "l1 9002 x 1 1 1 no current kind=lxc template=9055\nk1 9003 x 1 1 1 no current kind=zzz\n",
    );
    write(
        format!("{}/.vmlist", f.paths.pve_etc),
        "{\"ids\":{\"9002\":{\"node\":\"pve2\",\"type\":\"qemu\"},\"9003\":{\"node\":\"pve1\"}}}",
    );
    write(
        &f.paths.pool_env,
        &format!(
            "APP_ID=12345\nKEY_PATH={}\nAPI_URL=http://127.0.0.1:{}\nORG=acme\nFIREWALL=no\nPROFILE=nope\nBRIDGE=vmbr9\nSTORAGE=gone\n",
            key.display(),
            gh.port
        ),
    );
    let pve = base_pve();
    let (code, out) = f.run_check(&pve).unwrap();
    let want = format!(
        "\
ok   GitHub    App my-app, 3 runners in acme
bad  storage   gone not found (STORAGE in {pool})
bad  network   bridge vmbr9 not found (BRIDGE in {pool})
ok   firewall  off (FIREWALL=no): isolate job VMs by VLAN or on the host
warn template  none yet: mayfly template build, then mayfly template promote <vmid>
bad  profile   nope not found in {conf}/profiles or {share}/profiles
bad  l1        VMID 9002 is taken on node pve2
bad  l1        kind=lxc, but lxc-attach is missing
bad  l1        template=9055 is not a template
bad  k1        unknown kind=zzz (vm, pod or lxc)
ok   slots     2 in {slots}
ok   hooks     1 in {conf}/hooks.d

7 problem(s) to fix.
",
        pool = f.paths.pool_env,
        conf = f.paths.conf_dir,
        share = f.paths.share,
        slots = f.paths.slots_conf,
    );
    assert_eq!(out, want);
    assert_eq!(code, 1);
}

#[test]
fn check_caps_pool_env_warnings_at_five() {
    let dir = tmp("key4");
    let (key, _) = make_key(&dir, false);
    let gh = fake_github(200);
    let (f, pve) = check_fixture(gh.port, &key);
    let mut env = fs::read_to_string(&f.paths.pool_env).unwrap();
    for i in 0..6 {
        env.push_str(&format!("K{i}=$x\n"));
    }
    write(&f.paths.pool_env, &env);
    let (_, out) = f.run_check(&pve).unwrap();
    let lines: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("warn pool.env"))
        .collect();
    assert_eq!(lines.len(), 6, "{out}");
    assert!(lines[0].contains("line 6:"));
    assert!(lines[4].contains("line 10:"));
    assert_eq!(
        lines[5],
        "warn pool.env  and 2 more lines that are not plain KEY=value"
    );
}

// ------------------------------------------------------------------ the binary

fn mayfly(share: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mayfly"));
    c.env("MAYFLY_SHARE", share)
        .env("MAYFLY_CONF_DIR", share.join("none"))
        .env_remove("NO_COLOR");
    c
}

/// Runs the command; a script that was written a moment ago can be "Text file busy" while another test
/// forks, so that one is tried again.
fn run_retry(c: &mut Command) -> std::process::Output {
    for _ in 0..20 {
        let out = c.output().unwrap();
        if !String::from_utf8_lossy(&out.stderr).contains("Text file busy") {
            return out;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    c.output().unwrap()
}

fn script(share: &Path, body: &str) {
    let p = share.join("mayfly.sh");
    write(&p, body);
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn version_and_help_need_no_script() {
    let share = tmp("noscript");
    let out = mayfly(&share).arg("version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("mayfly {}\n", env!("CARGO_PKG_VERSION"))
    );
    for flag in ["--version", "-v"] {
        assert!(mayfly(&share).arg(flag).output().unwrap().status.success());
    }
    let out = mayfly(&share).arg("help").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success());
    assert!(text.starts_with(&format!("mayfly {}: throwaway", env!("CARGO_PKG_VERSION"))));
    assert!(text.contains(&format!(
        "Config in {}/none: pool.env, slots.conf.",
        share.display()
    )));
    assert!(!text.contains('\x1b'));
}

#[test]
fn other_commands_are_forwarded_with_args_env_and_exit_code() {
    let share = tmp("forward");
    script(
        &share,
        "#!/bin/sh\necho \"args=$#:$*\"\necho \"env=$MAYFLY_TEST_VAR\"\necho oops >&2\nexit 7\n",
    );
    for args in [vec!["slot", "build-1"], vec!["frobnicate", "--now"], vec![]] {
        let out = run_retry(mayfly(&share).args(&args).env("MAYFLY_TEST_VAR", "kept"));
        assert_eq!(out.status.code(), Some(7), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("args={}:{}\nenv=kept\n", args.len(), args.join(" ")),
            "{args:?}"
        );
        assert_eq!(String::from_utf8_lossy(&out.stderr), "oops\n");
    }
}

#[test]
fn a_missing_script_is_a_clear_error() {
    let share = tmp("missing");
    let out = mayfly(&share).args(["slot", "a-1"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(&format!(
            "{}/mayfly.sh is missing; run install.sh again",
            share.display()
        )),
        "{err}"
    );
}

// ------------------------------------------------------------------ GitHub column, polara-ci templates

#[test]
fn status_github_column() {
    let dir = tmp("key5");
    let (key, _) = make_key(&dir, false);
    let gh = fake_github(200);
    let (f, pve) = status_fixture(&format!(
        "APP_ID=12345\nKEY_PATH={}\nAPI_URL=http://127.0.0.1:{}\n",
        key.display(),
        gh.port
    ));
    let pve = pve
        .answer("pct status 9051", "status: running\n")
        .answer(
            "qm config 9002",
            "name: ci-build-1\ndescription: runner ci-pve1-build-1-1700000000, template 9098\n",
        )
        .answer(
            "pct config 9051",
            "hostname: ci-ct-1\ndescription: runner ci-pve1-ct-1-1700000001, template 9096\n",
        );
    let (_, out) = f.run_status(&pve);
    let want = "\
SLOT       VMID   KIND LABELS                 SIZE    SERVICE   GUEST    JOB    GITHUB
build-1    9002   vm   linux                  8c/16G  active    running  busy   busy
pods-1     9041   pod  small                  12c/36G activating running  6 containers 2/3
ct-1       9051   lxc  small                  2c/4G   failed    running  idle   offline
off-1      9052   vm   small                  1c/1G   inactive  none     -      -

warning: 1 running guest(s) have no online runner on GitHub; a job VM that boots but never registers points at the template (mayfly check)
storage    local-lvm 42% used, limit 70%
ram        32 GB free, 8 GB kept for the host
template   9098, next 9099, profile default
update     {version}, no update run yet
";
    assert_eq!(out, want.replace("{version}", env!("CARGO_PKG_VERSION")));
    let seen = gh.seen.lock().unwrap();
    let paths: Vec<&str> = seen.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/orgs/acme/installation",
            "/app/installations/42/access_tokens",
            "/orgs/acme/actions/runners?per_page=100&page=1",
            "/orgs/acme/actions/runners?per_page=100&page=2"
        ]
    );
}

#[test]
fn status_colours_the_github_column() {
    // paint is what colours it; the values are the ones bash uses.
    use mayfly::out::Colour;
    let tty = Out { tty: true };
    let map = [
        ("online", Colour::Grn),
        ("busy", Colour::Blu),
        ("offline", Colour::Red),
        ("none", Colour::Red),
    ];
    assert_eq!(tty.paint("offline", 0, &map), "\x1b[31moffline\x1b[0m");
    assert_eq!(tty.paint("2/3", 0, &map), "2/3");
}

#[test]
fn check_warns_about_polara_ci_templates() {
    let dir = tmp("key6");
    let (key, _) = make_key(&dir, false);
    let gh = fake_github(200);
    let (f, pve) = check_fixture(gh.port, &key);
    let mut env = fs::read_to_string(&f.paths.pool_env).unwrap();
    env.push_str("NEXT_TEMPLATE_VMID=9099\n");
    write(&f.paths.pool_env, &env);
    f.guest("qemu-server", "9099");
    // 9098 has no "description: profile", 9099 has one; a container template is never flagged.
    let pve = pve
        .answer("qm config 9098", "name: ci-template\ntemplate: 1\n")
        .answer(
            "qm config 9099",
            "name: ci-next\ndescription: profile default\ntemplate: 1\n",
        );
    let (_, out) = f.run_check(&pve).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    let at = lines
        .iter()
        .position(|l| *l == "ok   template  9098")
        .unwrap();
    assert_eq!(
        lines[at + 1],
        "warn template  9098 was built by polara-ci; jobs get its /etc/polara-ci paths. Rebuild: mayfly template build"
    );
    assert_eq!(lines[at + 2], "ok   profile   default");
    // Both templates built here: no warning.
    let pve = pve.answer(
        "qm config 9098",
        "name: ci-template\ndescription: profile default\ntemplate: 1\n",
    );
    let (_, out) = f.run_check(&pve).unwrap();
    assert!(!out.contains("polara-ci"), "{out}");
}

// ------------------------------------------------------------------ mayfly update

/// A tiny HTTP server: `routes` maps a path to its body, anything else is a 404. Returns the port and
/// the paths asked for.
fn fake_release_server(routes: HashMap<String, Vec<u8>>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).unwrap_or(0) == 0 {
                continue;
            }
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                    break;
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
            let (code, body) = match routes.get(&path) {
                Some(b) => (200, b.clone()),
                None => (404, b"{}".to_vec()),
            };
            log.lock().unwrap().push(path);
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        }
    });
    (port, seen)
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A stand-in for the `mayfly` command: prints its version, passes or fails `check`, and `install --from <dir>`
/// copies the release files where MAYFLY_SBIN, MAYFLY_SHARE and MAYFLY_SYSTEMD_DIR say.
fn fake_mayfly(version: &str, check_exit: i32) -> String {
    format!(
        "#!/bin/sh\nset -e\ncase \"$1\" in\n  version) echo \"mayfly {version}\" ;;\n  check) echo \"check says {check_exit}\"; exit {check_exit} ;;\n  install)\n    here=$3\n    cp \"$here/mayfly\" \"$MAYFLY_SBIN/mayfly\"\n    cp \"$here/mayfly.sh\" \"$MAYFLY_SHARE/mayfly.sh\"\n    cp \"$here/mayfly-update.service\" \"$here/mayfly-update.timer\" \"$MAYFLY_SYSTEMD_DIR/\" ;;\nesac\n"
    )
}

/// The release tarball of version `v`: a fake command (see `fake_mayfly`), mayfly.sh and the update units.
fn release_tarball(v: &str, mayfly: &str) -> Vec<u8> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut b = tar::Builder::new(enc);
    let files: [(&str, &str, u32); 4] = [
        ("mayfly", mayfly, 0o755),
        ("mayfly.sh", "new mayfly.sh\n", 0o755),
        ("mayfly-update.service", "new update service\n", 0o644),
        ("mayfly-update.timer", "new update timer\n", 0o644),
    ];
    for (name, body, mode) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(mode);
        h.set_mtime(0);
        b.append_data(&mut h, format!("mayfly-{v}/{name}"), body.as_bytes())
            .unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

/// The JSON of the release list, newest first, with the installed version's release at the end.
fn release_list(newer: &[(&str, &str)]) -> String {
    let mut items: Vec<serde_json::Value> = newer
        .iter()
        .map(|(v, body)| {
            serde_json::json!({
                "tag_name": format!("v{v}"),
                "body": body,
                "html_url": format!("https://example.test/releases/tag/v{v}"),
                "draft": false,
                "prerelease": false
            })
        })
        .collect();
    items.push(serde_json::json!({
        "tag_name": format!("v{}", env!("CARGO_PKG_VERSION")),
        "body": "### Bug Fixes\n",
        "draft": false,
        "prerelease": false
    }));
    serde_json::Value::Array(items).to_string()
}

const BREAKING_BODY: &str = "### ⚠ BREAKING CHANGES\n\n* the config moved\n";

struct Release {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Release {
    /// A server whose latest release is `v`; `sums` overrides the checksum line (to make it wrong).
    fn serve(v: &str, mayfly: &str, sums: Option<&str>) -> Release {
        let list = release_list(&[(v, "### Bug Fixes\n\n* a fix\n")]);
        Release::serve_with(v, mayfly, sums, Some(list))
    }

    /// Like `serve`, with the body of `/releases?per_page=100` (None: that route is a 404).
    fn serve_with(v: &str, mayfly: &str, sums: Option<&str>, list: Option<String>) -> Release {
        let tgz = release_tarball(v, mayfly);
        let name = format!("mayfly-{v}.tar.gz");
        let sums = match sums {
            Some(s) => s.to_string(),
            None => sha256_hex(&tgz),
        };
        let mut routes = HashMap::new();
        let tag = format!("{{\"tag_name\":\"v{v}\"}}").into_bytes();
        routes.insert("/api/releases/latest".to_string(), tag.clone());
        routes.insert(format!("/api/releases/tags/v{v}"), tag);
        routes.insert(
            format!("/repo/releases/download/v{v}/SHA256SUMS"),
            format!("{sums}  {name}\n").into_bytes(),
        );
        routes.insert(format!("/repo/releases/download/v{v}/{name}"), tgz);
        if let Some(list) = list {
            routes.insert("/api/releases?per_page=100".to_string(), list.into_bytes());
        }
        let (port, seen) = fake_release_server(routes);
        Release { port, seen }
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn downloads(&self) -> usize {
        self.requests()
            .iter()
            .filter(|p| p.contains("/download/"))
            .count()
    }
}

/// An installed Mayfly in a temporary directory, with a fake `systemctl` that logs its calls.
struct Host {
    root: PathBuf,
}

impl Host {
    fn new(name: &str, old_check_exit: i32, pool_env: &str) -> Host {
        let root = tmp(name);
        let h = Host { root };
        write(
            h.sbin().join("mayfly"),
            &fake_mayfly("0.0.1", old_check_exit),
        );
        fs::set_permissions(h.sbin().join("mayfly"), fs::Permissions::from_mode(0o755)).unwrap();
        write(h.share().join("mayfly.sh"), "old mayfly.sh\n");
        write(
            h.share().join("profiles/default/provision.sh"),
            "old profile\n",
        );
        write(h.units().join("mayfly-gc.timer"), "old gc timer\n");
        write(h.units().join("mayfly-slot@.service"), "old slot service\n");
        write(h.root.join("conf/pool.env"), pool_env);
        let systemctl = h.root.join("bin/systemctl");
        write(
            &systemctl,
            &format!(
                "#!/bin/sh\necho \"$*\" >> {}/systemctl.log\n",
                h.root.display()
            ),
        );
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(h.tmp()).unwrap();
        h
    }

    fn sbin(&self) -> PathBuf {
        self.root.join("sbin")
    }
    fn share(&self) -> PathBuf {
        self.root.join("share")
    }
    fn units(&self) -> PathBuf {
        self.root.join("units")
    }
    fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }
    fn state(&self) -> String {
        fs::read_to_string(self.root.join("state/update.state")).unwrap_or_default()
    }
    fn systemctl_log(&self) -> String {
        fs::read_to_string(self.root.join("systemctl.log")).unwrap_or_default()
    }
    fn read(&self, p: PathBuf) -> String {
        fs::read_to_string(p).unwrap_or_else(|_| "<missing>".to_string())
    }

    /// `mayfly update <args>` against `port` (0: no server).
    fn update(&self, port: u16, args: &[&str]) -> std::process::Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mayfly"));
        c.arg("update")
            .args(args)
            .env("MAYFLY_SHARE", self.share())
            .env("MAYFLY_CONF_DIR", self.root.join("conf"))
            .env("MAYFLY_SBIN", self.sbin())
            .env("MAYFLY_SYSTEMD_DIR", self.units())
            .env("MAYFLY_STATE_DIR", self.root.join("state"))
            .env("MAYFLY_TMP", self.tmp())
            .env("MAYFLY_RELEASE_API", format!("http://127.0.0.1:{port}/api"))
            .env("MAYFLY_REPO_URL", format!("http://127.0.0.1:{port}/repo"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env_remove("NO_COLOR");
        run_retry(&mut c)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(o: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).to_string(),
        String::from_utf8_lossy(&o.stderr).to_string(),
    )
}

fn no_temp_left(h: &Host) {
    let left: Vec<_> = fs::read_dir(h.tmp()).unwrap().collect();
    assert!(left.is_empty(), "temporary files left: {left:?}");
}

/// The current version with the patch raised by one.
fn next_patch() -> String {
    let mut p: Vec<u64> = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|n| n.parse().unwrap())
        .collect();
    p[2] += 1;
    format!("{}.{}.{}", p[0], p[1], p[2])
}

#[test]
fn update_says_when_there_is_nothing_newer() {
    let h = Host::new("up-to-date", 0, "ORG=acme\n");
    let rel = Release::serve(env!("CARGO_PKG_VERSION"), &fake_mayfly("9", 0), None);
    let o = h.update(rel.port, &[]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(0));
    assert!(out.contains("is up to date"), "{out}");
    assert_eq!(rel.downloads(), 0);
    assert!(h.state().contains("last_result=up-to-date"));
    assert!(!h.share().join("previous").exists());
}

#[test]
fn update_check_only_reports() {
    let h = Host::new("check-only", 0, "ORG=acme\n");
    let rel = Release::serve("9.9.9", &fake_mayfly("9.9.9", 0), None);
    let o = h.update(rel.port, &["--check"]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(0));
    assert!(out.contains("mayfly 9.9.9 is available"), "{out}");
    assert_eq!(rel.downloads(), 0);
    assert_eq!(h.state(), "");
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("0.0.1", 0));
}

#[test]
fn update_installs_the_release_and_keeps_the_old_files() {
    let h = Host::new("update-ok", 0, "ORG=acme\n");
    let rel = Release::serve("9.9.9", &fake_mayfly("9.9.9", 0), None);
    let o = h.update(rel.port, &[]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(out.contains("mayfly 9.9.9 installed"), "{out}");
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("9.9.9", 0));
    assert_eq!(h.read(h.share().join("mayfly.sh")), "new mayfly.sh\n");
    assert_eq!(
        h.read(h.units().join("mayfly-update.timer")),
        "new update timer\n"
    );
    // The old files are in previous/.
    let prev = h.share().join("previous");
    assert_eq!(
        h.read(prev.join("VERSION")),
        format!("{}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(h.read(prev.join("sbin/mayfly")), fake_mayfly("0.0.1", 0));
    assert_eq!(h.read(prev.join("share/mayfly.sh")), "old mayfly.sh\n");
    assert_eq!(
        h.read(prev.join("share/profiles/default/provision.sh")),
        "old profile\n"
    );
    assert_eq!(
        h.read(prev.join("systemd/mayfly-gc.timer")),
        "old gc timer\n"
    );
    assert!(!prev.join("systemd/mayfly-update.timer").exists());
    assert!(!prev.join("share/previous").exists());
    let state = h.state();
    assert!(
        state.contains("last_result=ok\n") && !state.contains("skipped"),
        "{state}"
    );
    no_temp_left(&h);
}

#[test]
fn a_wrong_checksum_stops_before_anything_changes() {
    let h = Host::new("bad-sum", 0, "ORG=acme\n");
    let rel = Release::serve("9.9.9", &fake_mayfly("9.9.9", 0), Some(&"0".repeat(64)));
    let o = h.update(rel.port, &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains("checksum mismatch for mayfly-9.9.9.tar.gz"),
        "{err}"
    );
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("0.0.1", 0));
    assert_eq!(h.read(h.share().join("mayfly.sh")), "old mayfly.sh\n");
    assert!(!h.share().join("previous").exists());
    assert!(!h.units().join("mayfly-update.timer").exists());
    assert!(h.state().contains("last_result=failed"));
    no_temp_left(&h);
}

#[test]
fn a_new_command_with_the_wrong_version_is_not_installed() {
    let h = Host::new("bad-version", 0, "ORG=acme\n");
    let rel = Release::serve("9.9.9", &fake_mayfly("1.0.0", 0), None);
    let o = h.update(rel.port, &[]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains("prints \"mayfly 1.0.0\", not \"mayfly 9.9.9\""),
        "{err}"
    );
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("0.0.1", 0));
    assert!(!h.share().join("previous").exists());
    no_temp_left(&h);
}

#[test]
fn a_failing_new_check_rolls_back_and_skips_the_version() {
    let h = Host::new("rollback-check", 0, "ORG=acme\n");
    let rel = Release::serve("9.9.9", &fake_mayfly("9.9.9", 1), None);
    let o = h.update(rel.port, &[]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}{err}");
    assert!(err.contains("mayfly 9.9.9 failed mayfly check"), "{err}");
    assert!(err.contains("rolled back to"), "{err}");
    assert!(out.contains("check says 1"), "{out}");
    // Everything is as before; units the old version did not have are gone.
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("0.0.1", 0));
    assert_eq!(h.read(h.share().join("mayfly.sh")), "old mayfly.sh\n");
    assert_eq!(
        h.read(h.share().join("profiles/default/provision.sh")),
        "old profile\n"
    );
    assert_eq!(h.read(h.units().join("mayfly-gc.timer")), "old gc timer\n");
    assert!(!h.units().join("mayfly-update.timer").exists());
    assert!(!h.units().join("mayfly-update.service").exists());
    assert!(h.systemctl_log().contains("daemon-reload"));
    let state = h.state();
    assert!(state.contains("last_result=rolled-back\n"), "{state}");
    assert!(state.contains("skipped=9.9.9\n"), "{state}");
    no_temp_left(&h);
    let downloads = rel.downloads();
    assert_eq!(downloads, 2);

    // The timer and a plain update leave the skipped version alone ...
    for args in [&["--auto"][..], &[][..]] {
        let o = h.update(rel.port, args);
        let (out, _) = text(&o);
        assert_eq!(o.status.code(), Some(0), "{args:?}");
        assert!(out.contains("9.9.9 is skipped"), "{out}");
        assert_eq!(rel.downloads(), downloads);
    }
    // ... and --version forces it.
    let o = h.update(rel.port, &["--version", "9.9.9"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(rel.downloads(), downloads + 2);
}

#[test]
fn the_timer_logs_plain_lines_and_skips_a_skipped_version() {
    let h = Host::new("auto-skip", 0, "ORG=acme\n");
    write(
        h.root.join("state/update.state"),
        "last_attempt=2026-11-10T04:12:09Z\nlast_result=rolled-back\nskipped=9.9.9\n",
    );
    let rel = Release::serve("9.9.9", &fake_mayfly("9.9.9", 0), None);
    let o = h.update(rel.port, &["--auto"]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(0));
    assert!(out.starts_with("update: mayfly 9.9.9 is skipped"), "{out}");
    assert_eq!(rel.downloads(), 0);
    // A newer release than the skipped one is tried.
    let rel = Release::serve("9.9.10", &fake_mayfly("9.9.10", 0), None);
    let o = h.update(rel.port, &["--auto"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(out.contains("update: mayfly 9.9.10 installed"), "{out}");
    assert!(!h.state().contains("skipped"));
}

#[test]
fn auto_skips_a_breaking_target() {
    let h = Host::new("auto-breaking", 0, "ORG=acme\n");
    let list = release_list(&[("9.9.9", BREAKING_BODY)]);
    let rel = Release::serve_with("9.9.9", &fake_mayfly("9.9.9", 0), None, Some(list));
    let o = h.update(rel.port, &["--auto"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(
        out.contains(
            "update: mayfly 9.9.9 needs a manual mayfly update (breaking: see release notes)\n"
        ),
        "{out}"
    );
    assert!(
        out.contains("https://example.test/releases/tag/v9.9.9"),
        "{out}"
    );
    assert_eq!(rel.downloads(), 0);
    assert!(h.state().contains("last_result=breaking"), "{}", h.state());
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("0.0.1", 0));
    assert!(!h.share().join("previous").exists());
    no_temp_left(&h);
}

#[test]
fn auto_skips_when_a_release_in_between_is_breaking() {
    let h = Host::new("auto-between", 0, "ORG=acme\n");
    let list = release_list(&[
        ("9.9.9", "### Bug Fixes\n"),
        ("9.9.8", BREAKING_BODY),
        ("9.9.7", "### Features\n"),
    ]);
    let rel = Release::serve_with("9.9.9", &fake_mayfly("9.9.9", 0), None, Some(list));
    let o = h.update(rel.port, &["--auto"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(
        out.contains(
            "update: mayfly 9.9.9 needs a manual mayfly update (breaking: see release notes)"
        ),
        "{out}"
    );
    assert!(out.contains("v9.9.8"), "{out}");
    assert_eq!(rel.downloads(), 0);
    assert!(h.state().contains("last_result=breaking"));
    // The state names the version that waits (`mayfly status` prints it).
    let state = h.state();
    assert!(state.contains("breaking=9.9.9"), "{state}");
}

#[test]
fn auto_ignores_breaking_drafts_and_older_releases() {
    let h = Host::new("auto-ignored", 0, "ORG=acme\n");
    let mut items: Vec<serde_json::Value> = serde_json::from_str(&release_list(&[
        ("9.9.9", "### Bug Fixes\n"),
        ("9.9.8", BREAKING_BODY),
    ]))
    .unwrap();
    items[1]["draft"] = serde_json::json!(true);
    // Breaking, but older than what is installed: not between the two versions.
    items.push(serde_json::json!({
        "tag_name": "v0.0.1", "body": BREAKING_BODY, "draft": false, "prerelease": false
    }));
    let list = serde_json::Value::Array(items).to_string();
    let rel = Release::serve_with("9.9.9", &fake_mayfly("9.9.9", 0), None, Some(list));
    let o = h.update(rel.port, &["--auto"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(out.contains("update: mayfly 9.9.9 installed"), "{out}");
}

#[test]
fn plain_update_takes_a_breaking_release() {
    let h = Host::new("plain-breaking", 0, "ORG=acme\n");
    let list = release_list(&[("9.9.9", BREAKING_BODY)]);
    let rel = Release::serve_with("9.9.9", &fake_mayfly("9.9.9", 0), None, Some(list));
    let o = h.update(rel.port, &[]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(out.contains("mayfly 9.9.9 installed"), "{out}");
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("9.9.9", 0));
    assert!(h.state().contains("last_result=ok"));
    assert!(!h.state().contains("breaking"));

    let h = Host::new("version-breaking", 0, "ORG=acme\n");
    let o = h.update(rel.port, &["--version", "9.9.9"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o).1);
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("9.9.9", 0));
}

#[test]
fn check_mentions_a_breaking_release() {
    let h = Host::new("check-breaking", 0, "ORG=acme\n");
    let list = release_list(&[("9.9.9", BREAKING_BODY)]);
    let rel = Release::serve_with("9.9.9", &fake_mayfly("9.9.9", 0), None, Some(list));
    let o = h.update(rel.port, &["--check"]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        out.contains(
            "mayfly 9.9.9 is available (installed {}); mayfly update installs it (breaking: run mayfly update by hand)\n"
                .replace("{}", env!("CARGO_PKG_VERSION"))
                .as_str()
        ),
        "{out}"
    );
    assert_eq!(rel.downloads(), 0);
    assert_eq!(h.state(), "");
}

#[test]
fn auto_skips_when_the_release_list_cannot_be_read() {
    let h = Host::new("auto-no-list", 0, "ORG=acme\n");
    let rel = Release::serve_with("9.9.9", &fake_mayfly("9.9.9", 0), None, None);
    let o = h.update(rel.port, &["--auto"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(
        out.contains("update: cannot tell whether mayfly 9.9.9 is breaking"),
        "{out}"
    );
    assert_eq!(rel.downloads(), 0);
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("0.0.1", 0));
    assert!(!h.share().join("previous").exists());
    // By hand it does not need the list.
    let o = h.update(rel.port, &[]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("9.9.9", 0));
}

#[test]
fn update_patch_takes_patches_only() {
    let h = Host::new("patch-only", 0, "ORG=acme\nUPDATE=patch\n");
    let rel = Release::serve("9.9.9", &fake_mayfly("9.9.9", 0), None);
    let o = h.update(rel.port, &["--auto"]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(0));
    let minor: Vec<&str> = env!("CARGO_PKG_VERSION").split('.').take(2).collect();
    assert!(
        out.contains(&format!("UPDATE=patch only takes {}.x", minor.join("."))),
        "{out}"
    );
    assert_eq!(rel.downloads(), 0);
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("0.0.1", 0));
    // By hand UPDATE does not apply.
    let o = h.update(rel.port, &[]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("9.9.9", 0));

    let h = Host::new("patch-ok", 0, "ORG=acme\nUPDATE=\"patch\"\n");
    let v = next_patch();
    let rel = Release::serve(&v, &fake_mayfly(&v, 0), None);
    let o = h.update(rel.port, &["--auto"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly(&v, 0));
}

#[test]
fn update_off_does_nothing_and_asks_nobody() {
    let h = Host::new("off", 0, "ORG=acme\nUPDATE=off\n");
    let rel = Release::serve("9.9.9", &fake_mayfly("9.9.9", 0), None);
    let o = h.update(rel.port, &["--auto"]);
    let (out, _) = text(&o);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(out, "update: UPDATE=off in pool.env, nothing to do\n");
    assert!(rel.requests().is_empty());
    assert_eq!(h.state(), "");

    let h = Host::new("bad-mode", 0, "ORG=acme\nUPDATE=sometimes\n");
    let o = h.update(rel.port, &["--auto"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o)
        .1
        .contains("UPDATE=sometimes: use auto, patch or off"));
}

#[test]
fn a_failing_check_before_the_update_does_not_roll_back() {
    let h = Host::new("old-check-fails", 1, "ORG=acme\n");
    let rel = Release::serve("9.9.9", &fake_mayfly("9.9.9", 1), None);
    let o = h.update(rel.port, &[]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(out.contains("it failed with"), "{out}");
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("9.9.9", 1));
}

#[test]
fn rollback_restores_the_saved_files() {
    let h = Host::new("manual-rollback", 0, "ORG=acme\n");
    let o = h.update(0, &["--rollback"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).1.contains("no previous version saved"));

    let rel = Release::serve("9.9.9", &fake_mayfly("9.9.9", 0), None);
    assert_eq!(h.update(rel.port, &[]).status.code(), Some(0));
    assert_eq!(h.read(h.share().join("mayfly.sh")), "new mayfly.sh\n");
    let reloads = h.systemctl_log().matches("daemon-reload").count();

    let o = h.update(0, &["--rollback"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(
        out.contains(&format!("restored mayfly {}", env!("CARGO_PKG_VERSION"))),
        "{out}"
    );
    assert_eq!(h.read(h.sbin().join("mayfly")), fake_mayfly("0.0.1", 0));
    assert_eq!(h.read(h.share().join("mayfly.sh")), "old mayfly.sh\n");
    assert_eq!(
        h.read(h.share().join("profiles/default/provision.sh")),
        "old profile\n"
    );
    assert_eq!(h.read(h.units().join("mayfly-gc.timer")), "old gc timer\n");
    assert!(!h.units().join("mayfly-update.timer").exists());
    assert_eq!(
        h.systemctl_log().matches("daemon-reload").count(),
        reloads + 1
    );
    assert!(h.state().contains("last_result=rolled-back"));
}

#[test]
fn update_refuses_bad_arguments() {
    let h = Host::new("args", 0, "ORG=acme\n");
    for args in [
        vec!["--version", "1.2"],
        vec!["--rollback", "--check"],
        vec!["--frobnicate"],
    ] {
        let o = h.update(0, &args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
    }
    // An unreachable release server is a plain error, not a panic.
    let o = h.update(1, &["--check"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).1.contains("cannot look up the release"));
}

// ------------------------------------------------------------------ install

const UNIT_FILES: [&str; 5] = [
    "mayfly-slot@.service",
    "mayfly-gc.service",
    "mayfly-gc.timer",
    "mayfly-update.service",
    "mayfly-update.timer",
];
const TEMPLATE_FILES: [&str; 5] = [
    "provision.sh",
    "ci-job.sh",
    "pod-vm.sh",
    "mayfly-job.service",
    "template.env",
];

/// A release directory and a Proxmox host in a temporary directory: stubs for `id`, `qm`, `apt-get`,
/// `pvesh`, `pvesm` and `systemctl` on PATH log their calls in `calls.log`; the snippet storage's content
/// types are in `store` (`pvesm set` changes them).
struct Install {
    root: PathBuf,
}

impl Install {
    fn new(name: &str) -> Install {
        let i = Install { root: tmp(name) };
        let src = i.src();
        write(src.join("mayfly.sh"), "new mayfly.sh\n");
        fs::set_permissions(src.join("mayfly.sh"), fs::Permissions::from_mode(0o700)).unwrap();
        for f in TEMPLATE_FILES {
            write(src.join("template").join(f), &format!("template {f}\n"));
        }
        write(
            src.join("profiles/default/provision.sh"),
            "profile provision\n",
        );
        fs::set_permissions(
            src.join("profiles/default/provision.sh"),
            fs::Permissions::from_mode(0o750),
        )
        .unwrap();
        write(src.join("profiles/default/template.env"), "profile env\n");
        fs::set_permissions(
            src.join("profiles/default/template.env"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        for u in UNIT_FILES {
            write(src.join(u), &format!("unit {u}\n"));
        }
        write(src.join("pool.env.example"), "APP_ID=\nORG=\n");
        write(src.join("slots.conf.example"), "# slots\n");
        let stub = |name: &str, body: &str| {
            let p = i.root.join("bin").join(name);
            write(&p, &format!("#!/bin/sh\n{body}\n"));
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        };
        stub("id", "echo 0");
        stub("qm", "exit 0");
        stub("apt-get", "echo \"apt-get $*\" >>\"$CALLS\"");
        stub(
            "pvesh",
            "echo \"pvesh $*\" >>\"$CALLS\"\nprintf '{\"content\":\"%s\"}\\n' \"$(cat \"$STORE\")\"",
        );
        stub(
            "pvesm",
            "echo \"pvesm $*\" >>\"$CALLS\"\n[ \"$1\" != set ] || printf %s \"$4\" >\"$STORE\"",
        );
        stub("systemctl", "echo \"systemctl $*\" >>\"$CALLS\"");
        i.store("iso,vztmpl");
        i
    }

    fn src(&self) -> PathBuf {
        self.root.join("src")
    }
    fn conf(&self) -> PathBuf {
        self.root.join("conf")
    }
    fn share(&self) -> PathBuf {
        self.root.join("share")
    }
    fn sbin(&self) -> PathBuf {
        self.root.join("sbin")
    }
    fn units(&self) -> PathBuf {
        self.root.join("units")
    }
    fn store(&self, content: &str) {
        write(self.root.join("store"), content);
    }
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
    fn clear_calls(&self) {
        let _ = fs::remove_file(self.root.join("calls.log"));
    }
    fn read(&self, p: PathBuf) -> String {
        fs::read_to_string(p).unwrap_or_else(|_| "<missing>".to_string())
    }
    fn mode(&self, p: PathBuf) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    /// `mayfly install <args>` from the release directory.
    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut c = self.command(args);
        c.arg("--from").arg(self.src());
        run_retry(&mut c)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mayfly"));
        c.arg("install")
            .args(args)
            .env("MAYFLY_SHARE", self.share())
            .env("MAYFLY_CONF_DIR", self.conf())
            .env("MAYFLY_SBIN", self.sbin())
            .env("MAYFLY_SYSTEMD_DIR", self.units())
            .env("CALLS", self.root.join("calls.log"))
            .env("STORE", self.root.join("store"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env_remove("NO_COLOR")
            .env_remove("MAYFLY_REPO_URL")
            .stdin(std::process::Stdio::null());
        c
    }

    /// Names in a directory, sorted; nothing is left over from the atomic writes.
    fn names(&self, dir: PathBuf) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const V: &str = env!("CARGO_PKG_VERSION");

#[test]
fn the_first_install_creates_the_files_and_enables_the_timers() {
    let i = Install::new("install-first");
    let o = i.run(&[]);
    let (out, err) = text(&o);
    let c = i.conf().display().to_string();
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert_eq!(
        out,
        format!(
            "\u{203a} installing Mayfly {V}\n\
             \u{2713} storage local now holds snippets\n\
             \u{2713} Mayfly {V} installed, created pool.env slots.conf in {c}\n\
             \nNext\n\
             \x20 1. GitHub App private key to {c}/app.pem (chmod 600)\n\
             \x20 2. APP_ID and ORG (or REPO) in {c}/pool.env\n\
             \x20 3. slots in {c}/slots.conf\n\
             \x20 4. mayfly check\n\
             \x20 5. mayfly template build, then mayfly template promote <vmid>\n\
             \x20 6. mayfly enable\n\
             \x20 Getting started: https://github.com/variiuz/mayfly#get-started\n"
        )
    );
    assert_eq!(err, "");
    assert_eq!(
        i.calls(),
        [
            "apt-get install -y -q jq curl openssl",
            "pvesh get /storage/local --output-format json",
            "pvesm set local --content iso,vztmpl,snippets",
            "systemctl daemon-reload",
            "systemctl enable -q --now mayfly-update.timer",
            "systemctl enable -q --now mayfly-gc.timer",
        ]
    );
    for (d, m) in [
        (i.conf(), 0o700),
        (i.conf().join("env"), 0o700),
        (i.conf().join("hooks.d"), 0o755),
        (i.conf().join("profiles"), 0o755),
        (i.share().join("profiles"), 0o755),
        (i.share().join("profiles/default"), 0o755),
    ] {
        assert_eq!(i.mode(d.clone()), m, "{}", d.display());
    }
    // The binary is a copy of the one that ran, the rest comes from the release directory.
    assert_eq!(
        fs::read(i.sbin().join("mayfly")).unwrap(),
        fs::read(env!("CARGO_BIN_EXE_mayfly")).unwrap()
    );
    assert_eq!(i.mode(i.sbin().join("mayfly")), 0o755);
    assert_eq!(i.read(i.share().join("mayfly.sh")), "new mayfly.sh\n");
    assert_eq!(i.mode(i.share().join("mayfly.sh")), 0o755);
    for f in TEMPLATE_FILES {
        assert_eq!(i.read(i.share().join(f)), format!("template {f}\n"));
        assert_eq!(i.mode(i.share().join(f)), 0o644, "{f}");
    }
    assert_eq!(
        i.read(i.share().join("profiles/default/provision.sh")),
        "profile provision\n"
    );
    assert_eq!(
        i.mode(i.share().join("profiles/default/provision.sh")),
        0o755
    );
    assert_eq!(
        i.mode(i.share().join("profiles/default/template.env")),
        0o644
    );
    for u in UNIT_FILES {
        assert_eq!(i.read(i.units().join(u)), format!("unit {u}\n"));
        assert_eq!(i.mode(i.units().join(u)), 0o644, "{u}");
    }
    assert_eq!(i.read(i.conf().join("pool.env")), "APP_ID=\nORG=\n");
    assert_eq!(i.mode(i.conf().join("pool.env")), 0o600);
    assert_eq!(i.read(i.conf().join("slots.conf")), "# slots\n");
    assert_eq!(i.mode(i.conf().join("slots.conf")), 0o644);
    assert_eq!(i.names(i.sbin()), ["mayfly"]);
    assert_eq!(i.names(i.units()).len(), 5);
    assert_eq!(i.names(i.share()).len(), 7);
}

#[test]
fn a_second_install_keeps_the_config_the_timers_and_your_profiles() {
    let i = Install::new("install-second");
    assert_eq!(i.run(&[]).status.code(), Some(0));
    write(i.conf().join("pool.env"), "APP_ID=7\nORG=acme\n");
    write(i.conf().join("slots.conf"), "a-1 9001 x 1 1 1 no current\n");
    write(i.conf().join("app.pem"), "key\n");
    write(i.conf().join("profiles/mine/provision.sh"), "mine\n");
    // A profile that is no longer shipped goes, and a changed one is replaced.
    write(i.share().join("profiles/gone/provision.sh"), "old\n");
    write(i.share().join("profiles/default/provision.sh"), "stale\n");
    write(i.share().join("mayfly.sh"), "stale\n");
    i.clear_calls();
    let o = i.run(&[]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert_eq!(
        out,
        format!("\u{203a} installing Mayfly {V} (was {V})\n\u{2713} Mayfly {V} installed\n")
    );
    assert_eq!(
        i.calls(),
        [
            "apt-get install -y -q jq curl openssl",
            "pvesh get /storage/local --output-format json",
            "systemctl daemon-reload",
        ]
    );
    assert_eq!(i.read(i.conf().join("pool.env")), "APP_ID=7\nORG=acme\n");
    assert_eq!(
        i.read(i.conf().join("slots.conf")),
        "a-1 9001 x 1 1 1 no current\n"
    );
    assert_eq!(
        i.read(i.conf().join("profiles/mine/provision.sh")),
        "mine\n"
    );
    assert_eq!(i.names(i.share().join("profiles")), ["default"]);
    assert_eq!(
        i.read(i.share().join("profiles/default/provision.sh")),
        "profile provision\n"
    );
    assert_eq!(i.read(i.share().join("mayfly.sh")), "new mayfly.sh\n");
}

#[test]
fn no_timer_leaves_the_gc_timer_off() {
    let i = Install::new("install-no-timer");
    let o = i.run(&["--no-timer"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(
        out.ends_with("! gc timer is off (--no-timer); mayfly enable starts it\n"),
        "{out}"
    );
    let calls = i.calls();
    assert!(calls.contains(&"systemctl disable -q --now mayfly-gc.timer".to_string()));
    assert!(!calls.contains(&"systemctl enable -q --now mayfly-gc.timer".to_string()));
    // The update timer is not the gc timer's business.
    assert!(calls.contains(&"systemctl enable -q --now mayfly-update.timer".to_string()));
    // Still a first install: the list of next steps is there.
    assert!(out.contains("\nNext\n"), "{out}");
}

#[test]
fn update_off_keeps_the_update_timer_off() {
    let i = Install::new("install-update-off");
    write(
        i.conf().join("pool.env"),
        "UPDATE=\"off\" # by hand\nAPP_ID=1\n",
    );
    let o = i.run(&[]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    let calls = i.calls();
    assert!(
        !calls.iter().any(|c| c.contains("mayfly-update.timer")),
        "{calls:?}"
    );
    assert!(calls.contains(&"systemctl enable -q --now mayfly-gc.timer".to_string()));
    // pool.env was there, so only slots.conf is created; APP_ID is set, so step 2 is not listed.
    let c = i.conf().display().to_string();
    assert!(
        out.contains(&format!("installed, created slots.conf in {c}\n")),
        "{out}"
    );
    assert!(!out.contains("APP_ID and ORG"), "{out}");
    assert_eq!(
        i.read(i.conf().join("pool.env")),
        "UPDATE=\"off\" # by hand\nAPP_ID=1\n"
    );
}

#[test]
fn the_update_timer_is_enabled_when_its_units_are_new() {
    let i = Install::new("install-update-timer");
    assert_eq!(i.run(&[]).status.code(), Some(0));
    fs::remove_file(i.units().join("mayfly-update.timer")).unwrap();
    i.clear_calls();
    assert_eq!(i.run(&[]).status.code(), Some(0));
    assert_eq!(
        i.calls(),
        [
            "apt-get install -y -q jq curl openssl",
            "pvesh get /storage/local --output-format json",
            "systemctl daemon-reload",
            "systemctl enable -q --now mayfly-update.timer",
        ]
    );
}

#[test]
fn snippets_content_is_added_once_to_the_snippet_storage() {
    let i = Install::new("install-snippets");
    write(
        i.conf().join("pool.env"),
        "SNIPPET_STORAGE=tank\nSNIPPET_STORAGE=fast\n",
    );
    i.store("images,rootdir");
    let o = i.run(&[]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}{err}");
    assert!(
        out.contains("\u{2713} storage fast now holds snippets\n"),
        "{out}"
    );
    let calls = i.calls();
    assert!(calls.contains(&"pvesh get /storage/fast --output-format json".to_string()));
    assert!(calls.contains(&"pvesm set fast --content images,rootdir,snippets".to_string()));
    assert_eq!(i.read(i.root.join("store")), "images,rootdir,snippets");
    i.clear_calls();
    let o = i.run(&[]);
    let (out, _) = text(&o);
    assert!(!out.contains("snippets"), "{out}");
    assert!(!i.calls().iter().any(|c| c.starts_with("pvesm")));
}

#[test]
fn install_refuses_without_root_qm_or_files() {
    let i = Install::new("install-refuse");
    write(i.root.join("bin/id"), "#!/bin/sh\necho 1\n");
    let o = i.run(&[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).1.contains("run as root"));
    write(i.root.join("bin/id"), "#!/bin/sh\necho 0\n");
    fs::set_permissions(i.root.join("bin/id"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(i.root.join("bin/qm")).unwrap();
    let mut c = i.command(&[]);
    c.env("PATH", i.root.join("bin")).arg("--from").arg(i.src());
    let o = run_retry(&mut c);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).1.contains("not a Proxmox host (qm is missing)"));
    write(i.root.join("bin/qm"), "#!/bin/sh\n");
    fs::set_permissions(i.root.join("bin/qm"), fs::Permissions::from_mode(0o755)).unwrap();
    // No --from and no mayfly.sh next to the binary, a directory without mayfly.sh, an unknown argument.
    let o = run_retry(&mut i.command(&[]));
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o).1.contains("no mayfly.sh next to"),
        "{:?}",
        text(&o)
    );
    let o = run_retry(i.command(&[]).arg("--from").arg(i.root.join("bin")));
    assert!(text(&o).1.contains("has no mayfly.sh"), "{:?}", text(&o));
    let o = i.run(&["--frobnicate"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o).1.contains("unknown argument --frobnicate"));
    assert!(!i.sbin().exists() && !i.conf().exists());
    assert!(i.calls().is_empty());
}

#[test]
fn a_failing_command_stops_the_install_with_its_exit_code() {
    let i = Install::new("install-fails");
    write(
        i.root.join("bin/pvesm"),
        "#!/bin/sh\necho 'no such storage' >&2\nexit 7\n",
    );
    fs::set_permissions(i.root.join("bin/pvesm"), fs::Permissions::from_mode(0o755)).unwrap();
    let o = i.run(&[]);
    assert_eq!(o.status.code(), Some(7));
    assert!(text(&o).1.contains("no such storage"));
    // Nothing after the failed step ran.
    assert!(!i.calls().iter().any(|c| c.starts_with("systemctl")));
}

// ------------------------------------------------------------------ engine= and SLOT_ENGINE

#[test]
fn status_shows_the_engine_only_while_a_slot_runs_rust() {
    // No engine setting anywhere: the columns of the bash status.
    let (f, pve) = status_fixture("");
    let (_, out) = f.run_status(&pve);
    assert!(!out.contains("ENGINE"), "{out}");
    assert!(out.starts_with(
        "SLOT       VMID   KIND LABELS                 SIZE    SERVICE   GUEST    JOB    GITHUB\n"
    ));

    // One slot on rust: a column for all of them; pods and containers are bash whatever they ask.
    write(
        &f.paths.slots_conf,
        "build-1 9002 linux 8 16 60 no current engine=rust\n\
         pods-1 9041 small 12 36 120 no current kind=pod containers=6 engine=rust\n\
         ct-1 9051 small 2 4 20 no current kind=lxc engine=rust\n\
         off-1 9052 small 1 1 1 no current engine=bash\n",
    );
    let (_, out) = f.run_status(&pve);
    let rows: Vec<&str> = out.lines().take(5).collect();
    assert_eq!(
        rows[0],
        "SLOT       VMID   KIND ENGINE LABELS                 SIZE    SERVICE   GUEST    JOB    GITHUB"
    );
    assert!(rows[1].starts_with("build-1    9002   vm   rust   linux                  8c/16G  "));
    assert!(rows[2].starts_with("pods-1     9041   pod  bash   small                  12c/36G "));
    assert!(rows[3].starts_with("ct-1       9051   lxc  bash   small                  2c/4G   "));
    assert!(rows[4].starts_with("off-1      9052   vm   bash   small                  1c/1G   "));

    // SLOT_ENGINE in pool.env is the default of the vm slots.
    let (f, pve) = status_fixture("SLOT_ENGINE=rust\n");
    let (_, out) = f.run_status(&pve);
    let rows: Vec<&str> = out.lines().take(5).collect();
    assert!(
        rows[1].starts_with("build-1    9002   vm   rust   "),
        "{out}"
    );
    assert!(
        rows[2].starts_with("pods-1     9041   pod  bash   "),
        "{out}"
    );
    assert!(
        rows[4].starts_with("off-1      9052   vm   rust   "),
        "{out}"
    );

    // Only an unknown value: the slot runs on bash, and no slot runs rust.
    let (f, pve) = status_fixture("SLOT_ENGINE=nope\n");
    let (_, out) = f.run_status(&pve);
    assert!(!out.contains("ENGINE"), "{out}");
}

#[test]
fn check_accepts_engine_and_warns_about_an_unknown_value() {
    let dir = tmp("key-engine");
    let (key, _) = make_key(&dir, false);
    let gh = fake_github(200);
    let (f, pve) = check_fixture(gh.port, &key);
    write(
        &f.paths.slots_conf,
        "r-1 9002 x 1 1 1 no current engine=rust\n\
         b-1 9003 x 1 1 1 no current engine=bash\n\
         u-1 9004 x 1 1 1 no current engine=fast\n\
         p-1 9005 x 1 1 1 no current kind=pod engine=rust\n",
    );
    write(
        &f.paths.pool_env,
        &format!(
            "APP_ID=12345\nKEY_PATH={}\nAPI_URL=http://127.0.0.1:{}\nORG=acme\nTEMPLATE_VMID=9098\nSLOT_ENGINE=nope\n",
            key.display(),
            gh.port
        ),
    );
    let (code, out) = f.run_check(&pve).unwrap();
    let got: Vec<&str> = out
        .lines()
        .filter(|l| l.to_ascii_lowercase().contains("engine"))
        .collect();
    assert_eq!(
        got,
        [
            format!(
                "warn {:<9} SLOT_ENGINE=nope is not rust or bash; slots run on bash",
                "pool.env"
            ),
            format!(
                "warn {:<9} engine=fast is not rust or bash; the slot runs on bash",
                "u-1"
            ),
        ]
    );
    assert_eq!(code, 0);
}

#[test]
fn slot_engine_defaults_to_bash() {
    let f = Fixture::new("engine-default", "ORG=acme\n", "");
    assert_eq!(load(&f, true).unwrap().get("SLOT_ENGINE"), "bash");
    write(&f.paths.pool_env, "ORG=acme\nSLOT_ENGINE=rust\n");
    assert_eq!(load(&f, true).unwrap().get("SLOT_ENGINE"), "rust");
}
