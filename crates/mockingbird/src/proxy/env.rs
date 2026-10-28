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
/// - host: `proxy.host`, with `0.0.0.0`/`::` read as `127.0.0.1`;
/// - CA directory: `proxy.ca_dir`, else `MB_CA_DIR` or `~/.mockingbird/ca`.
pub fn options_for(config: Option<&Path>, port: Option<u16>) -> anyhow::Result<ProxyEnvOptions> {
    let mut cfg = match config {
        Some(path) => file_config::load(path)?,
        None => FileConfig::default(),
    };
    file_config::apply_env(&mut cfg)?;
    let host = match cfg.proxy.host {
        ip if ip.is_unspecified() => IpAddr::from([127, 0, 0, 1]),
        ip => ip,
    };
    Ok(ProxyEnvOptions {
        host: host.to_string(),
        port: port.unwrap_or(cfg.proxy.port),
        ca_dir: cfg.proxy.effective_ca_dir()?,
    })
}

/// The Node.js preload `NODE_OPTIONS` requires (see the file).
pub const NODE_PRELOAD: &str = include_str!("node-preload.cjs");

/// The preload's file name in the CA directory.
const NODE_PRELOAD_FILE: &str = "node-proxy.cjs";

/// The variables to export, with the files they point at.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ProxyEnv {
    pub vars: BTreeMap<String, String>,
    pub ca_path: PathBuf,
    pub bundle_path: PathBuf,
    /// The Node.js preload (`NODE_OPTIONS=--require …`).
    pub node_preload: PathBuf,
    /// The Java truststore (`JAVA_TOOL_OPTIONS`).
    pub truststore_path: PathBuf,
    /// Base64 SHA-256 of the CA's public key, for Chrome's
    /// `--ignore-certificate-errors-spki-list`.
    pub ca_spki_sha256: String,
    /// The proxy URL.
    pub proxy_url: String,
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

/// The proxy variables for `opts`, merged with this process's
/// `NODE_OPTIONS` and `JAVA_TOOL_OPTIONS`; see [`proxy_env_with`].
pub fn proxy_env(opts: &ProxyEnvOptions) -> anyhow::Result<ProxyEnv> {
    proxy_env_with(opts, |name| std::env::var(name).ok())
}

