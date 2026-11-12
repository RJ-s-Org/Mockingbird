//! `mockingbird server start|stop|status|logs` run `mockingbird serve` in
//! the background (pid and log under `--state-dir`), and `mockingbird
//! doctor` reports what is installed, configured and running. Both print
//! JSON, for agents and scripts.

use std::fs::OpenOptions;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
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
    /// Data-plane port [default: the config's `server.port`, else 8080].
    /// With `-c`, a port the config leaves at its default that is taken is
    /// replaced by a free one, written into the config; a port set by a
    /// flag, `MB_*` variable or config key is kept, and the start fails
    /// when it is taken. A port given by this flag (or `--admin-port`,
    /// `--grpc-port`) that differs from the config is written into it, so
    /// that the config and the running server agree.
    #[arg(long)]
    pub port: Option<u16>,
    /// Admin API port [default: the config's `server.admin_port`, else
    /// 9090; picked free like `--port`].
    #[arg(long)]
    pub admin_port: Option<u16>,
    /// gRPC port [default: the config's `server.grpc_port`, else 50051;
    /// picked free like `--port`].
    #[arg(long)]
    pub grpc_port: Option<u16>,
    /// Faker only, no LLM.
    #[arg(long)]
    pub offline: bool,
    /// Also run the HTTP(S) proxy (`proxy.enabled`); see `mockingbird proxy
    /// env`.
    #[arg(long)]
    pub proxy: bool,
    /// Do not watch the config file for changes.
    #[arg(long)]
    pub no_watch: bool,
    /// Allow a non-loopback --host without authentication.
    #[arg(long)]
    pub insecure_no_auth: bool,
    /// Record every LLM answer into this snapshot directory (`serve
    /// --record`).
    #[arg(long, value_name = "DIR")]
    pub record: Option<PathBuf>,
    /// Answer from this snapshot directory without calling an LLM (`serve
    /// --replay`).
    #[arg(long, value_name = "DIR")]
    pub replay: Option<PathBuf>,
    /// How long to wait for readiness; the command fails (exit 1) when the
    /// server is not ready by then.
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

/// The variable naming the stop file of a server [`start`] spawns: `serve`
/// shuts down gracefully once that file exists (and removes it). It is how
/// [`stop`] asks a detached server on Windows, which has no console to
/// receive Ctrl-C, to stop without killing it.
pub const STOP_FILE_ENV: &str = "MB_STOP_FILE";

fn stop_file(dir: &Path) -> PathBuf {
    dir.join("server.stop")
}

/// The variables `mockingbird proxy env` sets that route HTTP traffic.
const PROXY_VARS: &[&str] = &["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"];

/// `http://<proxy.host>:<proxy.port>` of `config` (with `MB_*` applied).
fn proxy_url(config: Option<&Path>) -> anyhow::Result<String> {
    let opts = crate::proxy::env::options_for(config, None)?;
    let host = if opts.host.contains(':') {
        format!("[{}]", opts.host)
    } else {
        opts.host
    };
    Ok(format!("http://{host}:{}", opts.port))
}

/// The proxy's address when the server `args` start runs it (`--proxy`
/// or `proxy.enabled`).
fn proxy_listener(args: &StartArgs) -> anyhow::Result<Option<SocketAddr>> {
    let mut cfg = match &args.config {
        Some(path) => file_config::load(path)?,
        None => file_config::FileConfig::default(),
    };
    file_config::apply_env(&mut cfg)?;
    Ok((args.proxy || cfg.proxy.enabled).then(|| SocketAddr::new(cfg.proxy.host, cfg.proxy.port)))
}

fn env_port(name: &str) -> Option<u16> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

/// The ports and host `serve` will use: flag, then `MB_*`, then the
/// config file, then the default. The last value says whether the gRPC port
/// was set, so that `serve` fails rather than skips it when it is busy.
fn effective(args: &StartArgs) -> anyhow::Result<(IpAddr, u16, u16, u16, bool)> {
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
    let grpc_explicit =
        args.grpc_port.is_some() || env_port("MB_GRPC_PORT").is_some() || server.grpc_port_explicit;
    Ok((host, port, admin, grpc, grpc_explicit))
}

/// A listener `server start` cannot bind: another process holds its port,
/// or the address is not this machine's.
#[derive(Debug)]
pub struct BindError {
    /// `data_plane`, `admin`, `grpc` or `proxy`.
    pub listener: &'static str,
    pub address: SocketAddr,
    pub source: std::io::Error,
    /// Which Mockingbird server holds the port, when one answers `/readyz`
    /// there (see [`holder`]).
    pub holder: Option<String>,
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot bind the {} listener on {}: {}",
            self.listener, self.address, self.source
        )?;
        if let Some(holder) = &self.holder {
            write!(f, "; {holder} holds it")?;
        }
        Ok(())
    }
}

impl std::error::Error for BindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// What to do about a [`BindError`].
pub const BIND_HINT: &str = "nothing was started and no other server was used. Leave the other server alone and give this one free ports: --port, --admin-port, --grpc-port, or server.port, server.admin_port, server.grpc_port and proxy.port in its config (ports a config leaves at their defaults are picked free automatically)";

