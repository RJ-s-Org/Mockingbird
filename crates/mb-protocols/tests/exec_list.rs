//! The state-first executor's lists: a list the description classifies is answered from
//! the session's records from its first call, with no model call, and logged with
//! `exec=list`; one it cannot answer goes on to generation, logged with `exec_fallback`
//! and its reason.
//!
//! The list shapes are those of the bench's contracts: an offset page with a count and
//! nullable links (NetBox), a cursor with a more flag (Stripe-like), a nested collection
//! (a project's tasks), a bare array (RabbitMQ).
//!
//! Its own binary, as `tests/exec.rs` is: it installs a tracing subscriber, and tracing
//! caches each callsite's interest process-wide.
#![cfg(test)]
#![cfg(feature = "http-rest")]

#[path = "it/support.rs"]
#[allow(dead_code)]
mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use tracing::field::{Field, Visit};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

use support::DescribedHarness;

/// The fields of one logged event that name the executor.
#[derive(Clone, Debug, Default, PartialEq)]
struct ExecLine {
    operation: String,
    exec: String,
    exec_fallback: Option<String>,
}

impl Visit for ExecLine {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "exec" => self.exec = value.to_owned(),
            "exec_fallback" => self.exec_fallback = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "operation" {
            self.operation = format!("{value:?}");
        }
    }
}

/// Records the executor's log lines of this thread.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<ExecLine>>>);

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut line = ExecLine::default();
        event.record(&mut line);
        if !line.exec.is_empty() {
            self.0.lock().unwrap().push(line);
        }
    }
}

impl Capture {
    fn install() -> (Self, DefaultGuard) {
        let capture = Self::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
        (capture, guard)
    }

    /// The reasons logged for `operation`: `None` for each call the executor answered.
    fn of(&self, operation: &str) -> Vec<Option<String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.operation == operation)
            .inspect(|l| assert_eq!(l.exec, "list", "{l:?}"))
            .map(|l| l.exec_fallback.clone())
            .collect()
    }
}

fn source(headers: &HeaderMap) -> &str {
    headers
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// A `200` answer's source and body.
async fn get(h: &DescribedHarness, uri: &str) -> (String, Value) {
    let (status, headers, body) = h.send("GET", uri, None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    (source(&headers).to_owned(), body)
}

fn ids(items: &Value) -> Vec<Value> {
    items
        .as_array()
        .map(|items| items.iter().map(|i| i["id"].clone()).collect())
        .unwrap_or_default()
}

/// NetBox's sites: an offset page counted, with nullable links to the pages beside it,
/// a status filter, a search and a switch at its default.
const SITES: &str = r##"
openapi: 3.0.0
info: {title: Sites, version: "1"}
paths:
  /api/dcim/sites/:
    get:
      operationId: listSites
      parameters:
        - {name: limit, in: query, schema: {type: integer, minimum: 1, default: 50}}
        - {name: offset, in: query, schema: {type: integer, minimum: 0, default: 0}}
        - {name: status, in: query, schema: {type: string}}
        - {name: q, in: query, schema: {type: string}}
        - {name: brief, in: query, schema: {type: boolean, default: false}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [count, next, previous, results]
                properties:
                  count: {type: integer}
                  next: {type: string, format: uri, nullable: true}
                  previous: {type: string, format: uri, nullable: true}
                  results: {type: array, items: {$ref: "#/components/schemas/Site"}}
    post:
      operationId: createSite
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Site"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Site"}}}}}
  /api/dcim/sites/{id}/:
    get:
      operationId: getSite
      parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Site"}}}}}
components:
  schemas:
    Site:
      type: object
      required: [id, name, status]
      properties:
        id: {type: integer}
        name: {type: string}
        status: {type: string, enum: [active, planned, retired]}
"##;

fn sites_world() -> Value {
    json!({"operations": {"listSites": {
        "filters": [{"param": "status", "member": "status", "kind": "eq"}],
        "paging": {"style": "offset", "request": "offset", "size": "limit"}
    }}})
}

fn site(id: i64, status: &str) -> Value {
    json!({"id": id, "name": format!("site {id}"), "status": status})
}

