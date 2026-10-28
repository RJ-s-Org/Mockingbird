//! `mockingbird run -- <command…>`: runs a command against the mocks. Starts
//! the server (with its proxy) unless one already answers, waits until it
//! is ready, runs the command with the proxy environment for every runtime
//! ([`crate::proxy::env`]), stops what it started and exits with the
//! command's exit code.

use std::ffi::OsString;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::daemon::{self, StartArgs, StateArgs};
use crate::file_config;
use crate::proxy::env::{options_for, proxy_env};
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
    let opts = options_for(Some(&config), None)?;
    let proxy_addr = SocketAddr::new(opts.host.parse()?, opts.port);

    let mut started = Started::default();
    let result = prepare(
        &args,
        &config,
        &cfg,
        &admin_url,
        &data_url,
        proxy_addr,
        &mut started,
    )
    .await;
    let code = match result {
        Ok(()) => run_command(&args.command, &opts).await,
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

/// Makes sure the server and its proxy answer, starting what is missing.
async fn prepare(
    args: &RunArgs,
    config: &std::path::Path,
    cfg: &file_config::FileConfig,
    admin_url: &str,
    data_url: &str,
    proxy_addr: SocketAddr,
    started: &mut Started,
) -> anyhow::Result<()> {
    if !answers(admin_url).await {
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
            "mockingbird: server started (log {}), proxy http://{proxy_addr}",
            args.state_dir.join("server.log").display()
        );
        return Ok(());
    }
    wait_ready(admin_url, Duration::from(args.wait)).await?;
    if TcpStream::connect(proxy_addr).await.is_ok() {
        return Ok(());
    }
    // A server without its proxy: run one in front of it for the command.
    let listener = TcpListener::bind(proxy_addr)
        .await
        .with_context(|| format!("cannot bind the proxy on {proxy_addr}"))?;
    let standalone = StandaloneOptions {
        data_url: reqwest::Url::parse(data_url)?,
        admin_url: admin_url.to_owned(),
        ca_dir: cfg.proxy.effective_ca_dir()?,
        unmapped: cfg.proxy.unmapped,
        images: cfg.proxy.images.clone(),
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    let task = tokio::spawn(run_standalone(listener, standalone, stop_rx));
    started.proxy = Some((stop_tx, task));
    eprintln!("mockingbird: proxy http://{proxy_addr} in front of the server at {admin_url}");
    Ok(())
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .unwrap_or_default()
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
