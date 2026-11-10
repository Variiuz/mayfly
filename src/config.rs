//! `pool.env` (plain KEY=value, never run as shell), `slots.conf` and the directories around them.

use crate::out::{die, R};
use crate::pve::Pve;
use std::collections::HashMap;
use std::fs;

/// Where Mayfly keeps things; every one can be moved with an environment variable (tests do).
#[derive(Clone, Debug)]
pub struct Paths {
    pub conf_dir: String,
    pub pool_env: String,
    pub slots_conf: String,
    pub env_dir: String,
    pub share: String,
    pub pve_etc: String,
    pub pve_fw_dir: String,
    /// Where the `mayfly` binary lives (`MAYFLY_SBIN`).
    pub sbin: String,
    /// Where the systemd units live (`MAYFLY_SYSTEMD_DIR`).
    pub systemd_dir: String,
    /// `update.state` is kept here (`MAYFLY_STATE_DIR`).
    pub state_dir: String,
    /// Where `mayfly update` makes its temporary directory (`MAYFLY_TMP`).
    pub tmp_dir: String,
}

pub fn env_or(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

impl Paths {
    /// `MAYFLY_CONF_DIR`, `MAYFLY_SHARE`, `MAYFLY_PVE_ETC` and `MAYFLY_PVE_FW_DIR`, as bash reads them, and
    /// `MAYFLY_SBIN`, `MAYFLY_SYSTEMD_DIR`, `MAYFLY_STATE_DIR` and `MAYFLY_TMP` for `update`.
    pub fn from_env() -> Paths {
        let pve_etc = env_or("MAYFLY_PVE_ETC", "/etc/pve");
        let fw = env_or("MAYFLY_PVE_FW_DIR", &format!("{pve_etc}/firewall"));
        let mut p = Paths::new(
            &env_or("MAYFLY_CONF_DIR", "/etc/mayfly"),
            &env_or("MAYFLY_SHARE", "/usr/local/share/mayfly"),
            &pve_etc,
            &fw,
        );
        p.sbin = env_or("MAYFLY_SBIN", &p.sbin);
        p.systemd_dir = env_or("MAYFLY_SYSTEMD_DIR", &p.systemd_dir);
        p.state_dir = env_or("MAYFLY_STATE_DIR", &p.state_dir);
        p.tmp_dir = env_or("MAYFLY_TMP", &p.tmp_dir);
        p
    }

    pub fn new(conf_dir: &str, share: &str, pve_etc: &str, pve_fw_dir: &str) -> Paths {
        Paths {
            conf_dir: conf_dir.to_string(),
            pool_env: format!("{conf_dir}/pool.env"),
            slots_conf: format!("{conf_dir}/slots.conf"),
            env_dir: format!("{conf_dir}/env"),
            share: share.to_string(),
            pve_etc: pve_etc.to_string(),
            pve_fw_dir: pve_fw_dir.to_string(),
            sbin: "/usr/local/sbin".to_string(),
            systemd_dir: "/etc/systemd/system".to_string(),
            state_dir: "/var/lib/mayfly".to_string(),
            tmp_dir: "/var/tmp".to_string(),
        }
    }
}

// ------------------------------------------------------------------ pool.env syntax

/// A line of pool.env that is not plain KEY=value.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Issue {
    pub line: usize,
    pub reason: &'static str,
}

/// Same set as bash `[[:space:]]`.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

