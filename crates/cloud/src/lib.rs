//! `simien login` / `simien logout`: who you are on Simien Cloud (APS 6).
//!
//! Sign-in goes through Simien's own accounts, account.simien.cloud, with its
//! device sign-in: the CLI asks for a code, opens account.simien.cloud's
//! `/activate` page with the code filled in, and you sign in there with
//! GitHub, Apple or Google and approve. Meanwhile the CLI asks every few
//! seconds whether the code was approved; once it is, it gets a Simien
//! session token. Nothing here talks to a provider, and the CLI holds no
//! secret but the session it is given.
//!
//! The session is kept, readable only by you, in
//! `~/.config/simien/credentials.json`. It is an ordinary account session:
//! it shows under Devices on account.simien.cloud and can be signed out
//! there, as `simien logout` does here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod command;

/// Simien's accounts.
pub const ACCOUNT_ORIGIN: &str = "https://account.simien.cloud";

/// A signed-in user, as kept on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// The Simien account id.
    pub account: u64,
    /// The provider login it was made with, `octocat`, when there is one.
    pub login: Option<String>,
    /// The account's display name, when it has one.
    pub name: Option<String>,
    /// The session token: `Authorization: Bearer` for account.simien.cloud
    /// and api.simien.cloud.
    pub token: String,
}

impl Session {
    /// How to call the person in output: their login, name, or account id.
    pub fn label(&self) -> String {
        self.login
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| format!("account {}", self.account))
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

/// What account.simien.cloud answers the first request with: the code to show.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    /// `https://account.simien.cloud/activate`.
    pub verification_uri: String,
    /// The same page with the code filled in: what the browser opens.
    pub verification_uri_complete: String,
    /// Seconds the code is good for.
    pub expires_in: u64,
    /// Seconds to wait between polls.
    pub interval: u64,
}

pub struct Client {
    origin: String,
    http: reqwest::blocking::Client,
}

impl Client {
    /// A client for the accounts at `origin` (`ACCOUNT_ORIGIN`, or a
    /// stand-in in tests).
    pub fn new(origin: impl Into<String>) -> Result<Self, String> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("simien/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|err| err.to_string())?;
        Ok(Self { origin: origin.into(), http })
    }

    fn post(&self, path: &str, body: &Value) -> Result<(u16, Value), String> {
        let response = self
            .http
            .post(format!("{}{path}", self.origin))
            .json(body)
            .send()
            .map_err(|err| format!("could not reach {}: {err}", self.origin))?;
        let status = response.status().as_u16();
        let body = response.json().unwrap_or(Value::Null);
        Ok((status, body))
    }

    /// Step 1: a code for the person to approve. The device name and
    /// platform are what account.simien.cloud shows on the approval page and
    /// under Devices.
    pub fn request_code(&self) -> Result<DeviceCode, String> {
        let name = format!("Simien CLI on {}", platform_name());
        let (status, body) = self.post("/auth/device/start", &json!({ "name": name, "platform": "cli" }))?;
        if status != 200 {
            return Err(refusal("could not start sign-in", status, &body));
        }
        serde_json::from_value(body).map_err(|err| format!("account.simien.cloud's device code answer: {err}"))
    }

    /// Step 2: waits until the code is approved (or expires), and returns
    /// the session token. `sleep` is how time passes, so tests need not wait.
    pub fn wait_for_token(&self, code: &DeviceCode, sleep: &dyn Fn(Duration)) -> Result<String, String> {
        let mut interval = code.interval;
        let mut waited = 0;
        loop {
            if waited >= code.expires_in {
                return Err(expired());
            }
            sleep(Duration::from_secs(interval));
            waited += interval;
            let (status, body) = self.post("/auth/device/token", &json!({ "device_code": code.device_code }))?;
            match status {
                200 => {
                    return body
                        .get("token")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| "account.simien.cloud approved the code but sent no token".into());
                }
                202 => {}
                // Asked too soon: account.simien.cloud says how long to wait.
                429 => interval = body.get("interval").and_then(Value::as_u64).unwrap_or(interval + 5),
                410 => return Err(expired()),
                _ => return Err(refusal("sign-in failed", status, &body)),
            }
        }
    }

    /// Who a session token belongs to.
    pub fn me(&self, token: &str) -> Result<Session, String> {
        let response = self
            .http
            .get(format!("{}/auth/me", self.origin))
            .bearer_auth(token)
            .send()
            .map_err(|err| format!("could not reach {}: {err}", self.origin))?;
        let body: Value = response.json().map_err(|err| format!("account.simien.cloud's /auth/me answer: {err}"))?;
        if body.get("signedIn").and_then(Value::as_bool) != Some(true) {
            return Err("account.simien.cloud does not recognise this session".into());
        }
        let account = body.get("account").and_then(Value::as_u64).ok_or("/auth/me has no account")?;
        let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_string);
        Ok(Session { account, login: text("login"), name: text("name"), token: token.to_string() })
    }

    /// The whole sign-in: a code (handed to `show`), the wait, who it is.
    pub fn sign_in(&self, show: &dyn Fn(&DeviceCode), sleep: &dyn Fn(Duration)) -> Result<Session, String> {
        let code = self.request_code()?;
        show(&code);
        let token = self.wait_for_token(&code, sleep)?;
        self.me(&token)
    }
}

fn expired() -> String {
    "the code expired before it was approved; run `simien login` again".into()
}

fn refusal(what: &str, status: u16, body: &Value) -> String {
    match body.get("error").and_then(Value::as_str) {
        Some(error) => format!("{what}: {error} ({status})"),
        None => format!("{what}: account.simien.cloud answered {status}"),
    }
}

fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    }
}
