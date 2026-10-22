//! `mockingbird proxy` on its own: the proxy in front of a running server,
//! forwarding intercepted requests to its data plane with `Host` intact.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, anyhow};
use axum::extract::Request;
use clap::Subcommand;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::ca::LocalCa;
use super::env::{options_for, proxy_env};
use super::forward::{forward, forward_client};
use super::{HostClaims, ProxyState};
use crate::file_config::{self, FileConfig, Unmapped};
use crate::registry::request_host;

/// How often a standalone proxy refreshes the claimed hosts.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// `mockingbird ca …`.
#[derive(Subcommand, Debug)]
pub enum CaCommand {
    /// Print the local CA certificate path (created if missing).
    Path,
}

/// `mockingbird proxy`: run the proxy in front of a running server, or
/// `mockingbird proxy env` to print the variables to export.
#[derive(clap::Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ProxyArgs {
    #[command(subcommand)]
    pub cmd: Option<ProxyCommand>,
    #[command(flatten)]
    pub run: ProxyRunArgs,
}

#[derive(Subcommand, Debug)]
pub enum ProxyCommand {
    /// Print the variables that send an app through the proxy and make it
    /// trust the local CA: `eval "$(mockingbird proxy env)"`.
    Env(ProxyEnvArgs),
}

#[derive(clap::Args, Debug)]
pub struct ProxyEnvArgs {
    /// Config file whose `server.host`, `proxy.port` and `proxy.ca_dir`
    /// apply.
    #[arg(short = 'c', long)]
    pub config: Option<PathBuf>,
    /// The proxy's port [default: proxy.port, else 8888].
    #[arg(long)]
    pub port: Option<u16>,
    /// Print JSON (`vars`, `ca_path`, `bundle_path`) instead of `export`
    /// lines.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Debug, Default)]
pub struct ProxyRunArgs {
    /// Config file whose `server` and `proxy` sections apply.
    #[arg(short = 'c', long)]
    pub config: Option<PathBuf>,
    /// Bind address [default: proxy.host, else 127.0.0.1]; off loopback the
    /// unmapped mode must be block.
    #[arg(long)]
    pub host: Option<IpAddr>,
    /// Port [default: proxy.port, else 8888].
    #[arg(long)]
    pub port: Option<u16>,
    /// The server's data plane [default: http://<server.host>:<server.port>].
    #[arg(long)]
    pub data_url: Option<String>,
    /// What to do with hosts no mock claims [default: proxy.unmapped, else
    /// pass].
    #[arg(long, value_enum)]
    pub unmapped: Option<Unmapped>,
    /// A host answered with placeholder images (repeatable; adds to
    /// proxy.images).
    #[arg(long = "image-host")]
    pub image_host: Vec<String>,
}

/// What a standalone proxy serves with.
#[derive(Debug, Clone)]
pub struct StandaloneOptions {
    /// The running server's data plane.
    pub data_url: reqwest::Url,
    /// Its admin (ops) port, which lists the claimed hosts.
    pub admin_url: String,
    pub ca_dir: PathBuf,
    pub unmapped: Unmapped,
    pub images: Vec<String>,
}

/// The hosts a running server claims, refreshed from its `/proxy/hosts`.
#[derive(Default)]
struct RemoteClaims(RwLock<HashSet<String>>);

impl HostClaims for RemoteClaims {
    fn claims(&self, host: &str) -> bool {
        self.0.read().is_ok_and(|hosts| hosts.contains(host))
    }
}

#[derive(Deserialize)]
struct HostsBody {
    hosts: Vec<String>,
}

async fn fetch_hosts(client: &reqwest::Client, admin_url: &str) -> anyhow::Result<HashSet<String>> {
    let url = format!("{}/proxy/hosts", admin_url.trim_end_matches('/'));
    let body: HostsBody = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(body.hosts.into_iter().collect())
}

/// Runs the proxy on `listener` until `stop` turns true, intercepting the
/// hosts the server at `opts.admin_url` claims (refreshed every 2 s) and
/// forwarding those requests to `opts.data_url` with their `Host`.
pub async fn run_standalone(
    listener: TcpListener,
    opts: StandaloneOptions,
    stop: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let client = forward_client()?;
    let claims = Arc::new(RemoteClaims::default());
    let initial = fetch_hosts(&client, &opts.admin_url)
        .await
        .with_context(|| format!("no Mockingbird server answers at {}", opts.admin_url))?;
    if let Ok(mut hosts) = claims.0.write() {
        *hosts = initial;
    }
    let refresher = {
        let (claims, client, admin_url) = (claims.clone(), client.clone(), opts.admin_url.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REFRESH_INTERVAL);
            tick.tick().await;
            loop {
                tick.tick().await;
                match fetch_hosts(&client, &admin_url).await {
                    Ok(fresh) => {
                        if let Ok(mut hosts) = claims.0.write() {
                            *hosts = fresh;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%admin_url, error = %format!("{error:#}"), "proxy: cannot refresh the claimed hosts; keeping the last list");
                    }
                }
            }
        })
    };
    let data_url = opts.data_url.clone();
    let upstream = axum::Router::new().fallback(move |req: Request| {
        let (client, data_url) = (client.clone(), data_url.clone());
        async move {
            let host = request_host(&req);
            let path = req
                .uri()
                .path_and_query()
                .map_or("/", |p| p.as_str())
                .to_owned();
            let target = upstream_url(&data_url, &path);
            forward(&client, target, host.as_deref(), req).await
        }
    });
    let ca = LocalCa::load_or_create(&opts.ca_dir)
        .with_context(|| format!("loading the local CA in {}", opts.ca_dir.display()))?;
    let state = ProxyState::new(claims, upstream, Arc::new(ca), opts.unmapped, opts.images)?;
    let served = super::serve(listener, state, stop).await;
    refresher.abort();
    served.map_err(|e| anyhow!("proxy: {e}"))
}