fn is_key(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What may follow a value: nothing, or whitespace and then a `#` comment.
fn tail_ok(rest: &str) -> bool {
    let t = rest.trim_start_matches(is_space);
    t.is_empty() || (rest.starts_with(is_space) && t.starts_with('#'))
}

/// The value after the `=`: its text, or why it is refused.
fn parse_value(v: &str) -> Result<String, &'static str> {
    if let Some(body) = v.strip_prefix('"') {
        let Some(end) = body.find('"') else {
            return Err("unterminated double quote");
        };
        let (inner, rest) = (&body[..end], &body[end + 1..]);
        if inner.contains(['$', '`', '\\']) {
            return Err("$, backtick or backslash inside double quotes");
        }
        if !tail_ok(rest) {
            return Err("text after the closing quote");
        }
        return Ok(inner.to_string());
    }
    if let Some(body) = v.strip_prefix('\'') {
        let Some(end) = body.find('\'') else {
            return Err("unterminated single quote");
        };
        if !tail_ok(&body[end + 1..]) {
            return Err("text after the closing quote");
        }
        return Ok(body[..end].to_string());
    }
    if v.ends_with('\\') {
        return Err("line continuation");
    }
    let split = v.find(is_space).unwrap_or(v.len());
    let (tok, rest) = (&v[..split], &v[split..]);
    if tok.contains(['$', '`']) {
        return Err("variable or command substitution");
    }
    if tok.contains('\\') {
        return Err("backslash in an unquoted value");
    }
    if tok.contains(['"', '\'']) {
        return Err("quote inside an unquoted value");
    }
    if tok.contains([';', '&', '|']) {
        return Err("more than one command on a line");
    }
    if tok.contains(['<', '>', '(', ')']) {
        return Err("shell syntax in an unquoted value");
    }
    if tok.starts_with('~') || tok.contains(":~") {
        return Err("~ is not expanded");
    }
    if !tail_ok(rest) {
        if rest.contains([';', '&', '|']) {
            return Err("more than one command on a line");
        }
        let t = rest.trim_start_matches(is_space);
        let word = t
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .next();
        let assignment = rest.starts_with(is_space)
            && word.is_some_and(|w| is_key(w) && t[w.len()..].starts_with('='));
        return Err(if assignment {
            "more than one assignment on a line"
        } else {
            "spaces in an unquoted value (quote it)"
        });
    }
    Ok(tok.to_string())
}

/// Why a line without a KEY=value start is refused.
fn not_assignment(text: &str) -> &'static str {
    let Some(eq) = text.find('=') else {
        return "not a KEY=value assignment";
    };
    let key = &text[..eq];
    let bare = key.trim_end_matches(is_space);
    if bare.len() < key.len() && is_key(bare) {
        "space before ="
    } else if !key.is_empty() && !key.contains(is_space) {
        "KEY is letters, digits and _, not starting with a digit"
    } else {
        "not a KEY=value assignment"
    }
}

/// Reads pool.env as plain text: the assignments in order, and every line that is not plain KEY=value.
/// The grammar is the one `mayfly check` of the Bash version warns about.
pub fn parse_pool_env(text: &str) -> (Vec<(String, String)>, Vec<Issue>) {
    let mut vars = Vec::new();
    let mut issues = Vec::new();
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    for (i, line) in lines.iter().enumerate() {
        let n = i + 1;
        let mut bad = |reason| issues.push(Issue { line: n, reason });
        if line.ends_with('\r') {
            bad("carriage return (Windows line ending)");
            continue;
        }
        let mut t = line.trim_start_matches(is_space);
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix("export") {
            if rest.starts_with(is_space) {
                t = rest.trim_start_matches(is_space);
            }
        }
        match t.find('=') {
            Some(eq) if is_key(&t[..eq]) => match parse_value(&t[eq + 1..]) {
                Ok(value) => vars.push((t[..eq].to_string(), value)),
                Err(reason) => bad(reason),
            },
            _ => bad(not_assignment(t)),
        }
    }
    (vars, issues)
}

// ------------------------------------------------------------------ pool.env settings

/// The settings of pool.env with their defaults.
pub struct Config {
    vars: HashMap<String, String>,
    /// `orgs/<org>` or `repos/<owner>/<name>`.
    pub scope: String,
    pub snippet_dir: String,
}

