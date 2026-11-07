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
    exec_fallback_on: Option<String>,
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
        match field.name() {
            "operation" => self.operation = format!("{value:?}"),
            "exec_fallback_on" => self.exec_fallback_on = Some(format!("{value:?}")),
            _ => {}
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

    /// What caused each fallback logged for `operation` (`exec_fallback_on`).
    fn causes(&self, operation: &str) -> Vec<Option<String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.operation == operation && l.exec_fallback.is_some())
            .map(|l| l.exec_fallback_on.clone())
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
    // The line names the parameter.
    assert_eq!(capture.causes("listSites"), [Some("q".to_owned())]);
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

/// A type the session never held a record of (a catalog the model makes up, such as a
/// flight search's offers) is generated: the store does not know it is empty.
#[tokio::test]
async fn a_list_of_a_type_the_session_never_held_is_generated() {
    let (capture, _guard) = Capture::install();
    let generated = json!([{"name": "orders", "vhost": "/", "durable": true}]);
    let contract = QUEUES.replace(
        "  /api/queues/durable:\n    get:\n      operationId: listDurableQueues\n      responses:\n        \"200\": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: \"#/components/schemas/Queue\"}}}}}\n",
        "",
    );
    assert!(!contract.contains("durable:\n    get"));
    let h = support::described(&contract, queues_world(), vec![generated.clone()]);
    let (source, body) = get(&h, "/api/queues").await;
    assert_eq!(source, "llm", "{body}");
    assert_eq!(body, generated);
    assert_eq!(capture.of("listQueues"), [Some("never_written".to_owned())]);
}

/// The contract of [`QUEUES`] with its one list of every queue, and a delete.
fn one_queue_list() -> String {
    let contract = QUEUES
        .replace(
            "  /api/queues/durable:\n    get:\n      operationId: listDurableQueues\n      responses:\n        \"200\": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: \"#/components/schemas/Queue\"}}}}}\n",
            "",
        )
        .replace(
            "    put:\n      operationId: putQueue\n",
            "    delete:\n      operationId: deleteQueue\n      responses: {\"204\": {description: deleted}}\n    put:\n      operationId: putQueue\n",
        );
    assert!(contract.contains("deleteQueue") && !contract.contains("listDurableQueues"));
    contract
}