async fn sites(script: Vec<Value>) -> DescribedHarness {
    let h = support::described(SITES, sites_world(), script);
    h.hold(
        "sites",
        &[
            site(1, "active"),
            site(2, "planned"),
            site(3, "active"),
            site(4, "active"),
            site(5, "retired"),
        ],
    )
    .await;
    h
}

#[tokio::test]
async fn an_empty_session_lists_nothing_with_no_model_call() {
    let (capture, _guard) = Capture::install();
    let h = support::described(SITES, sites_world(), Vec::new());
    let (source, body) = get(&h, "/api/dcim/sites/").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(
        body,
        json!({"count": 0, "next": null, "previous": null, "results": []})
    );
    assert_eq!(capture.of("listSites"), [None]);
    assert!(h.provider.requests().is_empty());
}

#[tokio::test]
async fn an_offset_page_lists_the_held_records_its_filter_keeps() {
    let (capture, _guard) = Capture::install();
    let h = sites(Vec::new()).await;
    let (source, first) = get(&h, "/api/dcim/sites/?status=active&limit=2").await;
    assert_eq!(source, "state", "{first}");
    assert_eq!(ids(&first["results"]), [json!(1), json!(3)]);
    // Every page counts what the filter keeps.
    assert_eq!(first["count"], json!(3), "{first}");
    // The first page links the next, with the request's filter and size.
    let next = first["next"].as_str().expect("a next link");
    let at = next.find("/api/dcim/sites/").expect("the route's URL");
    let next = &next[at..];
    assert!(next.contains("offset=2"), "{next}");
    assert!(
        next.contains("status=active") && next.contains("limit=2"),
        "{next}"
    );
    let (_, second) = get(&h, next).await;
    assert_eq!(ids(&second["results"]), [json!(4)]);
    assert_eq!(second["count"], json!(3), "{second}");
    assert_eq!(second["next"], Value::Null, "{second}");
    assert_eq!(capture.of("listSites"), [None, None]);
    assert!(h.provider.requests().is_empty());
}

