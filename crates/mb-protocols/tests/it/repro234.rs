//! Regressions from the review of #234: false refusals on weak evidence, an LLM-written
//! error code overwritten, and an answer under the holder's identity stored anew.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

fn contract(op_description: &str, info: &str, responses: &str) -> String {
    format!(
        r##"
openapi: 3.0.0
info:
  title: Catalog
  version: "1"
  description: "{info}"
paths:
  /widgets:
    post:
      operationId: createWidget
      description: "{op_description}"
      requestBody:
        required: true
        content: {{application/json: {{schema: {{type: object, properties: {{sku: {{type: string}}, email: {{type: string}}, external_ref: {{type: string}}, label: {{type: string}}}}}}}}}}
      responses:
        "201": {{description: created, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Widget"}}}}}}}}
{responses}
  /widgets/{{widget_id}}:
    get:
      operationId: getWidget
      parameters: [{{name: widget_id, in: path, required: true, schema: {{type: string}}}}]
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Widget"}}}}}}}}
components:
  schemas:
    Widget:
      type: object
      required: [id]
      properties: {{id: {{type: string}}, sku: {{type: string}}, email: {{type: string}}, external_ref: {{type: string}}, label: {{type: string}}}}
    Error:
      type: object
      required: [code, message]
      properties: {{code: {{type: string}}, message: {{type: string}}}}
"##
    )
}

const R400: &str = r##"        "400": {description: "Bad request.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}"##;
const R409_422: &str = r##"        "409": {description: "Conflict.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
        "422": {description: "Unprocessable.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}"##;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<dyn StateStore>,
}

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "default".into(),
    }
}

fn harness(contract: &str, script: Vec<Value>) -> Harness {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&store),
    );
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

impl Harness {
    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn prompt(&self, n: usize) -> String {
        let requests = self.provider.requests();
        requests[n].messages.last().unwrap().content.clone()
    }

    async fn exported(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .store
            .export(&session())
            .await
            .unwrap()
            .into_iter()
            .map(|r| format!("{}/{}", r.resource_type, r.id))
            .collect();
        ids.sort();
        ids
    }
}

fn success(record: Value) -> Value {
    json!({"effects": [], "response": record})
}

/// R1: the op description gives a length limit (400 characters) beside `sku`; nothing says
/// the SKU is unique. A second widget with the same SKU, answered 201, must stand.
#[tokio::test]
async fn a_length_limit_beside_the_member_refuses_nothing() {
    let c = contract(
        "The `sku` holds at most 400 characters.",
        "A catalog.",
        R400,
    );
    let h = harness(
        &c,
        vec![
            json!({"id": "wd-000001", "sku": "WID-7781-BLU"}),
            success(json!({"id": "wd-000002", "sku": "WID-7781-BLU"})),
        ],
    );
    let body = json!({"sku": "WID-7781-BLU"});
    h.post("/widgets", body.clone()).await;
    let (status, out) = h.post("/widgets", body).await;
    assert!(h.prompt(1).contains("same_values"), "{}", h.prompt(1));
    assert_eq!(status, StatusCode::CREATED, "false refusal: {out}");
}

/// R2: the info description counts products with the number 400 in a sentence naming
/// `sku` and the type.
#[tokio::test]
async fn a_count_in_the_api_description_refuses_nothing() {
    let c = contract(
        "Creates a widget.",
        "Each widget shows its `sku`, and a page lists at most 400 widgets.",
        R400,
    );
    let h = harness(
        &c,
        vec![
            json!({"id": "wd-000001", "sku": "WID-7781-BLU"}),
            success(json!({"id": "wd-000002", "sku": "WID-7781-BLU"})),
        ],
    );
    let body = json!({"sku": "WID-7781-BLU"});
    h.post("/widgets", body.clone()).await;
    let (status, out) = h.post("/widgets", body).await;
    assert_eq!(status, StatusCode::CREATED, "false refusal: {out}");
}

