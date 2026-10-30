use std::collections::HashMap;

use mb_mcm::{Binding, Bundle, HttpMethod};
use serde_json::{Map, Value};

/// Why a contract's HTTP bindings cannot be routed ([`RouteTable::new`]).
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    /// Operation `op`'s path conflicts with an earlier route of the same
    /// method, or is not a valid route pattern. The message includes the
    /// `matchit` error, which is also the
    /// [`source`](std::error::Error::source).
    #[error("operation {op}: cannot route {method:?} {path}: {source}")]
    Insert {
        op: String,
        method: HttpMethod,
        path: String,
        source: matchit::InsertError,
    },
}

/// Maps `(method, path)` to an operation id.
pub struct RouteTable {
    by_method: HashMap<HttpMethodKey, matchit::Router<String>>,
}

/// Shows how many methods have routes; `matchit` routers are opaque.
impl std::fmt::Debug for RouteTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteTable")
            .field("methods", &self.by_method.len())
            .finish_non_exhaustive()
    }
}

/// `HttpMethod` is not `Hash`; key the table by its declaration order.
type HttpMethodKey = u8;

fn key(method: HttpMethod) -> HttpMethodKey {
    method as u8
}

const ALL_METHODS: [HttpMethod; 7] = [
    HttpMethod::Get,
    HttpMethod::Post,
    HttpMethod::Put,
    HttpMethod::Patch,
    HttpMethod::Delete,
    HttpMethod::Head,
    HttpMethod::Options,
];

impl RouteTable {
    /// The routes of every HTTP operation of `bundle`.
    ///
    /// # Errors
    ///
    /// [`RouteError::Insert`] when two operations of one method claim the
    /// same path, or a path is not a valid route.
    pub fn new(bundle: &Bundle) -> Result<Self, RouteError> {
        let mut by_method: HashMap<HttpMethodKey, matchit::Router<String>> = HashMap::new();
        for op in &bundle.operations {
            let Binding::Http { method, path } = &op.binding else {
                continue;
            };
            let route = normalize(path);
            by_method
                .entry(key(*method))
                .or_default()
                .insert(route.to_string(), op.id.clone())
                .map_err(|source| RouteError::Insert {
                    op: op.id.clone(),
                    method: *method,
                    path: path.clone(),
                    source,
                })?;
        }
        Ok(Self { by_method })
    }

    /// The operation id and percent-decoded path parameters for a raw
    /// (still percent-encoded) request path. One trailing `/` is ignored.
    pub fn find(&self, method: HttpMethod, path: &str) -> Option<(String, Map<String, Value>)> {
        let router = self.by_method.get(&key(method))?;
        let matched = router.at(normalize(path)).ok()?;
        let params = matched
            .params
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(percent_decode(v))))
            .collect();
        Some((matched.value.clone(), params))
    }

    /// Methods that have an operation matching `path`, in a stable order.
    pub fn allowed(&self, path: &str) -> Vec<HttpMethod> {
        let path = normalize(path);
        ALL_METHODS
            .into_iter()
            .filter(|m| {
                self.by_method
                    .get(&key(*m))
                    .is_some_and(|r| r.at(path).is_ok())
            })
            .collect()
    }
}

/// Strip one trailing `/`, except for the root path.
fn normalize(path: &str) -> &str {
    match path.strip_suffix('/') {
        Some(stripped) if !stripped.is_empty() => stripped,
        _ => path,
    }
}

/// Decode `%XX` escapes; invalid escapes are kept verbatim and invalid
/// UTF-8 is replaced.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && hex.iter().all(u8::is_ascii_hexdigit)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_trailing_slash() {
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("/orders/"), "/orders");
        assert_eq!(normalize("/orders"), "/orders");
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("o%201"), "o 1");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%+1"), "%+1");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
    }
}
