//! `/api/v1/workspaces/{ws}/mocks/{mock}/scenarios…`.

use axum::extract::State;
use axum::http::StatusCode;
use mb_auth::Permission;
use mb_engine::SchemaValidator;
use mb_mcm::{Bundle, Operation, StatusSelector};
use mb_store::{ChaosSettings, ResourceSource, Scenario, ScenarioRules};
use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::access::WsAccess;
use crate::audit::snapshot;
use crate::mocks::{ChaosSchema, MockPath};
use crate::problem::{ApiResult, Json, Path, Problem};
use crate::{AdminState, Source, ensure_api_managed, valid_identifier};

/// Title of the 422 for an invalid scenario body.
const INVALID_SCENARIO: &str = "Invalid scenario";

/// Longest accepted `stream_item_interval_ms` (30 s).
const MAX_STREAM_ITEM_INTERVAL_MS: u64 = 30_000;

/// A named scenario of a mock.
#[derive(Serialize, ToSchema, Debug)]
pub struct ScenarioView {
    pub name: String,
    /// Extra instructions for the LLM.
    pub instructions: Option<String>,
    /// Forced response status.
    pub status: Option<u16>,
    /// Added latency.
    pub latency_ms: Option<u64>,
    /// Fields forced into the response body.
    #[schema(value_type = Option<Object>)]
    pub set: Option<Value>,
    /// Operation ids the scenario applies to; empty means all.
    pub operations: Vec<String>,
    /// Items per streamed response unless `X-Mockingbird-Stream-Items`
    /// overrides it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_items: Option<u32>,
    /// Pause between streamed items, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_item_interval_ms: Option<u64>,
    /// Replaces the mock's chaos while the scenario applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ChaosSchema>)]
    pub chaos: Option<ChaosSettings>,
    /// Instructions replacing `instructions` for one operation, by
    /// operation id; absent when none.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub operation_instructions: BTreeMap<String, String>,
    /// Response headers the scenario sets, by name; absent when none.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    pub source: Source,
}

impl From<Scenario> for ScenarioView {
    fn from(s: Scenario) -> Self {
        Self {
            source: Source::from(&s.source),
            name: s.name,
            instructions: s.instructions,
            status: s.rules.status,
            latency_ms: s.rules.latency_ms,
            set: s.rules.set,
            operations: s.rules.operations,
            stream_items: s.rules.stream_items,
            stream_item_interval_ms: s.rules.stream_item_interval_ms,
            chaos: s.rules.chaos,
            operation_instructions: s.rules.operation_instructions,
            headers: s.rules.headers,
        }
    }
}

/// The full scenario; absent fields are cleared.
#[derive(Deserialize, ToSchema, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct PutScenario {
    pub instructions: Option<String>,
    /// 100–599. When the mock has an active contract, an operation the
    /// scenario applies to must declare it, unless it is a 5xx (answered
    /// natively where undeclared).
    pub status: Option<u16>,
    pub latency_ms: Option<u64>,
    /// A JSON object of fields merged (JSON merge patch) into the response
    /// body. When the mock has an active contract, each key present must fit
    /// the response schema of at least one operation the scenario applies to.
    #[schema(value_type = Option<Object>)]
    pub set: Option<Value>,
    /// Operation ids the scenario applies to; empty means all. When the
    /// mock has an active contract, each must be one of its operations.
    #[serde(default)]
    pub operations: Vec<String>,
    /// Items per streamed response (1–10000) unless
    /// `X-Mockingbird-Stream-Items` overrides it.
    #[serde(default)]
    pub stream_items: Option<u32>,
    /// Pause between streamed items, in milliseconds (at most 30000).
    #[serde(default)]
    #[schema(maximum = 30000)]
    pub stream_item_interval_ms: Option<u64>,
    /// Replaces the mock's chaos while the scenario applies.
    #[serde(default)]
    #[schema(value_type = Option<ChaosSchema>)]
    pub chaos: Option<ChaosSettings>,
    /// Instructions replacing `instructions` for one operation, by
    /// operation id. Each must be one of `operations` when that is set,
    /// and an operation of the active contract when there is one.
    #[serde(default)]
    pub operation_instructions: BTreeMap<String, String>,
    /// Response headers the scenario sets, by name (`Retry-After: "30"`);
    /// they replace any the contract declares. Not `Content-Type`,
    /// `Content-Length`, `Transfer-Encoding`, `Connection` or
    /// `X-Mockingbird-*`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// A scenario status applies only where an operation it covers declares it