fn defaults(conf_dir: &str) -> Vec<(&'static str, String)> {
    let s = |v: &str| v.to_string();
    vec![
        ("APP_ID", s("")),
        ("INSTALLATION_ID", s("")),
        ("ORG", s("")),
        ("REPO", s("")),
        ("RUNNER_GROUP_ID", s("1")),
        ("KEY_PATH", format!("{conf_dir}/app.pem")),
        ("API_URL", s("https://api.github.com")),
        ("RUNNER_LABELS_BASE", s("self-hosted,Linux,X64")),
        ("STORAGE", s("local-lvm")),
        ("SNIPPET_STORAGE", s("local")),
        ("BRIDGE", s("vmbr0")),
        ("VLAN", s("")),
        ("GATEWAY", s("")),
        ("CACHE_IP", s("")),
        ("FIREWALL", s("yes")),
        (
            "BLOCK_NETS",
            s("10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 100.64.0.0/10 169.254.0.0/16"),
        ),
        ("ALLOW_OUT", s("")),
        ("VMID_MIN", s("9000")),
        ("VMID_MAX", s("9099")),
        ("TEMPLATE_VMIDS", s("9097 9098 9099")),
        ("TEMPLATE_VMID", s("")),
        ("NEXT_TEMPLATE_VMID", s("")),
        ("TAG", s("mayfly")),
        ("LEGACY_TAGS", s("")),
        ("NAME_PREFIX", s("ci-")),
        ("CPU_TYPE", s("")),
        ("POD_MAX_AGE_HOURS", s("24")),
        ("MAX_POOL_PERCENT", s("70")),
        ("POOL_PANIC_PERCENT", s("85")),
        ("MIN_FREE_RAM_GB", s("8")),
        ("MAX_JOB_HOURS", s("3")),
        ("MAX_IDLE_HOURS", s("2")),
        ("DISK_CACHE", s("")),
        ("TEMPLATE_DISK", s("80")),
        ("PROFILE", s("default")),
        ("TEMPLATE_CORES", s("8")),
        ("TEMPLATE_MEMORY_GB", s("16")),
        ("HOOK_DIR", format!("{conf_dir}/hooks.d")),
        ("LXC_TEMPLATE_VMID", s("")),
        ("NEXT_LXC_TEMPLATE_VMID", s("")),
        ("LXC_OSTEMPLATE", s("ubuntu-24.04-standard")),
        ("LXC_TEMPLATE_STORAGE", s("local")),
        ("IMAGE_URL", s("")),
        (
            "UBUNTU_IMAGE_URL",
            s("https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"),
        ),
        ("RUNNER_PREFIX", s("")),
        ("UPDATE", s("auto")),
    ]
}

impl Config {
    /// A setting; "" when unset. Asking for a name that has no default is a bug.
    pub fn get(&self, key: &str) -> &str {
        debug_assert!(
            self.vars.contains_key(key) || key.is_empty(),
            "no setting {key}"
        );
        self.vars.get(key).map(String::as_str).unwrap_or("")
    }

    /// A whole-number setting, as bash `[ "$x" -ge ... ]` reads it: None when it is not one.
    pub fn number(&self, key: &str) -> Option<i64> {
        self.get(key).trim().parse().ok()
    }
}

fn name_ok(s: &str, extra: char) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == extra)
}

