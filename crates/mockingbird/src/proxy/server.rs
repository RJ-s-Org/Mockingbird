//! The proxy listener: HTTP/1.1 with `CONNECT`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, header};
use axum::response::{IntoResponse, Response};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use mb_protocols::http_rest::problem;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tower::ServiceExt as _;

use super::ProxyState;
use super::images::placeholder_response;
use crate::file_config::Unmapped;
use crate::registry::{normalize_host, request_host};

/// Accepts proxy connections on `listener` until `stop` turns true.
pub async fn serve(
    listener: TcpListener,
    state: Arc<ProxyState>,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return Ok(());
                }
                continue;
            }
        };
        let tcp = match accepted {
            Ok((tcp, _)) => tcp,
            Err(error) => {
                // Out of file descriptors and the like: back off briefly.
                tracing::warn!(%error, "proxy: accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let state = Arc::clone(&state);
                async move { Ok::<_, Infallible>(handle(state, req.map(Body::new)).await) }
            });
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(tcp), svc)
                .with_upgrades()
                .await
            {
                tracing::debug!(%error, "proxy: connection ended with an error");
            }
        });
    }
}

fn blocked(host: &str) -> Response {
    problem(
        502,
        "Host not mocked",
        format!("proxy.unmapped is block: {host} is not served by any mock"),
    )
}

async fn handle(state: Arc<ProxyState>, req: Request) -> Response {
    if req.method() == Method::CONNECT {
        return connect(state, req);
    }
    let Some(host) = request_host(&req) else {
        return problem(
            400,
            "Not a proxy request",
            "the request names no host (send absolute-form requests or CONNECT)",
        );
    };
    if state.is_image_host(&host) {
        return placeholder_response(req.method(), req.uri().path());
    }
    if state.claims.claims(&host) {
        return serve_mapped(&state, req).await;
    }
    state.note_unmapped(&host);
    if state.unmapped == Unmapped::Block {
        return blocked(&host);
    }
    let target = match reqwest::Url::parse(&req.uri().to_string()) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => url,
        _ => {
            return problem(
                400,
                "Not a proxy request",
                format!("{} is not an absolute http:// URL", req.uri()),
            );
        }
    };
    super::forward::forward(&state.client, target, None, req).await
}

/// Serves a request for a claimed host from the data plane.
async fn serve_mapped(state: &ProxyState, req: Request) -> Response {
    match state.upstream.clone().oneshot(req).await {
        Ok(resp) => resp,
        Err(never) => match never {},
    }
}

/// Answers a `CONNECT`: intercepts a mapped host (TLS with a leaf from the
/// local CA, then the data plane), tunnels or blocks any other.
fn connect(state: Arc<ProxyState>, req: Request) -> Response {
    let Some(authority) = req.uri().authority().cloned() else {
        return problem(400, "Invalid CONNECT", "CONNECT needs a host:port target");
    };
    let Some(host) = normalize_host(authority.host()) else {
        return problem(400, "Invalid CONNECT", "CONNECT needs a host:port target");
    };
    let port = authority.port_u16().unwrap_or(443);
    let mapped = state.is_mapped(&host);
    if !mapped {
        state.note_unmapped(&host);
        if state.unmapped == Unmapped::Block {
            return blocked(&host);
        }
    }
    tokio::spawn(async move {
        let upgraded = match hyper::upgrade::on(req).await {
            Ok(upgraded) => TokioIo::new(upgraded),
            Err(error) => {
                tracing::debug!(%host, %error, "proxy: CONNECT upgrade failed");
                return;
            }
        };
        let result = if mapped {
            intercept(state, host.clone(), upgraded).await
        } else {
            tunnel(&host, port, upgraded).await
        };
        if let Err(error) = result {
            tracing::debug!(%host, error = %format!("{error:#}"), "proxy: CONNECT ended with an error");
        }
    });
    ().into_response()
}

async fn tunnel(
    host: &str,
    port: u16,
    mut client: TokioIo<hyper::upgrade::Upgraded>,
) -> anyhow::Result<()> {
    let mut upstream = TcpStream::connect((host, port)).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn intercept(
    state: Arc<ProxyState>,
    host: String,
    client: TokioIo<hyper::upgrade::Upgraded>,
) -> anyhow::Result<()> {
    let config = state.ca.server_config(&host)?;
    let tls = tokio_rustls::TlsAcceptor::from(config)
        .accept(client)
        .await?;
    let host_value = HeaderValue::from_str(&host)?;
    let svc = service_fn(move |req: Request<Incoming>| {
        let state = Arc::clone(&state);
        let host_value = host_value.clone();
        async move {
            let mut req = req.map(Body::new);
            if req.uri().authority().is_none() && !req.headers().contains_key(header::HOST) {
                req.headers_mut().insert(header::HOST, host_value.clone());
            }
            let host = host_value.to_str().unwrap_or_default().to_owned();
            let resp = if state.is_image_host(&host) {
                placeholder_response(req.method(), req.uri().path())
            } else {
                serve_mapped(&state, req).await
            };
            Ok::<_, Infallible>(resp)
        }
    });
    hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(tls), svc)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