/// (any 5xx is answered natively), so a status none of them declares could
/// never take effect: reject it rather than store a silent no-op.
fn check_status_declared(bundle: &Bundle, operations: &[String], status: u16) -> ApiResult<()> {
    match undeclared_scenario_status(bundle, operations, status) {
        None => Ok(()),
        Some(detail) => Err(Problem::titled(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Scenario status not declared",
            detail,
        )),
    }
}

/// Why a scenario forcing `status` on `operations` (all operations when
/// empty) could never take effect in `bundle`, or `None` when it can: a
/// non-5xx status applies only where one of the operations declares it (or a
/// `default` response). Shared by the Admin API (a 422) and config-file
/// validation, so both refuse the same scenarios with the same words.
pub fn undeclared_scenario_status(
    bundle: &Bundle,
    operations: &[String],
    status: u16,
) -> Option<String> {
    if (500..600).contains(&status) {
        return None;
    }
    let ops = scenario_operations(bundle, operations);
    if ops.iter().any(|op| op.output_for_status(status).is_some()) {
        return None;
    }
    let mut declared: Vec<u16> = ops
        .iter()
        .flat_map(|op| &op.outputs)
        .filter_map(|o| match o.status {
            StatusSelector::Code(c) => Some(c),
            StatusSelector::Default => None,
        })
        .collect();
    declared.sort_unstable();
    declared.dedup();
    let whom = match operations {
        [] => "any operation of the active contract".to_string(),
        [one] => format!("operation {one} of the active contract"),
        many => format!("operations {} of the active contract", many.join(", ")),
    };
    let alternatives = if declared.is_empty() {
        "a 5xx".to_string()
    } else {
        let codes: Vec<String> = declared.iter().map(u16::to_string).collect();
        format!("a status it declares ({}) or a 5xx", codes.join(", "))
    };
    Some(format!(
        "status {status} is not declared by {whom}, so the scenario could never answer it; \
         declare a {status} response, or use {alternatives}"
    ))
}

/// The operations of `bundle` a scenario on `operations` applies to (all
/// of them when empty; unknown ids are skipped).
fn scenario_operations<'a>(bundle: &'a Bundle, operations: &[String]) -> Vec<&'a Operation> {
    if operations.is_empty() {
        bundle.operations.iter().collect()
    } else {
        let index = operation_index(bundle);
        operations
            .iter()
            .filter_map(|id| index.get(id.as_str()).copied())
            .collect()
    }
}

/// Deepest `set` nesting that is checked; deeper values are accepted.
const MAX_SET_DEPTH: usize = 32;

/// `$ref` hops followed while resolving one schema.
const MAX_REF_HOPS: usize = 32;

/// Why the scenario `set` could never take effect in `bundle`, or `None`
/// when it can. At request time `set` is merged (JSON merge patch) into the
/// object body of the response the scenario selects (the forced `status`'s
/// response, else the operation's success response), and a result that fails
/// the response schema is dropped with only a WARN. This checks each key
/// present in `set`, by its pointer, against those response schemas: a key
/// an object schema forbids, or a value its property schema rejects. Absent
/// keys and `required` are not checked, `null` (which deletes the field) is
/// always accepted, and so is a key under a composition the check cannot
/// resolve (`anyOf`, `oneOf`, `patternProperties`). `set` must fit at least
/// one of the responses it can apply to. Shared by the Admin API (a 422) and
/// config-file validation.
pub fn invalid_scenario_set(
    bundle: &Bundle,
    operations: &[String],
    status: Option<u16>,
    set: &Value,
) -> Option<String> {
    let Value::Object(set) = set else {
        return None;
    };
    if set.is_empty() {
        return None;
    }
    let mut first_error = None;
    for op in scenario_operations(bundle, operations) {
        let output = match status {
            Some(code) => op.output_for_status(code),
            None => op.success_output(),
        };
        let Some((output, schema)) = output.and_then(|o| o.schema.as_ref().map(|s| (o, s))) else {
            continue;
        };
        match check_set(bundle, schema, set, "", 0) {
            SetFit::Fits => return None,
            SetFit::NotApplicable => {}
            SetFit::Rejected { pointer, why } => {
                if first_error.is_none() {
                    let code = match output.status {
                        StatusSelector::Code(c) => c.to_string(),
                        StatusSelector::Default => "default".to_string(),
                    };
                    first_error = Some(format!(
                        "set{pointer} fits no response the scenario can select ({} {code}: {why})",
                        op.id
                    ));
                }
            }
        }
    }
    first_error
}

