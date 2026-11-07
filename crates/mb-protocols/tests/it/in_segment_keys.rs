//! Records keyed by parameters inside a path segment: OData keys
//! (`/companies({company_id})/customers({customer_id})`), a named key
//! (`/carts/key={key}`) and a format suffix (`/files/{name}.json`). Each is one
//! record of one type on every route, read back from state. Driven over HTTP
//! with a scripted LLM, asserting what is served, what is stored and how many
//! calls it took.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, UnknownIds, http_rest};
use mb_state::{CachingStateStore, MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<MemoryStore>,
}

fn harness(contract: &str, script: Vec<Value>) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        CachingStateStore::wrap(
            Arc::clone(&store) as Arc<dyn StateStore>,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
    );
    rt.unknown_ids = UnknownIds::NotFound;
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

impl Harness {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let source = resp
            .headers()
            .get("x-mockingbird-source")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            source,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    /// What an export of the session lists, as (type, id), sorted.
    async fn exported(&self) -> Vec<(String, String)> {
        let session = SessionKey {
            workspace: "default".into(),
            mock: "m".into(),
            session: "default".into(),
        };
        let mut stored: Vec<_> = self
            .store
            .export(&session)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.resource_type, r.id))
            .collect();
        stored.sort();
        stored
    }
}

/// A ledger: companies keyed OData-style, with customers and vendors under
/// each, and a bound action on a customer.
const LEDGER: &str = r##"
openapi: 3.0.3
info: {title: Ledger, version: "1"}
paths:
  /companies({company_id})/customers:
    parameters: [{name: company_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createCustomer
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}
      responses: {"201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}}
  /companies({company_id})/customers({customer_id}):
    parameters:
      - {name: company_id, in: path, required: true, schema: {type: string}}
      - {name: customer_id, in: path, required: true, schema: {type: string}}
    get:
      operationId: getCustomer
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}}
    patch:
      operationId: updateCustomer
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}}
    delete:
      operationId: deleteCustomer
      responses: {"204": {description: gone}}
  /companies({company_id})/customers({customer_id})/Ledger.block:
    parameters:
      - {name: company_id, in: path, required: true, schema: {type: string}}
      - {name: customer_id, in: path, required: true, schema: {type: string}}
    post:
      operationId: blockCustomer
      responses: {"200": {description: blocked, content: {application/json: {schema: {$ref: "#/components/schemas/Customer"}}}}}
  /companies({company_id})/vendors:
    parameters: [{name: company_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createVendor
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Vendor"}}}}
      responses: {"201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Vendor"}}}}}
  /companies({company_id})/vendors({vendor_id}):
    parameters:
      - {name: company_id, in: path, required: true, schema: {type: string}}
      - {name: vendor_id, in: path, required: true, schema: {type: string}}
    get:
      operationId: getVendor
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Vendor"}}}}}
components:
  schemas:
    Customer:
      type: object
      properties:
        id: {type: string}
        displayName: {type: string}
        blocked: {type: boolean}
    Vendor:
      type: object
      properties:
        id: {type: string}
        displayName: {type: string}
"##;

const C1: &str = "/companies(c1)";

