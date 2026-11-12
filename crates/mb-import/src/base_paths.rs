//! The path prefixes an OpenAPI or Swagger contract declares for its
//! operations: Swagger `basePath` and OpenAPI `servers[].url`.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{SourceFormat, archive, detect_format, http_root, unpack_archive};

/// The top-level fields [`base_paths`] reads; the rest of the document is
/// skipped without being built.
#[derive(Deserialize)]
struct Header {
    openapi: Option<Value>,
    swagger: Option<Value>,
    asyncapi: Option<Value>,
    #[serde(rename = "basePath")]
    base_path: Option<Value>,
    host: Option<Value>,
    servers: Option<Value>,
}

/// The path prefixes the HTTP operations of the contract in `source` (the
/// bytes [`compile`](crate::compile) takes, an archive included) are also
/// served under, in declaration order and without duplicates. Each starts
/// with `/` and has no trailing `/`; a root prefix (`/`) is left out.
///
/// - Swagger 2.0: `basePath`.
/// - OpenAPI 3.x: the path of each top-level `servers[].url`, its
///   `{variables}` replaced by their `default`. A server whose URL names a
///   variable without a default is skipped, as is a relative URL that does
///   not start with `/` (it is relative to wherever the document is
///   served). The query and fragment are dropped. Path-item and operation
///   `servers` are not read.
///
/// Every other format, and bytes that are no contract, have none. The
/// operations' own paths are unchanged, so a mock serves each operation
/// both at its contract path and under every prefix.
#[must_use]
pub fn base_paths(source: &[u8]) -> Vec<String> {
    if archive::is_archive(source) {
        return unpack_archive(source)
            .ok()
            .and_then(|files| http_root(&files).map(|root| base_paths(&root.bytes)))
            .unwrap_or_default();
    }
    let header = serde_json::from_slice::<Header>(source)
        .ok()
        .or_else(|| serde_saphyr::from_slice::<Header>(source).ok());
    header.map(|h| of_header(&h)).unwrap_or_default()
}

