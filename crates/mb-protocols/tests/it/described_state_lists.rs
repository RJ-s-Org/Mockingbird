//! Lists answered from state.
//!
//! A list whose operation the description pages, filters, scopes or orders is answered
//! from the session's records, before any model call, once the session has listed that
//! collection; the first list of a collection is still generated and seeded. The
//! "listed" mark lives in the state store, so it survives a rebuild.
//!
//! A list the state-first executor takes is answered from the session's records from
//! its first call (`tests/exec.rs`); the lists here that are generated first share
//! their item type with other lists with no path parameter, which the executor leaves.
#![cfg(test)]
#![cfg(feature = "http-rest")]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::MockRuntime;
use mb_protocols::world_gaps::GapCode;
use mb_state::{MemoryStore, StateStore};
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness};

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Templates, version: "1"}
paths:
  /templates:
    get:
      operationId: listTemplates
      parameters:
        - {name: sheet, in: query, schema: {type: integer, minimum: 1}}
        - {name: rows, in: query, schema: {type: integer}}
        - {name: from, in: query, schema: {type: string}}
        - {name: email, in: query, schema: {type: string, format: email}}
        - {name: minAmount, in: query, schema: {type: integer}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [templates]
                properties:
                  templates: {type: array, items: {$ref: "#/components/schemas/Template"}}
                  continuation: {type: string}
    post:
      operationId: createTemplate
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}}
  /templates/{templateId}:
    get:
      operationId: getTemplate
      parameters: [{name: templateId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}}
  /strict:
    get:
      operationId: listStrict
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [templates, tally]
                properties:
                  templates: {type: array, items: {$ref: "#/components/schemas/Template"}}
                  tally: {type: integer}
  /strict2:
    get:
      operationId: listStrictFixed
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [templates, tally]
                properties:
                  templates: {type: array, items: {$ref: "#/components/schemas/Template"}}
                  tally: {type: integer, default: 0}
components:
  schemas:
    Template:
      type: object
      required: [id]
      properties:
        id: {type: string}
        email: {type: string, format: email}
        amount: {type: integer}
        createdAt: {type: string, format: date-time}
"##;

/// Notes listed and created under a folder; `{parent}` is replaced by the members a
/// note declares besides its `id` and `title`.
const NOTES: &str = r##"
openapi: 3.0.0
info: {title: Notes, version: "1"}
paths:
  /folders/{folderId}/notes:
    parameters: [{name: folderId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: listNotes
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [notes]
                properties:
                  notes: {type: array, items: {$ref: "#/components/schemas/Note"}}
    post:
      operationId: createNote
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
components:
  schemas:
    Note:
      type: object
      required: [id]
      properties:
        id: {type: string}
        title: {type: string}{parent}
"##;

/// Something requests are sent to: a harness, or a rebuilt runtime's router.
trait Sends {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value);
}

impl Sends for DescribedHarness {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        self.send(method, uri, body, &[]).await
    }
}

impl Sends for axum::Router {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut req = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let body = body.map_or_else(axum::body::Body::empty, |b| {
            axum::body::Body::from(b.to_string())
        });
        support::send_to(self, req.body(body).unwrap()).await
    }
}

fn template(id: &str, email: &str, amount: i64) -> Value {
    json!({"id": id, "email": email, "amount": amount})
}

fn list(items: Vec<Value>) -> Value {
    json!({"templates": items})
}