#[tokio::test]
async fn an_offset_page_with_no_place_for_its_next_link_is_generated() {
    let (capture, _guard) = Capture::install();
    // `next` holds no URL by its schema: a `null` there would say no page follows.
    let contract = SITES.replace("format: uri, ", "");
    let generated = json!({
        "count": 3,
        "next": "http://localhost/api/dcim/sites/?limit=2&offset=2&status=active",
        "previous": null,
        "results": [site(1, "active"), site(3, "active")]
    });
    let h = support::described(&contract, sites_world(), vec![generated]);
    h.hold(
        "sites",
        &[site(1, "active"), site(3, "active"), site(4, "active")],
    )
    .await;
    let (source, body) = get(&h, "/api/dcim/sites/?status=active&limit=2").await;
    assert_eq!(source, "llm", "{body}");
    assert_eq!(capture.of("listSites"), [Some("no_next_place".to_owned())]);
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn a_parameter_at_its_declared_default_asks_nothing() {
    let (capture, _guard) = Capture::install();
    let h = sites(Vec::new()).await;
    let (source, body) = get(&h, "/api/dcim/sites/?brief=false&offset=0").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(body["count"], json!(5), "{body}");
    assert_eq!(capture.of("listSites"), [None]);
}

#[tokio::test]
async fn a_search_the_description_does_not_name_is_generated() {
    let (capture, _guard) = Capture::install();
    let generated =
        json!({"count": 1, "next": null, "previous": null, "results": [site(3, "active")]});
    let h = sites(vec![generated]).await;
    let (source, body) = get(&h, "/api/dcim/sites/?q=site%203").await;
    assert_eq!(source, "llm", "{body}");
    assert_eq!(
        capture.of("listSites"),
        [Some("undescribed_param".to_owned())]
    );
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn a_held_record_that_does_not_fit_the_items_is_generated() {
    let (capture, _guard) = Capture::install();
    let generated = json!({"count": 0, "next": null, "previous": null, "results": []});
    let h = sites(vec![generated]).await;
    h.hold("sites", &[site(6, "demolished")]).await;
    let (source, body) = get(&h, "/api/dcim/sites/").await;
    assert_eq!(source, "llm", "{body}");
    assert_eq!(capture.of("listSites"), [Some("render_failed".to_owned())]);
}

/// Stripe-like items: a cursor after an item, a more flag, and a fixed `object`.
const ITEMS: &str = r##"
openapi: 3.0.0
info: {title: Items, version: "1"}
paths:
  /v1/items:
    get:
      operationId: listItems
      parameters:
        - {name: limit, in: query, schema: {type: integer, minimum: 1, maximum: 100}}
        - {name: starting_after, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [object, data, has_more]
                properties:
                  object: {type: string, enum: [list]}
                  data: {type: array, items: {$ref: "#/components/schemas/Item"}}
                  has_more: {type: boolean}
                  next_cursor: {type: string, nullable: true}
    post:
      operationId: createItem
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
      responses: {"200": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /v1/items/{id}:
    get:
      operationId: getItem
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
components:
  schemas:
    Item:
      type: object
      required: [id, amount]
      properties:
        id: {type: string}
        amount: {type: integer}
"##;

fn items_world() -> Value {
    json!({"operations": {"listItems": {"paging": {
        "style": "cursor", "request": "starting_after", "response": "next_cursor", "size": "limit"
    }}}})
}

async fn items(script: Vec<Value>) -> DescribedHarness {
    let h = support::described(ITEMS, items_world(), script);
    let held: Vec<Value> = (1..=3)
        .map(|i| json!({"id": format!("it_{i}"), "amount": i * 100}))
        .collect();
    h.hold("items", &held).await;
    h
}

#[tokio::test]
async fn a_cursor_walks_the_held_records_and_says_when_more_follow() {
    let (capture, _guard) = Capture::install();
    let h = items(Vec::new()).await;
    let (source, first) = get(&h, "/v1/items?limit=2").await;
    assert_eq!(source, "state", "{first}");
    assert_eq!(first["object"], json!("list"));
    assert_eq!(ids(&first["data"]), [json!("it_1"), json!("it_2")]);
    assert_eq!(first["has_more"], json!(true), "{first}");
    let token = first["next_cursor"].as_str().expect("a minted cursor");
    let (source, last) = get(&h, &format!("/v1/items?limit=2&starting_after={token}")).await;
    assert_eq!(source, "state", "{last}");
    assert_eq!(ids(&last["data"]), [json!("it_3")]);
    assert_eq!(last["has_more"], json!(false), "{last}");
    assert!(last["next_cursor"].is_null(), "{last}");
    assert_eq!(capture.of("listItems"), [None, None]);
    assert!(h.provider.requests().is_empty());
}

#[tokio::test]
async fn a_cursor_the_session_did_not_mint_is_generated() {
    let (capture, _guard) = Capture::install();
    let generated = json!({"object": "list", "data": [], "has_more": false});
    let h = items(vec![generated]).await;
    let (source, body) = get(&h, "/v1/items?limit=2&starting_after=it_9").await;
    assert_eq!(source, "llm", "{body}");
    assert_eq!(capture.of("listItems"), [Some("unknown_token".to_owned())]);
}

#[tokio::test]
async fn a_create_after_the_first_list_keeps_the_collection_s_order() {
    let (capture, _guard) = Capture::install();
    let h = items(vec![json!({"id": "it_4", "amount": 400})]).await;
    get(&h, "/v1/items").await;
    let (status, _, body) = h
        .send(
            "POST",
            "/v1/items",
            Some(json!({"id": "it_4", "amount": 400})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // A later write of `it_1` does not move it to the end.
    h.hold("items", &[json!({"id": "it_1", "amount": 150})])
        .await;
    let (source, body) = get(&h, "/v1/items").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(
        ids(&body["data"]),
        [json!("it_1"), json!("it_2"), json!("it_3"), json!("it_4")]
    );
    assert_eq!(body["data"][0]["amount"], json!(150));
    assert_eq!(capture.of("listItems"), [None, None]);
}

/// A project's tasks, which name their project, filtered by status.
const TASKS: &str = r##"
openapi: 3.0.0
info: {title: Tasks, version: "1"}
paths:
  /projects/{projectId}:
    get:
      operationId: getProject
      parameters: [{name: projectId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Project"}}}}}
  /projects/{projectId}/tasks:
    parameters: [{name: projectId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listTasks
      parameters: [{name: state, in: query, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [tasks, request_id]
                properties:
                  tasks: {type: array, items: {$ref: "#/components/schemas/Task"}}
                  request_id: {type: string}
    post:
      operationId: createTask
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}}
  /projects/{projectId}/tasks/{taskId}:
    parameters:
      - {name: projectId, in: path, required: true, schema: {type: string}}
      - {name: taskId, in: path, required: true, schema: {type: string}}
    get:
      operationId: getTask
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}}
components:
  schemas:
    Project:
      type: object
      required: [id]
      properties: {id: {type: string}, name: {type: string}}
    Task:
      type: object
      required: [id, title]
      properties:
        id: {type: string}
        owner: {type: string}
        title: {type: string}
        status: {type: string}
"##;

#[tokio::test]
async fn a_nested_list_holds_its_parent_s_records_only() {
    let (capture, _guard) = Capture::install();
    let world = json!({
        "types": {"tasks": {"parent": {"type": "projects", "member": "owner", "path_param": "projectId"}}},
        "operations": {"listTasks": {"filters": [{"param": "state", "member": "status", "kind": "eq"}]}}
    });
    let h = support::described(TASKS, world, Vec::new());
    let task = |id: &str, project: &str, status: &str| json!({"id": id, "owner": project, "title": id, "status": status});
    h.hold(
        "tasks",
        &[
            task("t1", "p1", "open"),
            task("t2", "p2", "open"),
            task("t3", "p1", "done"),
        ],
    )
    .await;
    let (source, body) = get(&h, "/projects/p1/tasks").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(ids(&body["tasks"]), [json!("t1"), json!("t3")]);
    // A required member beside the list is the faker's.
    assert!(body["request_id"].is_string(), "{body}");
    let (_, body) = get(&h, "/projects/p1/tasks?state=done").await;
    assert_eq!(ids(&body["tasks"]), [json!("t3")]);
    let (_, body) = get(&h, "/projects/p3/tasks").await;
    assert_eq!(body["tasks"], json!([]));
    assert_eq!(capture.of("listTasks"), [None, None, None]);
    assert!(h.provider.requests().is_empty());
}

/// RabbitMQ's queues: a bare array, named by `name`, beside another list of them.
const QUEUES: &str = r##"
openapi: 3.0.0
info: {title: Queues, version: "1"}
paths:
  /api/queues:
    get:
      operationId: listQueues
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Queue"}}}}}
  /api/queues/{vhost}/{name}:
    parameters:
      - {name: vhost, in: path, required: true, schema: {type: string}}
      - {name: name, in: path, required: true, schema: {type: string}}
    get:
      operationId: getQueue
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Queue"}}}}}
    put:
      operationId: putQueue
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Queue"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Queue"}}}}}
  /api/queues/durable:
    get:
      operationId: listDurableQueues
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Queue"}}}}}
components:
  schemas:
    Queue:
      type: object
      required: [name, vhost]
      properties:
        name: {type: string}
        vhost: {type: string}
        durable: {type: boolean}
"##;

fn queues_world() -> Value {
    json!({"types": {"queues": {"identity": {"member": "name"}}}})
}

#[tokio::test]
async fn a_bare_array_lists_the_held_records() {
    let (capture, _guard) = Capture::install();
    let contract = QUEUES.replace(
        "  /api/queues/durable:\n    get:\n      operationId: listDurableQueues\n      responses:\n        \"200\": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: \"#/components/schemas/Queue\"}}}}}\n",
        "",
    );
    assert!(!contract.contains("durable:\n    get"));
    let h = support::described(&contract, queues_world(), Vec::new());
    let queue = |name: &str| json!({"name": name, "vhost": "/", "durable": true});
    h.hold_by("queues", "name", &[queue("orders"), queue("emails")])
        .await;
    let (source, body) = get(&h, "/api/queues").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(body, json!([queue("orders"), queue("emails")]));
    assert_eq!(capture.of("listQueues"), [None]);
}

#[tokio::test]
async fn a_list_of_part_of_its_type_s_records_is_generated() {
    let (capture, _guard) = Capture::install();
    let h = support::described(QUEUES, queues_world(), vec![json!([])]);
    let (source, body) = get(&h, "/api/queues/durable").await;
    assert_eq!(source, "llm", "{body}");
    assert_eq!(
        capture.of("listDurableQueues"),
        [Some("unscoped_list".to_owned())]
    );
}

/// With no LLM at all, a list is still the session's records.
#[tokio::test]
async fn with_no_llm_a_list_is_the_session_s_records() {
    use mb_engine::{Engine, EngineConfig};
    use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};

    let (capture, _guard) = Capture::install();
    let bundle = mb_import::compile(SITES.as_bytes()).unwrap();
    let rt = mb_protocols::MockRuntime::new(
        "offline",
        Arc::new(bundle),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))) as Arc<dyn StateStore>,
    );
    let rt = support::with_effective(rt, sites_world(), false);
    let session = SessionKey {
        workspace: rt.workspace.clone(),
        mock: rt.name.clone(),
        session: "default".into(),
    };
    for record in [site(1, "active"), site(2, "planned")] {
        let resource = StoredResource {
            resource_type: "sites".into(),
            id: record["id"].to_string(),
            data: record,
        };
        rt.state.upsert(&session, resource).await.unwrap();
    }
    let router = mb_protocols::http_rest::router(Arc::new(rt));
    let req = axum::http::Request::builder()
        .uri("/api/dcim/sites/?status=planned")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, headers, body) = support::send_to(&router, req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(ids(&body["results"]), [json!(2)]);
    assert_eq!(body["count"], json!(1));
    assert_eq!(capture.of("listSites"), [None]);
}

