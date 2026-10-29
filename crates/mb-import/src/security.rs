//! Security requirements (OpenAPI 3 `security`/`securitySchemes`, Swagger 2
//! `security`/`securityDefinitions`) compiled into MCM form.

use crate::ImportError;
use crate::openapi3::deref;
use mb_mcm::{ApiKeyLocation, SecurityRequirement, SecurityScheme};
use serde_json::Value;

/// The prefix of the header name of the requirement that stands in for
/// alternatives the mock cannot check (see [`compile_security`]). It holds a
/// space, so it is not a valid header name and no request satisfies it.
const UNSUPPORTED_SCHEME_PREFIX: &str = "unsupported security scheme ";

/// The requirements that apply to `op`: its own `security` when present
/// (`[]` meaning none), else the document's.
///
/// `schemes_pointer` locates the scheme definitions
/// (`/components/securitySchemes` or `/securityDefinitions`). A requirement
/// object with no entries (`{}`) is kept empty, making authentication
/// optional. A requirement naming a scheme the mock cannot check (an `http`
/// scheme other than bearer and basic, `mutualTLS`, an `apiKey` outside
/// header, query and cookie) or an undefined one is unsatisfiable: it is
/// never weakened to its other schemes. All such requirements are replaced
/// by one requirement no request satisfies, placed after the supported
/// ones, so an operation whose every alternative is unsupported stays
/// locked rather than becoming unsecured, and [`crate::bundle_warnings`]
/// reports it as `security_scheme_unsupported`.
pub(crate) fn compile_security(
    doc: &Value,
    op: &Value,
    schemes_pointer: &str,
    at: &str,
) -> Result<Vec<SecurityRequirement>, ImportError> {
    let Some(list) = op
        .get("security")
        .or_else(|| doc.get("security"))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };
    let definitions = doc.pointer(schemes_pointer);
    let mut out = Vec::new();
    let mut unsupported: Vec<String> = Vec::new();
    for requirement in list {
        let Some(entries) = requirement.as_object() else {
            continue;
        };
        let mut schemes = Vec::new();
        let mut satisfiable = true;
        for (name, scopes) in entries {
            let scheme = match definitions.and_then(|d| d.get(name)) {
                Some(def) => {
                    let def = deref(doc, def, &format!("{at}/security ({name})"))?;
                    map_scheme(def, scopes)
                }
                None => None,
            };
            match scheme {
                Some(scheme) => schemes.push(scheme),
                None => {
                    satisfiable = false;
                    unsupported.push(header_safe(name));
                }
            }
        }
        if satisfiable {
            out.push(SecurityRequirement { schemes });
        }
    }
    if !unsupported.is_empty() {
        unsupported.sort();
        unsupported.dedup();
        out.push(SecurityRequirement {
            schemes: vec![SecurityScheme::ApiKey {
                location: ApiKeyLocation::Header,
                name: format!("{UNSUPPORTED_SCHEME_PREFIX}{}", unsupported.join(" ")),
            }],
        });
    }
    Ok(out)
}

/// The scheme names the stand-in requirement of `security` (see
/// [`compile_security`]) lists, when it has one.
pub(crate) fn unsupported_schemes(security: &[SecurityRequirement]) -> Option<&str> {
    security
        .iter()
        .flat_map(|r| &r.schemes)
        .find_map(|s| match s {
            SecurityScheme::ApiKey {
                location: ApiKeyLocation::Header,
                name,
            } => name.strip_prefix(UNSUPPORTED_SCHEME_PREFIX),
            _ => None,
        })
}

/// `name` restricted to characters safe in a quoted `WWW-Authenticate`
/// parameter, since the stand-in's name can surface in a challenge.
fn header_safe(name: &str) -> String {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    if safe.is_empty() {
        "unnamed".to_owned()
    } else {
        safe
    }
}

fn map_scheme(def: &Value, scopes: &Value) -> Option<SecurityScheme> {
    let text = |key: &str| def.get(key).and_then(Value::as_str);
    let scopes = || -> Vec<String> {
        scopes
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    match text("type")? {
        "http" => match text("scheme")?.to_ascii_lowercase().as_str() {
            "bearer" => Some(SecurityScheme::Bearer),
            "basic" => Some(SecurityScheme::Basic),
            _ => None,
        },
        "basic" => Some(SecurityScheme::Basic),
        "apiKey" => {
            let location = match text("in")? {
                "header" => ApiKeyLocation::Header,
                "query" => ApiKeyLocation::Query,
                "cookie" => ApiKeyLocation::Cookie,
                _ => return None,
            };
            Some(SecurityScheme::ApiKey {
                location,
                name: text("name")?.to_owned(),
            })
        }
        "oauth2" => Some(SecurityScheme::OAuth2 { scopes: scopes() }),
        "openIdConnect" => Some(SecurityScheme::OpenIdConnect { scopes: scopes() }),
        _ => None,
    }
}
