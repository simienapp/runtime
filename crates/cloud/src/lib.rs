//! `simien login` / `simien logout`: who you are on Simien Cloud (APS 6).
//!
//! Sign-in is GitHub's device flow with Simien's OAuth App: the CLI shows a
//! code, you enter it at github.com/login/device, and GitHub hands the CLI a
//! token. The app has no client secret here and needs none: the device flow
//! is made for programs that cannot keep one.
//!
//! GitHub's token names you (`GET /user`) and is kept, readable only by you,
//! in `~/.config/simien/credentials.json`. It expires after eight hours;
//! once the control plane exists, login trades it for a Simien session once
//! and keeps that instead, so nothing here depends on refreshing it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod command;

/// Simien's OAuth App on GitHub (simienapp). Not a secret: GitHub shows it
/// on every sign-in page.
pub const GITHUB_CLIENT_ID: &str = "Ov23liAruacAFHLZXxn8";

/// Where GitHub is: the real one, or a stand-in in tests.
#[derive(Debug, Clone)]
pub struct GitHub {
    /// `https://github.com`: the device and token endpoints.
    pub web: String,
    /// `https://api.github.com`: who a token belongs to.
    pub api: String,
    pub client_id: String,
}

impl Default for GitHub {
    fn default() -> Self {
        Self {
            web: "https://github.com".into(),
            api: "https://api.github.com".into(),
            client_id: GITHUB_CLIENT_ID.into(),
        }
    }
}

/// A signed-in user, as kept on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// The GitHub login, `octocat`.
    pub login: String,
    /// GitHub's numeric user id: a login can be renamed, the id cannot.
    pub github_id: u64,
    pub github_token: String,
    /// Seconds since the epoch; none when GitHub gave no expiry.
    pub expires_at: Option<u64>,
}

impl Session {
    pub fn expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
    }
}

/// The credentials file.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// `~/.config/simien/credentials.json`, next to the port map.
    pub fn user() -> Result<Self, String> {
        let home = std::env::home_dir().ok_or("no home directory to keep credentials in")?;
        Ok(Self::at(home.join(".config").join("simien").join("credentials.json")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<Option<Session>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|err| format!("{}: {err}", self.path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(format!("{}: {err}", self.path.display())),
        }
    }

    /// Writes the session readable by its owner only, whole or not at all.
    pub fn write(&self, session: &Session) -> Result<(), String> {
        let fail = |err: std::io::Error| format!("{}: {err}", self.path.display());
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(fail)?;
        }
        let partial = self.path.with_extension("json.partial");
        let text = serde_json::to_string_pretty(session).expect("a session is serializable");
        write_private(&partial, text.as_bytes()).map_err(fail)?;
        std::fs::rename(&partial, &self.path).map_err(fail)
    }

    /// Removes the file; false when there was none.
    pub fn remove(&self) -> Result<bool, String> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(format!("{}: {err}", self.path.display())),
        }
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// What GitHub answers the first request with: the code to show.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// Seconds the code is good for.
    pub expires_in: u64,
    /// Seconds to wait between polls.
    pub interval: u64,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub access_token: String,
    /// Seconds from now; none for a token that does not expire.
    pub expires_in: Option<u64>,
}

pub struct Client {
    github: GitHub,
    http: reqwest::blocking::Client,
}

impl Client {
    pub fn new(github: GitHub) -> Result<Self, String> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("simien/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|err| err.to_string())?;
        Ok(Self { github, http })
    }

    fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, String> {
        let url = format!("{}{path}", self.github.web);
        let response = self
            .http
            .post(&url)
            .header("Accept", "application/json")
            .form(form)
            .send()
            .map_err(|err| format!("could not reach GitHub: {err}"))?;
        let status = response.status();
        let body: Value = response
            .json()
            .map_err(|err| format!("GitHub answered {status} with something that isn't JSON: {err}"))?;
        if !status.is_success() && body.get("error").is_none() {
            return Err(format!("GitHub answered {status}: {body}"));
        }
        Ok(body)
    }

