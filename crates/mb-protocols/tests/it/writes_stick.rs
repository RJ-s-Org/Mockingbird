//! Writes that stick: an update's answer, and a delete. Driven over HTTP with a
//! scripted LLM, asserting what is served and how many calls it took.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, http_rest};
use mb_state::{CachingStateStore, MemoryStore, SessionKey, StateStore};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<MemoryStore>,
}

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "default".into(),
    }
}

fn harness_with(
    contract: &[u8],
    script: Vec<Value>,
    change: impl FnOnce(&mut MockRuntime),
) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract).unwrap()),
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
    change(&mut rt);
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

/// [`harness_with`], with an output budget.
fn budgeted(contract: &[u8], script: Vec<Value>, output_budget: u32) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig {
                output_budget,
                ..EngineConfig::default()
            },
        )),
        CachingStateStore::wrap(
            Arc::clone(&store) as Arc<dyn StateStore>,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
    );
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
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, source, body)
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }
}

fn hubspot_contract() -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/apis/hubspot/hubspot.openapi.yaml"
    );
    std::fs::read(path).unwrap()
}

/// One resource that answers its reads on every update but one, in all the ways its
/// members can differ: what a client may not set, what only the path or an owner
/// says, an array, an object, a union, and nulls.
const SETTLING: &str = r##"
openapi: 3.0.0
info: {title: Settling, version: "1"}
paths:
  /widgets/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getWidget
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
    patch:
      operationId: patchWidget
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
    put:
      operationId: putWidget
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
  /gadgets/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getGadget
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Gadget"}}}}
    patch:
      operationId: patchGadget
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}
  /orgs/{org}/teams/{team_slug}:
    parameters:
      - {name: org, in: path, required: true, schema: {type: string}}
      - {name: team_slug, in: path, required: true, schema: {type: string}}
    get:
      operationId: getTeam
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Team"}}}}
    patch:
      operationId: patchTeam
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Team"}}}}
components:
  schemas:
    Widget:
      type: object
      required: [id, name, status, lines]
      properties:
        id: {type: string}
        name: {type: string}
        status: {type: string, enum: [open, closed]}
        created_at: {type: string}
        closed_at: {type: string}
        completed_at: {type: string, readOnly: true}
        revision: {type: integer, readOnly: true}
        owner_id: {type: string}
        owner:
          type: object
          required: [login, profile]
          properties:
            id: {type: string}
            name: {type: string}
            status: {type: string}
            login: {type: string}
            profile:
              type: object
              required: [bio]
              properties: {bio: {type: string}, name: {type: string}}
        shipping:
          type: object
          properties: {city: {type: string}}
        labels:
          type: object
          additionalProperties: {type: string}
        lines:
          type: array
          items:
            type: object
            required: [sku, qty]
            properties: {sku: {type: string}, qty: {type: integer}}
        meta:
          type: object
          properties:
            a: {type: string}
            b: {type: string}
            status: {type: string, enum: [red, gray, blue, teal]}
            due: {type: object, nullable: true, properties: {date: {type: string, nullable: true}}}
        address:
          oneOf:
            - type: object
              additionalProperties: false
              required: [kind, city]
              properties: {kind: {type: string, enum: [home]}, city: {type: string}}
            - type: object
              additionalProperties: false
              required: [kind, box]
              properties: {kind: {type: string, enum: [po]}, box: {type: integer}}
    Gadget:
      type: object
      required: [id, status, count]
      properties:
        id: {type: string}
        status: {type: string}
        count: {type: integer}
    Ack:
      type: object
      required: [status, count]
      properties:
        status: {type: string}
        message: {type: string}
        count: {type: integer}
    Team:
      type: object
      required: [id, slug, organization, html_url, description]
      properties:
        id: {type: integer}
        slug: {type: string}
        organization: {type: string}
        html_url: {type: string}
        description: {type: string}
"##;

const WIDGET: &str = "/widgets/w1";

fn widget() -> Value {
    json!({
        "id": "w1",
        "name": "before",
        "status": "open",
        "created_at": "2020-01-01",
        "revision": 3,
        "owner_id": "u1",
        "owner": {
            "id": "u1",
            "name": "Dana",
            "status": "vip",
            "login": "ana",
            "profile": {"bio": "real", "name": "Dana R"},
        },
        "shipping": {"city": "Lisbon"},
        "labels": {"kept": "yes"},
        "lines": [
            {"sku": "a", "qty": 1}, {"sku": "b", "qty": 2}, {"sku": "c", "qty": 3},
            {"sku": "d", "qty": 4}, {"sku": "e", "qty": 5}, {"sku": "f", "qty": 6},
        ],
        "meta": {"a": "kept", "b": "kept", "status": "red", "due": {"date": "2026-12-14"}},
        "address": {"kind": "po", "box": 7},
    })
}

