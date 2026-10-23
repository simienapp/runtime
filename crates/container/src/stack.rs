//! The local stack, like `supabase start`: Postgres with pg_zega, and Zega,
//! one pair per project. kebele runs the containers (it talks to Docker
//! over its socket); data lives in Docker volumes and survives `stop`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use project::portmap::{Claim, PortMap};
use project::{Project, write_private};

/// The images this simien pins.
pub const POSTGRES_IMAGE: &str = "addiscorp/pg_zega:0.1";
pub const ZEGA_IMAGE: &str = "addiscorp/zega:0.2.0";
pub const SERVICES: [&str; 2] = ["postgres", "zega"];

/// Every Simien project's stack is a kebele stack named `simien-<project>`.
pub const PREFIX: &str = "simien-";

pub fn stack_name(project: &Project) -> String {
    let name: String = project
        .name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    format!("{PREFIX}{name}")
}

/// The kebele stack description, secrets included.
pub fn describe(project: &Project) -> Result<Value, String> {
    let ports = project.ports()?;
    let token = project.secret("zega_token")?;
    Ok(json!({
        "name": stack_name(project),
        "services": {
            "zega": {
                "image": ZEGA_IMAGE,
                "env": { "ZEGA_TOKEN": token },
                "ports": [{ "host": ports.zega, "container": 9342 }],
                "volumes": [{ "volume": "zega", "path": "/var/lib/zega" }],
            },
            "postgres": {
                "image": POSTGRES_IMAGE,
                "env": {
                    "POSTGRES_PASSWORD": project.secret("postgres_password")?,
                    "ZEGA_PASSWORD": token,
                    "ZEGA_URL": "http://zega:9342",
                },
                "ports": [{ "host": ports.postgres, "container": 5432 }],
                "volumes": [{ "volume": "postgres", "path": "/var/lib/postgresql/data" }],
                "health": { "command": ["pg_isready", "-U", "postgres"] },
                "dependsOn": ["zega"],
            },
        },
    }))
}

/// Writes the description where only the user can read it, next to the
/// secrets it holds: `.simien/kebele.json`.
fn write_description(project: &Project) -> Result<PathBuf, String> {
    let file = project.state_dir().join("kebele.json");
    let text = serde_json::to_string_pretty(&describe(project)?).expect("json");
    write_private(&file, &text)?;
    Ok(file)
}

/// The kebele Simien starts when neither the project nor PATH has one:
/// installed on first use into Simien's cache and run on its Node layer,
/// pre-approved in permissions.json, so a new app needs no Bun.
pub const KEBELE: &str = "kebele@0.11.0";

/// How to start kebele.
pub enum Kebele {
    /// A kebele the project or PATH has; it runs as it is.
    Installed(PathBuf),
    /// Simien's own, in kebele's sandbox plus the one Docker socket it may
    /// use (given with --socket, so it needn't read ~/.docker, a secret).
    Simien(Box<SimienKebele>),
}

/// Simien's own kebele: the installed program, its sandbox, and the socket.
pub struct SimienKebele {
    program: packages::exec::Program,
    sandbox: packages::sandbox::Prepared,
    socket: PathBuf,
}

impl Kebele {
    fn command(&self, args: &[&str]) -> Result<Command, String> {
        match self {
            Kebele::Installed(program) => {
                let mut command = Command::new(program);
                command.args(args).arg("--nopretty");
                Ok(command)
            }
            Kebele::Simien(own) => {
                let SimienKebele { program, sandbox, socket } = own.as_ref();
                let mut all: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
                all.extend(["--socket".to_string(), socket.display().to_string(), "--nopretty".to_string()]);
                program.command(&sandbox.sandbox, &all).map_err(|err| format!("starting kebele: {err}"))
            }
        }
    }

    fn name(&self) -> String {
        match self {
            Kebele::Installed(program) => program.display().to_string(),
            Kebele::Simien(_) => KEBELE.to_string(),
        }
    }
}

/// The kebele program: the project's own (node_modules/.bin, the version it
/// pins) first, then the one on PATH, else Simien's own ([`KEBELE`]).
pub fn kebele(project_root: Option<&Path>) -> Result<Kebele, String> {
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("kebele"))
            .find(|candidate| candidate.is_file())
    });
    let local = project_root
        .map(|root| root.join("node_modules/.bin/kebele"))
        .filter(|candidate| candidate.is_file());
    if let Some(program) = local.or(on_path) {
        return Ok(Kebele::Installed(program));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("no HOME to find Docker's socket from")?;
    let socket = docker_socket(&home).ok_or(
        "no Docker socket found. Start Docker Desktop, OrbStack or Colima",
    )?;
    let root = match project_root {
        Some(root) => root.to_path_buf(),
        None => std::env::current_dir().map_err(|err| err.to_string())?,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| err.to_string())?;
    let program = runtime
        .block_on(packages::exec::resolve(&root, KEBELE, None))
        .map_err(|err| format!("installing {KEBELE}: {err:#}"))?;
    let mut sandbox = program.sandbox(&root).map_err(|err| format!("kebele's sandbox: {err:#}"))?;
    // Connecting to a unix socket is writing to it.
    sandbox.sandbox = sandbox.sandbox.allow_write(&socket);
    Ok(Kebele::Simien(Box::new(SimienKebele { program, sandbox, socket })))
}