/// Binds and releases each address, so that a busy port fails the start
/// before a server is spawned whose readiness another process would answer.
fn check_free(listeners: &[(&'static str, SocketAddr)]) -> Result<(), BindError> {
    for &(listener, address) in listeners {
        if address.port() == 0 {
            continue;
        }
        std::net::TcpListener::bind(address).map_err(|source| BindError {
            listener,
            address,
            source,
            holder: None,
        })?;
    }
    Ok(())
}

/// Which Mockingbird server holds `address`, from its `/readyz` there: the
/// config it was started with and its folder (since servers report them),
/// else that some Mockingbird server does; or, on a proxy port, which
/// Mockingbird proxy (from its [`crate::proxy::ORIGIN_PATH`]). `None` when
/// nothing there answers like one.
pub async fn holder(address: SocketAddr) -> Option<String> {
    // The proxy first: a proxy would count `/readyz` as a request for an
    // unmapped host in its project's report.
    let mut proxy = address;
    proxy.set_ip(reach(address.ip()));
    if let crate::proxy::owner::Listener::Proxy(body) = crate::proxy::owner::probe(proxy).await {
        return Some(crate::proxy::owner::describe_proxy(&body));
    }
    let (_, _, body) = readyz(&url(address.ip(), address.port())).await;
    describe_holder(&body)
}

/// [`holder`] from the `/readyz` body `body`.
pub(crate) fn describe_holder(body: &Value) -> Option<String> {
    // Every Mockingbird `/readyz` carries `status` and `role`.
    if !(body["status"].is_string() && body["role"].is_string()) {
        return None;
    }
    if let Some(dir) = body["dir"].as_str() {
        return Some(match body["config"].as_str() {
            Some(config) => {
                format!("the Mockingbird server started with config {config} (in {dir})")
            }
            None => format!("the Mockingbird server started without a config in {dir}"),
        });
    }
    Some("another Mockingbird server (it does not say whose)".to_owned())
}

/// Whether the `/readyz` body `body` is that of a server started with the
/// config `config`.
pub(crate) fn started_with(body: &Value, config: &Path) -> bool {
    body["config"]
        .as_str()
        .is_some_and(|c| same_file_key(Path::new(c)) == same_file_key(config))
}

/// Whether nothing listens on `port` of `host`: binds it and lets go.
pub(crate) fn port_free(host: IpAddr, port: u16) -> bool {
    std::net::TcpListener::bind(SocketAddr::new(host, port)).is_ok()
}

/// A listener's port as `init` or `server start` settles it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PortSlot {
    /// The config key: `server.port`, `server.admin_port`,
    /// `server.grpc_port` or `proxy.port`.
    pub key: &'static str,
    /// The address it binds.
    pub host: IpAddr,
    /// The port it gets so far (`0`: any).
    pub port: u16,
    /// Whether that port is only the default: no flag, `MB_*` variable or
    /// config key set it, so it may be replaced.
    pub defaulted: bool,
}

/// How far above a busy default [`choose_free_ports`] looks.
const PORT_SEARCH: u16 = 1000;

/// For each defaulted slot of `slots` whose port `is_free` (host, port)
/// says is taken: the first port above it that is free and no other slot
/// uses, as `(index, port)`.
///
/// # Errors
///
/// No free port lies within [`PORT_SEARCH`] above a busy default.
pub(crate) fn choose_free_ports(
    slots: &[PortSlot],
    is_free: impl Fn(IpAddr, u16) -> bool,
) -> anyhow::Result<Vec<(usize, u16)>> {
    let mut used: Vec<u16> = slots.iter().map(|s| s.port).filter(|&p| p != 0).collect();
    let mut chosen = Vec::new();
    for (i, slot) in slots.iter().enumerate() {
        if !slot.defaulted || slot.port == 0 || is_free(slot.host, slot.port) {
            continue;
        }
        let port = (slot.port.saturating_add(1)..=slot.port.saturating_add(PORT_SEARCH))
            .find(|p| !used.contains(p) && is_free(slot.host, *p))
            .ok_or_else(|| {
                anyhow!(
                    "{} {} is taken and no port up to {} above it is free; set {} in the config",
                    slot.key,
                    slot.port,
                    PORT_SEARCH,
                    slot.key
                )
            })?;
        used.push(port);
        chosen.push((i, port));
    }
    Ok(chosen)
}

/// A port `server start` takes in place of a busy default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChosenPort {
    /// `server.port`, `server.admin_port`, `server.grpc_port` or
    /// `proxy.port`.
    pub key: &'static str,
    /// The busy default.
    pub busy: u16,
    pub port: u16,
}

/// For `server start -c`: each listener (and the proxy, when it runs)
/// whose port is only the default (no flag, `MB_*` variable or config key)
/// and is taken, with the free port it gets instead. [`write_ports`] then
/// records them in the config, so that every command reading it (the
/// client subcommands, `proxy env`, `run`, the MCP server) finds them.
///
/// # Errors
///
/// The config cannot be read or does not load, or no free port is near a
/// busy default.
fn plan_free_ports(
    args: &StartArgs,
    is_free: impl Fn(IpAddr, u16) -> bool,
) -> anyhow::Result<Vec<ChosenPort>> {
    let Some(path) = &args.config else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config file {}", path.display()))?;
    let explicit = file_config::explicit_ports(&text);
    let mut cfg = file_config::load(path)?;
    file_config::apply_env(&mut cfg)?;
    let (host, port, admin, grpc, _) = effective(args)?;
    let unset =
        |flag: Option<u16>, var: &str, key: bool| flag.is_none() && env_port(var).is_none() && !key;
    let mut slots = vec![
        PortSlot {
            key: "server.port",
            host,
            port,
            defaulted: unset(args.port, "MB_PORT", explicit.port),
        },
        PortSlot {
            key: "server.admin_port",
            host,
            port: admin,
            defaulted: unset(args.admin_port, "MB_ADMIN_PORT", explicit.admin_port),
        },
        PortSlot {
            key: "server.grpc_port",
            host,
            port: grpc,
            defaulted: unset(args.grpc_port, "MB_GRPC_PORT", explicit.grpc_port),
        },
    ];
    if args.proxy || cfg.proxy.enabled {
        slots.push(PortSlot {
            key: "proxy.port",
            host: cfg.proxy.host,
            port: cfg.proxy.port,
            defaulted: !explicit.proxy_port,
        });
    }
    Ok(choose_free_ports(&slots, is_free)?
        .into_iter()
        .map(|(i, port)| ChosenPort {
            key: slots[i].key,
            busy: slots[i].port,
            port,
        })
        .collect())
}