/// Reads pool.env and applies the defaults, like bash `load_env`. With `strict`, a line that is not
/// plain KEY=value is an error (`pool.env line N: <reason>`); without it such lines are skipped and
/// returned, which is what `mayfly check` reports.
pub fn load(paths: &Paths, pve: &dyn Pve, node: &str, strict: bool) -> R<(Config, Vec<Issue>)> {
    let Ok(bytes) = fs::read(&paths.pool_env) else {
        return die(format!("{} missing (run install.sh)", paths.pool_env));
    };
    let (assigned, issues) = parse_pool_env(&String::from_utf8_lossy(&bytes));
    if strict {
        if let Some(i) = issues.first() {
            return die(format!("pool.env line {}: {}", i.line, i.reason));
        }
    }
    let mut vars: HashMap<String, String> = defaults(&paths.conf_dir)
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    for (k, v) in assigned {
        vars.insert(k, v);
    }
    let get = |vars: &HashMap<String, String>, k: &str| vars.get(k).cloned().unwrap_or_default();
    if get(&vars, "IMAGE_URL").is_empty() {
        let u = get(&vars, "UBUNTU_IMAGE_URL");
        vars.insert("IMAGE_URL".to_string(), u);
    }
    if get(&vars, "RUNNER_PREFIX").is_empty() {
        let p = format!("{}{}", get(&vars, "NAME_PREFIX"), node);
        vars.insert("RUNNER_PREFIX".to_string(), p);
    }
    let name_prefix = get(&vars, "NAME_PREFIX");
    if !name_ok(&name_prefix, '-') {
        return die(format!(
            "NAME_PREFIX={name_prefix}: lowercase letters, digits and dashes only"
        ));
    }
    let tag = get(&vars, "TAG");
    if !name_ok(&tag, '_') {
        return die(format!(
            "TAG={tag}: lowercase letters, digits, dashes and underscores only"
        ));
    }
    let (repo, org) = (get(&vars, "REPO"), get(&vars, "ORG"));
    let scope = if !repo.is_empty() {
        if !repo.contains('/') {
            return die(format!("REPO={repo}: use owner/name"));
        }
        format!("repos/{repo}")
    } else if !org.is_empty() {
        format!("orgs/{org}")
    } else {
        return die(format!(
            "set ORG (organization runners) or REPO=owner/name (repository runners) in {}",
            paths.pool_env
        ));
    };
    let snippet_storage = get(&vars, "SNIPPET_STORAGE");
    let listed = pve.pvesm_path(&format!("{snippet_storage}:snippets/x"));
    let snippet_dir = listed
        .lines()
        .map(|l| l.strip_suffix("/x").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n");
    let snippet_dir = snippet_dir.trim_end_matches('\n').to_string();
    if snippet_dir.is_empty() {
        return die(format!(
            "storage {snippet_storage} has no snippets content (install.sh adds it)"
        ));
    }
    Ok((
        Config {
            vars,
            scope,
            snippet_dir,
        },
        issues,
    ))
}

/// `UPDATE` of pool.env for the update timer: `auto` (the default), `patch` or `off`. Read on its own,
/// so the timer works before the rest of pool.env is filled in.
pub fn update_mode(paths: &Paths) -> R<String> {
    let text = fs::read(&paths.pool_env)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let (vars, _) = parse_pool_env(&text);
    let v = vars
        .iter()
        .rev()
        .find(|(k, _)| k == "UPDATE")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    match v {
        "" | "auto" => Ok("auto".to_string()),
        "patch" | "off" => Ok(v.to_string()),
        _ => die(format!("UPDATE={v}: use auto, patch or off")),
    }
}

// ------------------------------------------------------------------ slots.conf

/// One line of slots.conf: name vmid labels vcpu ram_gb disk_gb env template(current|next) [key=value ...]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slot {
    pub name: String,
    pub vmid: String,
    pub labels: String,
    pub vcpu: String,
    pub ram: String,
    pub disk: String,
    pub envspec: String,
    pub which: String,
    /// The trailing key=value fields, as written.
    pub extra: String,
}

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Splits like `read -r a b c ...`: at most `max` fields, the last one the rest of the line.
fn split_fields(line: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line.trim_start_matches(is_blank);
    while !rest.is_empty() {
        if out.len() + 1 == max {
            out.push(rest.trim_end_matches(is_blank));
            break;
        }
        match rest.find(is_blank) {
            Some(i) => {
                out.push(&rest[..i]);
                rest = rest[i..].trim_start_matches(is_blank);
            }
            None => {
                out.push(rest);
                break;
            }
        }
    }
    out
}

pub struct Slots {
    lines: Vec<Slot>,
}

