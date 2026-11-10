//! The GitHub App: a signed JWT, the installation, an installation token and the calls `check` makes.
//! A token only ever goes in a request header, never in a command line or a log.

use crate::out::{die, R};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An RS256 JWT for the App: `iat` a minute back (clock skew), `exp` nine minutes ahead, `iss` the App ID
/// (a number when it is all digits, else a string such as a client ID).
pub fn make_jwt(app_id: &str, key_pem: &[u8], now: u64) -> R<String> {
    let iss = match app_id.parse::<u64>() {
        Ok(n) if app_id.bytes().all(|b| b.is_ascii_digit()) => json!(n),
        _ => json!(app_id),
    };
    let claims = json!({"iat": now.saturating_sub(60), "exp": now + 540, "iss": iss});
    let key = EncodingKey::from_rsa_pem(key_pem).map_err(|_| {
        crate::out::Die("the App key is not a usable RSA private key (PEM)".to_string())
    })?;
    jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|e| crate::out::Die(format!("could not sign the App JWT: {e}")))
}

pub struct GitHub {
    api_url: String,
    agent: ureq::Agent,
}

impl GitHub {
    pub fn new(api_url: &str) -> GitHub {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(concat!("mayfly/", env!("CARGO_PKG_VERSION")))
            .build();
        GitHub {
            api_url: api_url.to_string(),
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// One call; the bearer goes in a header. Retried twice on a transport error, 408, 429 or 5xx, like
    /// `curl --retry 2`. An error status is an error.
    fn call(&self, bearer: &str, post: bool, path: &str) -> Result<Value, String> {
        let url = format!("{}{}", self.api_url, path);
        let auth = format!("Bearer {bearer}");
        let mut last = String::new();
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1));
            }
            let sent = if post {
                self.agent
                    .post(&url)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .header("Authorization", &auth)
                    .send_empty()
            } else {
                self.agent
                    .get(&url)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .header("Authorization", &auth)
                    .call()
            };
            match sent {
                Ok(mut resp) => {
                    return resp
                        .body_mut()
                        .read_json::<Value>()
                        .map_err(|e| format!("unreadable answer from {path}: {e}"))
                }
                Err(ureq::Error::StatusCode(code)) => {
                    last = format!("{path}: HTTP {code}");
                    if !(code == 408 || code == 429 || code >= 500) {
                        break;
                    }
                }
                Err(e) => last = format!("{path}: {e}"),
            }
        }
        Err(last)
    }

    /// `DELETE <path>`; the answer has no body that matters. Retried like `call`.
    pub fn delete(&self, bearer: &str, path: &str) -> Result<(), String> {
        let url = format!("{}{}", self.api_url, path);
        let auth = format!("Bearer {bearer}");
        let mut last = String::new();
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1));
            }
            let sent = self
                .agent
                .delete(&url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("Authorization", &auth)
                .call();
            match sent {
                Ok(_) => return Ok(()),
                Err(ureq::Error::StatusCode(code)) => {
                    last = format!("{path}: HTTP {code}");
                    if !(code == 408 || code == 429 || code >= 500) {
                        break;
                    }
                }
                Err(e) => last = format!("{path}: {e}"),
            }
        }
        Err(last)
    }

    /// `GET /app`: the App's slug.
    pub fn app_slug(&self, jwt: &str) -> Result<String, String> {
        let v = self.call(jwt, false, "/app")?;
        // jq -r prints "null" for a missing slug.
        Ok(match v.get("slug") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "null".to_string(),
        })
    }

    /// The installation ID: `installation_id` from pool.env, else looked up for the org or repository.
    pub fn installation_id(
        &self,
        jwt: &str,
        configured: &str,
        scope: &str,
    ) -> Result<String, String> {
        if !configured.is_empty() {
            return Ok(configured.to_string());
        }
        let v = self.call(jwt, false, &format!("/{scope}/installation"))?;
        match v.get("id") {
            Some(Value::Number(n)) => Ok(n.to_string()),
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(format!("/{scope}/installation: no id")),
        }
    }

    /// An installation access token.
    pub fn install_token(
        &self,
        jwt: &str,
        configured_id: &str,
        scope: &str,
    ) -> Result<String, String> {
        let id = self.installation_id(jwt, configured_id, scope)?;
        let v = self.call(jwt, true, &format!("/app/installations/{id}/access_tokens"))?;
        match v.get("token") {
            Some(Value::String(t)) if !t.is_empty() => Ok(t.clone()),
            _ => Err("no token in the answer".to_string()),
        }
    }

    /// `total_count` of the registered runners of the scope.
    pub fn runner_count(&self, token: &str, scope: &str) -> Result<String, String> {
        let v = self.call(
            token,
            false,
            &format!("/{scope}/actions/runners?per_page=1"),
        )?;
        match v.get("total_count") {
            Some(Value::Number(n)) => Ok(n.to_string()),
            _ => Err("no total_count in the answer".to_string()),
        }
    }
}

