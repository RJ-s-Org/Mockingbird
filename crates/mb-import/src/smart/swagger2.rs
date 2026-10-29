//! Swagger 2.0 -> OpenAPI 3.0 conversion of embedded documents, so a
//! Swagger block on a documentation page goes through the same embedded
//! path as an OpenAPI 3 one.
//!
//! What is converted: `definitions`, `parameters`, `responses` and
//! `securityDefinitions` (to `components`, with every `$ref` rewritten),
//! `host`, `basePath` and `schemes` (to one server), `in: body` parameters
//! and `in: formData` parameters (to a `requestBody` in each media type of
//! `consumes`, else JSON, or the form or multipart type), the inline
//! schemas of the other parameters and of response headers, response
//! schemas (in each JSON media type of `produces`, else JSON), and
//! `x-nullable` (to `nullable`). A form parameter of `type: file` becomes a
//! binary string. Path-level parameters are merged into each operation.

use serde_json::{Map, Value, json};

/// Parameter keys that describe the parameter itself, not its value schema.
const PARAM_META: [&str; 6] = [
    "name",
    "in",
    "required",
    "description",
    "allowEmptyValue",
    "collectionFormat",
];
const METHODS: [&str; 7] = ["get", "put", "post", "delete", "options", "head", "patch"];
const FORM: &str = "application/x-www-form-urlencoded";
const MULTIPART: &str = "multipart/form-data";
const JSON: &str = "application/json";
/// Swagger 2 ref prefixes and their OpenAPI 3 counterparts.
const REF_PREFIXES: [(&str, &str); 3] = [
    ("#/definitions/", "#/components/schemas/"),
    ("#/parameters/", "#/components/parameters/"),
    ("#/responses/", "#/components/responses/"),
];

/// The OpenAPI 3.0 form of the Swagger 2.0 document `doc`.
pub(crate) fn to_openapi3(doc: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    out.insert("openapi".into(), json!("3.0.3"));
    if let Some(info) = doc.get("info") {
        out.insert("info".into(), info.clone());
    }
    if let Some(url) = server_url(doc) {
        out.insert("servers".into(), json!([{"url": url}]));
    }
    for key in ["security", "tags", "externalDocs"] {
        if let Some(v) = doc.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    let consumes = media_types(doc.get("consumes"));
    let produces = media_types(doc.get("produces"));

    let mut paths = Map::new();
    if let Some(items) = doc.get("paths").and_then(Value::as_object) {
        for (path, item) in items {
            let Some(item) = item.as_object() else {
                continue;
            };
            let shared = item
                .get("parameters")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut converted = Map::new();
            for (key, value) in item {
                match key.as_str() {
                    "parameters" => {}
                    k if METHODS.contains(&k) => {
                        if let Some(op) = value.as_object() {
                            let op = operation(doc, op, shared, &consumes, &produces);
                            converted.insert(key.clone(), op);
                        }
                    }
                    _ => {
                        converted.insert(key.clone(), value.clone());
                    }
                }
            }
            paths.insert(path.clone(), Value::Object(converted));
        }
    }
    out.insert("paths".into(), Value::Object(paths));

    let mut components = Map::new();
    if let Some(defs) = doc.get("definitions").filter(|d| d.is_object()) {
        components.insert("schemas".into(), defs.clone());
    }
    if let Some(params) = doc.get("parameters").and_then(Value::as_object) {
        // Body and form parameters are inlined where they are used.
        let params: Map<String, Value> = params
            .iter()
            .filter(|(_, p)| !is_body_or_form(p))
            .map(|(name, p)| (name.clone(), parameter(p)))
            .collect();
        if !params.is_empty() {
            components.insert("parameters".into(), Value::Object(params));
        }
    }
    if let Some(responses) = doc.get("responses").and_then(Value::as_object) {
        let responses: Map<String, Value> = responses
            .iter()
            .map(|(name, r)| (name.clone(), response(r, &produces)))
            .collect();
        components.insert("responses".into(), Value::Object(responses));
    }
    if let Some(schemes) = doc.get("securityDefinitions").and_then(Value::as_object) {
        let schemes: Map<String, Value> = schemes
            .iter()
            .map(|(name, s)| (name.clone(), security_scheme(s)))
            .collect();
        components.insert("securitySchemes".into(), Value::Object(schemes));
    }
    if !components.is_empty() {
        out.insert("components".into(), Value::Object(components));
    }
    let mut out = Value::Object(out);
    rewrite(&mut out);
    out
}

/// `<scheme>://<host><basePath>` (the first of `schemes`, else `https`),
/// or the `basePath` alone without a host.
fn server_url(doc: &Map<String, Value>) -> Option<String> {
    let host = doc
        .get("host")
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty());
    let base = doc
        .get("basePath")
        .and_then(Value::as_str)
        .filter(|b| *b != "/")
        .unwrap_or("");
    match host {
        Some(host) => {
            let scheme = doc
                .get("schemes")
                .and_then(Value::as_array)
                .and_then(|s| s.first())
                .and_then(Value::as_str)
                .unwrap_or("https");
            Some(format!("{scheme}://{host}{base}"))
        }
        None if !base.is_empty() => Some(base.to_owned()),
        None => None,
    }
}