/// Writes `chosen` into the config at `path`; returns what was written,
/// as JSON.
///
/// # Errors
///
/// The config cannot be rewritten.
fn write_ports(path: &Path, chosen: &[ChosenPort]) -> anyhow::Result<Value> {
    let backup = crate::onboard::rewrite_config(path, |root| {
        for chosen in chosen {
            let (section, key) = chosen.key.split_once('.').unwrap_or(("server", chosen.key));
            let entry = root.entry(section).or_insert_with(|| json!({}));
            if entry.is_null() {
                *entry = json!({});
            }
            entry
                .as_object_mut()
                .ok_or_else(|| anyhow!("{section}: is not a mapping"))?
                .insert(key.into(), json!(chosen.port));
        }
        Ok(())
    })?;
    let ports: Vec<Value> = chosen
        .iter()
        .map(|c| json!({"key": c.key, "busy": c.busy, "port": c.port}))
        .collect();
    Ok(json!({"written": path, "backup": backup, "ports": ports}))
}

/// For `server start -c`: each port given by a flag (`--port`,
/// `--admin-port`, `--grpc-port`; the MCP `start_server` arguments) that
/// differs from what the config file says, as a [`ChosenPort`] whose
/// `busy` is the config's port. [`write_ports`] records them in the config,
/// so that the config and the running server never disagree: `run`,
/// `verify --run`, `proxy env` and the client subcommands, which read the
/// ports from the config, then find this server.
///
/// # Errors
///
/// The config does not load.
fn given_ports(args: &StartArgs) -> anyhow::Result<Vec<ChosenPort>> {
    let Some(path) = &args.config else {
        return Ok(Vec::new());
    };
    let server = file_config::load(path)?.server;
    Ok([
        ("server.port", args.port, server.port),
        ("server.admin_port", args.admin_port, server.admin_port),
        ("server.grpc_port", args.grpc_port, server.grpc_port),
    ]
    .into_iter()
    .filter_map(|(key, flag, file)| {
        flag.filter(|&p| p != 0 && p != file)
            .map(|port| ChosenPort {
                key,
                busy: file,
                port,
            })
    })
    .collect())
}

/// `port` with `chosen`'s replacement for `key` applied.
fn chosen_or(chosen: &[ChosenPort], key: &str, port: u16) -> u16 {
    chosen
        .iter()
        .find(|c| c.key == key)
        .map_or(port, |c| c.port)
}

/// The JSON `server start` prints when it fails: `ready: false` and the
/// error, with the listener, address, OS error, holder and a hint for a
/// [`BindError`].
#[must_use]
pub fn start_error(error: &anyhow::Error) -> Value {
    let mut out = json!({"ready": false, "error": format!("{error:#}")});
    if let Some(bind) = error.downcast_ref::<BindError>() {
        out["listener"] = json!(bind.listener);
        out["address"] = json!(bind.address.to_string());
        out["os_error"] = json!(bind.source.to_string());
        out["holder"] = json!(bind.holder);
        out["hint"] = json!(BIND_HINT);
    }
    out
}

/// The address to reach a server bound to `host` from this machine.
fn reach(host: IpAddr) -> IpAddr {
    if host.is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        host
    }
}

pub(crate) fn url(host: IpAddr, port: u16) -> String {
    match reach(host) {
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
    }
}

/// A client for the local Admin API: a 3 s timeout, no proxy.
pub(crate) fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .unwrap_or_default()
}

/// `GET <admin>/readyz`: `(reachable, ready, body)`.
pub(crate) async fn readyz(admin_url: &str) -> (bool, bool, Value) {
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
    default_profile(&cfg)
        .map(crate::providers::secret_status)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s["set"] == false)
        .filter_map(|s| s["ref"].as_str().map(String::from))
        .collect()
}

/// `llm.default_profile`, else the only profile.
fn default_profile(cfg: &file_config::FileConfig) -> Option<&file_config::FileProfile> {
    match &cfg.llm.default_profile {
        Some(name) => cfg.llm.profiles.get(name),
        None if cfg.llm.profiles.len() == 1 => cfg.llm.profiles.values().next(),
        None => None,
    }
}

/// The environment variable names `config`'s default LLM profile references
/// (`api_key: env:NAME`, `${env:NAME}` in headers), sorted; empty when the
/// config is missing or does not load.
pub fn default_profile_env_names(config: &Path) -> Vec<String> {
    let Ok(cfg) = file_config::load(config) else {
        return Vec::new();
    };
    let mut names: Vec<String> = default_profile(&cfg)
        .map(crate::providers::secret_status)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|s| s["ref"].as_str()?.strip_prefix("env:").map(String::from))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// `server start`. The server process stays this process's child: it is
/// kept so that [`stop`] and [`status`] in a long-lived caller (the MCP
/// server) reap it once it exits, instead of seeing the exited process (a
/// zombie) as still running.
///
/// # Errors
///
/// Fails when the state directory or the log cannot be written, the ports
/// or proxy settings do not resolve, a port is busy (a [`BindError`]), the
/// binary cannot be located or started, or the server exits before it is
/// ready.
pub async fn start(args: &StartArgs) -> anyhow::Result<Value> {
    let (mut info, child) = start_child(args).await?;
    // Which `shares` group each mock of the config keeps its sessions in, and
    // the mocks of one host in none together: the CLI and MCP answers alike.
    if let Some(config) = &args.config {
        crate::shares::add_start_notice(&mut info, config);
    }
    if let Ok(mut started) = STARTED.lock() {
        started.push(child);
    }
    Ok(info)
}