/// A runner of ours on GitHub.
pub struct Runner {
    pub name: String,
    pub status: String,
    pub busy: bool,
}

impl GitHub {
    /// Every runner whose name starts with `<prefix>-`, over all pages (100 at a time). Fails when GitHub
    /// cannot be asked.
    pub fn runners(&self, token: &str, scope: &str, prefix: &str) -> Result<Vec<Runner>, String> {
        let want = format!("{prefix}-");
        let mut found = Vec::new();
        for page in 1.. {
            let v = self.call(
                token,
                false,
                &format!("/{scope}/actions/runners?per_page=100&page={page}"),
            )?;
            let list = match v.get("runners").and_then(Value::as_array) {
                Some(l) if !l.is_empty() => l,
                _ => break,
            };
            for r in list {
                let text = |k: &str| match r.get(k) {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => "null".to_string(),
                };
                let name = text("name");
                if name.starts_with(&want) {
                    found.push(Runner {
                        name,
                        status: text("status"),
                        busy: r.get("busy") == Some(&Value::Bool(true)),
                    });
                }
            }
        }
        Ok(found)
    }
}

/// Reads the key and makes the JWT; the error says what is wrong with the file.
pub fn jwt_from_file(app_id: &str, key_path: &str) -> R<String> {
    let pem = match std::fs::read(key_path) {
        Ok(p) => p,
        Err(_) => return die(format!("{key_path} is not readable")),
    };
    make_jwt(app_id, &pem, now())
}

/// One runner as `gc` reads it: the four fields of `jq -r '"\(.id) \(.name) \(.status) \(.busy)"'`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerRec {
    pub id: String,
    pub name: String,
    pub status: String,
    pub busy: String,
}

/// What `gc` needs from GitHub; tests use a fake.
pub trait RunnerApi {
    /// An installation token.
    fn install_token(&self) -> Result<String, String>;
    /// Page `page` (1, 2, ...) of the runners of the scope, 100 at a time. Empty at the end and when
    /// GitHub cannot be asked.
    fn runners_page(&self, token: &str, page: u32) -> Vec<RunnerRec>;
    /// Removes a registration; a failure is ignored.
    fn delete_runner(&self, token: &str, id: &str);
}

/// A JSON field as jq prints it in a string: strings raw, anything else as JSON, `null` when missing.
fn field_text(r: &Value, key: &str) -> String {
    match r.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "null".to_string(),
    }
}

/// The real GitHub, asked as the App.
pub struct AppRunners {
    gh: GitHub,
    app_id: String,
    key_path: String,
    installation_id: String,
    scope: String,
}

impl AppRunners {
    pub fn new(
        api_url: &str,
        app_id: &str,
        key_path: &str,
        installation_id: &str,
        scope: &str,
    ) -> AppRunners {
        AppRunners {
            gh: GitHub::new(api_url),
            app_id: app_id.to_string(),
            key_path: key_path.to_string(),
            installation_id: installation_id.to_string(),
            scope: scope.to_string(),
        }
    }
}

impl RunnerApi for AppRunners {
    fn install_token(&self) -> Result<String, String> {
        let jwt = jwt_from_file(&self.app_id, &self.key_path).map_err(|d| d.0)?;
        self.gh
            .install_token(&jwt, &self.installation_id, &self.scope)
    }

    fn runners_page(&self, token: &str, page: u32) -> Vec<RunnerRec> {
        let path = format!("/{}/actions/runners?per_page=100&page={page}", self.scope);
        let Ok(v) = self.gh.call(token, false, &path) else {
            return vec![];
        };
        let Some(list) = v.get("runners").and_then(Value::as_array) else {
            return vec![];
        };
        list.iter()
            .map(|r| RunnerRec {
                id: field_text(r, "id"),
                name: field_text(r, "name"),
                status: field_text(r, "status"),
                busy: field_text(r, "busy"),
            })
            .collect()
    }

    fn delete_runner(&self, token: &str, id: &str) {
        let _ = self
            .gh
            .delete(token, &format!("/{}/actions/runners/{id}", self.scope));
    }
}