fn media_types(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn location(p: &Value) -> &str {
    p.get("in").and_then(Value::as_str).unwrap_or("")
}

fn is_body_or_form(p: &Value) -> bool {
    matches!(location(p), "body" | "formData")
}

/// `p`, or the document parameter its `#/parameters/` ref names.
fn resolve<'a>(doc: &'a Map<String, Value>, p: &'a Value) -> &'a Value {
    p.get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/parameters/"))
        .and_then(|name| doc.get("parameters")?.get(name))
        .unwrap_or(p)
}

/// The value schema of a non-body parameter or a header: its own keys but
/// the descriptive ones; `type: file` is a binary string.
fn inline_schema(p: &Value) -> Value {
    if p.get("type").and_then(Value::as_str) == Some("file") {
        return json!({"type": "string", "format": "binary"});
    }
    let schema: Map<String, Value> = p
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !PARAM_META.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    Value::Object(schema)
}

/// An OpenAPI 3 parameter object for a Swagger 2 non-body parameter.
fn parameter(p: &Value) -> Value {
    if p.get("$ref").is_some() {
        return p.clone();
    }
    let mut out = Map::new();
    for key in ["name", "in", "required", "description", "allowEmptyValue"] {
        if let Some(v) = p.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    out.insert("schema".into(), inline_schema(p));
    Value::Object(out)
}

fn operation(
    doc: &Map<String, Value>,
    op: &Map<String, Value>,
    shared: &[Value],
    doc_consumes: &[String],
    doc_produces: &[String],
) -> Value {
    let consumes = match op.get("consumes") {
        Some(c) => media_types(Some(c)),
        None => doc_consumes.to_vec(),
    };
    let produces = match op.get("produces") {
        Some(p) => media_types(Some(p)),
        None => doc_produces.to_vec(),
    };
    // Path-level parameters first, replaced by an operation's own of the
    // same name and location.
    let own: Vec<&Value> = op
        .get("parameters")
        .and_then(Value::as_array)
        .map(|ps| ps.iter().map(|p| resolve(doc, p)).collect())
        .unwrap_or_default();
    let key = |p: &Value| {
        (
            location(p).to_owned(),
            p.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        )
    };
    let mut params: Vec<&Value> = shared
        .iter()
        .map(|p| resolve(doc, p))
        .filter(|p| !own.iter().any(|o| key(o) == key(p)))
        .collect();
    params.extend(own);

    let mut out = Map::new();
    for (k, v) in op {
        if !matches!(
            k.as_str(),
            "parameters" | "responses" | "consumes" | "produces" | "schemes"
        ) {
            out.insert(k.clone(), v.clone());
        }
    }
    let converted: Vec<Value> = params
        .iter()
        .filter(|p| !is_body_or_form(p))
        .map(|p| parameter(p))
        .collect();
    if !converted.is_empty() {
        out.insert("parameters".into(), Value::Array(converted));
    }
    if let Some(body) = request_body(&params, &consumes) {
        out.insert("requestBody".into(), body);
    }
    let responses: Map<String, Value> = op
        .get("responses")
        .and_then(Value::as_object)
        .map(|rs| {
            rs.iter()
                .map(|(status, r)| (status.clone(), response(r, &produces)))
                .collect()
        })
        .unwrap_or_default();
    out.insert("responses".into(), Value::Object(responses));
    Value::Object(out)
}

/// The `requestBody` of the `in: body` parameter (in each media type of
/// `consumes`, else JSON), else of the `in: formData` parameters (as one
/// object schema, in the form or multipart type).
fn request_body(params: &[&Value], consumes: &[String]) -> Option<Value> {
    let required = |p: &Value| p.get("required").and_then(Value::as_bool) == Some(true);
    let (content, is_required) = if let Some(body) = params.iter().find(|p| location(p) == "body") {
        let schema = body.get("schema").cloned().unwrap_or_else(|| json!({}));
        let types: Vec<&str> = if consumes.is_empty() {
            vec![JSON]
        } else {
            consumes.iter().map(String::as_str).collect()
        };
        let content: Map<String, Value> = types
            .into_iter()
            .map(|ct| (ct.to_owned(), json!({"schema": schema})))
            .collect();
        (content, required(body))
    } else {
        let form: Vec<&&Value> = params
            .iter()
            .filter(|p| location(p) == "formData")
            .collect();
        if form.is_empty() {
            return None;
        }
        let mut props = Map::new();
        let mut names = Vec::new();
        for p in &form {
            let Some(name) = p.get("name").and_then(Value::as_str) else {
                continue;
            };
            props.insert(name.to_owned(), inline_schema(p));
            if required(p) {
                names.push(json!(name));
            }
        }
        let mut schema = Map::new();
        schema.insert("type".into(), json!("object"));
        schema.insert("properties".into(), Value::Object(props));
        if !names.is_empty() {
            schema.insert("required".into(), Value::Array(names));
        }
        let has = |ct: &str| consumes.iter().any(|c| c.eq_ignore_ascii_case(ct));
        let ct = if !has(FORM) && has(MULTIPART) {
            MULTIPART
        } else {
            FORM
        };
        let mut content = Map::new();
        content.insert(ct.to_owned(), json!({"schema": Value::Object(schema)}));
        (content, form.iter().any(|p| required(p)))
    };
    let mut body = Map::new();
    if is_required {
        body.insert("required".into(), json!(true));
    }
    body.insert("content".into(), Value::Object(content));
    Some(Value::Object(body))
}

/// An OpenAPI 3 response object: the schema in each JSON media type of
/// `produces` (else JSON, or the first type when none is JSON), each
/// header's inline schema under `schema`.
fn response(r: &Value, produces: &[String]) -> Value {
    if r.get("$ref").is_some() {
        return r.clone();
    }
    let mut out = Map::new();
    out.insert(
        "description".into(),
        r.get("description").cloned().unwrap_or_else(|| json!("")),
    );
    if let Some(headers) = r.get("headers").and_then(Value::as_object) {
        let headers: Map<String, Value> = headers
            .iter()
            .map(|(name, h)| {
                let mut header = Map::new();
                if let Some(d) = h.get("description") {
                    header.insert("description".into(), d.clone());
                }
                header.insert("schema".into(), inline_schema(h));
                (name.clone(), Value::Object(header))
            })
            .collect();
        out.insert("headers".into(), Value::Object(headers));
    }
    if let Some(schema) = r.get("schema") {
        let json_types: Vec<&str> = produces
            .iter()
            .map(String::as_str)
            .filter(|ct| crate::openapi3::is_json_media(ct))
            .collect();
        let types = match (json_types.is_empty(), produces.first()) {
            (false, _) => json_types,
            (true, Some(first)) => vec![first.as_str()],
            (true, None) => vec![JSON],
        };
        let content: Map<String, Value> = types
            .into_iter()
            .map(|ct| (ct.to_owned(), json!({"schema": schema})))
            .collect();
        out.insert("content".into(), Value::Object(content));
    }
    Value::Object(out)
}

/// An OpenAPI 3 security scheme for a Swagger 2 one.
fn security_scheme(s: &Value) -> Value {
    match s.get("type").and_then(Value::as_str) {
        Some("basic") => {
            let mut out = json!({"type": "http", "scheme": "basic"});
            if let (Some(d), Value::Object(o)) = (s.get("description"), &mut out) {
                o.insert("description".into(), d.clone());
            }
            out
        }
        Some("oauth2") => {
            let scopes = s.get("scopes").cloned().unwrap_or_else(|| json!({}));
            let mut flow = Map::new();
            for key in ["authorizationUrl", "tokenUrl"] {
                if let Some(v) = s.get(key) {
                    flow.insert(key.into(), v.clone());
                }
            }
            flow.insert("scopes".into(), scopes);
            let name = match s.get("flow").and_then(Value::as_str) {
                Some("implicit") => "implicit",
                Some("password") => "password",
                Some("application") => "clientCredentials",
                _ => "authorizationCode",
            };
            let mut flows = Map::new();
            flows.insert(name.into(), Value::Object(flow));
            json!({"type": "oauth2", "flows": Value::Object(flows)})
        }
        _ => s.clone(),
    }
}

/// Rewrites every `$ref` to its OpenAPI 3 location and every `x-nullable`
/// key to `nullable`.
fn rewrite(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            if let Some(Value::String(r)) = obj.get_mut("$ref")
                && let Some((from, to)) = REF_PREFIXES.iter().find(|(from, _)| r.starts_with(from))
            {
                *r = format!("{to}{}", &r[from.len()..]);
            }
            if let Some(nullable) = obj.remove("x-nullable") {
                obj.insert("nullable".into(), nullable);
            }
            obj.values_mut().for_each(rewrite);
        }
        Value::Array(items) => items.iter_mut().for_each(rewrite),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{rewrite, server_url};
    use serde_json::json;

    #[test]
    fn server_url_from_host_base_path_and_schemes() {
        let doc = |v: serde_json::Value| v.as_object().cloned().unwrap();
        assert_eq!(
            server_url(&doc(
                json!({"host": "h", "basePath": "/v1", "schemes": ["http"]})
            )),
            Some("http://h/v1".into())
        );
        assert_eq!(
            server_url(&doc(json!({"host": "h"}))),
            Some("https://h".into())
        );
        assert_eq!(
            server_url(&doc(json!({"basePath": "/v2"}))),
            Some("/v2".into())
        );
        assert_eq!(server_url(&doc(json!({"basePath": "/"}))), None);
    }

    #[test]
    fn rewrite_moves_refs_and_nullable() {
        let mut v = json!({"a": [{"$ref": "#/definitions/A"}, {"$ref": "#/parameters/p"}],
            "b": {"$ref": "other.json#/definitions/X", "x-nullable": true}});
        rewrite(&mut v);
        assert_eq!(
            v,
            json!({"a": [{"$ref": "#/components/schemas/A"}, {"$ref": "#/components/parameters/p"}],
                "b": {"$ref": "other.json#/definitions/X", "nullable": true}})
        );
    }
}