/// `data_url` with `path_and_query` appended below its path. Never a URL
/// join: `//other.test/x` must stay a path on the data plane.
fn upstream_url(data_url: &reqwest::Url, path_and_query: &str) -> reqwest::Url {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    let mut url = data_url.clone();
    let base = url.path().trim_end_matches('/').to_owned();
    url.set_path(&format!("{base}{path}"));
    url.set_query(query);
    url
}

/// Prints the local CA certificate path, creating the CA if missing.
pub fn ca_command(cmd: CaCommand) -> anyhow::Result<()> {
    match cmd {
        CaCommand::Path => {
            println!("{}", super::ca::ca_cert_path()?.display());
            Ok(())
        }
    }
}

/// `mockingbird proxy …`: prints the environment (`env`), or runs the
/// proxy in front of the server at `admin_url` until SIGINT/SIGTERM.
pub async fn run(args: ProxyArgs, admin_url: &str) -> anyhow::Result<()> {
    if let Some(ProxyCommand::Env(env)) = args.cmd {
        let vars = proxy_env(&options_for(env.config.as_deref(), env.port)?)?;
        if env.json {
            println!("{}", serde_json::to_string_pretty(&vars)?);
        } else {
            print!("{}", vars.to_shell());
        }
        return Ok(());
    }
    let run = args.run;
    let mut cfg = match &run.config {
        Some(path) => file_config::load(path)?,
        None => FileConfig::default(),
    };
    file_config::apply_env(&mut cfg)?;
    let host = run.host.unwrap_or(cfg.proxy.host);
    let port = run.port.unwrap_or(cfg.proxy.port);
    let data_host = match cfg.server.host {
        ip if ip.is_unspecified() => IpAddr::from([127, 0, 0, 1]),
        ip => ip,
    };
    let data_url = match &run.data_url {
        Some(url) => url.clone(),
        None => format!("http://{}", SocketAddr::new(data_host, cfg.server.port)),
    };
    let data_url =
        reqwest::Url::parse(&data_url).with_context(|| format!("--data-url {data_url}"))?;
    let mut images = cfg.proxy.images.clone();
    images.extend(run.image_host.iter().map(|h| h.to_ascii_lowercase()));
    mb_store::validate_hosts(&images).map_err(|e| anyhow!("--image-host: {e}"))?;
    let unmapped = run.unmapped.unwrap_or(cfg.proxy.unmapped);
    crate::file_config::ProxyConfig {
        unmapped,
        ..cfg.proxy.clone()
    }
    .check_bind(host)
    .map_err(|e| anyhow!(e))?;
    let opts = StandaloneOptions {
        data_url,
        admin_url: admin_url.to_owned(),
        ca_dir: cfg.proxy.effective_ca_dir()?,
        unmapped,
        images,
    };
    let addr = SocketAddr::new(host, port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the proxy on {addr}"))?;
    let ca_path = opts.ca_dir.join("ca.pem");
    eprintln!(
        "proxy on http://{addr} for the server at {} (CA {}); `eval \"$(mockingbird proxy env)\"` in the app's shell",
        opts.admin_url,
        ca_path.display()
    );
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        crate::server::shutdown_signal().await;
        let _ = stop_tx.send(true);
    });
    run_standalone(listener, opts, stop_rx).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::upstream_url;

    #[test]
    fn upstream_paths_stay_on_the_data_plane() {
        let base: reqwest::Url = "http://127.0.0.1:8080".parse().unwrap();
        let u = upstream_url(&base, "//evil.test/x?a=1");
        assert_eq!(u.host_str(), Some("127.0.0.1"));
        assert_eq!(u.path(), "//evil.test/x");
        assert_eq!(u.query(), Some("a=1"));
        let prefixed: reqwest::Url = "http://127.0.0.1:8080/base/".parse().unwrap();
        assert_eq!(upstream_url(&prefixed, "/orders").path(), "/base/orders");
    }
}