/// The servers [`start`] spawned from this process.
static STARTED: Mutex<Vec<std::process::Child>> = Mutex::new(Vec::new());

/// Whether `pid` is a server [`start`] spawned here: `Some(true)` while it
/// runs, `Some(false)` once it exited (it is then reaped), `None` for any
/// other process.
fn started_alive(pid: u32) -> Option<bool> {
    let mut started = STARTED.lock().ok()?;
    let at = started.iter().position(|c| c.id() == pid)?;
    match started[at].try_wait() {
        Ok(None) => Some(true),
        Ok(Some(_)) => {
            // Already reaped: `wait` returns the recorded status at once.
            let _ = started.swap_remove(at).wait();
            Some(false)
        }
        Err(_) => None,
    }
}

/// [`start`], also returning the server process, which the caller reaps
/// (`mockingbird run` stops it itself; a dropped handle is not waited for).
///
/// # Errors
///
/// As [`start`].
pub async fn start_child(args: &StartArgs) -> anyhow::Result<(Value, std::process::Child)> {
    let dir = &args.state.state_dir;
    if let Some(pid) = read_pid(dir)
        && alive(pid)
    {
        bail!(
            "a background server already runs (pid {pid}); stop it with `mockingbird server stop`"
        );
    }
    let chosen = plan_free_ports(args, port_free)?;
    let (host, port, admin, grpc, grpc_explicit) = effective(args)?;
    if let (Some(config), true) = (&args.config, chosen.iter().any(|c| c.key != "proxy.port")) {
        // A busy default may be this config's own server, started another
        // way (`serve`, another state directory): never start a second one.
        let (_, _, body) = readyz(&url(host, admin)).await;
        if started_with(&body, config) {
            bail!(
                "a server of {} already runs at {} (started outside this state directory); use it, or stop it before starting another",
                config.display(),
                url(host, admin)
            );
        }
    }
    let port = chosen_or(&chosen, "server.port", port);
    let admin = chosen_or(&chosen, "server.admin_port", admin);
    let grpc = chosen_or(&chosen, "server.grpc_port", grpc);
    let mut listeners = vec![
        ("data_plane", SocketAddr::new(host, port)),
        ("admin", SocketAddr::new(host, admin)),
    ];
    if grpc_explicit || chosen.iter().any(|c| c.key == "server.grpc_port") {
        listeners.push(("grpc", SocketAddr::new(host, grpc)));
    }
    if let Some(mut proxy) = proxy_listener(args)? {
        proxy.set_port(chosen_or(&chosen, "proxy.port", proxy.port()));
        listeners.push(("proxy", proxy));
    }
    if let Err(mut error) = check_free(&listeners) {
        error.holder = holder(error.address).await;
        return Err(error.into());
    }
    // Only once every port is known to be free: a start that fails on a
    // port leaves the config as it was.
    let given = given_ports(args)?;
    let (ports_chosen, ports_given) = match &args.config {
        Some(path) if !chosen.is_empty() || !given.is_empty() => {
            let all: Vec<ChosenPort> = chosen.iter().chain(&given).copied().collect();
            let written = write_ports(path, &all)?;
            let part = |ports: &[ChosenPort], name: &str| {
                if ports.is_empty() {
                    return Value::Null;
                }
                let entries: Vec<Value> = ports
                    .iter()
                    .map(|c| json!({"key": c.key, name: c.busy, "port": c.port}))
                    .collect();
                json!({"written": written["written"], "backup": written["backup"], "ports": entries})
            };
            (part(&chosen, "busy"), part(&given, "was"))
        }
        _ => (Value::Null, Value::Null),
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
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
    // The proxy's own URL: from `proxy.port` / `proxy.host`, or the
    // defaults. A shell that exported `proxy env` would otherwise send the
    // server's own LLM calls back through its proxy.
    let proxy_url = proxy_url(args.config.as_deref())?;
    for (name, value) in std::env::vars() {
        if PROXY_VARS.contains(&name.as_str()) && value.trim_end_matches('/') == proxy_url {
            cmd.env_remove(name);
        }
    }
    if args.proxy {
        cmd.arg("--proxy");
    }
    if let Some(dir) = &args.record {
        cmd.arg("--record").arg(dir);
    }
    if let Some(dir) = &args.replay {
        cmd.arg("--replay").arg(dir);
    }
    cmd.args(&args.contracts);
    // A stop file left by an earlier server would stop this one at once.
    let stop = stop_file(dir);
    let _ = std::fs::remove_file(&stop);
    cmd.env(STOP_FILE_ENV, std::path::absolute(&stop).unwrap_or(stop));
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
    let mut info = json!({
        "pid": pid,
        "data_plane": url(host, port),
        "admin": admin_url,
        "grpc": format!("{}:{grpc}", reach(host)),
        "config": args.config,
        "contracts": args.contracts,
        "log": log_path,
    });
    if args.proxy {
        info["proxy"] = json!(proxy_url);
    }
    if !ports_chosen.is_null() {
        info["ports_chosen"] = ports_chosen;
    }
    if !ports_given.is_null() {
        let set: Vec<String> = given
            .iter()
            .map(|c| format!("{} {} (was {})", c.key, c.port, c.busy))
            .collect();
        info["note"] = json!(format!(
            "the ports given here differ from the config, so they were written into {}: {}; run, verify --run, proxy env and the client commands read them from there",
            ports_given["written"].as_str().unwrap_or_default(),
            set.join(", ")
        ));
        info["ports_written"] = ports_given;
    }
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
            return Ok((out, child));
        }
        if Instant::now() >= deadline {
            let mut out = info;
            out["ready"] = json!(false);
            out["warning"] = json!(format!(
                "the Admin API did not answer within {}; see the log",
                args.wait
            ));
            return Ok((out, child));
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

fn alive(pid: u32) -> bool {
    started_alive(pid).unwrap_or_else(|| process_alive(pid))
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    quiet(Command::new("kill").arg("-0").arg(pid.to_string()))
}

#[cfg(not(unix))]
fn process_alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
}

#[cfg(unix)]
pub(crate) fn terminate(pid: u32, force: bool) -> bool {
    let signal = if force { "-KILL" } else { "-TERM" };
    quiet(Command::new("kill").arg(signal).arg(pid.to_string()))
}

/// `taskkill`, with `/F` only when forcing (without it, a process that
/// has a window is asked to close).
#[cfg(not(unix))]
pub(crate) fn terminate(pid: u32, force: bool) -> bool {
    let pid = pid.to_string();
    let mut args = vec!["/PID", pid.as_str(), "/T"];
    if force {
        args.push("/F");
    }
    quiet(Command::new("taskkill").args(args))
}

/// Asks the server `pid` of state directory `dir` to stop gracefully: its
/// stop file (see [`STOP_FILE_ENV`]), which works on every platform, and a
/// SIGTERM on Unix.
fn request_stop(dir: &Path, pid: u32) {
    let _ = std::fs::write(stop_file(dir), b"");
    terminate(pid, false);
}

/// Stops `child`, a server [`start_child`] started in this process: a
/// graceful stop request (its stop file, see [`STOP_FILE_ENV`], and a
/// SIGTERM on Unix), then a kill after 10 seconds, and reaps it.
///
/// # Errors
///
/// Fails when the stop file cannot be written or the process cannot be
/// signalled, killed or reaped.
pub async fn stop_child(args: &StateArgs, mut child: std::process::Child) -> anyhow::Result<()> {
    request_stop(&args.state_dir, child.id());
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait()?;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = std::fs::remove_file(pid_file(&args.state_dir));
    let _ = std::fs::remove_file(stop_file(&args.state_dir));
    Ok(())
}

/// `server stop`.
///
/// # Errors
///
/// Fails when the state directory cannot be read or the server cannot be
/// stopped.
pub async fn stop(args: &StateArgs) -> anyhow::Result<Value> {
    let dir = &args.state_dir;
    let Some(pid) = read_pid(dir) else {
        return Ok(json!({ "stopped": false, "detail": "no background server recorded" }));
    };
    let mut forced = false;
    if alive(pid) {
        request_stop(dir, pid);
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if alive(pid) {
            forced = terminate(pid, true);
        }
    }
    let _ = std::fs::remove_file(pid_file(dir));
    let _ = std::fs::remove_file(stop_file(dir));
    Ok(json!({ "stopped": true, "pid": pid, "forced": forced }))
}

/// `server status`.
///
/// # Errors
///
/// Fails when the state directory cannot be read.
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

/// `text` without terminal escape sequences: CSI (`ESC [` parameters,
/// intermediates, final byte) and OSC (`ESC ]` … BEL or `ESC \\`); any other
/// `ESC` is dropped on its own.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                while chars.next_if(|c| ('\x30'..='\x3f').contains(c)).is_some() {}
                while chars.next_if(|c| ('\x20'..='\x2f').contains(c)).is_some() {}
                chars.next_if(|c| ('\x40'..='\x7e').contains(c));
            }
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.next_if_eq(&'\\').is_some() {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// `server logs`, as plain text.
///
/// # Errors
///
/// Fails when the log file cannot be read.
pub fn logs(args: &StateArgs, lines: usize) -> anyhow::Result<String> {
    let path = log_file(&args.state_dir);
    if !path.exists() {
        return Err(anyhow!("no log at {}", path.display()));
    }
    Ok(strip_ansi(&tail(&path, lines)))
}

/// The state directory of the background server for `config`:
/// `state_dir` when absolute. Otherwise, with a config, the first
/// `<ancestor>/<state_dir>` above the config whose `server.json` records
/// that same config (a server started from a parent folder with `-c
/// sub/…`), else `<config dir>/<state_dir>`; never another project's
/// server. Without a config, `<cwd>/<state_dir>` (as `server start` does).
pub fn resolve_state_dir(state_dir: &Path, config: Option<&Path>, cwd: &Path) -> PathBuf {
    if state_dir.is_absolute() {
        return state_dir.to_path_buf();
    }
    let Some(config) = config.map(|c| cwd.join(c)) else {
        return cwd.join(state_dir);
    };
    let wanted = same_file_key(&config);
    let config_dir = config
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf());
    config_dir
        .ancestors()
        .map(|a| (a, a.join(state_dir)))
        .find(|(ancestor, dir)| {
            std::fs::read(info_file(dir))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v["config"].as_str().map(PathBuf::from))
                // A relative `-c` was relative to the folder it was run in,
                // which holds the state directory.
                .is_some_and(|recorded| same_file_key(&ancestor.join(recorded)) == wanted)
        })
        .map(|(_, dir)| dir)
        .unwrap_or_else(|| config_dir.join(state_dir))
}

