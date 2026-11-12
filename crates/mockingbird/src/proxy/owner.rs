//! Whose proxy listens on a port. `proxy env`, `proxy browser`, the MCP
//! `proxy_env` tool and `run` hand an app only the proxy of its project's
//! own server (or one run in front of it for that project), never another
//! project's proxy that happens to hold the same port: that proxy has none
//! of this project's mocks and would pass the app's calls on to the real
//! APIs.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use anyhow::bail;
use serde_json::Value;

use super::env::{ProxyEnvOptions, options_for};
use crate::daemon;
use crate::file_config;

/// What listens on a proxy address.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Listener {
    /// Nothing.
    Nothing,
    /// A Mockingbird proxy, with its [`super::ORIGIN_PATH`] answer.
    Proxy(Value),
    /// Something that does not answer like a Mockingbird proxy: another
    /// program, or a Mockingbird older than [`super::ORIGIN_PATH`].
    Other,
}

/// Asks the listener on `addr` whose proxy it is.
pub(crate) async fn probe(addr: SocketAddr) -> Listener {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .unwrap_or_default();
    match client
        .get(format!("http://{addr}{}", super::ORIGIN_PATH))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(body) if body["proxy"] == "mockingbird" => Listener::Proxy(body),
            _ => Listener::Other,
        },
        Err(error) if error.is_connect() => Listener::Nothing,
        Err(_) => Listener::Other,
    }
}

/// Who runs the proxy whose [`super::ORIGIN_PATH`] answer is `body`.
pub(crate) fn describe_proxy(body: &Value) -> String {
    match (body["config"].as_str(), body["dir"].as_str()) {
        (Some(config), Some(dir)) => {
            format!("the Mockingbird proxy of config {config} (in {dir})")
        }
        (None, Some(dir)) => format!("a Mockingbird proxy started without a config in {dir}"),
        _ => "a Mockingbird proxy that does not say whose it is".to_owned(),
    }
}

/// Who holds `addr`, when it is not this project's proxy.
fn describe_listener(listener: &Listener) -> String {
    match listener {
        Listener::Proxy(body) => describe_proxy(body),
        _ => "a program that does not answer as a Mockingbird proxy (another program, or an older Mockingbird)".to_owned(),
    }
}

/// The proxy of a project, as [`own_proxy`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnProxy {
    /// The project's proxy listens at this address.
    Running(SocketAddr),
    /// Nothing listens at `addr` (where the project's proxy would). When
    /// the project's server answers and runs no proxy, `bare_server` is its
    /// admin URL.
    Missing {
        addr: SocketAddr,
        bare_server: Option<String>,
    },
    /// Someone else's listener holds `addr`; `holder` says whose.
    Held { addr: SocketAddr, holder: String },
}

/// The proxy of the project whose config is `config`: where its server's
/// `/readyz` says its proxy listens (`port`, else `proxy.port`, when it
/// does not say), and whether the listener there is that project's proxy
/// (it answers [`super::ORIGIN_PATH`] with this config).
///
/// # Errors
///
/// `config` does not load.
pub async fn own_proxy(config: &Path, port: Option<u16>) -> anyhow::Result<OwnProxy> {
    let opts = options_for(Some(config), port)?;
    let mut cfg = file_config::load(config)?;
    file_config::apply_env(&mut cfg)?;
    let admin_url = daemon::url(cfg.server.host, cfg.server.admin_port);
    let (_, _, body) = daemon::readyz(&admin_url).await;
    let ours = daemon::started_with(&body, config);
    let reported = body
        .get("proxy")
        .filter(|_| ours && port.is_none())
        .and_then(Value::as_str)
        .and_then(|a| a.parse::<SocketAddr>().ok());
    let addr = match reported {
        Some(mut addr) => {
            if addr.ip().is_unspecified() {
                addr.set_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
            }
            addr
        }
        None => SocketAddr::new(opts.host.parse()?, opts.port),
    };
    Ok(match probe(addr).await {
        Listener::Proxy(found) if daemon::started_with(&found, config) => OwnProxy::Running(addr),
        Listener::Nothing => OwnProxy::Missing {
            addr,
            bare_server: (ours && body.get("proxy").is_some_and(Value::is_null))
                .then_some(admin_url),
        },
        other => OwnProxy::Held {
            addr,
            holder: describe_listener(&other),
        },
    })
}