#[tokio::test]
async fn records_list_in_the_order_they_were_created_whatever_was_written_since() {
    let (capture, _guard) = Capture::install();
    let h = items(vec![
        json!({"id": "it_4", "amount": 400}),
        json!({"id": "it_5", "amount": 500}),
    ])
    .await;
    for id in ["it_4", "it_5"] {
        let (status, _, body) = h
            .send(
                "POST",
                "/v1/items",
                Some(json!({"id": id, "amount": 1})),
                &[],
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    // A later write of `it_4` does not move it after `it_5`.
    h.hold("items", &[json!({"id": "it_4", "amount": 450})])
        .await;
    let (source, body) = get(&h, "/v1/items").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(
        ids(&body["data"]),
        [
            json!("it_1"),
            json!("it_2"),
            json!("it_3"),
            json!("it_4"),
            json!("it_5")
        ]
    );
    assert_eq!(body["data"][3]["amount"], json!(450));
    assert_eq!(capture.of("listItems"), [None]);
}

/// Links to the next page: a cursor carried in the next page's URL.
const LINKED: &str = r##"
openapi: 3.0.0
info: {title: Linked, version: "1"}
paths:
  /v2/notes:
    get:
      operationId: listNotes
      parameters:
        - {name: limit, in: query, schema: {type: integer, minimum: 1}}
        - {name: cursor, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [data, next]
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/Note"}}
                  next: {type: string, format: uri, nullable: true}
    post:
      operationId: createNote
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
  /v2/notes/{id}:
    get:
      operationId: getNote
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
components:
  schemas:
    Note:
      type: object
      required: [id, text]
      properties:
        id: {type: string}
        text: {type: string}
"##;

#[tokio::test]
async fn a_next_url_walks_the_held_records() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"listNotes": {"paging": {
        "style": "next_url", "request": "cursor", "response": "next", "size": "limit"
    }}}});
    let h = support::described(LINKED, world, Vec::new());
    let held: Vec<Value> = (1..=3)
        .map(|i| json!({"id": format!("n{i}"), "text": format!("note {i}")}))
        .collect();
    h.hold("notes", &held).await;
    let (source, first) = get(&h, "/v2/notes?limit=2").await;
    assert_eq!(source, "state", "{first}");
    assert_eq!(ids(&first["data"]), [json!("n1"), json!("n2")]);
    let next = first["next"].as_str().expect("a next link");
    let at = next.find("/v2/notes").expect("the route's URL");
    let next = &next[at..];
    assert!(
        next.contains("cursor=") && next.contains("limit=2"),
        "{next}"
    );
    let (source, last) = get(&h, next).await;
    assert_eq!(source, "state", "{last}");
    assert_eq!(ids(&last["data"]), [json!("n3")]);
    assert!(last["next"].is_null(), "{last}");
    assert_eq!(capture.of("listNotes"), [None, None]);
    assert!(h.provider.requests().is_empty());
}

