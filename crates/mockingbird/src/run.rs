//! `mockingbird run -- <command…>`: runs a command against the mocks. Starts
//! the server (with its proxy) unless the config's own server already
//! answers, waits until it is ready, runs the command with the proxy
//! environment for every runtime ([`crate::proxy::env`]), stops what it
//! started and exits with the command's exit code. The command only ever
//! gets the config's own proxy ([`crate::proxy::owner`]): when the server
//! runs none, or another project's proxy holds its port, `run` puts a proxy
//! of its own in front of the server for the command.

use std::ffi::OsString;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::daemon::{self, StartArgs, StateArgs};
use crate::file_config;
use crate::proxy::env::{options_for, proxy_env};
use crate::proxy::owner::{OwnProxy, held_message, own_proxy};
use crate::proxy::{StandaloneOptions, run_standalone};

/// The config `run` uses when `-c` is not given.
pub const DEFAULT_CONFIG: &str = "mockingbird.yaml";

/// `mockingbird run`.
#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Config file [default: ./mockingbird.yaml].
    #[arg(short = 'c', long)]
    pub config: Option<PathBuf>,
    /// Faker only, no LLM (for a server `run` starts).
    #[arg(long)]
    pub offline: bool,
    /// Record every LLM answer into this snapshot directory (for a server
    /// `run` starts; `serve --record`).
    #[arg(long, value_name = "DIR")]
    pub record: Option<PathBuf>,
    /// Answer from this snapshot directory without calling an LLM (for a
    /// server `run` starts; `serve --replay`).
    #[arg(long, value_name = "DIR")]
    pub replay: Option<PathBuf>,
    /// How long to wait for the server to be ready.
    #[arg(long, default_value = "60s")]
    pub wait: humantime::Duration,
    /// Leave a server `run` started running after the command exits.
    #[arg(long)]
    pub keep: bool,
    /// Where the pid and log of a server `run` starts are kept.
    #[arg(long, env = "MB_STATE_DIR", default_value = daemon::DEFAULT_STATE_DIR)]
    pub state_dir: PathBuf,
    /// The command to run, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

/// What `run` started, stopped again when it ends.
#[derive(Default)]
struct Started {
    server: Option<(StateArgs, std::process::Child)>,
    proxy: Option<(
        watch::Sender<bool>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    )>,
}

impl Started {
    async fn stop(self) {
        if let Some((stop, task)) = self.proxy {
            let _ = stop.send(true);
            let _ = task.await;
        }
        if let Some((state, child)) = self.server
            && let Err(error) = daemon::stop_child(&state, child).await
        {
            eprintln!("mockingbird: cannot stop the server: {error:#}");
        }
    }
}