/// A running background server, as its state directory records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedServer {
    /// Its Admin API URL.
    pub admin: String,
    /// Its data plane URL, when recorded.
    pub data_plane: Option<String>,
    /// The config it was started with (absolute; `None` without one).
    pub config: Option<PathBuf>,
}

/// The background server of the project in `cwd` that the state directory
/// `state_dir` records (found as [`resolve_state_dir`] finds it for
/// `config`), while its process runs. With `config`, only a server started
/// with that same config counts; without one, the server recorded in
/// `<cwd>/<state_dir>`, whatever it serves.
///
/// This is the first place the MCP server, `run`, `verify --run` and the
/// client subcommands look for a project's server ([`project_admin_url`]):
/// a server started on other ports than the config says (flags, `MB_*`
/// variables) is still found.
#[must_use]
pub fn recorded_server(
    state_dir: &Path,
    config: Option<&Path>,
    cwd: &Path,
) -> Option<RecordedServer> {
    let dir = resolve_state_dir(state_dir, config, cwd);
    read_pid(&dir).filter(|&pid| alive(pid))?;
    let info: Value = serde_json::from_slice(&std::fs::read(info_file(&dir)).ok()?).ok()?;
    // A relative `-c` was relative to the folder `server start` ran in:
    // the one holding the state directory, else this one.
    let base = if state_dir.is_absolute() {
        cwd
    } else {
        dir.ancestors()
            .nth(state_dir.components().count())
            .unwrap_or(cwd)
    };
    let recorded = info["config"].as_str().map(|c| {
        [base, cwd]
            .iter()
            .map(|b| same_file_key(&b.join(c)))
            .find(|p| p.exists())
            .unwrap_or_else(|| same_file_key(&base.join(c)))
    });
    if let Some(config) = config
        && recorded.as_ref() != Some(&same_file_key(&cwd.join(config)))
    {
        return None;
    }
    Some(RecordedServer {
        admin: info["admin"].as_str()?.to_owned(),
        data_plane: info["data_plane"].as_str().map(String::from),
        config: recorded,
    })
}