/// A session whose seeded records were all deleted held the type: it lists nothing, from
/// state, where one that never held the type is generated.
#[tokio::test]
async fn a_list_whose_held_records_were_all_deleted_lists_nothing() {
    let (capture, _guard) = Capture::install();
    let h = support::described(&one_queue_list(), queues_world(), Vec::new());
    h.hold_by(
        "queues",
        "name",
        &[json!({"name": "orders", "vhost": "v", "durable": true})],
    )
    .await;
    let (status, _, body) = h.send("DELETE", "/api/queues/v/orders", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (source, body) = get(&h, "/api/queues").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(body, json!([]));
    assert_eq!(capture.of("listQueues"), [None]);
    assert!(h.provider.requests().is_empty());
}

/// A nested list whose route nothing ties its type's held records to (no described
/// `parent`, no create on its path, no member named for its parameter), never listed,
/// is generated: its records may sit under another route.
#[tokio::test]
async fn a_nested_list_no_held_record_is_tied_to_is_generated() {
    let (capture, _guard) = Capture::install();
    let generated = json!({"tasks": [{"id": "t9", "title": "made up"}], "request_id": "r"});
    // Tasks are created elsewhere (`/teams/{teamId}/tasks`): not on the list's path.
    let contract = TASKS.replace(
        "    post:
      operationId: createTask
",
        "    put:
      operationId: putTasks
",
    );
    assert!(!contract.contains("createTask"));
    let h = support::described(&contract, json!({}), vec![generated.clone()]);
    h.hold(
        "tasks",
        &[json!({"id": "t1", "owner": "p1", "title": "t1"})],
    )
    .await;
    let (source, body) = get(&h, "/projects/p1/tasks").await;
    assert_eq!(source, "llm", "{body}");
    assert_eq!(body, generated);
    assert_eq!(
        capture.of("listTasks"),
        [Some("scope_unresolved".to_owned())]
    );
}

/// A nested collection its route places records in (a create of the type on its path)
/// holds none of the held records of another: it is empty, from state, though never
/// listed.
#[tokio::test]
async fn a_nested_list_its_route_places_records_in_is_empty_from_state() {
    let (capture, _guard) = Capture::install();
    let h = support::described(TASKS, json!({}), Vec::new());
    h.hold(
        "tasks",
        &[json!({"id": "t1", "owner": "p1", "title": "t1"})],
    )
    .await;
    let (source, body) = get(&h, "/projects/p2/tasks").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(body["tasks"], json!([]));
    assert_eq!(capture.of("listTasks"), [None]);
}

/// A held record holding a member named for the route's parameter ties it: another
/// collection of the route is empty, from state.
#[tokio::test]
async fn a_member_named_for_the_route_s_parameter_ties_its_collections() {
    let (capture, _guard) = Capture::install();
    let contract = TASKS.replace(
        "    post:\n      operationId: createTask\n",
        "    put:\n      operationId: putTasks\n",
    );
    assert!(!contract.contains("createTask"));
    let h = support::described(&contract, json!({}), Vec::new());
    h.hold(
        "tasks",
        &[json!({"id": "t1", "projectId": "p1", "title": "t1"})],
    )
    .await;
    let (source, body) = get(&h, "/projects/p2/tasks").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(body["tasks"], json!([]));
    assert_eq!(capture.of("listTasks"), [None]);
}

/// A list asked by a POST (Notion's `query` of a data source) reads its filter from the
/// body: a body member that is no described parameter falls back as a query parameter
/// does (`undescribed_param`), and a body that asks nothing lists the held records.
#[tokio::test]
async fn a_body_filter_the_executor_does_not_read_is_generated() {
    let (capture, _guard) = Capture::install();
    let contract = QUEUES
        .replace(
            "  /api/queues/durable:\n    get:\n      operationId: listDurableQueues\n      responses:\n        \"200\": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: \"#/components/schemas/Queue\"}}}}}\n",
            "",
        )
        .replace(
            "  /api/queues:\n    get:\n      operationId: listQueues\n",
            "  /api/queues/query:\n    post:\n      operationId: queryQueues\n      requestBody: {content: {application/json: {schema: {type: object, properties: {filter: {type: object}}}}}}\n",
        );
    assert!(contract.contains("queryQueues") && !contract.contains("listQueues"));
    let mut world = queues_world();
    world["operations"] = json!({"queryQueues": {"kind": "list", "type": "queues"}});
    let generated = json!([{"name": "emails", "vhost": "/", "durable": true}]);
    let h = support::described(&contract, world, vec![generated.clone(), generated.clone()]);
    let queue = |name: &str| json!({"name": name, "vhost": "/", "durable": name == "emails"});
    h.hold_by("queues", "name", &[queue("orders"), queue("emails")])
        .await;
    let query = |body: Value| h.send("POST", "/api/queues/query", Some(body), &[]);
    let (status, headers, body) = query(json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(body, json!([queue("orders"), queue("emails")]));
    let (status, headers, body) = query(json!({"filter": {"durable": true}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "llm", "{body}");
    assert_eq!(body, generated);
    // A cursor in the body is not read either: it is not served the first page again.
    let (_, headers, body) = query(json!({"start_cursor": "abc"})).await;
    assert_eq!(source(&headers), "llm", "{body}");
    assert_eq!(
        capture.of("queryQueues"),
        [
            None,
            Some("undescribed_param".to_owned()),
            Some("undescribed_param".to_owned())
        ]
    );
    // Each line names the body member it could not read.
    assert_eq!(
        capture.causes("queryQueues"),
        [Some("filter".to_owned()), Some("start_cursor".to_owned())]
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

/// Twilio's messages: a next-page URL beside the list, a page size and a page token
/// that shares its first word.
const MESSAGES: &str = r##"
openapi: 3.0.0
info: {title: Messages, version: "1"}
paths:
  /v2/messages:
    get:
      operationId: listMessages
      parameters:
        - {name: PageSize, in: query, schema: {type: integer, minimum: 1}}
        - {name: Page, in: query, schema: {type: integer, minimum: 0}}
        - {name: PageToken, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [messages, next_page_uri]
                properties:
                  messages: {type: array, items: {$ref: "#/components/schemas/Message"}}
                  next_page_uri: {type: string, format: uri-reference, nullable: true}
  /v2/messages/{id}:
    get:
      operationId: getMessage
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Message"}}}}}
components:
  schemas:
    Message:
      type: object
      required: [id, body]
      properties:
        id: {type: string}
        body: {type: string}
"##;

/// Walks [`MESSAGES`] described with `paging` from a page of one: the first page holds
/// the first message and a next link carrying `PageToken`, which reaches the second.
async fn walk_messages(paging: Value) {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"listMessages": {"paging": paging}}});
    let h = support::described(MESSAGES, world, Vec::new());
    let held: Vec<Value> = (1..=2)
        .map(|i| json!({"id": format!("SM{i}"), "body": format!("text {i}")}))
        .collect();
    h.hold("messages", &held).await;
    let (source, first) = get(&h, "/v2/messages?PageSize=1").await;
    assert_eq!(source, "state", "{first}");
    let sids = |page: &Value| -> Vec<Value> {
        page["messages"]
            .as_array()
            .map(|items| items.iter().map(|i| i["id"].clone()).collect())
            .unwrap_or_default()
    };
    assert_eq!(sids(&first), [json!("SM1")], "{first}");
    let next = first["next_page_uri"].as_str().expect("a next link");
    assert!(next.contains("PageToken="), "{next}");
    let at = next.find("/v2/messages").expect("the route's URL");
    let (source, last) = get(&h, &next[at..]).await;
    assert_eq!(source, "state", "{last}");
    assert_eq!(sids(&last), [json!("SM2")], "{last}");
    assert_eq!(capture.of("listMessages"), [None, None]);
    assert!(h.provider.requests().is_empty());
}

/// A next-page URL whose token parameter the description leaves out (the client only
/// follows the URL): the described member holds the link, its parameter guessed.
#[tokio::test]
async fn a_next_url_with_no_described_parameter_cuts_the_page_to_its_size() {
    walk_messages(json!({"style": "next_url", "response": "next_page_uri", "size": "PageSize"}))
        .await;
}

/// A token paging naming its parameter and no place answering it: the parameter as
/// described, the place (the one next-page URL member) guessed.
#[tokio::test]
async fn a_token_paging_naming_only_its_parameter_guesses_its_place() {
    walk_messages(json!({"style": "next_url", "request": "PageToken", "size": "PageSize"})).await;
}

/// SendGrid's templates: the list answers a lean schema of the record the read answers
/// in full, under an envelope member named for neither.
const TEMPLATES: &str = r##"
openapi: 3.0.0
info: {title: Templates, version: "1"}
paths:
  /v3/templates:
    get:
      operationId: listTemplates
      parameters:
        - {name: page_size, in: query, schema: {type: integer, minimum: 1}}
        - {name: page_token, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [result]
                properties:
                  result: {type: array, items: {$ref: "#/components/schemas/TemplateLean"}}
    post:
      operationId: createTemplate
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}}
  /v3/templates/{template_id}:
    get:
      operationId: getTemplate
      parameters: [{name: template_id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}}
components:
  schemas:
    TemplateLean:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Template:
      type: object
      required: [id, name, versions]
      properties:
        id: {type: string}
        name: {type: string}
        versions: {type: array, items: {type: object, properties: {id: {type: string}}}}
"##;

/// A path's only list of its type answers every held record of it, whatever schema
/// name its items go by.
#[tokio::test]
async fn the_only_unscoped_list_of_a_described_type_lists_its_records() {
    let (capture, _guard) = Capture::install();
    let world = json!({
        "types": {"templates": {"identity": {"member": "id"}}},
        "operations": {"listTemplates": {"type": "templates", "kind": "list", "answer": {"records": "/result"}}}
    });
    let h = support::described(TEMPLATES, world, Vec::new());
    let held: Vec<Value> = (1..=2)
        .map(|i| json!({"id": format!("t{i}"), "name": format!("template {i}"), "versions": []}))
        .collect();
    h.hold("templates", &held).await;
    let (source, page) = get(&h, "/v3/templates").await;
    assert_eq!(capture.of("listTemplates"), [None]);
    assert_eq!(source, "state", "{page}");
    assert_eq!(ids(&page["result"]), [json!("t1"), json!("t2")]);
}

/// A token paging naming its request parameter, whose answer has no place the
/// heuristics find for a token either: the page is served whole rather than cut with
/// the rest out of reach.
#[tokio::test]
async fn a_token_paging_with_no_place_found_serves_the_page_whole() {
    let (capture, _guard) = Capture::install();
    let world = json!({
        "types": {"templates": {"identity": {"member": "id"}}},
        "operations": {"listTemplates": {
            "type": "templates", "kind": "list", "answer": {"records": "/result"},
            "paging": {"style": "cursor", "request": "page_token", "size": "page_size"}
        }}
    });
    let h = support::described(TEMPLATES, world, Vec::new());
    let held: Vec<Value> = (1..=2)
        .map(|i| json!({"id": format!("t{i}"), "name": format!("template {i}"), "versions": []}))
        .collect();
    h.hold("templates", &held).await;
    let (source, page) = get(&h, "/v3/templates?page_size=1").await;
    assert_eq!(capture.of("listTemplates"), [None]);
    assert_eq!(source, "state", "{page}");
    assert_eq!(ids(&page["result"]), [json!("t1"), json!("t2")], "{page}");
}

/// GitHub's repository search: a query with qualifiers, answered by the model.
const SEARCH: &str = r##"
openapi: 3.0.0
info: {title: Search, version: "1"}
paths:
  /search/repositories:
    get:
      operationId: searchRepos
      parameters:
        - {name: q, in: query, required: true, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [items]
                properties:
                  items: {type: array, items: {$ref: "#/components/schemas/Repo"}}
  /repositories/{id}:
    get:
      operationId: getRepo
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Repo"}}}}}
components:
  schemas:
    Repo:
      type: object
      required: [id, full_name]
      properties: {id: {type: string}, full_name: {type: string}}
"##;

/// A text filter naming no member says nothing the model's items can be checked
/// against (`language:rust` is in none of their members): the answered items stay.
#[tokio::test]
async fn a_text_filter_naming_no_member_keeps_the_model_s_items() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"searchRepos": {
        "type": "repositories", "kind": "list", "answer": {"records": "/items"},
        "filters": [{"kind": "text", "param": "q"}]
    }}, "types": {"repositories": {"identity": {"member": "id"}}}});
    let answered = json!({"items": [
        {"id": "r1", "full_name": "octo/one"},
        {"id": "r2", "full_name": "octo/two"}
    ]});
    let h = support::described(SEARCH, world, vec![answered]);
    let (source, page) = get(&h, "/search/repositories?q=language:rust").await;
    assert_eq!(source, "llm", "{page}");
    assert_eq!(ids(&page["items"]), [json!("r1"), json!("r2")], "{page}");
    assert_eq!(
        capture.of("searchRepos"),
        [Some("never_written".to_owned())]
    );
}