/// The Docker socket the docker CLI would use (its current context), then
/// where Docker Desktop, OrbStack, Colima and Rancher put theirs: the order
/// kebele looks in. Read here, outside kebele's sandbox: ~/.docker holds
/// registry credentials, which kebele is never given.
pub fn docker_socket(home: &Path) -> Option<PathBuf> {
    let context = || -> Option<PathBuf> {
        let config: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".docker/config.json")).ok()?).ok()?;
        let name = config["currentContext"].as_str().filter(|name| !name.is_empty() && *name != "default")?;
        let id = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(name.as_bytes()));
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".docker/contexts/meta").join(id).join("meta.json")).ok()?)
                .ok()?;
        meta["Endpoints"]["docker"]["Host"].as_str()?.strip_prefix("unix://").map(PathBuf::from)
    };
    context()
        .into_iter()
        .chain([
            PathBuf::from("/var/run/docker.sock"),
            home.join(".docker/run/docker.sock"),
            home.join(".orbstack/run/docker.sock"),
            home.join(".colima/default/docker.sock"),
            home.join(".rd/docker.sock"),
        ])
        .find(|path| path.exists())
}

/// Runs kebele with `args` and returns its JSON answer.
fn run(kebele: &Kebele, args: &[&str]) -> Result<Value, String> {
    let output = kebele
        .command(args)?
        .output()
        .map_err(|err| format!("running {}: {err}", kebele.name()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(kebele_error(&output.stderr, &stdout));
    }
    serde_json::from_str(stdout.trim())
        .map_err(|_| format!("kebele gave an answer that isn't JSON: {}", stdout.trim()))
}

/// kebele's error message from its JSON failure, or its raw output.
fn kebele_error(stderr: &[u8], stdout: &str) -> String {
    let text = String::from_utf8_lossy(stderr);
    serde_json::from_str::<Value>(text.trim())
        .ok()
        .and_then(|value| value["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| {
            let raw = if text.trim().is_empty() { stdout.trim() } else { text.trim() };
            format!("kebele failed: {raw}")
        })
}

/// Runs kebele and hands each JSON line it prints to `on_line`.
fn stream(kebele: &Kebele, args: &[&str], mut on_line: impl FnMut(Value)) -> Result<(), String> {
    let mut child = kebele
        .command(args)?
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("running {}: {err}", kebele.name()))?;
    let stdout = child.stdout.take().expect("piped");
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            on_line(value);
        }
    }
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    let status = child.wait().map_err(|err| err.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(kebele_error(&stderr, ""))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Port {
    pub host: u16,
    pub container: u16,
}

/// A container as Docker sees it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Service {
    pub service: String,
    pub state: String,
    #[serde(default)]
    pub health: Option<String>,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub ports: Vec<Port>,
}

#[derive(Debug, Deserialize)]
struct StackStatus {
    #[serde(default)]
    services: Vec<Service>,
}

/// Whether each database answers: Postgres's health check, and Zega's
/// /health over HTTP with the project's token.
#[derive(Debug, Clone, Serialize)]
pub struct Readiness {
    pub postgres: Endpoint,
    pub zega: Endpoint,
}

#[derive(Debug, Clone, Serialize)]
pub struct Endpoint {
    pub url: String,
    pub ready: bool,
}

impl Readiness {
    pub fn ready(&self) -> bool {
        self.postgres.ready && self.zega.ready
    }
}

pub fn containers(project: &Project) -> Result<Vec<Service>, String> {
    let kebele = kebele(Some(&project.root))?;
    let value = run(&kebele, &["status", &stack_name(project)])?;
    let status: StackStatus = serde_json::from_value(value).map_err(|err| err.to_string())?;
    Ok(status.services)
}

pub fn readiness(project: &Project, services: &[Service]) -> Result<Readiness, String> {
    let postgres = services
        .iter()
        .any(|s| s.service == "postgres" && s.health.as_deref() == Some("healthy"));
    let ports = project.ports()?;
    let zega = zega_answers(ports.zega, &project.secret("zega_token")?);
    Ok(Readiness {
        postgres: Endpoint {
            url: format!("postgres://postgres@127.0.0.1:{}/postgres", ports.postgres),
            ready: postgres,
        },
        zega: Endpoint {
            url: format!("http://127.0.0.1:{}", ports.zega),
            ready: zega,
        },
    })
}