/// The proxy variables for `opts`:
/// - `HTTPS_PROXY`/`HTTP_PROXY` (and their lowercase forms, which curl and
///   several Python and Go tools read), `NO_PROXY=localhost,127.0.0.1`;
/// - Node.js: `NODE_EXTRA_CA_CERTS` (the CA), `NODE_USE_ENV_PROXY=1`, and
///   `NODE_OPTIONS=--require <preload>` (the preload routes every agent and
///   `fetch` through the proxy; `MOCKINGBIRD_CA_FILE` names the CA for it);
/// - `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`: system roots
///   plus the CA (Python, Ruby, Go, curl, .NET on Linux); `DENO_CERT`;
/// - Java: `JAVA_TOOL_OPTIONS` with the proxy properties and a truststore.
///
/// `inherited` reads the current `NODE_OPTIONS`/`JAVA_TOOL_OPTIONS`, which
/// are kept after ours (once). Creates the CA when it is missing and
/// rewrites the bundle, the truststore and the preload.
pub fn proxy_env_with(
    opts: &ProxyEnvOptions,
    inherited: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<ProxyEnv> {
    let ca = LocalCa::load_or_create(&opts.ca_dir)
        .with_context(|| format!("loading the local CA in {}", opts.ca_dir.display()))?;
    let bundle_path = ca.write_bundle().context("writing the CA bundle")?;
    let truststore_path = ca
        .write_truststore()
        .context("writing the Java truststore")?;
    let node_preload = opts.ca_dir.join(NODE_PRELOAD_FILE);
    write_if_changed(&node_preload, NODE_PRELOAD)?;
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
    let preload_text = node_preload.display().to_string();
    let node_options = merge_options(
        &format!("--require {}", node_quote(&preload_text)),
        &preload_text,
        inherited("NODE_OPTIONS"),
    );
    let store = truststore_path.display().to_string();
    let java = [
        format!("-Dhttp.proxyHost={}", opts.host),
        format!("-Dhttp.proxyPort={}", opts.port),
        format!("-Dhttps.proxyHost={}", opts.host),
        format!("-Dhttps.proxyPort={}", opts.port),
        "-Dhttp.nonProxyHosts=localhost|127.0.0.1".to_owned(),
        format!("-Djavax.net.ssl.trustStore={}", java_quote(&store)),
        "-Djavax.net.ssl.trustStoreType=JKS".to_owned(),
        format!(
            "-Djavax.net.ssl.trustStorePassword={}",
            super::ca::TRUSTSTORE_PASSWORD
        ),
    ]
    .join(" ");
    let java = merge_options(&java, &store, inherited("JAVA_TOOL_OPTIONS"));
    let vars = BTreeMap::from([
        ("HTTPS_PROXY".to_owned(), proxy.clone()),
        ("HTTP_PROXY".to_owned(), proxy.clone()),
        ("https_proxy".to_owned(), proxy.clone()),
        ("http_proxy".to_owned(), proxy.clone()),
        ("NO_PROXY".to_owned(), no_proxy.clone()),
        ("no_proxy".to_owned(), no_proxy),
        ("NODE_EXTRA_CA_CERTS".to_owned(), ca_text.clone()),
        ("MOCKINGBIRD_CA_FILE".to_owned(), ca_text.clone()),
        ("DENO_CERT".to_owned(), ca_text),
        ("SSL_CERT_FILE".to_owned(), bundle_text.clone()),
        ("REQUESTS_CA_BUNDLE".to_owned(), bundle_text.clone()),
        ("CURL_CA_BUNDLE".to_owned(), bundle_text),
        ("NODE_USE_ENV_PROXY".to_owned(), "1".to_owned()),
        ("NODE_OPTIONS".to_owned(), node_options),
        ("JAVA_TOOL_OPTIONS".to_owned(), java),
    ]);
    Ok(ProxyEnv {
        vars,
        ca_path,
        bundle_path,
        node_preload,
        truststore_path,
        ca_spki_sha256: ca.spki_sha256(),
        proxy_url: proxy,
    })
}

/// `ours` followed by `existing`, unless `existing` already names `marker`
/// (a second `eval` of `proxy env` changes nothing).
fn merge_options(ours: &str, marker: &str, existing: Option<String>) -> String {
    match existing.map(|e| e.trim().to_owned()) {
        Some(e) if e.contains(marker) => e,
        Some(e) if !e.is_empty() => format!("{ours} {e}"),
        _ => ours.to_owned(),
    }
}

/// A path as one `NODE_OPTIONS` word: double-quoted when it needs it.
fn node_quote(path: &str) -> String {
    if path.contains([' ', '"', '\\', '\'']) {
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        path.to_owned()
    }
}

/// A path as one `JAVA_TOOL_OPTIONS` word (the JVM splits on spaces and
/// honours double quotes since Java 9).
fn java_quote(path: &str) -> String {
    if path.contains(' ') {
        format!("\"{path}\"")
    } else {
        path.to_owned()
    }
}

/// Writes `contents` to `path` unless it already holds them.
fn write_if_changed(path: &Path, contents: &str) -> anyhow::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == contents) {
        return Ok(());
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn env_names_proxy_ca_and_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let env = proxy_env_with(
            &ProxyEnvOptions {
                host: "127.0.0.1".into(),
                port: 9999,
                ca_dir: dir.path().into(),
            },
            |_| None,
        )
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
        assert_eq!(v("CURL_CA_BUNDLE"), v("SSL_CERT_FILE"));
        assert_eq!(v("MOCKINGBIRD_CA_FILE"), v("NODE_EXTRA_CA_CERTS"));
        assert_eq!(env.vars.len(), 15);
        assert!(
            env.to_shell()
                .contains("export HTTPS_PROXY='http://127.0.0.1:9999'\n")
        );
    }

    #[test]
    fn node_and_java_options_are_merged_once() {
        let dir = tempfile::tempdir().unwrap();
        let opts = ProxyEnvOptions {
            host: "127.0.0.1".into(),
            port: 9999,
            ca_dir: dir.path().into(),
        };
        let env = proxy_env_with(&opts, |name| {
            (name == "NODE_OPTIONS").then(|| "--max-old-space-size=4096".to_owned())
        })
        .unwrap();
        let node = &env.vars["NODE_OPTIONS"];
        let preload = dir.path().join("node-proxy.cjs");
        assert_eq!(
            node,
            &format!("--require {} --max-old-space-size=4096", preload.display())
        );
        assert_eq!(std::fs::read_to_string(&preload).unwrap(), NODE_PRELOAD);
        // A second run in a shell that already has it changes nothing.
        let again =
            proxy_env_with(&opts, |name| (name == "NODE_OPTIONS").then(|| node.clone())).unwrap();
        assert_eq!(&again.vars["NODE_OPTIONS"], node);
        let java = &env.vars["JAVA_TOOL_OPTIONS"];
        assert!(java.contains("-Dhttps.proxyHost=127.0.0.1 -Dhttps.proxyPort=9999"));
        assert!(java.contains(&format!(
            "-Djavax.net.ssl.trustStore={}",
            dir.path().join("truststore.jks").display()
        )));
        let jks = std::fs::read(&env.truststore_path).unwrap();
        assert_eq!(&jks[..8], &[0xFE, 0xED, 0xFE, 0xED, 0, 0, 0, 2]);
        assert_eq!(env.ca_spki_sha256.len(), 44);
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        assert_eq!(node_quote("/a b/x.cjs"), "\"/a b/x.cjs\"");
        assert_eq!(node_quote("/ab/x.cjs"), "/ab/x.cjs");
        assert_eq!(java_quote("/a b/t.jks"), "\"/a b/t.jks\"");
    }
}
