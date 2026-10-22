//! `serve`: validates the configuration, binds the listeners the node's
//! role needs and serves the [`App`] until SIGINT/SIGTERM, then shuts down
//! gracefully. Other binaries (custom builds with their own
//! [`Extensions`]) reuse it through [`run_with`].

use std::net::SocketAddr;

use axum_server::Handle;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use mb_ext::Extensions;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::config::{
    App, LlmEnv, ServeArgs, WATCH_ON_DATA_NODE, bind_admin, bind_grpc, build_app_with,
    missing_env_api_key, resolve_config,
};
use crate::file_config::Role;

/// How long open connections may delay shutdown after the stop signal.
pub const GRACEFUL_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Why serving failed.
#[derive(Debug)]
pub enum ServeError {
    /// A command-line or configuration mistake (exit code 2).
    Usage(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for ServeError {
    fn from(e: anyhow::Error) -> Self {
        ServeError::Other(e)
    }
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::Usage(msg) => f.write_str(msg),
            ServeError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ServeError {}

/// The bound listeners; `None` for a listener the role does not use (or a
/// default admin/gRPC port that was busy).
pub struct Listeners {
    pub data: Option<TcpListener>,
    pub admin: Option<TcpListener>,
    pub grpc: Option<TcpListener>,
    /// The intercepting proxy, when [`App::proxy`] is set.
    pub proxy: Option<TcpListener>,
}

/// Which listeners a role binds: (data, admin, grpc). A `control` node
/// serves no mocks, so it binds neither the data nor the gRPC listener.
pub fn binds_for(role: Role) -> (bool, bool, bool) {
    match role {
        Role::All | Role::Data => (true, true, true),
        Role::Control => (false, true, false),
    }
}

/// [`run_with`] with `MB_LLM_*` from the environment and the self-hosted
/// [`Extensions`].
pub async fn run(args: ServeArgs) -> Result<(), ServeError> {
    run_with(args, Some(LlmEnv::from_env()), Extensions::default()).await
}

/// Validates the configuration, binds the role's listeners (see
/// [`binds_for`]), builds the app and serves it until SIGINT/SIGTERM, then
/// shuts down gracefully (see [`serve_app`]).
pub async fn run_with(
    args: ServeArgs,
    llm: Option<LlmEnv>,
    ext: Extensions,
) -> Result<(), ServeError> {
    let cfg = resolve_config(&args, llm.as_ref())?;
    let role = cfg.server.role;
    if role == Role::Data && args.watch {
        return Err(ServeError::Usage(WATCH_ON_DATA_NODE.into()));
    }
    cfg.server
        .check_bind(&cfg.auth)
        .map_err(ServeError::Usage)?;
    if missing_env_api_key(&cfg, args.offline) {
        return Err(ServeError::Usage(
            "MB_LLM_API_KEY is not set (use --offline for faker-only mode)".into(),
        ));
    }
    let (bind_data, bind_admin_port, bind_grpc_port) = binds_for(role);
    let data_addr = SocketAddr::new(cfg.server.host, cfg.server.port);
    let admin_addr = SocketAddr::new(cfg.server.host, cfg.server.admin_port);
    let grpc_addr = SocketAddr::new(cfg.server.host, cfg.server.grpc_port);
    let data = match bind_data {
        true => Some(
            TcpListener::bind(data_addr)
                .await
                .map_err(|e| anyhow::anyhow!("cannot bind {data_addr}: {e}"))?,
        ),
        false => None,
    };
    let admin = match bind_admin_port {
        true => bind_admin(admin_addr, cfg.server.admin_port_explicit).await?,
        false => None,
    };
    let grpc = match bind_grpc_port {
        true => bind_grpc(grpc_addr, cfg.server.grpc_port_explicit).await?,
        false => None,
    };

    let app = build_app_with(&args, llm, ext).await?;
    let proxy_addr = SocketAddr::new(cfg.proxy.host, cfg.proxy.port);
    let proxy = match &app.proxy {
        Some(_) if bind_data => Some(
            TcpListener::bind(proxy_addr)
                .await
                .map_err(|e| anyhow::anyhow!("cannot bind the proxy on {proxy_addr}: {e}"))?,
        ),
        _ => None,
    };
    tracing::info!(role = role.as_str(), "starting");
    let (scheme, grpc_mode) = match app.tls {
        Some(_) => ("https", "TLS"),
        None => ("http", "h2c"),
    };
    match &data {
        Some(_) => tracing::info!("data plane on {scheme}://{data_addr}"),
        None => tracing::info!("no data plane"),
    }
    match &admin {
        Some(_) if role == Role::Data => tracing::info!("ops endpoints on {scheme}://{admin_addr}"),
        Some(_) => tracing::info!("admin API on {scheme}://{admin_addr}"),
        None => tracing::info!("no admin API"),
    }
    match &grpc {
        Some(_) => tracing::info!("gRPC ({grpc_mode}) on {grpc_addr}"),
        None => tracing::info!("no gRPC listener"),
    }
    if let (Some(_), Some(state)) = (&proxy, &app.proxy) {
        tracing::info!(
            "proxy on http://{proxy_addr} (CA {})",
            state.ca_cert_path().display()
        );
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutting down");
        crate::runtime::stop_open_streams();
        let _ = stop_tx.send(true);
    });
    serve_app(
        app,
        Listeners {
            data,
            admin,
            grpc,
            proxy,
        },
        stop_rx,
    )
    .await?;
    Ok(())
}

/// Serves `app` on `listeners` (the data plane, the admin port with the
/// peer address, gRPC as h2c; all three over TLS when [`App::tls`] is set)
/// until `stop` turns true, then drains the open connections for at most
/// [`GRACEFUL_SHUTDOWN_TIMEOUT`] and calls [`App::shutdown`].
pub async fn serve_app(
    app: App,
    listeners: Listeners,
    stop: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let Listeners {
        data,
        admin,
        grpc,
        proxy,
    } = listeners;
    let grace_rx = stop.clone();
    let tls = app.tls.clone();
    // The peer address keys the login-failure audit rate limit.
    let data = serve_listener(data, app.data.clone(), tls.clone(), stop.clone());
    let admin = serve_listener(admin, app.admin.clone(), tls.clone(), stop.clone());
    // axum serves HTTP/2 prior knowledge (h2c), which is what gRPC uses;
    // over TLS, ALPN negotiates h2.
    let grpc = serve_listener(grpc, app.grpc.clone(), tls, stop.clone());
    let proxy = serve_proxy(proxy, app.proxy.clone(), stop);
    let servers = async { tokio::join!(data, admin, grpc, proxy) };
    // Connections still open this long after the stop signal (a gRPC
    // stream, a WebSocket) are abandoned, so the request log is still
    // flushed before the supervisor's kill.
    let grace = async {
        stopped(grace_rx).await;
        tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT).await;
    };
    tokio::select! {
        (data, admin, grpc, proxy) = servers => {
            data.map_err(|e| anyhow::anyhow!("data plane: {e}"))?;
            admin.map_err(|e| anyhow::anyhow!("admin API: {e}"))?;
            grpc.map_err(|e| anyhow::anyhow!("gRPC: {e}"))?;
            proxy.map_err(|e| anyhow::anyhow!("proxy: {e}"))?;
        }
        () = grace => tracing::warn!(
            "connections still open {GRACEFUL_SHUTDOWN_TIMEOUT:?} after the stop signal; closing them"
        ),
    }
    app.shutdown().await;
    Ok(())
}

/// Serves the proxy on `listener` until `stop` turns true; nothing to do
/// without both a listener and a proxy.
async fn serve_proxy(
    listener: Option<TcpListener>,
    state: Option<std::sync::Arc<crate::proxy::ProxyState>>,
    stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    match (listener, state) {
        (Some(listener), Some(state)) => crate::proxy::serve(listener, state, stop).await,
        _ => Ok(()),
    }
}

/// Resolves once `rx` turns true.
async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

/// Serves `router` (with the peer address as connect info) on `listener`
/// until `stop` turns true: plain HTTP, or TLS with `tls` through
/// `axum_server`, whose [`Handle`] then shuts down gracefully within
/// [`GRACEFUL_SHUTDOWN_TIMEOUT`]. Nothing to do without a listener.
async fn serve_listener(
    listener: Option<TcpListener>,
    router: axum::Router,
    tls: Option<RustlsConfig>,
    stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let Some(listener) = listener else {
        return Ok(());
    };
    let service = router.into_make_service_with_connect_info::<SocketAddr>();
    match tls {
        None => {
            axum::serve(listener, service)
                .with_graceful_shutdown(stopped(stop))
                .await
        }
        Some(tls) => {
            let handle = Handle::<SocketAddr>::new();
            let shutdown = handle.clone();
            let watcher = tokio::spawn(async move {
                stopped(stop).await;
                shutdown.graceful_shutdown(Some(GRACEFUL_SHUTDOWN_TIMEOUT));
            });
            let served = axum_server::Server::from_listener(listener)
                .acceptor(RustlsAcceptor::new(tls))
                .handle(handle)
                .serve(service)
                .await;
            watcher.abort();
            served
        }
    }
}

/// Resolves on SIGINT (Ctrl-C) or, on Unix, SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