    /// Step 1: a code for the user to enter.
    pub fn request_code(&self) -> Result<DeviceCode, String> {
        let body = self.post_form("/login/device/code", &[("client_id", &self.github.client_id), ("scope", "")])?;
        if let Some(error) = github_error(&body) {
            return Err(error);
        }
        serde_json::from_value(body).map_err(|err| format!("GitHub's device code answer: {err}"))
    }

    /// Step 2: waits until the user has entered the code (or refused, or
    /// let it expire). `sleep` is how time passes, so tests need not wait.
    pub fn wait_for_token(&self, code: &DeviceCode, sleep: &dyn Fn(Duration)) -> Result<Token, String> {
        let mut interval = code.interval;
        let mut waited = 0;
        loop {
            if waited >= code.expires_in {
                return Err("the code expired before it was entered; run `simien login` again".into());
            }
            sleep(Duration::from_secs(interval));
            waited += interval;
            let body = self.post_form(
                "/login/oauth/access_token",
                &[
                    ("client_id", &self.github.client_id),
                    ("device_code", &code.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )?;
            match body.get("error").and_then(Value::as_str) {
                None => {
                    let access_token = body
                        .get("access_token")
                        .and_then(Value::as_str)
                        .ok_or("GitHub's token answer has no access_token")?
                        .to_string();
                    let expires_in = body.get("expires_in").and_then(Value::as_u64);
                    return Ok(Token { access_token, expires_in });
                }
                Some("authorization_pending") => {}
                // GitHub says how long to wait from now on; five seconds more
                // when it doesn't.
                Some("slow_down") => interval = body.get("interval").and_then(Value::as_u64).unwrap_or(interval + 5),
                Some("expired_token") => return Err("the code expired before it was entered; run `simien login` again".into()),
                Some("access_denied") => return Err("sign-in was cancelled on GitHub".into()),
                Some(_) => return Err(github_error(&body).expect("an error field")),
            }
        }
    }

    /// Who a token belongs to: (login, id).
    pub fn user(&self, token: &str) -> Result<(String, u64), String> {
        let url = format!("{}/user", self.github.api);
        let response = self
            .http
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .bearer_auth(token)
            .send()
            .map_err(|err| format!("could not reach GitHub: {err}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("GitHub would not say who signed in ({status})"));
        }
        let body: Value = response.json().map_err(|err| format!("GitHub's user answer: {err}"))?;
        let login = body.get("login").and_then(Value::as_str).ok_or("GitHub's user answer has no login")?;
        let id = body.get("id").and_then(Value::as_u64).ok_or("GitHub's user answer has no id")?;
        Ok((login.to_string(), id))
    }

    /// The whole sign-in: a code (handed to `show`), the wait, who it is.
    pub fn sign_in(&self, show: &dyn Fn(&DeviceCode), sleep: &dyn Fn(Duration)) -> Result<Session, String> {
        let code = self.request_code()?;
        show(&code);
        let token = self.wait_for_token(&code, sleep)?;
        let (login, github_id) = self.user(&token.access_token)?;
        Ok(Session {
            login,
            github_id,
            github_token: token.access_token,
            expires_at: token.expires_in.map(|seconds| now() + seconds),
        })
    }
}

fn github_error(body: &Value) -> Option<String> {
    let error = body.get("error")?.as_str()?;
    let description = body.get("error_description").and_then(Value::as_str);
    Some(match error {
        "device_flow_disabled" => "device sign-in is turned off for Simien's GitHub app".to_string(),
        "incorrect_client_credentials" => "GitHub does not know Simien's app (client id)".to_string(),
        _ => match description {
            Some(description) => format!("GitHub: {description} ({error})"),
            None => format!("GitHub: {error}"),
        },
    })
}

/// Seconds since the epoch.
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}