/// Pipedrive's deals: a search beside the list, path-less too, answering hits that
/// hold the record one level down; the description gives both the record's type.
#[tokio::test]
async fn a_search_beside_the_list_leaves_it_the_type_s_only_list() {
    let (capture, _guard) = Capture::install();
    let contract = ITEMS.replace(
        "  /v1/items/{id}:",
        "  /v1/items/search:\n    get:\n      operationId: searchItems\n      parameters: [{name: term, in: query, schema: {type: string}}]\n      responses: {\"200\": {description: ok, content: {application/json: {schema: {type: object, properties: {data: {type: object, properties: {items: {type: array, items: {type: object, properties: {result_score: {type: number}, item: {$ref: \"#/components/schemas/Item\"}}}}}}}}}}}}\n  /v1/items/{id}:",
    );
    let mut world = items_world();
    world["operations"]["searchItems"] = json!({"kind": "list", "type": "items"});
    let h = support::described(&contract, world, Vec::new());
    let held: Vec<Value> = (1..=2)
        .map(|i| json!({"id": format!("it_{i}"), "amount": i * 100}))
        .collect();
    h.hold("items", &held).await;
    let (source, page) = get(&h, "/v1/items").await;
    assert_eq!(source, "state", "{page}");
    assert_eq!(ids(&page["data"]), [json!("it_1"), json!("it_2")]);
    assert_eq!(capture.of("listItems"), [None]);
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

/// A record the store holds lacking a filtered member is not set there: a filter
/// leaves it out (Pipedrive's deals of a person, one deal naming no person).
#[tokio::test]
async fn a_held_record_lacking_a_filtered_member_is_left_out() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"listOrders": {
        "filters": [{"param": "status_id", "member": "status_id", "kind": "eq"}]
    }}});
    let h = support::described(ORDERS, world, Vec::new());
    h.hold(
        "orders",
        &[json!({"id": 1, "status_id": 11}), json!({"id": 2})],
    )
    .await;
    let (status, headers, body) = h.send("GET", "/v2/orders?status_id=11", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    assert_eq!(ids(&body), [json!(1)]);
    assert_eq!(capture.of("listOrders"), [None]);
}

