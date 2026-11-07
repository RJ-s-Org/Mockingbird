//! A create that repeats a value the contract says is unique, which a stored record in
//! its collection holds, is refused whatever the LLM answered, with the body the
//! contract gives that refusal; on weaker evidence the refusal stays an offer and the
//! LLM's success is served. Driven over HTTP with a scripted LLM.
//!
//! Step 6 deletes these tests: they check complete()'s create internals (seeded creates).
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

/// Widgets with an `sku` unique in the catalog, refused with a 400 the API description
/// ties to the code; gadgets whose 409 names their `serial`, created under a shelf;
/// badges whose 409 says nothing of their `email`.
const CONTRACT: &str = r##"
openapi: 3.0.0
info:
  title: Catalog
  version: "1"
  description: >-
    A widget's `sku` is unique in the catalog: a create with an SKU another widget has
    is refused with 400 and the code `sku_taken`. An unknown id answers 404 with the
    code `missing`.
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {sku: {type: string}, label: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
        "400": {description: "`sku_taken` when another widget has the SKU; `bad_param` for a property that is not valid.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /widgets/{widget_id}:
    get:
      operationId: getWidget
      parameters: [{name: widget_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
        "404": {description: "No widget has the id.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /shelves/{shelf}/gadgets:
    post:
      operationId: createGadget
      parameters: [{name: shelf, in: path, required: true, schema: {type: string}}]
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {serial: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}
        "409": {description: A gadget with this `serial` is already on the shelf., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /gadgets/{gadget_id}:
    get:
      operationId: getGadget
      parameters: [{name: gadget_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}
  /tags:
    post:
      operationId: createTag
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {id: {type: string}, label: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}
        "409": {description: A tag with this id exists., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /tags/{tag_id}:
    get:
      operationId: getTag
      parameters: [{name: tag_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Tag"}}}}
  /badges:
    post:
      operationId: createBadge
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {email: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Badge"}}}}
        "409": {description: Conflict., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /badges/{badge_id}:
    get:
      operationId: getBadge
      parameters: [{name: badge_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Badge"}}}}
components:
  schemas:
    Widget:
      type: object
      required: [id, sku]
      properties: {id: {type: string}, sku: {type: string}, label: {type: string}}
    Gadget:
      type: object
      required: [id, serial]
      properties: {id: {type: string}, serial: {type: string}}
    Tag:
      type: object
      required: [id, label]
      properties: {id: {type: string}, label: {type: string}, existing: {type: boolean}}
    Badge:
      type: object
      required: [id, email]
      properties: {id: {type: string}, email: {type: string}}
    Error:
      type: object
      required: [code, message]
      properties: {code: {type: string}, message: {type: string}}
"##;

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

fn harness(script: Vec<Value>) -> Harness {
    harness_with(CONTRACT, script)
}

fn harness_with(contract: &str, script: Vec<Value>) -> Harness {
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
    /// A create, seeded: a seeded create takes the generated path, which stores the
    /// record the model answered under the id it chose, as these tests expect.
    async fn seeded(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.post_with(uri, body, &[crate::support::SEED]).await
    }

    async fn post_with(
        &self,
        uri: &str,
        body: Value,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let req = req.body(Body::from(body.to_string())).unwrap();
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

/// The LLM's success, as the envelope it is asked for when shown records.
fn success(record: Value) -> Value {
    json!({"effects": [], "response": record})
}

fn widget(id: &str, sku: &str) -> Value {
    json!({"id": id, "sku": sku, "label": "Blue widget"})
}

/// A 400 that names the SKU only in prose, beside two codes, and a sentence of the API
/// description are no tie strong enough to enforce: the LLM's success for a taken SKU
/// stands, the refusal an offer.
#[tokio::test]
async fn a_prose_tie_to_a_400_is_only_an_offer() {
    let h = harness(vec![
        widget("wd-000001", "WID-7781-BLU"),
        success(widget("wd-000002", "WID-7781-BLU")),
    ]);
    let body = json!({"sku": "WID-7781-BLU", "label": "Blue widget"});
    h.seeded("/widgets", body.clone()).await;
    let (status, out) = h.seeded("/widgets", body).await;
    let prompt = h.prompt(1);
    assert!(prompt.contains("same_values"), "{prompt}");
    assert_eq!(status, StatusCode::CREATED, "{out}");
}

/// A 400 is validation as often as uniqueness: even naming the SKU in code position
/// alone, it is only offered, and the LLM's success for a taken SKU stands.
#[tokio::test]
async fn a_400_naming_the_member_in_code_position_is_only_an_offer() {
    let contract = CONTRACT.replace(
        "\"`sku_taken` when another widget has the SKU; `bad_param` for a property that is not valid.\"",
        "\"Another widget has this `sku`.\"",
    );
    assert_ne!(contract, CONTRACT);
    let h = harness_with(
        &contract,
        vec![
            widget("wd-000001", "WID-7781-BLU"),
            success(widget("wd-000002", "WID-7781-BLU")),
        ],
    );
    let body = json!({"sku": "WID-7781-BLU", "label": "Blue widget"});
    h.seeded("/widgets", body.clone()).await;
    let (status, out) = h.seeded("/widgets", body).await;
    assert!(h.prompt(1).contains("same_values"), "{}", h.prompt(1));
    assert_eq!(status, StatusCode::CREATED, "{out}");
}

/// The LLM's own refusal may be of anything: the API description's sentence about a
/// repeat gives it no code.
#[tokio::test]
async fn an_llm_refusal_takes_no_code_from_a_repeats_sentence() {
    let h = harness(vec![
        widget("wd-000001", "WID-7781-BLU"),
        json!({"effects": [], "response": null, "refusal": 400}),
    ]);
    let body = json!({"sku": "WID-7781-BLU"});
    h.seeded("/widgets", body.clone()).await;
    let (status, out) = h.seeded("/widgets", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{out}");
    assert_ne!(out["code"], "sku_taken", "{out}");
}

/// Another SKU repeats nothing: the LLM's success is served and stored.
#[tokio::test]
async fn another_sku_is_created() {
    let h = harness(vec![
        widget("wd-000001", "WID-7781-BLU"),
        widget("wd-000002", "WID-7782-RED"),
    ]);
    h.seeded("/widgets", json!({"sku": "WID-7781-BLU"})).await;
    let (status, _) = h.seeded("/widgets", json!({"sku": "WID-7782-RED"})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        h.exported().await,
        ["widgets/wd-000001", "widgets/wd-000002"]
    );
}

fn gadget(id: &str) -> Value {
    json!({"id": id, "serial": "SN-0042-7781-X"})
}

/// A gadget whose serial one on the same shelf holds is refused with the 409 that names
/// the serial in code position; on another shelf it is created.
#[tokio::test]
async fn a_taken_serial_is_refused_on_its_shelf_only() {
    let h = harness(vec![gadget("gd-000001"), success(gadget("gd-000002"))]);
    let body = json!({"serial": "SN-0042-7781-X"});
    h.seeded("/shelves/s1/gadgets", body.clone()).await;
    let (status, out) = h.seeded("/shelves/s1/gadgets", body.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{out} {}", h.prompt(1));
    assert_eq!(h.exported().await, ["gadgets/gd-000001"]);

    // Shown nothing of the other shelf, the create is asked for a plain answer.
    let h = harness(vec![gadget("gd-000001"), gadget("gd-000002")]);
    h.seeded("/shelves/s1/gadgets", body.clone()).await;
    let (status, out) = h.seeded("/shelves/s2/gadgets", body).await;
    assert_eq!(status, StatusCode::CREATED, "another shelf: {out}");
    assert!(!h.prompt(1).contains("same_values"), "{}", h.prompt(1));
    assert_eq!(
        h.exported().await,
        ["gadgets/gd-000001", "gadgets/gd-000002"]
    );
}

/// A 409 that names no member is no tie: a badge repeating an email is offered the
/// refusal, and the LLM's success stands.
#[tokio::test]
async fn a_refusal_naming_no_member_stays_an_offer() {
    let badge = |id: &str| json!({"id": id, "email": "jane.roe@example.com"});
    let h = harness(vec![badge("bg-000001"), success(badge("bg-000002"))]);
    let body = json!({"email": "jane.roe@example.com"});
    h.seeded("/badges", body.clone()).await;
    let (status, out) = h.seeded("/badges", body).await;
    let prompt = h.prompt(1);
    assert!(prompt.contains("same_values"), "offered: {prompt}");
    assert_eq!(status, StatusCode::CREATED, "{out}");
    assert_eq!(h.exported().await, ["badges/bg-000001", "badges/bg-000002"]);
}

/// A create asking for a taken id, answered with the stored record as it is (an "it
/// exists" answer, `existing: true` beside it), is answered so: neither refused nor
/// replaced. An answer that is no such record is refused with the declared 409.
#[tokio::test]
async fn an_answer_that_is_the_held_record_is_never_refused() {
    let tag = json!({"id": "tg-launch-2026", "label": "Launch"});
    let mut existing = tag.clone();
    existing["existing"] = json!(true);
    let h = harness(vec![tag.clone(), success(existing.clone())]);
    h.seeded("/tags", tag.clone()).await;
    let (status, out) = h.seeded("/tags", tag.clone()).await;
    assert!(h.prompt(1).contains("same_values"), "{}", h.prompt(1));
    assert_eq!(status, StatusCode::CREATED, "{out}");
    assert_eq!(out, existing);
    assert_eq!(h.exported().await, ["tags/tg-launch-2026"]);

    let mut renamed = tag.clone();
    renamed["label"] = json!("Relaunch");
    let h = harness(vec![tag.clone(), success(renamed.clone())]);
    h.seeded("/tags", tag).await;
    let (status, out) = h.seeded("/tags", renamed).await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
}

/// A widget create repeating a taken SKU, answered with the widget that holds it, as
/// stored, stands too.
#[tokio::test]
async fn a_repeat_answered_with_its_holder_stands() {
    let held = widget("wd-000001", "WID-7781-BLU");
    let h = harness(vec![held.clone(), success(held.clone())]);
    let body = json!({"sku": "WID-7781-BLU", "label": "Blue widget"});
    h.seeded("/widgets", body.clone()).await;
    let (status, out) = h.seeded("/widgets", body).await;
    assert_eq!(status, StatusCode::CREATED, "{out}");
    assert_eq!(out["id"], "wd-000001", "{out}");
    assert_eq!(h.exported().await, ["widgets/wd-000001"]);
}