fn of_header(h: &Header) -> Vec<String> {
    let mut versions = Map::new();
    for (key, v) in [
        ("openapi", &h.openapi),
        ("swagger", &h.swagger),
        ("asyncapi", &h.asyncapi),
    ] {
        if let Some(v) = v {
            versions.insert(key.into(), v.clone());
        }
    }
    let candidates: Vec<String> = match detect_format(&Value::Object(versions)) {
        Ok(SourceFormat::Swagger2) => h
            .base_path
            .as_ref()
            .and_then(Value::as_str)
            .map(str::to_owned)
            .into_iter()
            .collect(),
        Ok(SourceFormat::OpenApi3) => h
            .servers
            .as_ref()
            .and_then(Value::as_array)
            .map(|servers| servers.iter().filter_map(server_path).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for prefix in candidates.iter().filter_map(|p| prefix(p)) {
        if !out.contains(&prefix) {
            out.push(prefix);
        }
    }
    out
}

/// The hosts the contract in `source` (the bytes [`compile`](crate::compile)
/// takes, an archive included) says its API is served at, in declaration
/// order, lower case, without duplicates, port or credentials:
///
/// - Swagger 2.0: `host`.
/// - OpenAPI 3.x: the host of each top-level `servers[].url` whose
///   `{variables}` all have a `default`; a relative URL has none.
///
/// Every other format, and bytes that are no contract, have none. A mock
/// links the records it answers under them (see `mb_protocols`' URL members).
#[must_use]
pub fn server_hosts(source: &[u8]) -> Vec<String> {
    if archive::is_archive(source) {
        return unpack_archive(source)
            .ok()
            .and_then(|files| http_root(&files).map(|root| server_hosts(&root.bytes)))
            .unwrap_or_default();
    }
    let header = serde_json::from_slice::<Header>(source)
        .ok()
        .or_else(|| serde_saphyr::from_slice::<Header>(source).ok());
    let Some(h) = header else {
        return Vec::new();
    };
    let mut versions = Map::new();
    for (key, v) in [("openapi", &h.openapi), ("swagger", &h.swagger)] {
        if let Some(v) = v {
            versions.insert(key.into(), v.clone());
        }
    }
    let candidates: Vec<String> = match detect_format(&Value::Object(versions)) {
        Ok(SourceFormat::Swagger2) => h
            .host
            .as_ref()
            .and_then(Value::as_str)
            .and_then(authority_host)
            .into_iter()
            .collect(),
        Ok(SourceFormat::OpenApi3) => h
            .servers
            .as_ref()
            .and_then(Value::as_array)
            .map(|servers| servers.iter().filter_map(server_host).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for host in candidates {
        if !out.contains(&host) {
            out.push(host);
        }
    }
    out
}

/// The host of a server object's URL, its variables replaced by their
/// defaults; `None` for a relative URL or a variable with no default.
fn server_host(server: &Value) -> Option<String> {
    let url = substitute(server.get("url")?.as_str()?, server.get("variables"))?;
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return None;
    }
    authority_host(rest.split(['/', '?', '#']).next().unwrap_or_default())
}

/// The host of an authority (`user@host:port`), lower case; `None` when empty.
fn authority_host(authority: &str) -> Option<String> {
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or(v6, |(h, _)| h),
        None => host.split(':').next().unwrap_or_default(),
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The path of a server object's URL, its variables replaced by their
/// defaults; `None` when a variable has no default or the URL has no
/// absolute path.
fn server_path(server: &Value) -> Option<String> {
    let url = substitute(server.get("url")?.as_str()?, server.get("variables"))?;
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let after_authority = match url.find("//") {
        // `scheme://authority/path` or `//authority/path`.
        Some(i)
            if url[..i]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.:".contains(c)) =>
        {
            let rest = &url[i + 2..];
            rest.find('/').map_or("", |slash| &rest[slash..])
        }
        _ => url,
    };
    after_authority
        .starts_with('/')
        .then(|| after_authority.to_owned())
}

/// `url` with each `{name}` replaced by `variables.name.default`; `None`
/// when a variable has no string default.
fn substitute(url: &str, variables: Option<&Value>) -> Option<String> {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = rest[open..].find('}')? + open;
        let name = &rest[open + 1..close];
        let default = variables?.get(name)?.get("default")?.as_str()?;
        out.push_str(default);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// `path` as a route prefix: it starts with `/`, has no trailing `/` and
/// no braces, and is not the root.
fn prefix(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    (path.starts_with('/') && !trimmed.is_empty() && !trimmed.contains(['{', '}']))
        .then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{prefix, server_path, substitute};
    use serde_json::json;

    #[test]
    fn server_paths() {
        let path = |url: &str| server_path(&json!({ "url": url }));
        assert_eq!(path("https://a.example.com/v1").as_deref(), Some("/v1"));
        assert_eq!(path("//a.example.com/v1").as_deref(), Some("/v1"));
        assert_eq!(path("https://a.example.com"), None);
        assert_eq!(path("/v1?x=1").as_deref(), Some("/v1"));
        assert_eq!(path("/v1//x").as_deref(), Some("/v1//x"));
        assert_eq!(path("v1"), None);
    }

    #[test]
    fn substitutes_defaults() {
        let vars = json!({"v": {"default": "2"}});
        assert_eq!(substitute("/v{v}", Some(&vars)).as_deref(), Some("/v2"));
        assert_eq!(substitute("/v{w}", Some(&vars)), None);
        assert_eq!(substitute("/v{v", Some(&vars)), None);
        assert_eq!(substitute("/plain", None).as_deref(), Some("/plain"));
    }

    #[test]
    fn prefixes() {
        assert_eq!(prefix("/v1/").as_deref(), Some("/v1"));
        assert_eq!(prefix("/"), None);
        assert_eq!(prefix(""), None);
        assert_eq!(prefix("/{x}"), None);
    }
}