#[tokio::test]
async fn a_list_left_with_nothing_answers_its_declared_204() {
    let (capture, _guard) = Capture::install();
    let world = json!({"operations": {"listOrders": {
        "filters": [{"param": "status_id", "member": "status_id", "kind": "eq"}]
    }}});
    let h = support::described(ORDERS, world, Vec::new());
    h.hold("orders", &[json!({"id": 1, "status_id": 11})]).await;
    let (status, _, body) = h.send("GET", "/v2/orders?status_id=11", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(ids(&body), [json!(1)]);
    let (status, headers, body) = h.send("GET", "/v2/orders?status_id=5", None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(source(&headers), "state");
    assert_eq!(capture.of("listOrders"), [None, None]);
    assert!(h.provider.requests().is_empty());
}

/// Users listed with a filter expression in each grammar a description can name: a
/// SCIM `filter`, an OData `$filter`, `field:value` pairs, deepObject brackets below
/// `where` (one of its keys declared literally) and a map member's keys below `meta`.
const USERS: &str = r##"
openapi: 3.0.0
info: {title: Users, version: "1"}
paths:
  /users:
    get:
      operationId: listUsers
      parameters:
        - {name: filter, in: query, schema: {type: string}}
        - {name: $filter, in: query, schema: {type: string}}
        - {name: q, in: query, schema: {type: string}}
        - {name: where, in: query, style: deepObject, explode: true, schema: {type: object}}
        - {name: "where[active]", in: query, schema: {type: boolean}}
        - {name: meta, in: query, style: deepObject, explode: true,
           schema: {type: object, additionalProperties: {type: string}}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [Resources]
                properties:
                  Resources: {type: array, items: {$ref: "#/components/schemas/User"}}
  /users/{id}:
    get:
      operationId: getUser
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}}
components:
  schemas:
    User:
      type: object
      required: [id]
      properties:
        id: {type: string}
        userName: {type: string}
        active: {type: boolean}
        age: {type: integer}
        emails:
          type: array
          items: {type: object, properties: {value: {type: string}, type: {type: string}}}
        meta: {type: object, additionalProperties: {type: string}}
"##;

fn users_world() -> Value {
    json!({"operations": {"listUsers": {"filters": [
        {"param": "filter", "grammar": "scim"},
        {"param": "$filter", "grammar": "odata"},
        {"param": "q", "grammar": "field_value"},
        {"param": "where", "grammar": "deep_object"},
        {"param": "meta", "grammar": "deep_object", "member_prefix": "meta"}
    ]}}, "types": {"users": {"identity": {"member": "id"}}}})
}

fn user(id: &str, name: &str, active: bool, age: i64, email: &str, run: &str) -> Value {
    json!({"id": id, "userName": name, "active": active, "age": age,
        "emails": [{"value": email, "type": "work"}], "meta": {"run": run}})
}

async fn users(script: Vec<Value>) -> DescribedHarness {
    let h = support::described(USERS, users_world(), script);
    h.hold(
        "users",
        &[
            user("u1", "ann", true, 31, "ann@acme.io", "r1"),
            user("u2", "Anna", false, 25, "anna@else.io", "r1"),
            user("u3", "bob", true, 40, "bob@acme.io", "r2"),
            // Holds neither an age nor emails.
            json!({"id": "u4", "userName": "dee", "active": true, "meta": {"run": "r2"}}),
        ],
    )
    .await;
    h
}

/// The query string of `pairs`, each value percent-encoded.
fn query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            let enc = |s: &str| {
                s.bytes()
                    .map(|b| match b {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                            (b as char).to_string()
                        }
                        b => format!("%{b:02X}"),
                    })
                    .collect::<String>()
            };
            format!("{}={}", enc(k), enc(v))
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// The ids a list of users answers from state for `pairs`, no model call made.
async fn listed(h: &DescribedHarness, pairs: &[(&str, &str)]) -> Vec<Value> {
    let (source, body) = get(h, &format!("/users?{}", query(pairs))).await;
    assert_eq!(source, "state", "{pairs:?}: {body}");
    ids(&body["Resources"])
}