/// `GET /health` answered 2xx.
fn zega_answers(port: u16, token: &str) -> bool {
    let Ok(mut stream) =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request = format!(
        "GET /health HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nauthorization: Bearer {token}\r\nconnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 12];
    stream.read_exact(&mut head).is_ok() && head.starts_with(b"HTTP/1.1 2")
}

/// Every Simien project's containers on this machine. `project_root`, when
/// run inside a project, is where to look for its kebele first.
pub fn all(project_root: Option<&Path>) -> Result<Vec<(String, Vec<Service>)>, String> {
    #[derive(Deserialize)]
    struct Listed {
        name: String,
        #[serde(default)]
        services: Vec<Service>,
    }
    #[derive(Deserialize)]
    struct List {
        stacks: Vec<Listed>,
    }
    let kebele = kebele(project_root)?;
    let list: List = serde_json::from_value(run(&kebele, &["ls"])?).map_err(|err| err.to_string())?;
    Ok(list
        .stacks
        .into_iter()
        .filter_map(|stack| {
            let project = stack.name.strip_prefix(PREFIX)?.to_string();
            Some((project, stack.services))
        })
        .collect())
}

/// Starts the stack, reporting kebele's events, then waits until both
/// databases answer. Its ports are recorded in `map` as the project's first;
/// another project's ports are refused.
pub fn start(project: &Project, map: &PortMap, timeout: Duration, mut on_event: impl FnMut(Value)) -> Result<Readiness, String> {
    let ports = project.ports()?;
    map.claim(&project.root, Claim { postgres: ports.postgres, zega: ports.zega })?;
    let kebele = kebele(Some(&project.root))?;
    let file = write_description(project)?;
    let seconds = timeout.as_secs().to_string();
    let file_arg = file.to_string_lossy().into_owned();
    stream(&kebele, &["up", &file_arg, "--timeout", &seconds], &mut on_event)?;
    let deadline = Instant::now() + timeout;
    loop {
        let now = readiness(project, &containers(project)?)?;
        if now.ready() {
            return Ok(now);
        }
        if Instant::now() > deadline {
            return Err("the local stack did not become ready; see `simien container logs`".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// What `update` did: the images pulled and the services whose containers
/// ran an older copy (removed, then started again on the new image).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Pulled {
    pub images: Vec<PulledImage>,
    pub replaced: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PulledImage {
    pub image: String,
    pub id: String,
}

/// Pulls the pinned images and removes containers on older ones (kebele
/// 0.10's `pull`); the data stays. Starting again is the caller's.
pub fn pull(project: &Project, on_event: impl FnMut(Value)) -> Result<Pulled, String> {
    let kebele = kebele(Some(&project.root))?;
    let file = write_description(project)?;
    let file = file.to_string_lossy().into_owned();
    // kebele prints its progress events, then the result, one JSON per line.
    let mut last = None;
    let mut on_event = on_event;
    stream(&kebele, &["pull", &file], |line| {
        if line.get("event").is_some() {
            on_event(line);
        } else {
            last = Some(line);
        }
    })
    .map_err(|err| {
        if err.contains("unknown command") {
            "simien container update needs kebele 0.10 or newer (it has `kebele pull`)".to_string()
        } else {
            err
        }
    })?;
    let result = last.ok_or("kebele pull gave no result")?;
    serde_json::from_value(result).map_err(|err| format!("kebele pull's result: {err}"))
}

/// Stops and removes the containers; the data stays. Returns the removed.
pub fn stop(project: &Project) -> Result<Vec<String>, String> {
    let kebele = kebele(Some(&project.root))?;
    let value = run(&kebele, &["down", &stack_name(project)])?;
    Ok(strings(&value["removed"]))
}

/// Removes the containers and deletes their volumes. Returns the volumes.
pub fn delete_data(project: &Project) -> Result<Vec<String>, String> {
    let kebele = kebele(Some(&project.root))?;
    // `down` deletes the volumes a description declares.
    let file = write_description(project)?;
    let value = run(&kebele, &["down", &file.to_string_lossy(), "--volumes"])?;
    Ok(strings(&value["volumes"]))
}

/// The containers' output, one `{service, stream, text}` per line.
pub fn logs(
    project: &Project,
    service: Option<&str>,
    tail: Option<&str>,
    follow: bool,
    on_line: impl FnMut(Value),
) -> Result<(), String> {
    let kebele = kebele(Some(&project.root))?;
    let name = stack_name(project);
    let mut args = vec!["logs", name.as_str()];
    if let Some(service) = service {
        args.push(service);
    }
    if let Some(tail) = tail {
        args.extend(["--tail", tail]);
    }
    if follow {
        args.push("--follow");
    }
    stream(&kebele, &args, on_line)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod socket_tests {
    use super::docker_socket;

    #[test]
    fn the_docker_clis_current_context_names_the_socket() {
        let home = tempfile::tempdir().unwrap();
        let socket = home.path().join("custom/docker.sock");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(&socket, "").unwrap();
        // The docker CLI keeps a context's endpoint under the sha256 of its name.
        let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(b"colima"));
        let meta = home.path().join(".docker/contexts/meta").join(&digest);
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(home.path().join(".docker/config.json"), r#"{ "currentContext": "colima" }"#).unwrap();
        std::fs::write(
            meta.join("meta.json"),
            format!(r#"{{ "Endpoints": {{ "docker": {{ "Host": "unix://{}" }} }} }}"#, socket.display()),
        )
        .unwrap();
        assert_eq!(docker_socket(home.path()), Some(socket));
    }
}
