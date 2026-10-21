//! `mockingbird server start|stop|status|logs` run `mockingbird serve` in
//! the background (pid and log under `--state-dir`), and `mockingbird
//! doctor` reports what is installed, configured and running. Both print
//! JSON, for agents and scripts.

use std::fs::OpenOptions;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::file_config;
use crate::providers::PRESETS;

/// Default `--state-dir`.
pub const DEFAULT_STATE_DIR: &str = ".mockingbird";

#[derive(Subcommand, Debug)]
pub enum ServerCommand {
    /// Start `mockingbird serve` in the background and wait until it is
    /// ready.
    Start(StartArgs),
    /// Stop the background server.
    Stop(StateArgs),
    /// Whether the background server runs, and its readiness.
    Status(StateArgs),
    /// Print the end of the background server's log.
    Logs {
        #[command(flatten)]
        state: StateArgs,
        /// Lines to print.
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
}

#[derive(Args, Debug, Clone)]
pub struct StateArgs {
    /// Where the pid, log and ports of the background server are kept.
    #[arg(long, env = "MB_STATE_DIR", default_value = DEFAULT_STATE_DIR)]
    pub state_dir: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct StartArgs {
    #[command(flatten)]
    pub state: StateArgs,
    /// Config file; reconciled again whenever it or a contract changes.
    #[arg(short = 'c', long, conflicts_with = "contracts")]
    pub config: Option<PathBuf>,
    /// Bind address [default: 127.0.0.1].
    #[arg(long)]
    pub host: Option<IpAddr>,
    /// Data-plane port [default: 8080].
    #[arg(long)]
    pub port: Option<u16>,
    /// Admin API port [default: 9090].
    #[arg(long)]
    pub admin_port: Option<u16>,
    /// gRPC port [default: 50051].
    #[arg(long)]
    pub grpc_port: Option<u16>,
    /// Faker only, no LLM.
    #[arg(long)]
    pub offline: bool,
    /// Do not watch the config file for changes.
    #[arg(long)]
    pub no_watch: bool,
    /// Allow a non-loopback --host without authentication.
    #[arg(long)]
    pub insecure_no_auth: bool,
    /// How long to wait for readiness.
    #[arg(long, default_value = "60s")]
    pub wait: humantime::Duration,
    /// Contracts to serve without a config file.
    #[arg(required_unless_present = "config")]
    pub contracts: Vec<PathBuf>,
}

fn pid_file(dir: &Path) -> PathBuf {
    dir.join("server.pid")
}

fn log_file(dir: &Path) -> PathBuf {
    dir.join("server.log")
}

fn info_file(dir: &Path) -> PathBuf {
    dir.join("server.json")
}

fn env_port(name: &str) -> Option<u16> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

/// The ports and host `serve` will use: flag, then `MB_*`, then the
/// config file, then the default.
fn effective(args: &StartArgs) -> anyhow::Result<(IpAddr, u16, u16, u16)> {
    let server = match &args.config {
        Some(path) => file_config::load(path)?.server,
        None => file_config::ServerConfig::default(),
    };
    let host = args.host.unwrap_or(server.host);
    let port = args
        .port
        .or_else(|| env_port("MB_PORT"))
        .unwrap_or(server.port);
    let admin = args
        .admin_port
        .or_else(|| env_port("MB_ADMIN_PORT"))
        .unwrap_or(server.admin_port);
    let grpc = args
        .grpc_port
        .or_else(|| env_port("MB_GRPC_PORT"))
        .unwrap_or(server.grpc_port);
    Ok((host, port, admin, grpc))
}

/// The address to reach a server bound to `host` from this machine.
fn reach(host: IpAddr) -> IpAddr {
    if host.is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        host
    }
}

fn url(host: IpAddr, port: u16) -> String {
    match reach(host) {
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .unwrap_or_default()
}

/// `GET <admin>/readyz`: `(reachable, ready, body)`.
async fn readyz(admin_url: &str) -> (bool, bool, Value) {
    match http().get(format!("{admin_url}/readyz")).send().await {
        Ok(resp) => {
            let ready = resp.status().is_success();
            let body = resp.json::<Value>().await.unwrap_or(Value::Null);
            (true, ready, body)
        }
        Err(_) => (false, false, Value::Null),
    }
}

/// The secret references of `config`'s default LLM profile that do not
/// resolve in this process. A config that does not load reports nothing: the
/// server says why when it starts.
pub fn missing_default_secrets(config: &Path) -> Vec<String> {
    let Ok(cfg) = file_config::load(config) else {
        return Vec::new();
    };
    let profile = match &cfg.llm.default_profile {
        Some(name) => cfg.llm.profiles.get(name),
        None if cfg.llm.profiles.len() == 1 => cfg.llm.profiles.values().next(),
        None => None,
    };
    profile
        .map(crate::providers::secret_status)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s["set"] == false)
        .filter_map(|s| s["ref"].as_str().map(String::from))
        .collect()
}

/// `server start`.
pub async fn start(args: &StartArgs) -> anyhow::Result<Value> {
    let dir = &args.state.state_dir;
    if let Some(pid) = read_pid(dir)
        && alive(pid)
    {
        bail!(
            "a background server already runs (pid {pid}); stop it with `mockingbird server stop`"
        );
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let (host, port, admin, grpc) = effective(args)?;
    let exe = std::env::current_exe().context("locating the mockingbird binary")?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve").arg("--admin-port").arg(admin.to_string());
    if let Some(c) = &args.config {
        cmd.arg("-c").arg(c);
        if !args.no_watch {
            cmd.arg("--watch");
        }
    }
    if let Some(h) = args.host {
        cmd.arg("--host").arg(h.to_string());
    }
    if let Some(p) = args.port {
        cmd.arg("--port").arg(p.to_string());
    }
    if let Some(p) = args.grpc_port {
        cmd.arg("--grpc-port").arg(p.to_string());
    }
    if args.offline {
        cmd.arg("--offline");
    }
    if args.insecure_no_auth {
        cmd.arg("--insecure-no-auth");
    }
    cmd.args(&args.contracts);
    let log_path = log_file(dir);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let err = log.try_clone()?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    detach(&mut cmd);
    let mut child = cmd.spawn().context("starting mockingbird serve")?;
    let pid = child.id();
    std::fs::write(pid_file(dir), pid.to_string())?;
    let admin_url = url(host, admin);
    let info = json!({
        "pid": pid,
        "data_plane": url(host, port),
        "admin": admin_url,
        "grpc": format!("{}:{grpc}", reach(host)),
        "config": args.config,
        "contracts": args.contracts,
        "log": log_path,
    });
    std::fs::write(info_file(dir), serde_json::to_vec_pretty(&info)?)?;

    let deadline = Instant::now() + Duration::from(args.wait);
    loop {
        if let Some(status) = child.try_wait()? {
            let _ = std::fs::remove_file(pid_file(dir));
            bail!(
                "mockingbird serve exited ({status}) before it was ready; log:\n{}",
                tail(&log_path, 30)
            );
        }
        let (reachable, ready, body) = readyz(&admin_url).await;
        if ready || (reachable && Instant::now() >= deadline) {
            let mut out = info;
            out["ready"] = json!(ready);
            out["readyz"] = body;
            return Ok(out);
        }
        if Instant::now() >= deadline {
            let mut out = info;
            out["ready"] = json!(false);
            out["warning"] = json!(format!(
                "the Admin API did not answer within {}; see the log",
                args.wait
            ));
            return Ok(out);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Its own process group: a Ctrl-C in the caller's terminal (or the end
    // of an agent's shell) does not stop it.
    cmd.process_group(0);
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_cmd: &mut Command) {}

fn read_pid(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_file(dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn quiet(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    quiet(Command::new("kill").arg("-0").arg(pid.to_string()))
}

#[cfg(not(unix))]
fn alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
}

#[cfg(unix)]
fn terminate(pid: u32, force: bool) -> bool {
    let signal = if force { "-KILL" } else { "-TERM" };
    quiet(Command::new("kill").arg(signal).arg(pid.to_string()))
}

#[cfg(not(unix))]
fn terminate(pid: u32, _force: bool) -> bool {
    quiet(Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]))
}

/// `server stop`.
pub async fn stop(args: &StateArgs) -> anyhow::Result<Value> {
    let dir = &args.state_dir;
    let Some(pid) = read_pid(dir) else {
        return Ok(json!({ "stopped": false, "detail": "no background server recorded" }));
    };
    let mut forced = false;
    if alive(pid) {
        terminate(pid, false);
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if alive(pid) {
            forced = terminate(pid, true);
        }
    }
    let _ = std::fs::remove_file(pid_file(dir));
    Ok(json!({ "stopped": true, "pid": pid, "forced": forced }))
}

/// `server status`.
pub async fn status(args: &StateArgs) -> anyhow::Result<Value> {
    let dir = &args.state_dir;
    let info: Value = std::fs::read(info_file(dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let pid = read_pid(dir);
    let running = pid.is_some_and(alive);
    let mut out = json!({ "running": running, "pid": pid, "server": info });
    if let Some(admin) = info["admin"].as_str() {
        let (reachable, ready, body) = readyz(admin).await;
        out["admin_reachable"] = json!(reachable);
        out["ready"] = json!(ready);
        out["readyz"] = body;
    }
    Ok(out)
}

/// The last `lines` lines of `path`.
pub fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// `server logs`.
pub fn logs(args: &StateArgs, lines: usize) -> anyhow::Result<String> {
    let path = log_file(&args.state_dir);
    if !path.exists() {
        return Err(anyhow!("no log at {}", path.display()));
    }
    Ok(tail(&path, lines))
}

/// `mockingbird doctor`.
#[derive(Args, Debug, Clone)]
pub struct DoctorArgs {
    /// Config file to check [default: ./mockingbird.yaml if present].
    #[arg(short = 'c', long)]
    pub config: Option<PathBuf>,
    #[command(flatten)]
    pub state: StateArgs,
}

pub async fn doctor(args: &DoctorArgs, admin_url: &str) -> Value {
    let mut problems: Vec<String> = Vec::new();
    let shell_env = crate::shell_env::imported();
    let not_set_here = if shell_env.is_some() {
        "not set in this MCP server process: export it where your agent starts, or in your shell profile, then restart the agent"
    } else {
        "not set here"
    };
    let config_path = args
        .config
        .clone()
        .or_else(|| Some(PathBuf::from("mockingbird.yaml")).filter(|p| p.exists()));
    let config = match &config_path {
        None => json!({ "found": false }),
        Some(path) => match file_config::load(path) {
            Ok(cfg) => {
                let mut profiles = Vec::new();
                for (name, p) in &cfg.llm.profiles {
                    let secrets = crate::providers::secret_status(p);
                    for s in &secrets {
                        if s["set"] == false {
                            problems.push(format!(
                                "LLM profile {name:?} needs {} ({not_set_here})",
                                s["ref"].as_str().unwrap_or_default()
                            ));
                        }
                    }
                    profiles.push(json!({
                        "name": name,
                        "base_url": p.base_url,
                        "model": p.model,
                        "secrets": secrets,
                    }));
                }
                json!({
                    "found": true,
                    "path": path,
                    "valid": true,
                    "default_profile": cfg.llm.default_profile,
                    "profiles": profiles,
                    "mocks": cfg.mocks.keys().collect::<Vec<_>>(),
                })
            }
            Err(e) => {
                problems.push(format!("{} does not load: {e:#}", path.display()));
                json!({ "found": true, "path": path, "valid": false, "error": format!("{e:#}") })
            }
        },
    };
    let keys: Vec<Value> = PRESETS
        .iter()
        .filter_map(|p| p.key_env.map(|e| (p.id, e)))
        .chain([("cloudflare-ai-gateway", "CF_AIG_API_KEY")])
        .map(|(id, env)| {
            json!({
                "provider": id,
                "env": env,
                "set": std::env::var(env).is_ok_and(|v| !v.trim().is_empty()),
            })
        })
        .collect();
    let (reachable, ready, readyz_body) = readyz(admin_url).await;
    let background = status(&args.state).await.unwrap_or(Value::Null);
    let docker = Command::new("docker")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    if !reachable {
        problems.push(format!(
            "no Admin API at {admin_url}: start one with `mockingbird server start -c mockingbird.yaml`"
        ));
    }
    if config_path.is_none() {
        problems.push("no mockingbird.yaml here: create one with `mockingbird init`".into());
    }
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "binary": std::env::current_exe().ok(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "config": config,
        "provider_keys": keys,
        "admin": { "url": admin_url, "reachable": reachable, "ready": ready, "readyz": readyz_body },
        "background_server": background,
        "docker": docker,
        "shell_env": shell_env.map(|names| json!({ "imported": names })),
        "problems": problems,
    })
}