#[tokio::test]
async fn a_scim_filter_keeps_the_held_records_it_matches() {
    let (capture, _guard) = Capture::install();
    let h = users(Vec::new()).await;
    // Operators and attribute names in any case, and text compared in any case.
    assert_eq!(
        listed(&h, &[("filter", r#"USERNAME eq "ANN" and active Eq true"#)]).await,
        [json!("u1")]
    );
    assert_eq!(
        listed(
            &h,
            &[("filter", r#"emails.value co "@ACME" or userName sw "dee""#)]
        )
        .await,
        [json!("u1"), json!("u3"), json!("u4")]
    );
    assert_eq!(
        listed(&h, &[("filter", "not (emails pr)")]).await,
        [json!("u4")]
    );
    assert_eq!(capture.of("listUsers"), [None, None, None]);
    assert!(h.provider.requests().is_empty());
}

#[tokio::test]
async fn an_odata_filter_keeps_the_held_records_it_matches() {
    let (capture, _guard) = Capture::install();
    let h = users(Vec::new()).await;
    assert_eq!(
        listed(&h, &[("$filter", "age ge 30 and contains(userName,'n')")]).await,
        [json!("u1")]
    );
    assert_eq!(
        listed(
            &h,
            &[(
                "$filter",
                "userName in ('bob', 'dee') or startswith(userName,'An')"
            )]
        )
        .await,
        [json!("u2"), json!("u3"), json!("u4")]
    );
    assert_eq!(capture.of("listUsers"), [None, None]);
}

#[tokio::test]
async fn field_value_pairs_keep_the_records_holding_every_one() {
    let (capture, _guard) = Capture::install();
    let h = users(Vec::new()).await;
    assert_eq!(
        listed(&h, &[("q", "active:true,meta.run:r2")]).await,
        [json!("u3"), json!("u4")]
    );
    assert_eq!(capture.of("listUsers"), [None]);
}

#[tokio::test]
async fn deep_object_brackets_are_the_filter_s_own_parameters() {
    let (capture, _guard) = Capture::install();
    let h = users(Vec::new()).await;
    // Operators after the member's keys, nested and literally declared keys alike.
    assert_eq!(
        listed(
            &h,
            &[("where[meta][run][_eq]", "r1"), ("where[age][_gt]", "26")]
        )
        .await,
        [json!("u1")]
    );
    assert_eq!(
        listed(
            &h,
            &[
                ("where[active]", "true"),
                ("where[userName][_in]", "bob,ann")
            ]
        )
        .await,
        [json!("u1"), json!("u3")]
    );
    // A map member's keys, below its `member_prefix`.
    assert_eq!(
        listed(&h, &[("meta[run]", "r2")]).await,
        [json!("u3"), json!("u4")]
    );
    assert_eq!(capture.of("listUsers"), [None, None, None]);
}

#[tokio::test]
async fn a_filter_expression_that_does_not_parse_is_generated() {
    let (capture, _guard) = Capture::install();
    let generated = json!({"Resources": [user("u1", "ann", true, 31, "ann@acme.io", "r1")]});
    let h = users(vec![generated.clone(), generated]).await;
    for pairs in [
        [("filter", r#"emails[type eq "work"]"#)],
        [("where[_or][0][age][_eq]", "1")],
    ] {
        let (source, body) = get(&h, &format!("/users?{}", query(&pairs))).await;
        assert_eq!(source, "llm", "{body}");
    }
    assert_eq!(
        capture.of("listUsers"),
        [
            Some("unparsed_filter".to_owned()),
            Some("unparsed_filter".to_owned())
        ]
    );
    assert_eq!(
        capture.causes("listUsers"),
        [Some("filter".to_owned()), Some("where".to_owned())]
    );
    assert_eq!(h.provider.requests().len(), 2);
}

#[tokio::test]
async fn a_held_record_lacking_an_expression_s_member_does_not_match() {
    let (capture, _guard) = Capture::install();
    let h = users(Vec::new()).await;
    // `u4` holds no age: not below 100, and not `ne 31` either.
    assert_eq!(
        listed(&h, &[("$filter", "age lt 100")]).await,
        [json!("u1"), json!("u2"), json!("u3")]
    );
    assert_eq!(
        listed(&h, &[("$filter", "age ne 31")]).await,
        [json!("u2"), json!("u3")]
    );
    // Under `not`, the comparison is false, so its negation holds.
    assert_eq!(
        listed(&h, &[("$filter", "not (age lt 100)")]).await,
        [json!("u4")]
    );
    assert_eq!(capture.of("listUsers"), [None, None, None]);
}

#[tokio::test]
async fn an_expression_value_of_the_wrong_type_narrows_nothing() {
    let (capture, _guard) = Capture::install();
    let h = users(Vec::new()).await;
    // `age` is an integer: `'abc'` says nothing, so only the other side narrows.
    assert_eq!(
        listed(&h, &[("$filter", "age eq 'abc'")]).await,
        [json!("u1"), json!("u2"), json!("u3"), json!("u4")]
    );
    assert_eq!(
        listed(&h, &[("q", "age:old,active:false")]).await,
        [json!("u2")]
    );
    assert_eq!(capture.of("listUsers"), [None, None]);
}

#[tokio::test]
async fn the_model_s_items_lacking_an_expression_s_member_are_kept() {
    let (capture, _guard) = Capture::install();
    let answered = json!({"Resources": [
        {"id": "m1", "userName": "ann"},
        {"id": "m2"},
        {"id": "m3", "userName": "bob"}
    ]});
    // No record held: the model answers, and its items are kept unless they disagree.
    let h = support::described(USERS, users_world(), vec![answered]);
    let (source, body) = get(
        &h,
        &format!("/users?{}", query(&[("filter", r#"userName eq "ann""#)])),
    )
    .await;
    assert_eq!(source, "llm", "{body}");
    assert_eq!(
        ids(&body["Resources"]),
        [json!("m1"), json!("m2")],
        "{body}"
    );
    assert_eq!(capture.of("listUsers"), [Some("never_written".to_owned())]);
}