fn ids(body: &Value, member: &str) -> Vec<String> {
    body[member]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn source(headers: &HeaderMap) -> String {
    headers
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn world(desc: Value) -> Value {
    json!({"operations": {"listTemplates": desc}})
}

fn page_paging() -> Value {
    json!({"paging": {"style": "page", "request": "sheet", "size": "rows"}})
}

fn harness(desc: Value, script: Vec<Value>) -> DescribedHarness {
    support::described(CONTRACT, world(desc), script)
}

/// A `200` list answer's source and body.
async fn get(to: &impl Sends, uri: &str) -> (String, Value) {
    let (status, headers, body) = to.call("GET", uri, None).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    (source(&headers), body)
}

async fn create(to: &impl Sends, uri: &str, body: Value) {
    let (status, _, answer) = to.call("POST", uri, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
}

#[tokio::test]
async fn a_second_list_is_answered_from_state_with_the_new_record() {
    let h = harness(
        page_paging(),
        vec![
            list(vec![
                template("t1", "a@x.io", 1),
                template("t2", "b@x.io", 2),
            ]),
            template("t3", "c@x.io", 3),
        ],
    );
    let (first, body) = get(&h, "/templates").await;
    assert_ne!(first, "state");
    assert_eq!(ids(&body, "templates"), ["t1", "t2"]);
    create(&h, "/templates", template("t3", "c@x.io", 3)).await;
    let (second, body) = get(&h, "/templates").await;
    assert_eq!(second, "state", "{body}");
    assert_eq!(ids(&body, "templates"), ["t1", "t2", "t3"]);
    assert_eq!(h.provider.requests().len(), 2);
}

#[tokio::test]
async fn a_described_filter_answers_from_state_exactly() {
    let h = harness(
        json!({"filters": [{"param": "minAmount", "member": "amount", "kind": "gte"}]}),
        vec![
            list(vec![
                template("t1", "a@x.io", 1),
                template("t2", "a@x.io", 5),
                template("t3", "a@x.io", 9),
            ]),
            template("t4", "a@x.io", 7),
        ],
    );
    get(&h, "/templates").await;
    create(&h, "/templates", template("t4", "a@x.io", 7)).await;
    let (source, body) = get(&h, "/templates?minAmount=5").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(ids(&body, "templates"), ["t2", "t3", "t4"]);
    assert_eq!(h.provider.requests().len(), 2);
}

/// From state, the store fills the page itself: a stored record lacking a described
/// filter's member does not match it.
#[tokio::test]
async fn a_described_filter_from_state_leaves_out_a_record_lacking_the_member() {
    let h = harness(
        json!({"filters": [{"param": "minAmount", "member": "amount", "kind": "gte"}]}),
        vec![
            list(vec![
                template("t1", "a@x.io", 1),
                json!({"id": "t2", "email": "a@x.io"}),
                template("t3", "a@x.io", 9),
            ]),
            template("t4", "a@x.io", 7),
        ],
    );
    get(&h, "/templates").await;
    create(&h, "/templates", template("t4", "a@x.io", 7)).await;
    let (source, body) = get(&h, "/templates?minAmount=5").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(ids(&body, "templates"), ["t3", "t4"]);
}

#[tokio::test]
async fn pages_from_state_never_repeat_an_item() {
    let five = || {
        list(
            (1..=5)
                .map(|i| template(&format!("t{i}"), "a@x.io", i))
                .collect(),
        )
    };
    // By a cursor the mock mints.
    let h = harness(
        json!({"paging": {"style": "cursor", "request": "from", "response": "continuation", "size": "rows"}}),
        vec![five(), template("t6", "a@x.io", 6)],
    );
    get(&h, "/templates").await;
    create(&h, "/templates", template("t6", "a@x.io", 6)).await;
    let mut seen = Vec::new();
    let mut uri = "/templates?rows=2".to_string();
    for _ in 0..10 {
        let (_, body) = get(&h, &uri).await;
        seen.extend(ids(&body, "templates"));
        match body["continuation"].as_str().filter(|t| !t.is_empty()) {
            Some(token) => uri = format!("/templates?rows=2&from={token}"),
            None => break,
        }
    }
    assert_eq!(seen, ["t1", "t2", "t3", "t4", "t5", "t6"]);
    assert_eq!(h.provider.requests().len(), 2);

    // By a page number.
    let h = harness(page_paging(), vec![five(), template("t6", "a@x.io", 6)]);
    get(&h, "/templates").await;
    create(&h, "/templates", template("t6", "a@x.io", 6)).await;
    let mut seen = Vec::new();
    for sheet in 1..=4 {
        let (source, body) = get(&h, &format!("/templates?rows=4&sheet={sheet}")).await;
        assert_eq!(source, "state", "{body}");
        seen.extend(ids(&body, "templates"));
    }
    assert_eq!(seen, ["t1", "t2", "t3", "t4", "t5", "t6"]);
    assert_eq!(h.provider.requests().len(), 2);
}

#[tokio::test]
async fn a_list_page_whose_envelope_cannot_be_filled_is_generated() {
    let strict = |id: &str| json!({"templates": [template(id, "a@x.io", 1)], "tally": 7});
    let ordered = json!({"order": {"member": "amount"}});

    // `tally` is required and its schema fixes it (a `default`): the envelope is
    // filled as a held answer's is, and the second list comes from state.
    let h = support::described(
        CONTRACT,
        json!({"operations": {"listStrictFixed": ordered.clone()}}),
        vec![strict("t1")],
    );
    let (first, _) = get(&h, "/strict2").await;
    assert_ne!(first, "state");
    let (second, body) = get(&h, "/strict2").await;
    assert_eq!(second, "state", "{body}");
    assert_eq!(body["tally"], json!(0), "{body}");
    assert_eq!(ids(&body, "templates"), ["t1"]);
    assert_eq!(h.provider.requests().len(), 1);

    // The same list, but nothing fixes `tally`'s value: no answer from state.
    let h = support::described(
        CONTRACT,
        json!({"operations": {"listStrict": ordered}}),
        vec![strict("t1"), strict("t1")],
    );
    let (first, _) = get(&h, "/strict").await;
    assert_ne!(first, "state");
    let (second, body) = get(&h, "/strict").await;
    assert_ne!(second, "state", "{body}");
    assert_eq!(h.provider.requests().len(), 2);
}

#[tokio::test]
async fn an_undescribed_list_is_generated_as_today() {
    let h = support::described(
        CONTRACT,
        json!({}),
        vec![
            list(vec![template("t1", "a@x.io", 1)]),
            list(vec![template("t1", "a@x.io", 1)]),
        ],
    );
    let (first, _) = get(&h, "/templates").await;
    let (second, _) = get(&h, "/templates").await;
    assert_ne!(first, "state");
    assert_ne!(second, "state");
    assert_eq!(h.provider.requests().len(), 2);
}

#[tokio::test]
async fn a_list_from_state_with_only_paging_keeps_name_matched_filters() {
    let h = harness(
        page_paging(),
        vec![list(vec![
            template("t1", "a@x.io", 1),
            template("t2", "b@x.io", 2),
            template("t3", "a@x.io", 3),
        ])],
    );
    get(&h, "/templates").await;
    let (source, body) = get(&h, "/templates?email=a@x.io").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(ids(&body, "templates"), ["t1", "t3"]);
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn a_list_from_state_carries_the_ops_world_gap() {
    let gaps = HashMap::from([("listTemplates".to_string(), vec![GapCode::NoTarget])]);
    let h = support::described_with_gaps(
        CONTRACT,
        world(page_paging()),
        vec![list(vec![template("t1", "a@x.io", 1)])],
        gaps,
    );
    get(&h, "/templates").await;
    let (source, _) = get(&h, "/templates").await;
    assert_eq!(source, "state");
    let records = h.records();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[0].request.get("world_gap"),
        Some(&json!(["no_target"]))
    );
    assert_eq!(
        records[1].request.get("world_gap"),
        records[0].request.get("world_gap")
    );
}

/// The router of a runtime of `contract` over `store` and `provider`, serving `world`
/// at `revision`, as a rebuild after a world revision builds it.
fn on_store(
    contract: &str,
    store: &Arc<dyn StateStore>,
    provider: &Arc<ScriptedProvider>,
    world: Value,
    revision: i64,
) -> axum::Router {
    use mb_engine::{Engine, EngineConfig};
    let rt = MockRuntime::new(
        "described",
        Arc::new(mb_import::compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(store),
    );
    let (base, effective, refused) = support::effective_world(rt.bundle(), world);
    assert!(refused.is_empty(), "{refused:?}");
    mb_protocols::http_rest::router(Arc::new(rt.with_world(&base, &effective, revision)))
}

fn scripted(script: Vec<Value>) -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ))
}

fn memory() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new(Duration::from_secs(3600)))
}