/// Heroku's apps: paged by the `Range` request header, a `206` with a `Next-Range`
/// while more remain.
const APPS: &str = r##"
openapi: 3.0.0
info: {title: Apps, version: "1"}
paths:
  /apps:
    get:
      operationId: listApps
      parameters: [{name: Range, in: header, schema: {type: string}}]
      responses:
        "200": {description: all, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/App"}}}}}
        "206":
          description: a page
          headers: {Next-Range: {schema: {type: string}}}
          content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/App"}}}}
  /apps/{appId}:
    get:
      operationId: getApp
      parameters: [{name: appId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/App"}}}}}
components:
  schemas:
    App:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
"##;

#[tokio::test]
async fn a_range_header_orders_and_cuts_the_held_records() {
    let (capture, _guard) = Capture::install();
    let h = support::described(APPS, json!({}), Vec::new());
    let app = |id: &str, name: &str| json!({"id": id, "name": name});
    h.hold(
        "apps",
        &[app("a1", "gamma"), app("a2", "alpha"), app("a3", "beta")],
    )
    .await;
    let (status, headers, first) = h
        .send("GET", "/apps", None, &[("Range", "name ..; max=2")])
        .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT, "{first}");
    assert_eq!(source(&headers), "state", "{first}");
    assert_eq!(ids(&first), [json!("a2"), json!("a3")]);
    let next = headers
        .get("next-range")
        .and_then(|v| v.to_str().ok())
        .expect("a Next-Range")
        .to_string();
    assert_eq!(next, "name ]beta..; max=2;");
    let (status, _, last) = h.send("GET", "/apps", None, &[("Range", &next)]).await;
    assert_eq!(status, StatusCode::OK, "{last}");
    assert_eq!(ids(&last), [json!("a1")]);
    assert_eq!(capture.of("listApps"), [None, None]);
    assert!(h.provider.requests().is_empty());
}

/// BigCommerce's orders: a list that declares a bodiless `204` for no match.
const ORDERS: &str = r##"
openapi: 3.0.0
info: {title: Orders, version: "1"}
paths:
  /v2/orders:
    get:
      operationId: listOrders
      parameters: [{name: status_id, in: query, schema: {type: integer}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Order"}}}}}
        "204": {description: No order matches}
  /v2/orders/{id}:
    get:
      operationId: getOrder
      parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
components:
  schemas:
    Order:
      type: object
      required: [id, status_id]
      properties:
        id: {type: integer}
        status_id: {type: integer}
"##;

#[tokio::test]
async fn a_list_left_with_nothing_answers_its_declared_204() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"listOrders": {
        "filters": [{"param": "status_id", "member": "status_id", "kind": "eq"}]
    }}});
    let h = support::described(ORDERS, world, Vec::new());
    let (status, headers, body) = h.send("GET", "/v2/orders", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(source(&headers), "state");
    h.hold("orders", &[json!({"id": 1, "status_id": 11})]).await;
    let (status, _, body) = h.send("GET", "/v2/orders?status_id=11", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ids(&body), [json!(1)]);
    let (status, _, body) = h.send("GET", "/v2/orders?status_id=5", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(capture.of("listOrders"), [None, None, None]);
    assert!(h.provider.requests().is_empty());
}