/// R3: one sentence names two statuses for two members: 422 for a malformed `email`, 409
/// for a taken `external_ref`. A repeated email (different external_ref) must stand.
#[tokio::test]
async fn a_sentence_of_two_statuses_and_members_refuses_nothing() {
    let c = contract(
        "Answers 422 when the `email` is malformed, and 409 when the `external_ref` is taken by another widget.",
        "A catalog.",
        R409_422,
    );
    let h = harness(
        &c,
        vec![
            json!({"id": "wd-000001", "email": "jane.roe@example.com", "external_ref": "EXT-0001-AAAA"}),
            success(
                json!({"id": "wd-000002", "email": "jane.roe@example.com", "external_ref": "EXT-0002-BBBB"}),
            ),
        ],
    );
    h.post(
        "/widgets",
        json!({"email": "jane.roe@example.com", "external_ref": "EXT-0001-AAAA"}),
    )
    .await;
    let (status, out) = h
        .post(
            "/widgets",
            json!({"email": "jane.roe@example.com", "external_ref": "EXT-0002-BBBB"}),
        )
        .await;
    assert!(h.prompt(1).contains("same_values"), "{}", h.prompt(1));
    assert_eq!(status, StatusCode::CREATED, "false refusal: {out}");
}

/// R4: an LLM refusal with its own valid body: is its code kept?
#[tokio::test]
async fn an_llm_written_error_code_is_kept() {
    let c = contract(
        "Creates a widget.",
        "A widget create with an SKU another widget has is refused with 400 and the code `sku_taken`.",
        r##"        "400": {description: "`sku_taken` when another widget has the SKU; `bad_param` for a property that is not valid.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}"##,
    );
    let h = harness(
        &c,
        vec![
            json!({"id": "wd-000001", "sku": "WID-7781-BLU"}),
            json!({"effects": [], "response": null, "refusal": 400, "refusal_body": {"code": "bad_param", "message": "label is too long"}}),
        ],
    );
    h.post("/widgets", json!({"sku": "WID-7781-BLU"})).await;
    let (status, out) = h
        .post("/widgets", json!({"sku": "WID-7781-BLU", "label": "x"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{out}");
    assert_eq!(out["code"], "bad_param", "LLM-written code replaced: {out}");
}

/// R5: the holder is deleted between prompt and enforcement? (sanity: a record gone
/// before the call lists nothing). Upsert-like answer: holder id, new label.
#[tokio::test]
async fn an_answer_under_the_holders_identity_serves_the_stored_record() {
    let c = contract(
        "Creates a widget.",
        "A widget's `sku` is unique: a widget create with an SKU another widget has is refused with 400.",
        R400,
    );
    let h = harness(
        &c,
        vec![
            json!({"id": "wd-000001", "sku": "WID-7781-BLU", "label": "Old"}),
            success(json!({"id": "wd-000001", "sku": "WID-7781-BLU", "label": "New"})),
        ],
    );
    h.post("/widgets", json!({"sku": "WID-7781-BLU", "label": "Old"}))
        .await;
    let (status, out) = h
        .post("/widgets", json!({"sku": "WID-7781-BLU", "label": "New"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{out}");
    assert_eq!(
        out,
        json!({"id": "wd-000001", "sku": "WID-7781-BLU", "label": "Old"}),
        "the stored record, unchanged"
    );
    assert_eq!(h.exported().await, ["widgets/wd-000001"], "nothing stored");
    assert_eq!(
        h.store
            .get(&session(), "widgets", "wd-000001")
            .await
            .unwrap()
            .unwrap()
            .data["label"],
        "Old"
    );
}

// Round two of the review: a 400 or 422 naming a member is validation as often as
// uniqueness, and is only offered; a 409 naming one is taken for its uniqueness conflict.

fn one(status: &str, description: &str) -> String {
    format!(
        r##"        "{status}": {{description: "{description}", content: {{application/json: {{schema: {{$ref: "#/components/schemas/Error"}}}}}}}}"##
    )
}

async fn repeat_email(c: &str) -> (StatusCode, Value, String) {
    let h = harness(
        c,
        vec![
            json!({"id": "wd-000001", "email": "jane.roe@example.com"}),
            success(json!({"id": "wd-000002", "email": "jane.roe@example.com"})),
        ],
    );
    h.post("/widgets", json!({"email": "jane.roe@example.com"}))
        .await;
    let (s, o) = h
        .post("/widgets", json!({"email": "jane.roe@example.com"}))
        .await;
    (s, o, h.prompt(1))
}

async fn repeat_sku(c: &str) -> (StatusCode, Value, String) {
    let h = harness(
        c,
        vec![
            json!({"id": "wd-000001", "sku": "WID-7781-BLU"}),
            success(json!({"id": "wd-000002", "sku": "WID-7781-BLU"})),
        ],
    );
    h.post("/widgets", json!({"sku": "WID-7781-BLU"})).await;
    let (s, o) = h.post("/widgets", json!({"sku": "WID-7781-BLU"})).await;
    (s, o, h.prompt(1))
}

/// R6: a 422 that validates the `email` refuses nothing.
#[tokio::test]
async fn a_422_validating_the_member_refuses_nothing() {
    let c = contract(
        "Creates a widget.",
        "A catalog.",
        &one("422", "`email` must be a valid address."),
    );
    let (s, o, p) = repeat_email(&c).await;
    assert!(p.contains("same_values"), "{p}");
    assert_eq!(s, StatusCode::CREATED, "false refusal: {o}");
}

/// R7: a lone 400 saying the `sku` is malformed refuses nothing.
#[tokio::test]
async fn a_400_saying_the_member_is_malformed_refuses_nothing() {
    let c = contract(
        "Creates a widget.",
        "A catalog.",
        &one("400", "The `sku` is malformed."),
    );
    let (s, o, p) = repeat_sku(&c).await;
    assert!(p.contains("same_values"), "{p}");
    assert_eq!(s, StatusCode::CREATED, "false refusal: {o}");
}

/// R8, a known risk: a 409 naming a member is taken for that member's uniqueness
/// conflict, though this one speaks of a verification. Structure cannot tell the two
/// apart, so a repeated email is refused here.
#[tokio::test]
async fn known_risk_a_409_naming_a_member_is_taken_for_its_uniqueness_conflict() {
    let c = contract(
        "Creates a widget.",
        "A catalog.",
        &one(
            "409",
            "The `email` must be verified before a widget is made.",
        ),
    );
    let (s, o, p) = repeat_email(&c).await;
    assert!(p.contains("same_values"), "{p}");
    assert_eq!(s, StatusCode::CONFLICT, "{o}");
}

/// R9: a 422 naming the `sku` with a length limit refuses nothing.
#[tokio::test]
async fn a_422_with_a_length_limit_refuses_nothing() {
    let c = contract(
        "Creates a widget.",
        "A catalog.",
        &one("422", "The `sku` is longer than 64 characters."),
    );
    let (s, o, p) = repeat_sku(&c).await;
    assert!(p.contains("same_values"), "{p}");
    assert_eq!(s, StatusCode::CREATED, "false refusal: {o}");
}

/// R10: a 422 saying the `sku` is taken is offered to the model, never enforced: the
/// model's success stands.
#[tokio::test]
async fn a_422_naming_a_taken_member_is_only_an_offer() {
    let c = contract(
        "Creates a widget.",
        "A catalog.",
        &one("422", "Another widget has this `sku`."),
    );
    let (s, o, p) = repeat_sku(&c).await;
    assert!(p.contains("same_values"), "{p}");
    assert_eq!(s, StatusCode::CREATED, "{o}");
}

/// R11: an answer under the holder's id, holding its SKU but changing other members,
/// serves the stored record unchanged and stores nothing.
#[tokio::test]
async fn an_answer_under_the_holders_id_with_changes_serves_the_stored_record() {
    let c = contract(
        "Creates a widget.",
        "A catalog.",
        &one("409", "A widget with this `sku` exists."),
    );
    let held = json!({"id": "wd-000001", "sku": "WID-7781-BLU", "label": "Old"});
    let h = harness(
        &c,
        vec![
            held.clone(),
            success(
                json!({"id": "wd-000001", "sku": "WID-7781-BLU", "label": "New", "email": "x@example.com"}),
            ),
        ],
    );
    h.post("/widgets", json!({"sku": "WID-7781-BLU", "label": "Old"}))
        .await;
    let (s, o) = h
        .post(
            "/widgets",
            json!({"sku": "WID-7781-BLU", "label": "New", "email": "x@example.com"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{o}");
    assert_eq!(o, held, "the stored record, unchanged");
    assert_eq!(h.exported().await, ["widgets/wd-000001"], "nothing stored");
    let stored = h
        .store
        .get(&session(), "widgets", "wd-000001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.data, held);
}