/// [`recorded_server`], unless the server answering at its admin URL says
/// it was started with another config (its process ended and another
/// server took the port): when `/readyz` does not say (an older server, or
/// auth hides it) the record is trusted.
pub async fn project_server(
    state_dir: &Path,
    config: Option<&Path>,
    cwd: &Path,
) -> Option<RecordedServer> {
    let server = recorded_server(state_dir, config, cwd)?;
    let (_, _, body) = readyz(&server.admin).await;
    if body["dir"].is_string() {
        let same = match (&server.config, body["config"].as_str()) {
            (Some(config), Some(_)) => started_with(&body, config),
            (None, None) => true,
            _ => false,
        };
        if !same {
            return None;
        }
    }
    Some(server)
}

/// The Admin API of the project in `cwd` with `config` (else
/// `./mockingbird.yaml`), as `run`, `verify --run` and the client
/// subcommands find it when no `--admin-url`/`MB_ADMIN_URL` names one:
/// its running background server ([`project_server`]), else the admin
/// listener the config describes ([`crate::client::resolve_admin_url`]).
pub async fn project_admin_url(state_dir: &Path, config: Option<&Path>, cwd: &Path) -> String {
    match project_server(state_dir, config, cwd).await {
        Some(server) => server.admin,
        None => crate::client::resolve_admin_url(None, config),
    }
}

/// `path` canonicalized when it exists, else made absolute: for comparing
/// two spellings of one config path.
pub(crate) fn same_file_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
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