/// The ids of the records of type `t` the default session holds.
async fn held(store: &Arc<dyn StateStore>, t: &str) -> HashSet<String> {
    let session = mb_state::SessionKey {
        workspace: "default".into(),
        mock: "described".into(),
        session: "default".into(),
    };
    store
        .list(&session, t, 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
}

#[tokio::test]
async fn a_world_revision_mid_session_keeps_lists_from_state() {
    let store = memory();
    let provider = scripted(vec![
        list(vec![
            template("t1", "a@x.io", 1),
            template("t2", "b@x.io", 2),
        ]),
        template("t3", "c@x.io", 3),
        list(vec![template("m1", "m@x.io", 9)]),
    ]);
    let before = on_store(CONTRACT, &store, &provider, world(page_paging()), 1);
    get(&before, "/templates").await;
    create(&before, "/templates", template("t3", "c@x.io", 3)).await;
    let types = held(&store, "templates").await;
    assert_eq!(types.len(), 3, "{types:?}");
    // A new revision: the runtime is rebuilt, its in-memory indexes empty.
    let mut revised = page_paging();
    revised["order"] = json!({"member": "amount"});
    let after = on_store(CONTRACT, &store, &provider, world(revised), 2);
    let (source, body) = get(&after, "/templates").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(ids(&body, "templates"), ["t1", "t2", "t3"]);
    // Nothing was seeded again.
    assert_eq!(held(&store, "templates").await, types);
    assert_eq!(provider.requests().len(), 2);
}

fn note(id: &str, parent: Option<&str>) -> Value {
    match parent {
        Some(p) => json!({"id": id, "title": id, "folderId": p}),
        None => json!({"id": id, "title": id}),
    }
}

#[tokio::test]
async fn a_nested_list_keeps_its_parents_children_across_a_rebuild() {
    let variants = [("", false), ("\n        folderId: {type: string}", true)];
    for (parent_member, named) in variants {
        let contract = NOTES.replace("{parent}", parent_member);
        let parent = |p: &'static str| named.then_some(p);
        let store = memory();
        let provider = scripted(vec![
            note("c1", parent("p1")),
            note("c2", parent("p1")),
            note("c3", parent("p2")),
            note("c4", parent("p1")),
        ]);
        let desc = json!({"operations": {"listNotes": {"order": {"member": "title"}}}});
        let before = on_store(&contract, &store, &provider, desc.clone(), 1);
        create(&before, "/folders/p1/notes", note("c1", parent("p1"))).await;
        create(&before, "/folders/p1/notes", note("c2", parent("p1"))).await;
        create(&before, "/folders/p2/notes", note("c3", parent("p2"))).await;
        // Listed from state, which marks each collection listed.
        get(&before, "/folders/p1/notes").await;
        get(&before, "/folders/p2/notes").await;
        create(&before, "/folders/p1/notes", note("c4", parent("p1"))).await;
        // A rebuild: the runtime's collections are empty.
        let after = on_store(&contract, &store, &provider, desc, 2);
        let (source, body) = get(&after, "/folders/p1/notes").await;
        assert_eq!(source, "state", "{named}: {body}");
        assert_eq!(ids(&body, "notes"), ["c1", "c2", "c4"], "{named}");
        let (source, body) = get(&after, "/folders/p2/notes").await;
        assert_eq!(source, "state", "{named}: {body}");
        assert_eq!(ids(&body, "notes"), ["c3"], "{named}");
        assert_eq!(provider.requests().len(), 4, "{named}");
    }
}

#[tokio::test]
async fn a_collection_past_the_cap_is_generated() {
    // The cap is the runtime's `MAX_SCAN`: 500 records.
    let many = list(
        (1..=500)
            .map(|i| template(&format!("t{i}"), "a@x.io", i))
            .collect(),
    );
    let h = harness(
        page_paging(),
        vec![
            many,
            template("t501", "a@x.io", 501),
            list(vec![template("t1", "a@x.io", 1)]),
        ],
    );
    get(&h, "/templates").await;
    let (source, body) = get(&h, "/templates").await;
    assert_eq!(source, "state");
    assert_eq!(ids(&body, "templates").len(), 500);
    create(&h, "/templates", template("t501", "a@x.io", 501)).await;
    let (source, _) = get(&h, "/templates").await;
    assert_ne!(source, "state");
    assert_eq!(h.provider.requests().len(), 3);
}

#[tokio::test]
async fn lists_after_an_identity_change_still_hold_earlier_records() {
    let store = memory();
    let provider = scripted(vec![
        list(vec![
            template("t1", "a@x.io", 1),
            template("t2", "b@x.io", 2),
        ]),
        template("t3", "c@x.io", 3),
    ]);
    let before = on_store(CONTRACT, &store, &provider, world(page_paging()), 1);
    get(&before, "/templates").await;
    let revised = json!({
        "types": {"templates": {"identity": {"member": "email"}}},
        "operations": {"listTemplates": page_paging()},
    });
    let after = on_store(CONTRACT, &store, &provider, revised, 2);
    create(&after, "/templates", template("t3", "c@x.io", 3)).await;
    let (source, body) = get(&after, "/templates").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(ids(&body, "templates"), ["t1", "t2", "t3"]);
    assert_eq!(provider.requests().len(), 2);
}

/// Templates listed in an envelope that counts them, in an optional member.
const COUNTED: &str = r##"
openapi: 3.0.0
info: {title: Counted, version: "1"}
paths:
  /templates:
    get:
      operationId: listTemplates
      parameters:
        - {name: Offset, in: query, schema: {type: integer, minimum: 0}}
        - {name: Count, in: query, schema: {type: integer, minimum: 1}}
        - {name: from, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  TotalCount: {type: integer}
                  Templates: {type: array, items: {$ref: "#/components/schemas/Template"}}
                  continuation: {type: string}
  /templates/{templateId}:
    get:
      operationId: getTemplate
      parameters: [{name: templateId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}}
components:
  schemas:
    Template:
      type: object
      required: [id]
      properties:
        id: {type: string}
        email: {type: string, format: email}
"##;

fn counted_three() -> Value {
    json!({
        "TotalCount": 3,
        "Templates": [
            {"id": "t1", "email": "a@x.io"},
            {"id": "t2", "email": "b@x.io"},
            {"id": "t3", "email": "c@x.io"},
        ],
    })
}

/// A page served from state counts the list as the generated first page did: every
/// item the query keeps, before the page is cut.
#[tokio::test]
async fn a_page_from_state_carries_the_list_count() {
    let h = support::described(
        COUNTED,
        world(json!({"paging": {"style": "offset", "request": "Offset", "size": "Count"}})),
        vec![counted_three()],
    );
    let (first, body) = get(&h, "/templates").await;
    assert_ne!(first, "state");
    assert_eq!(body["TotalCount"], json!(3), "{body}");
    let (second, body) = get(&h, "/templates?Count=1&Offset=1").await;
    assert_eq!(second, "state", "{body}");
    assert_eq!(ids(&body, "Templates"), ["t2"]);
    assert_eq!(body["TotalCount"], json!(3), "{body}");
    assert_eq!(h.provider.requests().len(), 1);
}

/// A cursor-paged list from state counts the list on its first page and on each page
/// its token serves.
#[tokio::test]
async fn a_cursor_paged_list_from_state_keeps_its_count() {
    let h = support::described(
        COUNTED,
        world(
            json!({"paging": {"style": "cursor", "request": "from", "response": "continuation", "size": "Count"}}),
        ),
        vec![counted_three()],
    );
    get(&h, "/templates").await;
    let mut seen = Vec::new();
    let mut uri = "/templates?Count=2".to_string();
    for _ in 0..5 {
        let (source, body) = get(&h, &uri).await;
        assert_eq!(source, "state", "{body}");
        assert_eq!(body["TotalCount"], json!(3), "{body}");
        seen.extend(ids(&body, "Templates"));
        match body["continuation"].as_str().filter(|t| !t.is_empty()) {
            Some(token) => uri = format!("/templates?Count=2&from={token}"),
            None => break,
        }
    }
    assert_eq!(seen, ["t1", "t2", "t3"]);
    assert_eq!(h.provider.requests().len(), 1);
}

/// Templates listed in an envelope that requires its next-page member, which declares
/// an example; `{member}` is replaced by that member's name and schema.
const REQUIRED_NEXT: &str = r##"
openapi: 3.1.0
info: {title: Required next, version: "1"}
paths:
  /templates:
    get:
      operationId: listTemplates
      parameters:
        - {name: from, in: query, schema: {type: string}}
        - {name: rows, in: query, schema: {type: integer, minimum: 1}}
        - {name: sheet, in: query, schema: {type: integer, minimum: 1}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [templates, {name}]
                properties:
                  templates: {type: array, items: {$ref: "#/components/schemas/Template"}}
                  {name}: {member}
  /templates/{templateId}:
    get:
      operationId: getTemplate
      parameters: [{name: templateId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Template"}}}}}
components:
  schemas:
    Template:
      type: object
      required: [id]
      properties:
        id: {type: string}
        email: {type: string, format: email}
"##;

fn required_next(name: &str, member: &str) -> String {
    REQUIRED_NEXT
        .replace("{name}", name)
        .replace("{member}", member)
}

/// The three templates the session holds.
fn three() -> Vec<Value> {
    vec![
        template("t1", "a@x.io", 1),
        template("t2", "b@x.io", 2),
        template("t3", "c@x.io", 3),
    ]
}

fn cursor_paging() -> Value {
    json!({"paging": {"style": "cursor", "request": "from", "response": "continuation", "size": "rows"}})
}

/// A page from state with no next position never carries the next-page member's
/// declared example: a required cursor is the empty text, or `null` where it may be.
#[tokio::test]
async fn a_last_page_from_state_carries_no_example_cursor() {
    for (member, end) in [
        (r#"{type: string, examples: ["c-example"]}"#, json!("")),
        (
            r#"{type: [string, "null"], examples: ["c-example"]}"#,
            Value::Null,
        ),
    ] {
        let h = support::described(
            &required_next("continuation", member),
            world(cursor_paging()),
            Vec::new(),
        );
        h.hold("templates", &three()).await;
        // The whole list, uncut: no next page.
        let (source, body) = get(&h, "/templates").await;
        assert_eq!(source, "state", "{member}: {body}");
        assert_eq!(ids(&body, "templates"), ["t1", "t2", "t3"]);
        assert_eq!(body["continuation"], end, "{member}: {body}");
        assert_eq!(h.provider.requests().len(), 0, "{member}");
    }
}

/// A page from state that is cut carries a token that reaches the next page, and the
/// last page ends the list.
#[tokio::test]
async fn a_middle_page_from_state_carries_a_token_to_the_next() {
    let h = support::described(
        &required_next("continuation", r#"{type: string, examples: ["c-example"]}"#),
        world(cursor_paging()),
        Vec::new(),
    );
    h.hold("templates", &three()).await;
    let (source, body) = get(&h, "/templates?rows=2").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(ids(&body, "templates"), ["t1", "t2"]);
    let token = body["continuation"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!token.is_empty() && token != "c-example", "{body}");
    let (_, body) = get(&h, &format!("/templates?rows=2&from={token}")).await;
    assert_eq!(ids(&body, "templates"), ["t3"], "{body}");
    assert_ne!(body["continuation"], json!("c-example"), "{body}");
    assert_eq!(h.provider.requests().len(), 0);
}

/// The last numbered page from state: a required next page number is `null` where it
/// may be; where no value says "no more", the example stays and the answer is marked.
#[tokio::test]
async fn a_last_numbered_page_from_state_carries_no_example_page() {
    let paging = json!({"paging": {"style": "page", "request": "sheet", "size": "rows", "response": "nextSheet"}});
    let h = support::described(
        &required_next("nextSheet", r#"{type: [integer, "null"], examples: [2]}"#),
        world(paging.clone()),
        Vec::new(),
    );
    h.hold("templates", &three()).await;
    let (source, body) = get(&h, "/templates?rows=2&sheet=1").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(body["nextSheet"], json!(2), "{body}");
    let (source, body) = get(&h, "/templates?rows=2&sheet=2").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(ids(&body, "templates"), ["t3"]);
    assert_eq!(body["nextSheet"], Value::Null, "{body}");
    assert!(
        h.records()
            .iter()
            .all(|r| r.request.get("world_gap").is_none()),
        "no gap is marked"
    );

    // No `null`, nor any other end: the answer is as before, and marked.
    let h = support::described(
        &required_next("nextSheet", "{type: integer, examples: [2]}"),
        world(paging),
        Vec::new(),
    );
    h.hold("templates", &three()).await;
    let (source, body) = get(&h, "/templates?rows=2&sheet=2").await;
    assert_eq!(source, "state", "{body}");
    assert_eq!(body["nextSheet"], json!(2), "{body}");
    let records = h.records();
    assert_eq!(
        records.last().and_then(|r| r.request.get("world_gap")),
        Some(&json!(["no_page_end"])),
    );
}