impl Slots {
    /// A missing file has no slots.
    pub fn read(path: &str) -> Slots {
        let text = fs::read(path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        Slots::parse(&text)
    }

    pub fn parse(text: &str) -> Slots {
        let mut lines = Vec::new();
        for line in text.split('\n') {
            let f = split_fields(line, 9);
            if f.is_empty() || f[0].starts_with('#') {
                continue;
            }
            let at = |i: usize| f.get(i).copied().unwrap_or("").to_string();
            lines.push(Slot {
                name: at(0),
                vmid: at(1),
                labels: at(2),
                vcpu: at(3),
                ram: at(4),
                disk: at(5),
                envspec: at(6),
                which: at(7),
                extra: at(8),
            });
        }
        Slots { lines }
    }

    /// Slot names in file order (a name used twice is listed twice).
    pub fn names(&self) -> Vec<&str> {
        self.lines.iter().map(|s| s.name.as_str()).collect()
    }

    /// The first line of that name.
    pub fn get(&self, name: &str) -> Option<&Slot> {
        self.lines.iter().find(|s| s.name == name)
    }
}

/// The value of a trailing `key=value` field of a slot (the first one), else `default`.
pub fn slot_opt(key: &str, extra: &str, default: &str) -> String {
    let prefix = format!("{key}=");
    extra
        .split(is_blank)
        .filter(|f| !f.is_empty())
        .find_map(|f| f.strip_prefix(prefix.as_str()))
        .unwrap_or(default)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad(line: &str) -> Vec<&'static str> {
        parse_pool_env(line).1.iter().map(|i| i.reason).collect()
    }

    fn value(line: &str) -> String {
        let (vars, issues) = parse_pool_env(line);
        assert!(issues.is_empty(), "{line}: {issues:?}");
        vars.last().map(|v| v.1.clone()).unwrap_or_default()
    }

    #[test]
    fn accepts_plain_assignments() {
        assert_eq!(value("ORG=acme"), "acme");
        assert_eq!(value("  export   ORG=acme"), "acme");
        assert_eq!(value("EMPTY="), "");
        assert_eq!(
            value("A=https://ghe.example.com/api/v3"),
            "https://ghe.example.com/api/v3"
        );
        assert_eq!(value("A=a#b"), "a#b");
        assert_eq!(value("A=value # comment"), "value");
        assert_eq!(value("A= # comment"), "");
        assert_eq!(value("A=value   "), "value");
        assert_eq!(value("A=\"two words\""), "two words");
        assert_eq!(value("A=\"a # b\" # c"), "a # b");
        assert_eq!(value("A=\"it's\""), "it's");
        assert_eq!(value("A='$HOME `x` \\ stay'"), "$HOME `x` \\ stay");
        assert_eq!(value("A='a\\'"), "a\\");
        assert_eq!(value("A=''"), "");
        assert_eq!(value("A=\"\""), "");
        assert_eq!(value("_x1=a"), "a");
    }

    #[test]
    fn skips_blank_lines_and_comments() {
        let (vars, issues) = parse_pool_env("\n   \n\t\n# c\n   # c\nORG=a");
        assert!(issues.is_empty());
        assert_eq!(vars, vec![("ORG".to_string(), "a".to_string())]);
    }

    #[test]
    fn last_assignment_wins_in_order() {
        let (vars, _) = parse_pool_env("A=1\nB=2\nA=3\n");
        assert_eq!(vars.len(), 3);
        assert_eq!(vars[2], ("A".to_string(), "3".to_string()));
    }