/// `mockingbird doctor`: what is set up and what is missing (the config,
/// the LLM key, a server answering at `admin_url`), as JSON. Its
/// `warnings` name each config mock whose world description the server
/// (asked with `token` as the bearer) reports refused entries for, such as
/// those of a `world:` file refused as the config was last loaded.
pub async fn doctor(args: &DoctorArgs, admin_url: &str, token: Option<&str>) -> Value {
    let mut problems: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut loaded = None;
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
                let cfg = &*loaded.insert(cfg);
                let mut profiles = Vec::new();
                for (name, p) in &cfg.llm.profiles {
                    let secrets = crate::providers::secret_status(p);
                    for s in &secrets {
                        if s["set"] == false {
                            problems.push(format!(
                                "LLM profile {name:?} needs {} ({not_set_here}), or store the key with `mockingbird keys setup` (MCP open_key_setup)",
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
    } else if let Some(cfg) = &loaded {
        warnings.extend(world_refusal_warnings(cfg, admin_url, token).await);
    }
    if config_path.is_none() {
        problems.push("no mockingbird.yaml here: create one with `mockingbird init`".into());
    }
    let key_store = match crate::keys::store::default_store_dir() {
        Ok(dir) => {
            let keys = crate::keys::store::list(&dir);
            for k in keys.iter().filter(|k| k["private"] == false) {
                let file = k["file"].as_str().unwrap_or_default();
                problems.push(format!(
                    "key file {file} is readable by other users: chmod 600 {file}"
                ));
            }
            json!({ "dir": dir, "keys": keys })
        }
        Err(_) => Value::Null,
    };
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "skills_level": crate::agents::SKILLS_LEVEL,
        "binary": std::env::current_exe().ok(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "config": config,
        "provider_keys": keys,
        "admin": { "url": admin_url, "reachable": reachable, "ready": ready, "readyz": readyz_body },
        "background_server": background,
        "docker": docker,
        "shell_env": shell_env.map(|names| json!({ "imported": names })),
        "key_store": key_store,
        "problems": problems,
        "warnings": warnings,
    })
}

/// One warning per mock of `cfg` with a `world:` file whose active world
/// description at `admin_url` has refused entries, naming their pointers.
/// A mock the server cannot answer for is skipped.
async fn world_refusal_warnings(
    cfg: &file_config::FileConfig,
    admin_url: &str,
    token: Option<&str>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for (name, mock) in cfg.mocks.iter().filter(|(_, m)| m.world.is_some()) {
        let Ok(mut url) = reqwest::Url::parse(admin_url) else {
            return warnings;
        };
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend([
                "api",
                "v1",
                "workspaces",
                &mock.workspace,
                "mocks",
                name,
                "contracts",
                "active",
                "world",
            ]);
        }
        let mut req = http().get(url);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let Ok(resp) = req.send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(view) = resp.json::<Value>().await else {
            continue;
        };
        let pointers: Vec<&str> = view["refused"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| match r["path"].as_str().unwrap_or_default() {
                "" => "the document",
                p => p,
            })
            .collect();
        if !pointers.is_empty() {
            let label = crate::registry::mock_label(&mock.workspace, name);
            let show = if mock.workspace == mb_store::DEFAULT_WORKSPACE {
                format!("mockingbird world show {name}")
            } else {
                format!(
                    "mockingbird --workspace {} world show {name}",
                    mock.workspace
                )
            };
            warnings.push(format!(
                "mock {label:?}: world entries refused at {}; `{show}` says why",
                pointers.join(", ")
            ));
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        assert_eq!(
            strip_ansi("\x1b[2m2026\x1b[0m \x1b[32m INFO\x1b[0m ready"),
            "2026  INFO ready"
        );
        assert_eq!(
            strip_ansi("a\x1b]8;;http://x\x07link\x1b]8;;\x1b\\b"),
            "alinkb"
        );
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("lone\x1b"), "lone");
    }

    #[test]
    fn state_dir_is_found_next_to_the_config() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("app/sub");
        std::fs::create_dir_all(root.path().join("app/.mockingbird")).unwrap();
        // Started from app/ with `-c sub/mockingbird.yaml`.
        std::fs::write(
            root.path().join("app/.mockingbird/server.json"),
            r#"{"config": "sub/mockingbird.yaml"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let rel = Path::new(".mockingbird");
        assert_eq!(
            resolve_state_dir(
                rel,
                Some(&project.join("mockingbird.yaml")),
                elsewhere.path()
            ),
            root.path().join("app/.mockingbird")
        );
        // Another project's server above this config is not this one's.
        let other = root.path().join("app/sub/other");
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(
            resolve_state_dir(rel, Some(&other.join("m.yaml")), elsewhere.path()),
            other.join(".mockingbird")
        );
        // Nor is a server found by walking up from the working folder.
        std::fs::create_dir_all(elsewhere.path().join(".mockingbird")).unwrap();
        std::fs::write(
            elsewhere.path().join(".mockingbird/server.json"),
            r#"{"config": "a.yaml"}"#,
        )
        .unwrap();
        let nested = elsewhere.path().join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        let fresh_cfg = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_state_dir(rel, Some(&fresh_cfg.path().join("m.yaml")), &nested),
            fresh_cfg.path().join(".mockingbird")
        );
        let fresh = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_state_dir(rel, Some(&fresh.path().join("m.yaml")), elsewhere.path()),
            fresh.path().join(".mockingbird")
        );
        assert_eq!(
            resolve_state_dir(rel, None, elsewhere.path()),
            elsewhere.path().join(".mockingbird")
        );
        assert_eq!(
            resolve_state_dir(Path::new("/abs/state"), None, elsewhere.path()),
            PathBuf::from("/abs/state")
        );
    }

    fn start_args(config: &Path) -> StartArgs {
        StartArgs {
            state: StateArgs {
                state_dir: config.with_file_name(DEFAULT_STATE_DIR),
            },
            config: Some(config.to_path_buf()),
            host: None,
            port: None,
            admin_port: None,
            grpc_port: None,
            offline: true,
            proxy: false,
            no_watch: false,
            insecure_no_auth: false,
            record: None,
            replay: None,
            wait: Duration::from_secs(1).into(),
            contracts: Vec::new(),
        }
    }

    #[test]
    fn start_writes_free_ports_for_taken_defaults_into_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("mockingbird.yaml");
        std::fs::write(
            &config,
            "# header\nstorage:\n  url: sqlite://./mockingbird.db\n",
        )
        .unwrap();
        let args = StartArgs {
            proxy: true,
            ..start_args(&config)
        };
        let taken = |_: IpAddr, p: u16| !matches!(p, 9090 | 8888 | 8889);
        let chosen = plan_free_ports(&args, taken).unwrap();
        let out = write_ports(&config, &chosen).unwrap();
        assert_eq!(
            out["ports"],
            json!([
                {"key": "server.admin_port", "busy": 9090, "port": 9091},
                {"key": "proxy.port", "busy": 8888, "port": 8890},
            ])
        );
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.starts_with("# header\n"), "{text}");
        let cfg = file_config::load(&config).unwrap();
        assert_eq!(cfg.server.admin_port, 9091);
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.proxy.port, 8890);
        // The ports are now set: a second start keeps them.
        let again = plan_free_ports(&args, |_, p| !matches!(p, 9091 | 8890)).unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn start_never_moves_a_port_that_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("mockingbird.yaml");
        std::fs::write(&config, "server:\n  admin_port: 9090\n  grpc_port: 50051\n").unwrap();
        let mut args = start_args(&config);
        args.port = Some(8080);
        let original = std::fs::read_to_string(&config).unwrap();
        // Everything is taken, but every port is set (flag or config key);
        // the proxy is off.
        let out = plan_free_ports(&args, |_, _| false).unwrap();
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
    }

    #[test]
    fn free_ports_skip_ports_other_listeners_use() {
        let host = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let slots = [
            PortSlot {
                key: "server.port",
                host,
                port: 8080,
                defaulted: true,
            },
            PortSlot {
                key: "server.admin_port",
                host,
                port: 8081,
                defaulted: false,
            },
        ];
        assert_eq!(
            choose_free_ports(&slots, |_, p| p != 8080).unwrap(),
            vec![(0, 8082)]
        );
        assert!(choose_free_ports(&slots, |_, _| false).is_err());
    }

    #[test]
    fn the_holder_of_a_port_is_named_from_its_readyz() {
        assert_eq!(
            describe_holder(
                &json!({"status": "ready", "role": "all", "config": "/a/mockingbird.yaml", "dir": "/a"})
            )
            .as_deref(),
            Some("the Mockingbird server started with config /a/mockingbird.yaml (in /a)")
        );
        assert_eq!(
            describe_holder(
                &json!({"status": "ready", "role": "all", "config": null, "dir": "/b"})
            )
            .as_deref(),
            Some("the Mockingbird server started without a config in /b")
        );
        assert_eq!(
            describe_holder(&json!({"status": "ready", "role": "all"})).as_deref(),
            Some("another Mockingbird server (it does not say whose)")
        );
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("mockingbird.yaml");
        std::fs::write(&config, "").unwrap();
        let ours = json!({"status": "ready", "role": "all", "config": std::fs::canonicalize(&config).unwrap()});
        assert!(started_with(&ours, &config));
        assert!(!started_with(&ours, &dir.path().join("other.yaml")));
        assert!(!started_with(&json!({"status": "ready"}), &config));
        // Another service's health answer is not taken for Mockingbird.
        assert_eq!(describe_holder(&json!({"status": "ok"})), None);
        assert_eq!(describe_holder(&json!({"status": 404})), None);
        assert_eq!(describe_holder(&Value::Null), None);
    }

    #[test]
    fn ports_given_that_differ_from_the_config_are_written_into_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("mockingbird.yaml");
        std::fs::write(&config, "# header\nserver:\n  admin_port: 9091\n").unwrap();
        let mut args = start_args(&config);
        args.port = Some(8080); // the config's (default) port: kept as is
        args.admin_port = Some(19091);
        args.grpc_port = Some(15052);
        let given = given_ports(&args).unwrap();
        assert_eq!(
            given,
            vec![
                ChosenPort {
                    key: "server.admin_port",
                    busy: 9091,
                    port: 19091
                },
                ChosenPort {
                    key: "server.grpc_port",
                    busy: 50051,
                    port: 15052
                },
            ]
        );
        write_ports(&config, &given).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.starts_with("# header\n"), "{text}");
        let cfg = file_config::load(&config).unwrap();
        assert_eq!(
            (cfg.server.port, cfg.server.admin_port, cfg.server.grpc_port),
            (8080, 19091, 15052)
        );
        // Now the config says so: nothing more to write.
        assert!(given_ports(&args).unwrap().is_empty());
        args.config = None;
        assert!(given_ports(&args).unwrap().is_empty());
    }

    /// Writes a background server record into `<dir>/.mockingbird` for
    /// process `pid`, started with `config` and on admin URL `admin`.
    fn record(dir: &Path, pid: u32, config: Value, admin: &str) {
        let state = dir.join(DEFAULT_STATE_DIR);
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(pid_file(&state), pid.to_string()).unwrap();
        std::fs::write(
            info_file(&state),
            json!({"pid": pid, "admin": admin, "data_plane": "http://127.0.0.1:18081", "config": config})
                .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn the_projects_recorded_server_is_found_wherever_it_listens() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("mockingbird.yaml");
        std::fs::write(&config, "server:\n  admin_port: 9091\n").unwrap();
        let rel = Path::new(DEFAULT_STATE_DIR);
        let me = std::process::id();
        // Recorded relative to the folder it was started in.
        record(
            dir.path(),
            me,
            json!("mockingbird.yaml"),
            "http://127.0.0.1:19091",
        );
        let found = recorded_server(rel, Some(Path::new("mockingbird.yaml")), dir.path()).unwrap();
        assert_eq!(found.admin, "http://127.0.0.1:19091");
        assert_eq!(found.data_plane.as_deref(), Some("http://127.0.0.1:18081"));
        assert_eq!(found.config, Some(same_file_key(&config)));
        // From a subfolder, by an absolute config path.
        let sub = dir.path().join("src");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(recorded_server(rel, Some(&config), &sub).is_some());
        // Without a config: the server recorded for this folder.
        assert!(recorded_server(rel, None, dir.path()).is_some());
        // Another config of the same folder is not that server's.
        assert_eq!(
            recorded_server(rel, Some(Path::new("other.yaml")), dir.path()),
            None
        );
        // A server started without a config is not the config's.
        record(dir.path(), me, Value::Null, "http://127.0.0.1:19091");
        assert_eq!(
            recorded_server(rel, Some(Path::new("mockingbird.yaml")), dir.path()),
            None
        );
        // A record whose process ended counts for nothing.
        record(
            dir.path(),
            99_999_999,
            json!(config),
            "http://127.0.0.1:19091",
        );
        assert_eq!(recorded_server(rel, None, dir.path()), None);
    }

    #[tokio::test]
    async fn a_recorded_server_whose_port_another_server_took_is_not_used() {
        use wiremock::matchers::path;
        let admin = wiremock::MockServer::start().await;
        wiremock::Mock::given(path("/readyz"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "status": "ready", "role": "all",
                "config": "/elsewhere/other/mockingbird.yaml", "dir": "/elsewhere/other",
            })))
            .mount(&admin)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("mockingbird.yaml");
        std::fs::write(&config, "").unwrap();
        record(dir.path(), std::process::id(), json!(config), &admin.uri());
        let rel = Path::new(DEFAULT_STATE_DIR);
        assert!(recorded_server(rel, Some(&config), dir.path()).is_some());
        assert_eq!(project_server(rel, Some(&config), dir.path()).await, None);
        // Not answering (or not saying whose it is): the record is trusted.
        record(
            dir.path(),
            std::process::id(),
            json!(config),
            "http://127.0.0.1:1",
        );
        assert_eq!(
            project_server(rel, Some(&config), dir.path())
                .await
                .map(|s| s.admin),
            Some("http://127.0.0.1:1".to_owned())
        );
    }
}