#[tokio::test]
async fn an_odata_keyed_record_round_trips_from_state() {
    let h = harness(
        LEDGER,
        vec![
            json!({"id": "cu-1", "displayName": "Ann"}),
            json!({"id": "cu-1", "displayName": "Annie"}),
        ],
    );
    let (status, _, made) = h
        .call(
            "POST",
            &format!("{C1}/customers"),
            Some(json!({"displayName": "Ann"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["id"], "cu-1");
    assert_eq!(
        h.exported().await,
        [("customers".to_owned(), "cu-1".to_owned())]
    );

    let customer = format!("{C1}/customers(cu-1)");
    let (status, source, read) = h.call("GET", &customer, None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read["displayName"], "Ann");
    assert_eq!(h.calls(), 1);

    let (status, _, patched) = h
        .call("PATCH", &customer, Some(json!({"displayName": "Annie"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    let (_, source, read) = h.call("GET", &customer, None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["displayName"], "Annie");
    assert_eq!(
        h.exported().await,
        [("customers".to_owned(), "cu-1".to_owned())],
        "one record of one type"
    );

    let (status, _, _) = h.call("DELETE", &customer, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, gone) = h.call("GET", &customer, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    assert!(h.exported().await.is_empty());
    assert_eq!(h.calls(), 1, "the patch is the state-first executor's");
}

#[tokio::test]
async fn a_bound_action_acts_on_the_stored_record() {
    let h = harness(
        LEDGER,
        vec![
            json!({"id": "cu-1", "displayName": "Ann", "blocked": false}),
            json!({"id": "cu-1", "displayName": "Ann", "blocked": true}),
        ],
    );
    let (status, _, made) = h
        .call(
            "POST",
            &format!("{C1}/customers"),
            Some(json!({"displayName": "Ann"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");

    let (status, _, blocked) = h
        .call("POST", &format!("{C1}/customers(cu-1)/Ledger.block"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{blocked}");
    assert_eq!(blocked["id"], "cu-1");
    // The action was shown the stored customer it acts on.
    let prompt = format!("{:?}", h.provider.requests()[1]);
    assert!(prompt.contains("Ann"), "{prompt}");

    let (_, source, read) = h.call("GET", &format!("{C1}/customers(cu-1)"), None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["blocked"], true);
    assert_eq!(
        h.exported().await,
        [("customers".to_owned(), "cu-1".to_owned())]
    );
    assert_eq!(h.calls(), 2);
}

#[tokio::test]
async fn sibling_collections_under_one_keyed_parent_keep_their_own_records() {
    let h = harness(
        LEDGER,
        vec![
            json!({"id": "cu-1", "displayName": "Ann"}),
            json!({"id": "ve-1", "displayName": "Bolts Inc"}),
        ],
    );
    let (status, _, _) = h
        .call(
            "POST",
            &format!("{C1}/customers"),
            Some(json!({"displayName": "Ann"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = h
        .call(
            "POST",
            &format!("{C1}/vendors"),
            Some(json!({"displayName": "Bolts Inc"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        h.exported().await,
        [
            ("customers".to_owned(), "cu-1".to_owned()),
            ("vendors".to_owned(), "ve-1".to_owned()),
        ]
    );
    let (_, source, vendor) = h.call("GET", &format!("{C1}/vendors(ve-1)"), None).await;
    assert_eq!(source, "state", "{vendor}");
    assert_eq!(vendor["displayName"], "Bolts Inc");
    let (_, source, customer) = h.call("GET", &format!("{C1}/customers(cu-1)"), None).await;
    assert_eq!(source, "state", "{customer}");
    assert_eq!(customer["displayName"], "Ann");
    // Neither collection holds the other's record.
    let (status, _, _) = h.call("GET", &format!("{C1}/vendors(cu-1)"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(h.calls(), 2);
}

/// Carts made by `POST /carts`, read and deleted by an alternate key, and
/// files read by a name with a format suffix.
const SHOP: &str = r##"
openapi: 3.0.3
info: {title: Shop, version: "1"}
paths:
  /carts:
    post:
      operationId: createCart
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}
      responses: {"201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}}
  /carts/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getCart
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}}
  /carts/key={key}:
    parameters: [{name: key, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getCartByKey
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}}
    patch:
      operationId: updateCartByKey
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cart"}}}}}
    delete:
      operationId: deleteCartByKey
      responses: {"204": {description: gone}}
  /files.json:
    post:
      operationId: createFile
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/File"}}}}
      responses: {"201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/File"}}}}}
  /files/{name}.json:
    parameters: [{name: name, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getFile
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/File"}}}}}
    delete:
      operationId: deleteFile
      responses: {"204": {description: gone}}
components:
  schemas:
    Cart:
      type: object
      properties:
        id: {type: string}
        key: {type: string}
        total: {type: number}
    File:
      type: object
      properties:
        name: {type: string}
        size: {type: integer}
"##;

#[tokio::test]
async fn a_record_round_trips_by_its_alternate_key() {
    let h = harness(
        SHOP,
        vec![
            json!({"id": "ca-1", "key": "k-1", "total": 0}),
            json!({"id": "ca-1", "key": "k-1", "total": 12.5}),
        ],
    );
    let (status, _, made) = h
        .call("POST", "/carts", Some(json!({"key": "k-1", "total": 0})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(
        h.exported().await,
        [("carts".to_owned(), "ca-1".to_owned())]
    );

    let (status, source, read) = h.call("GET", "/carts/key=k-1", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read["id"], "ca-1");
    let (_, source, read) = h.call("GET", "/carts/ca-1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(h.calls(), 1);

    let (status, _, updated) = h
        .call("PATCH", "/carts/key=k-1", Some(json!({"total": 12.5})))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let (_, source, read) = h.call("GET", "/carts/ca-1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["total"], 12.5);
    assert_eq!(
        h.exported().await,
        [("carts".to_owned(), "ca-1".to_owned())]
    );

    let (status, _, _) = h.call("DELETE", "/carts/key=k-1", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, gone) = h.call("GET", "/carts/ca-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    let (status, _, gone) = h.call("GET", "/carts/key=k-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    assert!(h.exported().await.is_empty());
    assert_eq!(h.calls(), 1, "the patch is the state-first executor's");
}

#[tokio::test]
async fn a_record_named_with_a_format_suffix_round_trips() {
    let h = harness(SHOP, vec![json!({"name": "report", "size": 3})]);
    let (status, _, made) = h
        .call(
            "POST",
            "/files.json",
            Some(json!({"name": "report", "size": 3})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(
        h.exported().await,
        [("files".to_owned(), "report".to_owned())]
    );
    let (status, source, read) = h.call("GET", "/files/report.json", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read["size"], 3);
    let (status, _, _) = h.call("DELETE", "/files/report.json", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, gone) = h.call("GET", "/files/report.json", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    assert_eq!(h.calls(), 1);
}

/// Order lines keyed by a document and a sequence together.
const LINES: &str = r##"
openapi: 3.0.3
info: {title: Lines, version: "1"}
paths:
  /salesOrderLines:
    post:
      operationId: createLine
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}
      responses: {"201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}}
  /salesOrderLines(documentId={documentId},sequence={sequence}):
    parameters:
      - {name: documentId, in: path, required: true, schema: {type: string}}
      - {name: sequence, in: path, required: true, schema: {type: integer}}
    get:
      operationId: getLine
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}}
    patch:
      operationId: updateLine
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Line"}}}}}
    delete:
      operationId: deleteLine
      responses: {"204": {description: gone}}
components:
  schemas:
    Line:
      type: object
      properties:
        documentId: {type: string}
        sequence: {type: integer}
        amount: {type: number}
"##;

/// No one part of a composite key identifies a record: two lines sharing a
/// sequence stay apart, and no write or read by one is of the other.
#[tokio::test]
async fn records_sharing_a_part_of_a_composite_key_do_not_collide() {
    let line =
        |doc: &str, amount: f64| json!({"documentId": doc, "sequence": 10000, "amount": amount});
    let h = harness(
        LINES,
        vec![
            line("A", 1.0),
            line("B", 2.0),
            line("A", 5.0),
            line("B", 7.0),
            line("A", 5.0),
            line("B", 7.0),
            line("A", 5.0),
        ],
    );
    for (doc, amount) in [("A", 1.0), ("B", 2.0)] {
        let (status, _, made) = h
            .call("POST", "/salesOrderLines", Some(line(doc, amount)))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        assert_eq!(made["documentId"], doc, "{made}");
    }
    let stored = h.exported().await;
    assert_ne!(stored.len(), 1, "two lines stored as one: {stored:?}");

    let a = "/salesOrderLines(documentId=A,sequence=10000)";
    let b = "/salesOrderLines(documentId=B,sequence=10000)";
    for (path, doc, amount) in [(a, "A", 5.0), (b, "B", 7.0)] {
        let (status, _, patched) = h.call("PATCH", path, Some(json!({"amount": amount}))).await;
        assert_eq!(status, StatusCode::OK, "{patched}");
        assert_eq!(patched["documentId"], doc, "{patched}");
        // Nothing is stored under the sequence alone.
        assert_eq!(h.exported().await, stored, "{path}");
    }
    // B's update was not shown A's line as its target.
    let shown = format!("{:?}", h.provider.requests()[3]);
    assert!(!shown.contains("5.0") && !shown.contains(":5,"), "{shown}");
    for (path, doc, amount) in [(a, "A", 5.0), (b, "B", 7.0)] {
        let (status, _, read) = h.call("GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "{read}");
        assert_eq!(
            (&read["documentId"], &read["amount"]),
            (&json!(doc), &json!(amount))
        );
    }
    let (status, _, _) = h.call("DELETE", b, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        h.exported().await,
        stored,
        "B's delete removed nothing of A's"
    );
    let (status, _, read) = h.call("GET", a, None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["documentId"], "A", "{read}");
}