/// How a `set` object fits one response schema.
enum SetFit {
    Fits,
    /// The schema is not an object's, so `set` never applies to it.
    NotApplicable,
    Rejected {
        pointer: String,
        why: String,
    },
}

/// What an object schema says about one property.
enum PropertySchema<'a> {
    Declared(&'a Value),
    Forbidden,
    /// Allowed, with no schema the check can resolve.
    Unknown,
}

fn check_set(
    bundle: &Bundle,
    schema: &Value,
    set: &serde_json::Map<String, Value>,
    pointer: &str,
    depth: usize,
) -> SetFit {
    let Some(schema) = resolve(bundle, schema) else {
        return SetFit::Fits;
    };
    if !accepts_object(schema) {
        return SetFit::NotApplicable;
    }
    for (key, value) in set {
        if value.is_null() {
            continue;
        }
        let pointer = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
        let property = match property_schema(bundle, schema, key) {
            PropertySchema::Unknown => continue,
            PropertySchema::Forbidden => {
                return SetFit::Rejected {
                    pointer,
                    why: "the response declares no such property".to_string(),
                };
            }
            PropertySchema::Declared(property) => property,
        };
        // A nested object is merged too, so only its present keys count.
        if let Value::Object(inner) = value {
            let resolved = resolve(bundle, property);
            if depth >= MAX_SET_DEPTH || resolved.is_none() {
                continue;
            }
            if resolved.is_some_and(accepts_object) {
                if let rejected @ SetFit::Rejected { .. } =
                    check_set(bundle, property, inner, &pointer, depth + 1)
                {
                    return rejected;
                }
                continue;
            }
        }
        let Ok(validator) = SchemaValidator::new(&bundle.with_reachable_defs(property)) else {
            continue;
        };
        if let Err(errors) = validator.validate(value) {
            // The value is the validated root, so its errors carry no path.
            let errors: Vec<&str> = errors
                .iter()
                .map(|e| e.strip_prefix(": ").unwrap_or(e))
                .collect();
            return SetFit::Rejected {
                pointer,
                why: errors.join("; "),
            };
        }
    }
    SetFit::Fits
}

/// `schema` with local `#/$defs/<name>` references followed; `None` for a
/// reference the check cannot follow.
fn resolve<'a>(bundle: &'a Bundle, mut schema: &'a Value) -> Option<&'a Value> {
    for _ in 0..MAX_REF_HOPS {
        let Some(reference) = schema.get("$ref") else {
            return Some(schema);
        };
        let name = reference.as_str()?.strip_prefix("#/$defs/")?;
        if name.contains('/') {
            return None;
        }
        schema = bundle
            .defs
            .get(&name.replace("~1", "/").replace("~0", "~"))?;
    }
    None
}

/// Whether `schema` (resolved) may describe an object.
fn accepts_object(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(types)) => types.iter().any(|t| t == "object"),
        _ => true,
    }
}

fn property_schema<'a>(bundle: &'a Bundle, schema: &'a Value, key: &str) -> PropertySchema<'a> {
    if let Some(property) = schema.get("properties").and_then(|p| p.get(key)) {
        return PropertySchema::Declared(property);
    }
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            if let Some(branch) = resolve(bundle, branch)
                && let PropertySchema::Declared(property) = property_schema(bundle, branch, key)
            {
                return PropertySchema::Declared(property);
            }
        }
        return PropertySchema::Unknown;
    }
    if ["anyOf", "oneOf", "patternProperties", "$ref"]
        .iter()
        .any(|k| schema.get(k).is_some())
    {
        return PropertySchema::Unknown;
    }
    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => PropertySchema::Forbidden,
        Some(property @ Value::Object(_)) => PropertySchema::Declared(property),
        _ => PropertySchema::Unknown,
    }
}

/// `bundle`'s operations by id; the first of a repeated id wins, as in
/// [`Bundle::operation`].
fn operation_index(bundle: &Bundle) -> HashMap<&str, &Operation> {
    let mut index = HashMap::with_capacity(bundle.operations.len());
    for op in &bundle.operations {
        index.entry(op.id.as_str()).or_insert(op);
    }
    index
}

/// Path of one scenario.
#[derive(Deserialize, Debug)]
pub struct ScenarioPath {
    pub mock: String,
    pub name: String,
}