    #[test]
    fn refuses_shell() {
        assert_eq!(bad("ORG=$ORG"), ["variable or command substitution"]);
        assert_eq!(bad("A=$(hostname)"), ["variable or command substitution"]);
        assert_eq!(bad("A=`hostname`"), ["variable or command substitution"]);
        assert_eq!(
            bad("A=\"$HOME\""),
            ["$, backtick or backslash inside double quotes"]
        );
        assert_eq!(
            bad("A=\"a\\\"b\""),
            ["$, backtick or backslash inside double quotes"]
        );
        assert_eq!(bad("A=a\\b"), ["backslash in an unquoted value"]);
        assert_eq!(bad("A=a\\"), ["line continuation"]);
        assert_eq!(bad("A=a b"), ["spaces in an unquoted value (quote it)"]);
        assert_eq!(bad("A= b"), ["spaces in an unquoted value (quote it)"]);
        assert_eq!(bad("A=a B=2"), ["more than one assignment on a line"]);
        assert_eq!(bad("A=1; B=2"), ["more than one command on a line"]);
        assert_eq!(bad("A=1 && B=2"), ["more than one command on a line"]);
        assert_eq!(bad("A=1|cat"), ["more than one command on a line"]);
        assert_eq!(bad("A=a>b"), ["shell syntax in an unquoted value"]);
        assert_eq!(bad("A=(a b)"), ["shell syntax in an unquoted value"]);
        assert_eq!(bad("A=a\"b\""), ["quote inside an unquoted value"]);
        assert_eq!(bad("A=~/x"), ["~ is not expanded"]);
        assert_eq!(bad("A=\"open"), ["unterminated double quote"]);
        assert_eq!(bad("A='open"), ["unterminated single quote"]);
        assert_eq!(bad("A=\"a\"b"), ["text after the closing quote"]);
        assert_eq!(bad("A='a' 'b'"), ["text after the closing quote"]);
        assert_eq!(bad("A=\"a\"#x"), ["text after the closing quote"]);
    }

    #[test]
    fn refuses_what_is_not_an_assignment() {
        let not = "not a KEY=value assignment";
        assert_eq!(bad("source /etc/other.env"), [not]);
        assert_eq!(bad(". /etc/other.env"), [not]);
        assert_eq!(bad("if [ -x /bin/x ]; then"), [not]);
        assert_eq!(bad("fi"), [not]);
        assert_eq!(bad("export KEY"), [not]);
        assert_eq!(bad("=value"), [not]);
        assert_eq!(bad("KEY =value"), ["space before ="]);
        assert_eq!(bad("export KEY = value"), ["space before ="]);
        let key = "KEY is letters, digits and _, not starting with a digit";
        assert_eq!(bad("1KEY=a"), [key]);
        assert_eq!(bad("A-B=c"), [key]);
        assert_eq!(bad("ORG=acme\r"), ["carriage return (Windows line ending)"]);
    }

    #[test]
    fn counts_lines_from_one() {
        let (_, issues) = parse_pool_env("# ok\nORG=acme\n\nKEY=$(x)\nKEY=\\\n");
        let lines: Vec<usize> = issues.iter().map(|i| i.line).collect();
        assert_eq!(lines, [4, 5]);
        let (vars, issues) = parse_pool_env("ORG=acme");
        assert!(issues.is_empty());
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn slot_lines() {
        let s = Slots::parse(
            "build-1 9002 linux 8 16 60 no current\n# off 9009 x 1 1 1 no current\n\n  pods-1   9041 small 12 36 120 no  current   kind=pod  containers=6 \nbuild-1 1 x 1 1 1 no current\n",
        );
        assert_eq!(s.names(), ["build-1", "pods-1", "build-1"]);
        let b = s.get("build-1").unwrap();
        assert_eq!(
            (b.vmid.as_str(), b.vcpu.as_str(), b.extra.as_str()),
            ("9002", "8", "")
        );
        let p = s.get("pods-1").unwrap();
        assert_eq!(p.extra, "kind=pod  containers=6");
        assert_eq!(p.which, "current");
        assert_eq!(slot_opt("kind", &p.extra, "vm"), "pod");
        assert_eq!(slot_opt("containers", &p.extra, "?"), "6");
        assert_eq!(slot_opt("template", &p.extra, ""), "");
        assert_eq!(slot_opt("kind", "", "vm"), "vm");
        assert_eq!(slot_opt("vlan", "vlan= x", "d"), "");
    }
}
