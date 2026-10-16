//! Security requirements (OpenAPI 3 `security`/`securitySchemes`, Swagger 2
//! `security`/`securityDefinitions`) compiled into MCM form.

use crate::ImportError;
use crate::openapi3::deref;
use mb_mcm::{ApiKeyLocation, SecurityRequirement, SecurityScheme};
use serde_json::Value;

/// The requirements that apply to `op`: its own `security` when present
/// (`[]` meaning none), else the document's.
///
/// `schemes_pointer` locates the scheme definitions
/// (`/components/securitySchemes` or `/securityDefinitions`). A requirement
/// object with no entries (`{}`) is kept empty, making authentication
/// optional. Unknown or undefined schemes are skipped; a requirement left
/// with no schemes by that is dropped.
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
    for requirement in list {
        let Some(entries) = requirement.as_object() else {
            continue;
        };
        if entries.is_empty() {
            out.push(SecurityRequirement {
                schemes: Vec::new(),
            });
            continue;
        }
        let mut schemes = Vec::new();
        for (name, scopes) in entries {
            let Some(def) = definitions.and_then(|d| d.get(name)) else {
                continue;
            };
            let def = deref(doc, def, &format!("{at}/security ({name})"))?;
            if let Some(scheme) = map_scheme(def, scopes) {
                schemes.push(scheme);
            }
        }
        if !schemes.is_empty() {
            out.push(SecurityRequirement { schemes });
        }
    }
    Ok(out)
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
