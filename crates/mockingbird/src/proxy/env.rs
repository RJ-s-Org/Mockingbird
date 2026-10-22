//! The environment variables that send an app's HTTP(S) traffic through
//! the proxy and make it trust the local CA (`mockingbird proxy env`).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;

use super::ca::LocalCa;
use crate::file_config::{self, FileConfig};

/// Where the proxy listens and where its CA lives.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyEnvOptions {
    pub host: String,
    pub port: u16,
    pub ca_dir: PathBuf,
}

/// The options for the proxy `config` (a config file, with the `MB_*`
/// environment applied; defaults without one) describes:
/// - port: `port`, else `proxy.port` (8888 by default);
/// - host: `server.host`, with `0.0.0.0`/`::` read as `127.0.0.1`;
/// - CA directory: `proxy.ca_dir`, else `MB_CA_DIR` or `~/.mockingbird/ca`.
pub fn options_for(config: Option<&Path>, port: Option<u16>) -> anyhow::Result<ProxyEnvOptions> {
    let mut cfg = match config {
        Some(path) => file_config::load(path)?,
        None => FileConfig::default(),
    };
    file_config::apply_env(&mut cfg)?;
    let host = match cfg.server.host {
        ip if ip.is_unspecified() => IpAddr::from([127, 0, 0, 1]),
        ip => ip,
    };
    Ok(ProxyEnvOptions {
        host: host.to_string(),
        port: port.unwrap_or(cfg.proxy.port),
        ca_dir: cfg.proxy.effective_ca_dir()?,
    })
}

/// The variables to export, with the CA certificate and the trust bundle
/// they point at.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ProxyEnv {
    pub vars: BTreeMap<String, String>,
    pub ca_path: PathBuf,
    pub bundle_path: PathBuf,
}

impl ProxyEnv {
    /// One `export KEY='value'` line per variable, sorted by name.
    pub fn to_shell(&self) -> String {
        self.vars
            .iter()
            .map(|(k, v)| format!("export {k}='{}'\n", v.replace('\'', "'\\''")))
            .collect()
    }
}

/// The proxy variables for `opts`: `HTTPS_PROXY`/`HTTP_PROXY` (and their
/// lowercase forms, which curl and several Python and Go tools read),
/// `NO_PROXY=localhost,127.0.0.1`, `NODE_EXTRA_CA_CERTS` (the CA),
/// `SSL_CERT_FILE`/`REQUESTS_CA_BUNDLE` (system roots plus the CA) and
/// `NODE_USE_ENV_PROXY=1`. Creates the CA when it is missing and rewrites
/// the bundle.
pub fn proxy_env(opts: &ProxyEnvOptions) -> anyhow::Result<ProxyEnv> {
    let ca = LocalCa::load_or_create(&opts.ca_dir)
        .with_context(|| format!("loading the local CA in {}", opts.ca_dir.display()))?;
    let bundle_path = ca.write_bundle().context("writing the CA bundle")?;
    let ca_path = ca.cert_path();
    let host = if opts.host.contains(':') {
        format!("[{}]", opts.host)
    } else {
        opts.host.clone()
    };
    let proxy = format!("http://{host}:{}", opts.port);
    let no_proxy = "localhost,127.0.0.1".to_owned();
    let ca_text = ca_path.display().to_string();
    let bundle_text = bundle_path.display().to_string();
    let vars = BTreeMap::from([
        ("HTTPS_PROXY".to_owned(), proxy.clone()),
        ("HTTP_PROXY".to_owned(), proxy.clone()),
        ("https_proxy".to_owned(), proxy.clone()),
        ("http_proxy".to_owned(), proxy),
        ("NO_PROXY".to_owned(), no_proxy.clone()),
        ("no_proxy".to_owned(), no_proxy),
        ("NODE_EXTRA_CA_CERTS".to_owned(), ca_text),
        ("SSL_CERT_FILE".to_owned(), bundle_text.clone()),
        ("REQUESTS_CA_BUNDLE".to_owned(), bundle_text),
        ("NODE_USE_ENV_PROXY".to_owned(), "1".to_owned()),
    ]);
    Ok(ProxyEnv {
        vars,
        ca_path,
        bundle_path,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn env_names_proxy_ca_and_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let env = proxy_env(&ProxyEnvOptions {
            host: "127.0.0.1".into(),
            port: 9999,
            ca_dir: dir.path().into(),
        })
        .unwrap();
        let v = |k: &str| env.vars[k].clone();
        assert_eq!(v("HTTPS_PROXY"), "http://127.0.0.1:9999");
        assert_eq!(v("http_proxy"), "http://127.0.0.1:9999");
        assert_eq!(v("NO_PROXY"), "localhost,127.0.0.1");
        assert_eq!(v("NODE_USE_ENV_PROXY"), "1");
        assert_eq!(
            PathBuf::from(v("NODE_EXTRA_CA_CERTS")),
            dir.path().join("ca.pem")
        );
        assert_eq!(
            PathBuf::from(v("SSL_CERT_FILE")),
            dir.path().join("bundle.pem")
        );
        assert_eq!(v("REQUESTS_CA_BUNDLE"), v("SSL_CERT_FILE"));
        assert_eq!(env.vars.len(), 10);
        assert!(
            env.to_shell()
                .contains("export HTTPS_PROXY='http://127.0.0.1:9999'\n")
        );
    }
}
