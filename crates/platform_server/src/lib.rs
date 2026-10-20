use anyhow::{Result, anyhow};
use platform::{Env, Fs, Io, Net, Platform, Ports, Process, Random, Time};
use rand::RngCore;
use simien_core::Extension;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct ServerPlatform {
    fs: ServerFs,
    env: ServerEnv,
    io: ServerIo,
    process: ServerProcess,
    time: ServerTime,
    random: ServerRandom,
    net: ServerNet,
    ports: ServerPorts,
}

impl Default for ServerPlatform {
    fn default() -> Self {
        Self {
            fs: ServerFs,
            env: ServerEnv,
            io: ServerIo,
            process: ServerProcess,
            time: ServerTime,
            random: ServerRandom,
            net: ServerNet,
            ports: ServerPorts::default(),
        }
    }
}

impl Platform for ServerPlatform {
    fn fs(&self) -> &dyn Fs {
        &self.fs
    }

    fn env(&self) -> &dyn Env {
        &self.env
    }

    fn io(&self) -> &dyn Io {
        &self.io
    }

    fn process(&self) -> &dyn Process {
        &self.process
    }

    fn time(&self) -> &dyn Time {
        &self.time
    }

    fn random(&self) -> &dyn Random {
        &self.random
    }

    fn net(&self) -> &dyn Net {
        &self.net
    }

    fn ports(&self) -> &dyn Ports {
        &self.ports
    }
}

pub struct ServerFs;

impl Fs for ServerFs {
    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        std::fs::read(path).map_err(Into::into)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        std::fs::write(path, bytes).map_err(Into::into)
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path).map_err(Into::into)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn cwd(&self) -> Result<PathBuf> {
        std::env::current_dir().map_err(Into::into)
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        std::fs::canonicalize(path).map_err(Into::into)
    }

    fn current_exe(&self) -> Result<PathBuf> {
        std::env::current_exe().map_err(Into::into)
    }
}

pub struct ServerEnv;

impl Env for ServerEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn vars(&self) -> Vec<(String, String)> {
        std::env::vars().collect()
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        // Process environment mutation is unsafe in Rust 2024.
        unsafe { std::env::set_var(key, value) };
        Ok(())
    }
}

#[derive(Default)]
pub struct ServerIo;

impl Io for ServerIo {
    fn stdout(&self, message: &str) -> Result<()> {
        let mut out = std::io::stdout();
        out.write_all(message.as_bytes())?;
        out.flush()?;
        Ok(())
    }

    fn stderr(&self, message: &str) -> Result<()> {
        let mut err = std::io::stderr();
        err.write_all(message.as_bytes())?;
        err.flush()?;
        Ok(())
    }
}

pub struct ServerProcess;

impl Process for ServerProcess {
    fn args(&self) -> Vec<String> {
        std::env::args().collect()
    }

    fn pid(&self) -> u32 {
        std::process::id()
    }

    fn exit(&self, code: i32) -> ! {
        std::process::exit(code)
    }
}

pub struct ServerTime;

impl Time for ServerTime {
    fn now_unix_ms(&self) -> i64 {
        let now = std::time::SystemTime::now();
        now.duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn sleep_ms(&self, milliseconds: u64) -> Result<()> {
        std::thread::sleep(std::time::Duration::from_millis(milliseconds));
        Ok(())
    }
}

pub struct ServerRandom;

impl Random for ServerRandom {
    fn fill_bytes(&self, buffer: &mut [u8]) -> Result<()> {
        let mut rng = rand::thread_rng();
        rng.fill_bytes(buffer);
        Ok(())
    }
}

pub struct ServerNet;

impl Net for ServerNet {
    fn fetch_bytes(&self, _url: &str, _method: &str, _body: Option<&[u8]>) -> Result<Vec<u8>> {
        Err(anyhow!(
            "server net adapter fetch not implemented in MVP scaffold"
        ))
    }
}

#[derive(Default)]
pub struct ServerPorts {
    reserved: Mutex<Vec<u16>>,
}

impl Ports for ServerPorts {
    fn reserve(&self, preferred: Option<u16>) -> Result<u16> {
        let mut reserved = self
            .reserved
            .lock()
            .map_err(|_| anyhow!("failed to lock port registry"))?;
        if let Some(port) = preferred
            && !reserved.contains(&port)
        {
            reserved.push(port);
            return Ok(port);
        }
        let mut candidate = 40000u16;
        while reserved.contains(&candidate) && candidate < u16::MAX {
            candidate = candidate.saturating_add(1);
        }
        reserved.push(candidate);
        Ok(candidate)
    }

    fn release(&self, port: u16) -> Result<()> {
        let mut reserved = self
            .reserved
            .lock()
            .map_err(|_| anyhow!("failed to lock port registry"))?;
        reserved.retain(|p| *p != port);
        Ok(())
    }
}

pub fn extensions_for_php_server() -> Vec<Extension> {
    let mut extensions = web_host::extensions();
    extensions.extend(deka_host::extensions());
    extensions
}

/// Same as [`extensions_for_php_server`], but the net bridge enforces the
/// given policy instead of resolving the per-execution security context on
/// every dispatch. Lets tests give each isolate pool its own net policy
/// instead of depending on the executing thread's context (deka#537).
pub fn extensions_for_php_server_with_net_policy(
    policy: deka_host::SecurityPolicy,
) -> Vec<Extension> {
    let mut extensions = web_host::extensions();
    extensions.extend(deka_host::extensions_with_net_policy(policy));
    extensions
}