/// What `request` leaves served and stored on [`widget`], when the model answers
/// [`widget`] changed by `change`.
async fn widget_after(request: Value, change: impl FnOnce(&mut Value)) -> Value {
    let mut answer = widget();
    change(&mut answer);
    let h = harness_with(SETTLING.as_bytes(), vec![widget(), answer], |_| {});
    let (status, _, _) = h.call("GET", WIDGET, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, updated) = h.call("PATCH", WIDGET, Some(request)).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let (_, source, read) = h.call("GET", WIDGET, None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read, updated);
    read
}

/// Defects 1 and 2: under a tight budget the model writes part of the answer and a
/// faker the rest, with arrays capped: neither reaches what is stored.
#[tokio::test]
async fn a_budget_neither_fills_over_nor_truncates_what_is_stored() {
    let h = budgeted(
        SETTLING.as_bytes(),
        vec![
            widget(),
            json!({"id": "w1", "name": "after", "status": "open", "lines": [{"sku": "a", "qty": 1}]}),
        ],
        100,
    );
    h.call("GET", WIDGET, None).await;
    let (status, _, updated) = h
        .call("PATCH", WIDGET, Some(json!({"name": "after"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["name"], "after", "{updated}");
    assert_eq!(updated["owner"]["profile"]["bio"], "real", "{updated}");
    assert_eq!(updated["lines"], widget()["lines"], "{updated}");
}

/// Defect 2: an array the request did not name stays whole, whatever the model wrote.
#[tokio::test]
async fn an_unnamed_array_keeps_every_item() {
    for lines in [json!([]), json!([{"sku": "z", "qty": 9}])] {
        let read = widget_after(json!({"name": "after"}), |w| w["lines"] = lines.clone()).await;
        assert_eq!(read["lines"], widget()["lines"], "{read}");
    }
}

/// An array the request named is replaced, by the request where it says, and by the
/// model's answer where the request nests it elsewhere.
#[tokio::test]
async fn a_named_array_is_replaced() {
    let lines = json!([{"sku": "z", "qty": 9}]);
    let read = widget_after(json!({"lines": lines}), |_| {}).await;
    assert_eq!(read["lines"], lines, "{read}");
    let read = widget_after(json!({"order": {"lines": lines}}), |w| {
        w["lines"] = lines.clone();
    })
    .await;
    assert_eq!(read["lines"], lines, "{read}");
}

/// Defect 3: identity, `readOnly`, owner and untouched members do not drift.
#[tokio::test]
async fn what_a_request_leaves_alone_does_not_drift() {
    let read = widget_after(json!({"name": "after"}), |w| {
        w["id"] = json!("w2");
        w["name"] = json!("after");
        w["created_at"] = json!("1999-09-09");
        w["revision"] = json!(99);
        w["owner_id"] = json!("u2");
        w["owner"]["login"] = json!("mallory");
    })
    .await;
    let mut want = widget();
    want["name"] = json!("after");
    assert_eq!(read, want);
}

/// A member the contract marks `readOnly` does not take the model's value even when
/// its key is named elsewhere in the request.
#[tokio::test]
async fn a_named_read_only_member_keeps_its_stored_value() {
    let read = widget_after(json!({"audit": {"revision": 99}}), |w| {
        w["revision"] = json!(99);
    })
    .await;
    assert_eq!(read["revision"], 3, "{read}");
}

/// What the stored copy lacks comes from the answer at the top level, as on main.
/// Below it a model's invention, a map's new key and a faker's value do not stick.
#[tokio::test]
async fn only_the_top_level_is_filled_from_the_answer() {
    let read = widget_after(json!({"name": "after"}), |w| {
        w["closed_at"] = json!("2026-01-01");
        w["meta"]["c"] = json!("invented");
        w["labels"]["invented"] = json!("yes");
    })
    .await;
    assert_eq!(read["closed_at"], "2026-01-01", "{read}");
    assert_eq!(read["meta"], widget()["meta"], "{read}");
    assert_eq!(read["labels"], widget()["labels"], "{read}");
}

/// A member below the top level the request names does go in, though not stored.
#[tokio::test]
async fn a_named_member_stored_lacks_goes_in() {
    let read = widget_after(json!({"meta": {"c": "new"}}), |w| {
        w["meta"]["c"] = json!("new");
    })
    .await;
    assert_eq!(read["meta"]["c"], "new", "{read}");
    assert_eq!(read["meta"]["a"], "kept", "{read}");
    let read = widget_after(json!({"labels": {"added": "yes"}}), |w| {
        w["labels"]["added"] = json!("yes");
        w["labels"]["invented"] = json!("yes");
    })
    .await;
    assert_eq!(read["labels"]["added"], "yes", "{read}");
    assert!(read["labels"].get("invented").is_none(), "{read}");
}

/// The same key at another path is not named, in either direction: what is stored
/// stands, as on main.
#[tokio::test]
async fn a_key_at_another_path_is_not_named() {
    let read = widget_after(json!({"name": "after"}), |w| {
        w["owner"]["name"] = json!("DRIFT");
        w["owner"]["profile"]["name"] = json!("DRIFT");
    })
    .await;
    assert_eq!(read["owner"]["name"], "Dana", "{read}");
    assert_eq!(read["owner"]["profile"]["name"], "Dana R", "{read}");

    let read = widget_after(json!({"status": "closed"}), |w| {
        w["owner"]["status"] = json!("DRIFT-banned");
    })
    .await;
    assert_eq!(read["owner"]["status"], "vip", "{read}");

    let read = widget_after(json!({"id": "w1", "name": "after"}), |w| {
        w["owner"]["id"] = json!("DRIFT-u9");
    })
    .await;
    assert_eq!(read["owner"]["id"], "u1", "{read}");

    let read = widget_after(json!({"owner": {"name": "Dana"}}), |w| {
        w["name"] = json!("DRIFT-top");
    })
    .await;
    assert_eq!(read["name"], "before", "{read}");
}

/// A faker's value for an invalid member at another path of a named key does not stick.
#[tokio::test]
async fn a_patched_member_at_another_path_is_not_named() {
    let mut answer = widget();
    answer["meta"]["status"] = json!("no such status");
    let h = harness_with(SETTLING.as_bytes(), vec![widget(), answer], |_| {});
    h.call("GET", WIDGET, None).await;
    let (status, source, updated) = h
        .call("PATCH", WIDGET, Some(json!({"status": "closed"})))
        .await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "patched"),
        "{updated}"
    );
    assert_eq!(updated["meta"]["status"], "red", "{updated}");
    assert_eq!(updated["status"], "closed", "{updated}");
}

/// Under a budget, a named key keeps a faker from every other path of it.
#[tokio::test]
async fn a_budget_fill_at_another_path_of_a_named_key_does_not_stick() {
    let h = budgeted(
        SETTLING.as_bytes(),
        vec![
            widget(),
            json!({"id": "w1", "name": "after", "status": "open", "lines": [{"sku": "a", "qty": 1}]}),
        ],
        100,
    );
    h.call("GET", WIDGET, None).await;
    let (status, _, updated) = h
        .call("PATCH", WIDGET, Some(json!({"name": "after"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["owner"]["profile"]["name"], "Dana R", "{updated}");
    assert_eq!(updated["owner"]["name"], "Dana", "{updated}");
}

/// `{shipping: {lines}}` on a response that declares `shipping` names `shipping.lines`,
/// not `lines`: a cap or an empty answer there leaves the array.
#[tokio::test]
async fn an_array_named_at_another_path_stays() {
    let request = json!({"shipping": {"lines": [{"sku": "z", "qty": 9}]}});
    let read = widget_after(request.clone(), |w| w["lines"] = json!([])).await;
    assert_eq!(read["lines"], widget()["lines"], "{read}");

    let h = budgeted(
        SETTLING.as_bytes(),
        vec![
            widget(),
            json!({"id": "w1", "name": "before", "status": "open", "lines": [{"sku": "a", "qty": 1}]}),
        ],
        100,
    );
    h.call("GET", WIDGET, None).await;
    let (status, _, updated) = h.call("PATCH", WIDGET, Some(request)).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["lines"], widget()["lines"], "{updated}");
}

/// An array named at its own path takes the request's value, not the answer's, which a
/// budget or a model could have cut, under a wrapper the response does not declare too.
#[tokio::test]
async fn a_named_array_takes_the_requests_value() {
    let lines = json!([
        {"sku": "x", "qty": 7}, {"sku": "y", "qty": 8}, {"sku": "z", "qty": 9}
    ]);
    for request in [json!({"lines": lines}), json!({"order": {"lines": lines}})] {
        let read = widget_after(request.clone(), |w| w["lines"] = json!([])).await;
        assert_eq!(read["lines"], lines, "{request}: {read}");

        let h = budgeted(
            SETTLING.as_bytes(),
            vec![
                widget(),
                json!({"id": "w1", "name": "before", "status": "open", "lines": [{"sku": "x", "qty": 7}]}),
            ],
            100,
        );
        h.call("GET", WIDGET, None).await;
        let (status, _, updated) = h.call("PATCH", WIDGET, Some(request.clone())).await;
        assert_eq!(status, StatusCode::OK, "{updated}");
        assert_eq!(updated["lines"], lines, "{request}: {updated}");
    }
}

/// A request's only key, undeclared by the response, is a wrapper: what it holds is
/// matched from the top. With a second key it is not a wrapper, and names nothing.
#[tokio::test]
async fn an_undeclared_wrapper_is_stripped_only_when_it_is_the_only_key() {
    let read = widget_after(json!({"ticket": {"status": "closed"}}), |w| {
        w["status"] = json!("closed");
        w["name"] = json!("drifted");
    })
    .await;
    assert_eq!(read["status"], "closed", "{read}");
    assert_eq!(read["name"], "before", "{read}");

    let request = json!({"ticket": {"status": "closed"}, "note": "x"});
    let read = widget_after(request, |w| w["status"] = json!("closed")).await;
    assert_eq!(read["status"], "open", "{read}");
}

/// A resource whose every member the faker has a say in, for telling what the faker
/// wrote from what is stored: a stored value it cannot draw.
const PATCHING: &str = r##"
openapi: 3.0.0
info: {title: Patching, version: "1"}
paths:
  /parts/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getPart
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Part"}}}}
    patch:
      operationId: patchPart
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Part"}}}}
components:
  schemas:
    Part:
      type: object
      required: [id, title]
      properties:
        id: {type: string}
        title: {type: string}
        code: {type: string, maxLength: 10}
        kind: {type: string, format: uuid}
        phase: {type: string, enum: [open, a, b, c, d, e, f, g]}
"##;

/// An answer invalid on `code`, `kind` and `phase`, with `title` as asked.
fn part_answer() -> Value {
    json!({
        "id": "p1",
        "title": "after",
        "code": "way-too-long-to-fit",
        "kind": "not a uuid",
        "phase": "stuck",
    })
}

/// What a `PATCH {title}` leaves after reading `stored`, with its source.
async fn part_after(stored: Value) -> (String, Value) {
    let h = harness_with(PATCHING.as_bytes(), vec![stored, part_answer()], |_| {});
    h.call("GET", "/parts/p1", None).await;
    let (status, source, updated) = h
        .call("PATCH", "/parts/p1", Some(json!({"title": "after"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    (source, updated)
}

/// The stored values are ones the faker cannot draw, so this tells the rules apart: the
/// answer is patched, and all three survive.
#[tokio::test]
async fn patched_values_do_not_replace_what_the_faker_could_not_draw() {
    let uuid = "7f9c24e8-3b12-4fef-91e8-0123456789ab";
    let (source, updated) = part_after(json!({
        "id": "p1", "title": "before", "code": "zzqstored1", "kind": uuid, "phase": "open",
    }))
    .await;
    assert_eq!(source, "patched", "{updated}");
    assert_eq!(updated["code"], "zzqstored1", "{updated}");
    assert_eq!(updated["kind"], uuid, "{updated}");
    assert_eq!(updated["phase"], "open", "{updated}");
    assert_eq!(updated["title"], "after", "{updated}");
}

/// The control: with those members not stored, the same answer's patched values fill
/// them, so the test above is not passing because no patching fired.
#[tokio::test]
async fn patched_values_fill_what_is_not_stored() {
    let uuid = "7f9c24e8-3b12-4fef-91e8-0123456789ab";
    let (source, updated) = part_after(json!({"id": "p1", "title": "before"})).await;
    assert_eq!(source, "patched", "{updated}");
    assert_ne!(updated["code"], "zzqstored1", "{updated}");
    assert_ne!(updated["kind"], uuid, "{updated}");
    assert!(updated.get("phase").is_some(), "{updated}");
}

/// Effects on the own target are held to what the answer is: identity, `readOnly`, a
/// path member and an owner's are left out, and what the request named stands.
#[tokio::test]
async fn an_effect_on_its_own_target_is_held_to_the_same_protections() {
    let mut answer = widget();
    answer["name"] = json!("asked");
    let h = harness_with(
        SETTLING.as_bytes(),
        vec![
            widget(),
            json!({
                "effects": [{"type": "widgets", "id": "w1", "set": {
                    "revision": 99,
                    "completed_at": "2026-01-01",
                    "name": "EFFECT-over-request",
                    "created_at": "2026-11-01",
                }}],
                "response": answer,
            }),
        ],
        |_| {},
    );
    h.call("GET", WIDGET, None).await;
    let (status, _, updated) = h
        .call("PATCH", WIDGET, Some(json!({"name": "asked"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let (_, _, read) = h.call("GET", WIDGET, None).await;
    assert_eq!(read["revision"], 3, "{read}");
    assert!(read.get("completed_at").is_none(), "{read}");
    assert_eq!(read["name"], "asked", "{read}");
    // What none of those rules cover still applies.
    assert_eq!(read["created_at"], "2026-11-01", "{read}");
}

/// An effect does not move a team: its path member or its owner's. The last effect is
/// the control, that one the protections leave does apply.
#[tokio::test]
async fn an_effect_does_not_move_its_own_target() {
    const CORE: &str = "/orgs/octo/teams/core";
    let elsewhere = "https://example.test/orgs/octo/teams/core/elsewhere";
    for (set, moved) in [
        (json!({"slug": "renamed"}), true),
        (
            json!({"organization": "acme", "html_url": "https://example.test/orgs/acme/teams/core"}),
            true,
        ),
        (json!({"html_url": elsewhere}), false),
    ] {
        let mut answer = team("octo", "core");
        answer["description"] = json!("after");
        let h = harness_with(
            SETTLING.as_bytes(),
            vec![
                team("octo", "core"),
                json!({
                    "effects": [{"type": "teams", "id": "1", "set": set}],
                    "response": answer,
                }),
            ],
            |_| {},
        );
        h.call("GET", CORE, None).await;
        h.call("PATCH", CORE, Some(json!({"description": "after"})))
            .await;
        let (_, _, read) = h.call("GET", CORE, None).await;
        assert_eq!(read["slug"], "core", "{read}");
        assert_eq!(read["organization"], "octo", "{read}");
        assert_eq!(read["html_url"] == elsewhere, !moved, "{read}");
    }
}

/// An answer moving a resource whose stored copy has no owner members does not stick.
#[tokio::test]
async fn an_answer_cannot_add_another_owner() {
    const OWNED: &str = r##"
openapi: 3.0.0
info: {title: Owned, version: "1"}
paths:
  /orgs/{org}/teams:
    parameters: [{name: org, in: path, required: true, schema: {type: string}}]
    post:
      operationId: createTeam
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "201": {description: made, content: {application/json: {schema: {$ref: "#/components/schemas/Team"}}}}
  /orgs/{org}/teams/{team_slug}:
    parameters:
      - {name: org, in: path, required: true, schema: {type: string}}
      - {name: team_slug, in: path, required: true, schema: {type: string}}
    get:
      operationId: getTeam
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Team"}}}}
    patch:
      operationId: patchTeam
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Team"}}}}
components:
  schemas:
    Team:
      type: object
      required: [id, slug]
      properties:
        id: {type: integer}
        slug: {type: string}
        description: {type: string}
        organization: {type: string}
        html_url: {type: string}
"##;
    const CORE: &str = "/orgs/octo/teams/core";
    let h = harness_with(
        OWNED.as_bytes(),
        vec![
            json!({"id": 1, "slug": "core", "description": "before"}),
            json!({
                "id": 1,
                "slug": "core",
                "description": "after",
                "organization": "acme",
                "html_url": "https://example.test/orgs/acme/teams/core",
            }),
        ],
        |_| {},
    );
    let (status, _, made) = h
        .call("POST", "/orgs/octo/teams", Some(json!({"slug": "core"})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let (status, _, updated) = h
        .call("PATCH", CORE, Some(json!({"description": "after"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["description"], "after", "{updated}");
    assert!(updated.get("organization").is_none(), "{updated}");
    assert!(updated.get("html_url").is_none(), "{updated}");
    let (_, source, read) = h.call("GET", CORE, None).await;
    assert_eq!(source, "state", "{read}");
    assert!(read.get("organization").is_none(), "{read}");
}

fn team(org: &str, slug: &str) -> Value {
    json!({
        "id": 1,
        "slug": slug,
        "organization": org,
        "html_url": format!("https://example.test/orgs/{org}/teams/{slug}"),
        "description": "before",
    })
}

/// Defect 3, on an owner's route: a slug the model changes does not make the team
/// answer under a second name, and another organization does not take it.
#[tokio::test]
async fn an_update_does_not_move_a_team() {
    let mut renamed = team("octo", "renamed");
    renamed["description"] = json!("after");
    let mut moved = team("acme", "core");
    moved["description"] = json!("after");
    for answer in [renamed, moved] {
        let h = harness_with(
            SETTLING.as_bytes(),
            vec![team("octo", "core"), answer, team("octo", "renamed")],
            |_| {},
        );
        const CORE: &str = "/orgs/octo/teams/core";
        h.call("GET", CORE, None).await;
        let request = json!({"description": "after"});
        let (status, _, updated) = h.call("PATCH", CORE, Some(request)).await;
        assert_eq!(status, StatusCode::OK, "{updated}");
        assert_eq!(updated["description"], "after", "{updated}");
        assert_eq!(updated["slug"], "core", "{updated}");
        assert_eq!(updated["organization"], "octo", "{updated}");
        let (_, source, read) = h.call("GET", CORE, None).await;
        assert_eq!(source, "state", "{read}");
        assert_eq!(read["organization"], "octo", "{read}");
        let (_, source, _) = h.call("GET", "/orgs/octo/teams/renamed", None).await;
        assert_ne!(source, "state");
    }
}

fn gadget() -> Value {
    json!({"id": "g1", "status": "active", "count": 12})
}

/// Defect 4: an answer that is not the resource's says nothing of it.
#[tokio::test]
async fn an_answer_of_another_schema_leaves_the_resource() {
    let h = harness_with(
        SETTLING.as_bytes(),
        vec![
            gadget(),
            json!({"status": "ok", "message": "done", "count": 1}),
        ],
        |_| {},
    );
    h.call("GET", "/gadgets/g1", None).await;
    let (status, _, _) = h
        .call("PATCH", "/gadgets/g1", Some(json!({"label": "x"})))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, source, read) = h.call("GET", "/gadgets/g1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["status"], "active", "{read}");
    assert_eq!(read["count"], 12, "{read}");
}

/// Defect 5: a request switching a union's branch applies, though merged into the
/// stored branch it fits neither, whether the model's answer kept up or the faker's.
#[tokio::test]
async fn a_request_switching_a_union_branch_applies() {
    let request = json!({"address": {"kind": "home", "city": "X"}});
    let read = widget_after(request.clone(), |_| {}).await;
    assert_eq!(read["address"], request["address"], "{read}");

    let h = harness_with(SETTLING.as_bytes(), vec![widget()], |_| {});
    h.call("GET", WIDGET, None).await;
    let (status, source, updated) = h.call("PATCH", WIDGET, Some(request.clone())).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "fallback"),
        "{updated}"
    );
    assert_eq!(updated["address"], request["address"], "{updated}");
}

/// A deep merge keeps what a nested request leaves out, and a request naming part of
/// a member is told apart from one naming none of it.
#[tokio::test]
async fn a_nested_request_keeps_its_siblings() {
    let read = widget_after(json!({"meta": {"a": "new"}}), |w| {
        w["meta"]["a"] = json!("new")
    })
    .await;
    assert_eq!(read["meta"]["a"], "new", "{read}");
    assert_eq!(read["meta"]["b"], "kept", "{read}");
}

/// Defect 7: an effect on the call's own target applies where the merged result
/// validates, with its undeclared members left out.
#[tokio::test]
async fn an_effect_on_its_own_target_applies_when_it_fits() {
    let h = harness_with(
        SETTLING.as_bytes(),
        vec![
            gadget(),
            json!({
                "effects": [{"type": "gadgets", "id": "g1", "set": {"count": 13, "junk": 1}}],
                "response": {"status": "ok", "count": 1},
            }),
        ],
        |_| {},
    );
    h.call("GET", "/gadgets/g1", None).await;
    h.call("PATCH", "/gadgets/g1", Some(json!({"label": "x"})))
        .await;
    let (_, _, read) = h.call("GET", "/gadgets/g1", None).await;
    assert_eq!(read["count"], 13, "{read}");
    assert!(read.get("junk").is_none(), "{read}");
}

/// An effect is how a model states what follows from an update on an answer that
/// settles, such as the `closed_at` of a close.
#[tokio::test]
async fn an_effect_states_a_derived_change() {
    let mut answer = widget();
    answer["status"] = json!("closed");
    let h = harness_with(
        SETTLING.as_bytes(),
        vec![
            widget(),
            json!({
                "effects": [{"type": "widgets", "id": "w1", "set": {"closed_at": "2026-11-01"}}],
                "response": answer,
            }),
        ],
        |_| {},
    );
    h.call("GET", WIDGET, None).await;
    let (status, _, updated) = h
        .call("PATCH", WIDGET, Some(json!({"status": "closed"})))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let (_, _, read) = h.call("GET", WIDGET, None).await;
    assert_eq!(read["closed_at"], "2026-11-01", "{read}");
    assert_eq!(read["status"], "closed", "{read}");
}

/// An effect that would break the resource is rejected whole.
#[tokio::test]
async fn an_effect_breaking_its_own_target_does_not_apply() {
    let h = harness_with(
        SETTLING.as_bytes(),
        vec![
            gadget(),
            json!({
                "effects": [{"type": "gadgets", "id": "g1", "set": {"count": "many"}}],
                "response": {"status": "ok", "count": 1},
            }),
        ],
        |_| {},
    );
    h.call("GET", "/gadgets/g1", None).await;
    h.call("PATCH", "/gadgets/g1", Some(json!({"label": "x"})))
        .await;
    let (_, _, read) = h.call("GET", "/gadgets/g1", None).await;
    assert_eq!(read["count"], 12, "{read}");
}

/// The Lightning case: a model nulls what it was not asked about, at any depth, and
/// the stored value stands.
#[tokio::test]
async fn a_models_null_does_not_erase_what_is_stored() {
    let read = widget_after(json!({"meta": {"a": "new"}}), |w| {
        w["meta"]["a"] = json!("new");
        w["meta"]["due"] = Value::Null;
    })
    .await;
    assert_eq!(read["meta"]["due"]["date"], "2026-12-14", "{read}");
    let read = widget_after(json!({"meta": {"a": "new"}}), |w| {
        w["meta"]["due"]["date"] = Value::Null;
    })
    .await;
    assert_eq!(read["meta"]["due"]["date"], "2026-12-14", "{read}");
}

/// A request naming a member as null clears it, whichever way the answer put it.
#[tokio::test]
async fn a_request_null_clears_a_nullable_member() {
    for answer in [Value::Null, json!({"date": "2026-12-14"})] {
        let read = widget_after(json!({"meta": {"due": null}}), |w| {
            w["meta"]["due"] = answer.clone();
        })
        .await;
        assert!(read["meta"]["due"].is_null(), "{read}");
        assert_eq!(read["meta"]["a"], "kept", "{read}");
    }
}

/// A request's null on a member that cannot be null keeps it, at any depth, and the
/// model's answer leaving it out changes nothing.
#[tokio::test]
async fn a_request_null_on_a_non_nullable_member_keeps_it() {
    let read = widget_after(json!({"status": null}), |w| {
        w.as_object_mut().unwrap().remove("created_at");
    })
    .await;
    assert_eq!(read["status"], "open", "{read}");
    let read = widget_after(json!({"meta": {"b": null}}), |w| {
        w["meta"].as_object_mut().unwrap().remove("b");
    })
    .await;
    assert_eq!(read["meta"]["b"], "kept", "{read}");
}

/// A Zendesk-style request nests the update under a member the answer does not: the
/// key the request names still says which of the answer's members to take.
#[tokio::test]
async fn a_request_nested_otherwise_names_the_answers_member() {
    let read = widget_after(json!({"ticket": {"status": "closed"}}), |w| {
        w["status"] = json!("closed");
        w["name"] = json!("drifted");
    })
    .await;
    assert_eq!(read["status"], "closed", "{read}");
    assert_eq!(read["name"], "before", "{read}");
    assert!(read.get("ticket").is_none(), "{read}");
}

/// HubSpot's nested `properties`: what the request names at depth takes the model's
/// value, and a value it changed unasked stays as stored.
#[tokio::test]
async fn a_hubspot_update_takes_only_what_it_names() {
    let mut read = contact("lead", None);
    read["properties"]["company"] = json!("Globex");
    let mut answer = contact("opportunity", Some("VP Sales"));
    answer["properties"]["company"] = json!("Drifted");
    let h = harness_with(&hubspot_contract(), vec![read, answer], |_| {});
    h.call("GET", CONTACT, None).await;
    let request = json!({"properties": {"lifecyclestage": "opportunity"}});
    let (status, _, updated) = h.call("PATCH", CONTACT, Some(request)).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["properties"]["lifecyclestage"], "opportunity");
    assert_eq!(updated["properties"]["company"], "Globex", "{updated}");
}

/// A branch the request did not name stays whole, though the model changed it.
#[tokio::test]
async fn an_unnamed_union_branch_stays() {
    let read = widget_after(json!({"name": "after"}), |w| {
        w["address"] = json!({"kind": "home", "city": "Elsewhere"});
    })
    .await;
    assert_eq!(read["address"], widget()["address"], "{read}");
}

/// Zendesk's own contract: its `PUT` answers `{ticket, audit}` and its `GET` `{ticket}`,
/// so the answer is not the resource's read and nothing settles: the `PUT` answers what
/// the request named.
#[tokio::test]
async fn a_zendesk_update_answers_what_it_names() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/apis/zendesk/zendesk.openapi.yaml"
    );
    let ticket = |status: &str, subject: &str| {
        json!({
            "id": 3543,
            "url": "https://juniper.zendesk.com/api/v2/tickets/3543.json",
            "subject": subject,
            "raw_subject": subject,
            "description": "x",
            "status": status,
            "priority": null,
            "type": null,
            "requester_id": 360000000001_u64,
            "submitter_id": 360000000001_u64,
            "assignee_id": null,
            "group_id": null,
            "tags": [],
            "via": {"channel": "web"},
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "is_public": true,
        })
    };
    let audit = json!({
        "id": 1,
        "ticket_id": 3543,
        "created_at": "2026-01-01T00:00:00Z",
        "author_id": 1,
        "events": [{"id": 1, "type": "Change"}],
    });
    let h = harness_with(
        &std::fs::read(path).unwrap(),
        vec![
            json!({"ticket": ticket("open", "Before")}),
            json!({"ticket": ticket("solved", "Drifted"), "audit": audit}),
        ],
        |_| {},
    );
    const TICKET: &str = "/api/v2/tickets/3543";
    let (status, source, first) = h.call("GET", TICKET, None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "llm"),
        "{first}"
    );
    let request = json!({"ticket": {"status": "solved"}});
    let (status, source, updated) = h.call("PUT", TICKET, Some(request)).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "llm"),
        "{updated}"
    );
    assert_eq!(updated["ticket"]["status"], "solved", "{updated}");
}

/// Without a provider an update is what it always was: the stored copy, then each
/// member of the request whole.
#[tokio::test]
async fn an_offline_update_overlays_the_request_whole() {
    let provider = None::<Arc<dyn Provider>>;
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(SETTLING.as_bytes()).unwrap()),
        Arc::new(Engine::new(provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let h = Harness {
        router: http_rest::router(Arc::new(rt)),
        provider: Arc::new(ScriptedProvider::new(vec![])),
        store: Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    };
    h.call("GET", WIDGET, None).await;
    let (status, _, updated) = h
        .call("PATCH", WIDGET, Some(json!({"meta": {"a": "new"}})))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["meta"], json!({"a": "new"}), "{updated}");
}

const CONTACT: &str = "/crm/v3/objects/contacts/512345";

/// A HubSpot contact as a model answers one, its stage and job title given.
fn contact(stage: &str, job: Option<&str>) -> Value {
    let mut contact = json!({
        "id": "512345",
        "properties": {
            "createdate": "2026-10-01T10:00:00Z",
            "lastmodifieddate": "2026-10-02T10:00:00Z",
            "hs_object_id": "512345",
            "email": "dana@example.com",
            "firstname": "Dana",
            "lastname": "Reyes",
            "lifecyclestage": stage,
        },
        "createdAt": "2026-10-01T10:00:00Z",
        "updatedAt": "2026-10-02T10:00:00Z",
        "archived": false,
    });
    if let Some(job) = job {
        contact["properties"]["jobtitle"] = json!(job);
    }
    contact
}

/// The benchmark's case: the model answers `opportunity` to a nested `PATCH` that
/// fails the schema alone, and the client got the stored `lead`.
#[tokio::test]
async fn an_update_keeps_the_answer_of_the_model() {
    let mut read = contact("lead", None);
    read["properties"]["company"] = json!("Globex");
    let h = harness_with(
        &hubspot_contract(),
        vec![read, contact("opportunity", Some("VP Sales"))],
        |_| {},
    );
    let (status, source, first) = h.call("GET", CONTACT, None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "llm"),
        "{first}"
    );
    assert_eq!(first["properties"]["lifecyclestage"], "lead");

    let request = json!({"properties": {"lifecyclestage": "opportunity", "jobtitle": "VP Sales"}});
    let (status, source, updated) = h.call("PATCH", CONTACT, Some(request)).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "llm"),
        "{updated}"
    );
    let properties = &updated["properties"];
    assert_eq!(properties["lifecyclestage"], "opportunity", "{updated}");
    assert_eq!(properties["jobtitle"], "VP Sales", "{updated}");
    // What the answer omitted comes from what is stored.
    assert_eq!(properties["company"], "Globex", "{updated}");
    assert_eq!(updated["id"], "512345", "{updated}");

    let (_, source, read) = h.call("GET", CONTACT, None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read, updated);
    assert_eq!(h.calls(), 2);
}

/// An update's effect on its own target loses what the resource does not declare:
/// `set` here holds a member of another nesting, which goes no top level.
#[tokio::test]
async fn an_updates_effect_on_its_own_target_adds_no_member() {
    let bundle = compile(&hubspot_contract()).unwrap();
    let op = bundle
        .operation("patch-/crm/v3/objects/contacts/{contactId}_update")
        .unwrap();
    let t = op.resource_type.clone().unwrap();
    let h = harness_with(
        &hubspot_contract(),
        vec![
            contact("lead", None),
            json!({
                "effects": [{"type": t, "id": "512345", "set": {"lifecyclestage": "opportunity"}}],
                "response": contact("opportunity", None),
            }),
        ],
        |_| {},
    );
    h.call("GET", CONTACT, None).await;
    let request = json!({"properties": {"lifecyclestage": "opportunity"}});
    let (status, _, updated) = h.call("PATCH", CONTACT, Some(request)).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["properties"]["lifecyclestage"], "opportunity");

    let (_, source, read) = h.call("GET", CONTACT, None).await;
    assert_eq!(source, "state");
    assert!(read.get("lifecyclestage").is_none(), "{read}");
    let held = h
        .store
        .get(&session(), &t, "512345")
        .await
        .unwrap()
        .unwrap();
    assert!(held.data.get("lifecyclestage").is_none(), "{}", held.data);
}