/// Runs `args.command` against the mocks; the result is its exit code
/// (`128 + n` when signal `n` ended it, 127 when it cannot be started).
///
/// # Errors
///
/// Fails when the config file does not load, the proxy options cannot be
/// set up, or the server cannot be started or reached; a command that
/// cannot be started is exit code 127, not an error.
pub async fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
    let config = match &args.config {
        Some(path) => path.clone(),
        None => {
            let path = PathBuf::from(DEFAULT_CONFIG);
            if !path.exists() {
                bail!(
                    "mockingbird run needs a config: pass -c <file> or add {DEFAULT_CONFIG} here"
                );
            }
            path
        }
    };
    let mut cfg = file_config::load(&config)?;
    file_config::apply_env(&mut cfg)?;
    let reach = |ip: IpAddr| {
        if ip.is_unspecified() {
            IpAddr::from([127, 0, 0, 1])
        } else {
            ip
        }
    };
    let server_ip = reach(cfg.server.host);
    let admin_url = format!(
        "http://{}",
        SocketAddr::new(server_ip, cfg.server.admin_port)
    );
    let data_url = format!("http://{}", SocketAddr::new(server_ip, cfg.server.port));

    let mut started = Started::default();
    let result = prepare(&args, &config, &cfg, &admin_url, &data_url, &mut started).await;
    let code = match result {
        // A server `run` started may have taken free ports in place of busy
        // defaults (written into the config): the proxy is read again.
        Ok(proxy) => match options_for(Some(&config), None) {
            Ok(mut opts) => {
                opts.host = proxy.ip().to_string();
                opts.port = proxy.port();
                run_command(&args.command, &opts).await
            }
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    if args.keep
        && let Some((state, _child)) = started.server.take()
    {
        eprintln!(
            "mockingbird: the server keeps running (stop it with `mockingbird server stop --state-dir {}`)",
            state.state_dir.display()
        );
    }
    started.stop().await;
    code
}

/// Makes sure the server and its proxy answer, starting what is missing;
/// returns the address of the proxy the command is sent through, which is
/// always the config's own.
async fn prepare(
    args: &RunArgs,
    config: &std::path::Path,
    cfg: &file_config::FileConfig,
    admin_url: &str,
    data_url: &str,
    started: &mut Started,
) -> anyhow::Result<SocketAddr> {
    let foreign = match readyz(admin_url).await {
        None => None,
        Some(body) => foreign_server(&body, config),
    };
    if let Some(holder) = &foreign {
        eprintln!(
            "mockingbird: {admin_url} is {holder}, not the server of {}; it is not used",
            config.display()
        );
    }
    if foreign.is_some() || !answers(admin_url).await {
        let state = StateArgs {
            state_dir: args.state_dir.clone(),
        };
        let start = StartArgs {
            state: state.clone(),
            config: Some(config.to_path_buf()),
            host: None,
            port: None,
            admin_port: None,
            grpc_port: None,
            offline: args.offline,
            proxy: true,
            no_watch: false,
            insecure_no_auth: false,
            wait: args.wait,
            record: args.record.clone(),
            replay: args.replay.clone(),
            contracts: Vec::new(),
        };
        let (info, child) = daemon::start_child(&start).await?;
        started.server = Some((state, child));
        if info["ready"] != true {
            bail!(
                "the server did not become ready within {}: {}\nlog: {}",
                args.wait,
                info["readyz"],
                daemon::tail(&args.state_dir.join("server.log"), 20)
            );
        }
        eprintln!(
            "mockingbird: server started (log {}), proxy {}",
            args.state_dir.join("server.log").display(),
            info["proxy"].as_str().unwrap_or_default()
        );
        // Read again: the start may have written a free proxy port.
        let opts = options_for(Some(config), None)?;
        return Ok(SocketAddr::new(opts.host.parse()?, opts.port));
    }
    if let Some(warning) = reuse_warning(args, admin_url) {
        eprintln!("{warning}");
    }
    wait_ready(admin_url, Duration::from(args.wait)).await?;
    let proxy_addr = match own_proxy(config, None).await? {
        OwnProxy::Running(addr) => return Ok(addr),
        // A server without its proxy: run one in front of it for the
        // command.
        OwnProxy::Missing { addr, .. } => addr,
        // Another proxy holds the port (often another project's server):
        // it has none of these mocks and would pass the command's calls to
        // the real APIs. A port the config leaves at its default is
        // replaced by a free one for this run; a port it sets is not.
        OwnProxy::Held { addr, holder } => {
            let text = std::fs::read_to_string(config)
                .with_context(|| format!("cannot read config file {}", config.display()))?;
            let free = free_proxy_port(config, &text, addr, &holder, daemon::port_free)?;
            eprintln!(
                "mockingbird: {addr} is held by {holder}, not by this project's proxy; it is not used"
            );
            free
        }
    };
    let listener = TcpListener::bind(proxy_addr)
        .await
        .with_context(|| format!("cannot bind the proxy on {proxy_addr}"))?;
    let standalone = StandaloneOptions {
        data_url: reqwest::Url::parse(data_url)?,
        admin_url: admin_url.to_owned(),
        ca_dir: cfg.proxy.effective_ca_dir()?,
        unmapped: cfg.proxy.unmapped,
        vendor_subdomains: cfg.proxy.vendor_subdomains,
        images: cfg.proxy.images.clone(),
        origin: Some(crate::ops::Origin::current(Some(config))),
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    let task = tokio::spawn(run_standalone(listener, standalone, stop_rx));
    started.proxy = Some((stop_tx, task));
    eprintln!("mockingbird: proxy http://{proxy_addr} in front of the server at {admin_url}");
    Ok(proxy_addr)
}

/// Where `run` puts its proxy for the config at `config` (whose text is
/// `text`) when `holder` holds the proxy's address `addr`: the first free
/// port above it (`is_free` says which are) while the config leaves
/// `proxy.port` at its default, as `server start` does; a port the config
/// sets is never moved.
///
/// # Errors
///
/// The config sets `proxy.port` ([`held_message`]), or no port near it is
/// free.
fn free_proxy_port(
    config: &std::path::Path,
    text: &str,
    addr: SocketAddr,
    holder: &str,
    is_free: impl Fn(IpAddr, u16) -> bool,
) -> anyhow::Result<SocketAddr> {
    if file_config::explicit_ports(text).proxy_port {
        bail!(held_message(config, addr, holder));
    }
    let slot = daemon::PortSlot {
        key: "proxy.port",
        host: addr.ip(),
        port: addr.port(),
        defaulted: true,
    };
    // The address is held, so a port is always chosen when one is free.
    let port = daemon::choose_free_ports(&[slot], |host, port| {
        port != addr.port() && is_free(host, port)
    })?
    .first()
    .map_or(addr.port(), |&(_, port)| port);
    Ok(SocketAddr::new(addr.ip(), port))
}

/// The warning `run` prints when it reuses the server at `admin_url`
/// although flags that only shape a server it starts were given (a
/// leftover dev server would otherwise turn `run --replay` into live LLM
/// calls without a word).
fn reuse_warning(args: &RunArgs, admin_url: &str) -> Option<String> {
    let ignored: Vec<&str> = [
        (args.config.is_some(), "-c"),
        (args.offline, "--offline"),
        (args.record.is_some(), "--record"),
        (args.replay.is_some(), "--replay"),
    ]
    .into_iter()
    .filter_map(|(given, flag)| given.then_some(flag))
    .collect();
    if ignored.is_empty() {
        return None;
    }
    Some(format!(
        "mockingbird: warning: a server already answers at {admin_url}; {} only shape a server `run` starts and do not change this one (stop it with `mockingbird server stop` to apply them)",
        ignored.join(", ")
    ))
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .unwrap_or_default()
}

/// The JSON `/readyz` at `admin_url` answers with, if any.
async fn readyz(admin_url: &str) -> Option<serde_json::Value> {
    http()
        .get(format!("{admin_url}/readyz"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()
}

/// Who runs the server whose `/readyz` body is `body` when it is not the
/// server of `config`: one that reports its origin (config and folder)
/// and was started with another config, or without one. A server that does
/// not report its origin (an older version) counts as `config`'s.
fn foreign_server(body: &serde_json::Value, config: &std::path::Path) -> Option<String> {
    body["dir"].as_str()?;
    if daemon::started_with(body, config) {
        return None;
    }
    daemon::describe_holder(body)
}

/// Whether anything answers `/readyz` at `admin_url`.
async fn answers(admin_url: &str) -> bool {
    http()
        .get(format!("{admin_url}/readyz"))
        .send()
        .await
        .is_ok()
}

/// Waits until `/readyz` answers 2xx.
async fn wait_ready(admin_url: &str, wait: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Ok(resp) = http().get(format!("{admin_url}/readyz")).send().await
            && resp.status().is_success()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("the server at {admin_url} is not ready after {wait:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Runs `command` with the proxy environment and returns its exit code.
/// SIGINT/SIGTERM sent to `run` are passed on to it, and `run` waits for
/// it to end.
async fn run_command(
    command: &[OsString],
    opts: &crate::proxy::env::ProxyEnvOptions,
) -> anyhow::Result<ExitCode> {
    let env = proxy_env(opts)?;
    let (program, rest) = command
        .split_first()
        .ok_or_else(|| anyhow!("no command given after --"))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(rest).envs(&env.vars);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "mockingbird: cannot run {}: {error}",
                program.to_string_lossy()
            );
            return Ok(ExitCode::from(127));
        }
    };
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            () = crate::server::shutdown_signal() => {
                // The terminal already sent Ctrl-C to the whole foreground
                // group; a SIGTERM (CI cancelling the job) is passed on.
                if let Some(pid) = child.id() {
                    daemon::terminate(pid, false);
                }
            }
        }
    };
    Ok(exit_code(status))
}

fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    if let Some(code) = status.code() {
        return ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return ExitCode::from(u8::try_from(128 + signal).unwrap_or(1));
        }
    }
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_default_proxy_port_is_replaced_and_a_set_one_is_not() {
        let config = std::path::Path::new("/p/mockingbird.yaml");
        let held: SocketAddr = "127.0.0.1:8888".parse().unwrap();
        let holder = "the Mockingbird proxy of config /q/mockingbird.yaml (in /q)";
        let free = |_: IpAddr, port: u16| port != 8889;
        assert_eq!(
            free_proxy_port(config, "hosts: [api.example.com]\n", held, holder, free).unwrap(),
            "127.0.0.1:8890".parse::<SocketAddr>().unwrap()
        );
        let error = free_proxy_port(config, "proxy:\n  port: 8888\n", held, holder, free)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(holder) && error.contains("/p/mockingbird.yaml"),
            "{error}"
        );
    }
}
