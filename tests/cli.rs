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
        let paths = Paths::new(
            &format!("{r}/conf"),
            &format!("{r}/share"),
            &format!("{r}/pve"),
            &format!("{r}/fw"),
        );
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
";
    assert_eq!(out, want);
    assert_eq!(code, 0);
}

#[test]
fn status_without_lxc_templates_has_no_lxc_line() {
    let (f, pve) = status_fixture("");
    let (_, out) = f.run_status(&pve);
    assert!(
        out.ends_with("template   9098, next 9099, profile default\n"),
        "{out}"
    );
    assert!(!out.contains("\nlxc "));
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
    for args in [
        vec!["slot", "build-1"],
        vec!["gc"],
        vec!["template", "build", "--lxc", "my profile"],
        vec!["frobnicate", "--now"],
        vec![],
    ] {
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
    let out = mayfly(&share).arg("gc").output().unwrap();
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
";
    assert_eq!(out, want);
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