/// Why `proxy env` (and the commands like it) hand out nothing when the
/// server of `config` at `admin_url` runs no proxy.
#[must_use]
pub fn no_proxy_message(config: &Path, admin_url: &str) -> String {
    let c = config.display();
    format!(
        "the server of {c} at {admin_url} runs no proxy, so this project has no proxy to send an app through. Run the app with `mockingbird run -c {c} -- <command>`, which puts a proxy in front of that server, or restart the server with its proxy: set `proxy:` `enabled: true` in {c}, or `mockingbird server stop` then `mockingbird server start -c {c} --proxy` (a taken default proxy port is replaced by a free one, written into the config)"
    )
}

/// Why `proxy env` (and the commands like it) hand out nothing when
/// `holder` holds `addr`, where the proxy of `config` would listen.
#[must_use]
pub fn held_message(config: &Path, addr: SocketAddr, holder: &str) -> String {
    let c = config.display();
    format!(
        "{addr} is held by {holder}, not by the proxy of {c}; nothing was handed out, so no app is sent through it. Leave it alone and give this project's proxy a free port: set proxy.port in {c}, or leave proxy.port unset and `mockingbird server start -c {c} --proxy` picks a free one and writes it there; `mockingbird run -c {c} -- <command>` also works"
    )
}

/// [`options_for`], checked against what listens: with a config, the
/// address is that of the project's own proxy ([`own_proxy`]). The second
/// value is a warning when nothing listens there yet.
///
/// # Errors
///
/// The config does not load; the project's server runs no proxy
/// ([`no_proxy_message`]); or another listener holds the proxy's address
/// ([`held_message`]).
pub async fn checked_options(
    config: Option<&Path>,
    port: Option<u16>,
) -> anyhow::Result<(ProxyEnvOptions, Option<String>)> {
    let mut opts = options_for(config, port)?;
    // Without a config there is no project to check the proxy against.
    let Some(config) = config else {
        return Ok((opts, None));
    };
    let (addr, warning) = match own_proxy(config, port).await? {
        OwnProxy::Running(addr) => (addr, None),
        OwnProxy::Missing {
            bare_server: Some(admin_url),
            ..
        } => bail!(no_proxy_message(config, &admin_url)),
        OwnProxy::Missing {
            addr,
            bare_server: None,
        } => {
            let c = config.display();
            (
                addr,
                Some(format!(
                    "nothing listens on {addr} yet: start this project's server with its proxy (`mockingbird server start -c {c} --proxy`), or run the app with `mockingbird run -c {c} -- <command>`"
                )),
            )
        }
        OwnProxy::Held { addr, holder } => bail!(held_message(config, addr, &holder)),
    };
    opts.host = addr.ip().to_string();
    opts.port = addr.port();
    Ok((opts, warning))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_proxy_is_named_from_its_origin() {
        assert_eq!(
            describe_proxy(&json!({"proxy": "mockingbird", "config": "/a/m.yaml", "dir": "/a"})),
            "the Mockingbird proxy of config /a/m.yaml (in /a)"
        );
        assert_eq!(
            describe_proxy(&json!({"proxy": "mockingbird", "config": null, "dir": "/b"})),
            "a Mockingbird proxy started without a config in /b"
        );
        assert_eq!(
            describe_proxy(&json!({"proxy": "mockingbird"})),
            "a Mockingbird proxy that does not say whose it is"
        );
    }

    #[tokio::test]
    async fn nothing_listening_is_told_apart_from_another_program() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = free.local_addr().unwrap();
        drop(free);
        assert_eq!(probe(addr).await, Listener::Nothing);
        let stub = wiremock::MockServer::start().await;
        assert_eq!(probe(*stub.address()).await, Listener::Other);
    }
}