async fn find(st: &AdminState, mock_id: uuid::Uuid, name: &str) -> ApiResult<Option<Scenario>> {
    Ok(st
        .store
        .list_scenarios(mock_id)
        .await?
        .into_iter()
        .find(|s| s.name == name))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/scenarios",
    tag = "scenarios",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
    ),
    responses(
        (status = 200, body = Vec<ScenarioView>),
        (status = 404, body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn list(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(MockPath { mock }): Path<MockPath>,
) -> ApiResult<Json<Vec<ScenarioView>>> {
    access.require(Permission::Read)?;
    let m = st.mock(&access.ws, &mock).await?;
    let scenarios = st.store.list_scenarios(m.id).await?;
    Ok(Json(scenarios.into_iter().map(Into::into).collect()))
}

#[utoipa::path(
    put,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/scenarios/{name}",
    tag = "scenarios",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("name" = String, Path, description = "Scenario name"),
    ),
    request_body = PutScenario,
    responses(
        (status = 200, body = ScenarioView),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid scenario, operations unknown to the active contract, a status none of its operations declares, or a `set` no response it can select accepts", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn put(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(ScenarioPath { mock, name }): Path<ScenarioPath>,
    Json(body): Json<PutScenario>,
) -> ApiResult<Json<ScenarioView>> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    if !valid_identifier(&name) {
        return Err(Problem::invalid(
            INVALID_SCENARIO,
            "scenario names are 1-128 characters of [A-Za-z0-9._-]",
        ));
    }
    let existing = find(&st, m.id, &name).await?;
    if let Some(existing) = &existing {
        ensure_api_managed(&existing.source, format_args!("scenario {name:?}"))?;
    }
    if body.status.is_some_and(|s| !(100..=599).contains(&s)) {
        return Err(Problem::invalid(
            INVALID_SCENARIO,
            "status must be between 100 and 599",
        ));
    }
    if body
        .stream_items
        .is_some_and(|n| !(1..=10_000).contains(&n))
    {
        return Err(Problem::invalid(
            INVALID_SCENARIO,
            "stream_items must be between 1 and 10000",
        ));
    }
    if body
        .stream_item_interval_ms
        .is_some_and(|ms| ms > MAX_STREAM_ITEM_INTERVAL_MS)
    {
        return Err(Problem::invalid(
            INVALID_SCENARIO,
            format!("stream_item_interval_ms must be at most {MAX_STREAM_ITEM_INTERVAL_MS} (30 s)"),
        ));
    }
    if body.set.as_ref().is_some_and(|s| !s.is_object()) {
        return Err(Problem::invalid(
            INVALID_SCENARIO,
            "set must be a JSON object",
        ));
    }
    if let Some(chaos) = &body.chaos {
        chaos.validate().map_err(Problem::unprocessable)?;
    }
    if !body.operations.is_empty() {
        let applies: HashSet<&str> = body.operations.iter().map(String::as_str).collect();
        let outside: Vec<&str> = body
            .operation_instructions
            .keys()
            .map(String::as_str)
            .filter(|op| !applies.contains(op))
            .collect();
        if !outside.is_empty() {
            return Err(Problem::invalid(
                INVALID_SCENARIO,
                format!(
                    "operation_instructions names operations {outside:?} the scenario does not apply to"
                ),
            ));
        }
    }
    // Without an active contract there is nothing to check against yet.
    if let Some(contract) = st.store.active_bundle(m.id).await? {
        let known: HashSet<&str> = contract
            .bundle
            .operations
            .iter()
            .map(|o| o.id.as_str())
            .collect();
        let mut unknown: Vec<&str> = body
            .operations
            .iter()
            .chain(body.operation_instructions.keys())
            .map(String::as_str)
            .filter(|op| !known.contains(op))
            .collect();
        if !unknown.is_empty() {
            unknown.sort_unstable();
            unknown.dedup();
            return Err(Problem::invalid(
                INVALID_SCENARIO,
                format!("the active contract of mock {mock:?} has no operations {unknown:?}"),
            ));
        }
        if let Some(status) = body.status {
            check_status_declared(&contract.bundle, &body.operations, status)?;
        }
        if let Some(why) = body.set.as_ref().and_then(|set| {
            invalid_scenario_set(&contract.bundle, &body.operations, body.status, set)
        }) {
            return Err(Problem::invalid(INVALID_SCENARIO, why));
        }
    }
    let scenario = Scenario {
        mock_id: m.id,
        name,
        instructions: body.instructions,
        rules: ScenarioRules {
            status: body.status,
            latency_ms: body.latency_ms,
            set: body.set,
            operations: body.operations,
            stream_items: body.stream_items,
            stream_item_interval_ms: body.stream_item_interval_ms,
            chaos: body.chaos,
            operation_instructions: body.operation_instructions,
            headers: body.headers,
        },
        source: ResourceSource::Api,
    };
    scenario
        .rules
        .check_headers()
        .map_err(|e| Problem::invalid(INVALID_SCENARIO, e))?;
    st.store.put_scenario(&scenario).await?;
    let view = ScenarioView::from(scenario);
    access
        .audit(
            &st.audit,
            "scenario.put",
            format!("scenario:{mock}/{}", view.name),
            existing.map(ScenarioView::from).as_ref().and_then(snapshot),
            snapshot(&view),
        )
        .await;
    Ok(Json(view))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{ws}/mocks/{mock}/scenarios/{name}",
    tag = "scenarios",
    params(
        ("ws" = String, Path, description = "Workspace name"),
        ("mock" = String, Path, description = "Mock name"),
        ("name" = String, Path, description = "Scenario name"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Managed by config file", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn delete(
    State(st): State<AdminState>,
    access: WsAccess,
    Path(ScenarioPath { mock, name }): Path<ScenarioPath>,
) -> ApiResult<StatusCode> {
    access.require(Permission::Edit)?;
    let m = st.mock(&access.ws, &mock).await?;
    let missing = || Problem::not_found(format!("mock {mock:?} has no scenario {name:?}"));
    let existing = find(&st, m.id, &name).await?.ok_or_else(missing)?;
    ensure_api_managed(&existing.source, format_args!("scenario {name:?}"))?;
    let before = snapshot(&ScenarioView::from(existing));
    if st.store.delete_scenario(m.id, &name).await? {
        access
            .audit(
                &st.audit,
                "scenario.delete",
                format!("scenario:{mock}/{name}"),
                before,
                None,
            )
            .await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(missing())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One operation answering a closed object with a nested object, an
    /// `enum` and an `anyOf` property.
    fn bundle() -> Bundle {
        let contract = json!({
            "openapi": "3.0.3",
            "info": { "title": "Users", "version": "1" },
            "paths": { "/me": { "get": {
                "operationId": "getMe",
                "responses": { "200": { "description": "ok", "content": { "application/json": {
                    "schema": { "$ref": "#/components/schemas/User" }
                } } } }
            } } },
            "components": { "schemas": {
                "User": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "address"],
                    "properties": {
                        "id": { "type": "string" },
                        "plan": { "type": "string", "enum": ["free", "pro"] },
                        "address": { "$ref": "#/components/schemas/Address" },
                        "extra": { "anyOf": [{ "type": "string" }, { "type": "object" }] }
                    }
                },
                "Address": {
                    "type": "object",
                    "required": ["city", "zip"],
                    "properties": { "city": { "type": "string" }, "zip": { "type": "string" } }
                }
            } }
        });
        mb_import::compile(contract.to_string().as_bytes()).unwrap()
    }

    fn check(set: Value) -> Option<String> {
        invalid_scenario_set(&bundle(), &[], None, &set)
    }

    #[test]
    fn set_keys_are_checked_against_the_response_schema() {
        assert_eq!(check(json!({ "plan": "pro" })), None);
        // Only present keys: a partial nested object, no `required`.
        assert_eq!(check(json!({ "address": { "city": "Oslo" } })), None);
        assert_eq!(check(json!({ "id": null })), None);
        // Keys under a composition are not resolved; scalars still validate.
        assert_eq!(check(json!({ "extra": { "anything": 1 } })), None);
        assert_eq!(check(json!({ "extra": "x" })), None);
        assert!(check(json!({ "extra": 5 })).is_some());

        let why = check(json!({ "plan": "gold" })).unwrap();
        assert!(why.starts_with("set/plan fits no response"), "{why}");
        assert!(why.contains("getMe 200"), "{why}");
        let why = check(json!({ "nickname": "x" })).unwrap();
        assert!(why.contains("no such property"), "{why}");
        let why = check(json!({ "address": { "zip": 1 } })).unwrap();
        assert!(why.starts_with("set/address/zip "), "{why}");
        let why = check(json!({ "a/b": 1 })).unwrap();
        assert!(why.starts_with("set/a~1b "), "{why}");
    }
}
