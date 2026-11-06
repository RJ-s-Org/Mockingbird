//! An update's request members that are objects, or that the request wraps one object
//! down, reach the stored record whether the answer or its effects carry them: an effect
//! on the update's own target is held to the request (see
//! [`write_memory::own_effect_members`]) without losing what the request changed.
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request as HttpRequest;
use mb_engine::{Engine, EngineConfig};
use mb_llm::{Provider, ScriptedProvider};
use mb_state::{MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::MockRuntime;

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "nested".into(),
        session: "default".into(),
    }
}

/// Stores `data` as `t`'s `id`, sends `method path body` (of `content_type`) to a mock
/// of `contract` whose one LLM answer is `answer`, and returns what the store then holds.
async fn after_update(
    contract: &str,
    (t, id, data): (&str, &str, Value),
    answer: Value,
    (method, path, content_type, body): (&str, &str, &str, Value),
) -> Value {
    let state = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let held = StoredResource {
        resource_type: t.into(),
        id: id.into(),
        data,
    };
    state.upsert(&session(), held).await.unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![Ok(answer.to_string())]));
    let rt = MockRuntime::new(
        "nested",
        Arc::new(mb_import::compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::clone(&state) as Arc<dyn StateStore>,
    );
    let req = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("content-type", content_type)
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = crate::http_rest::router(Arc::new(rt))
        .oneshot(req)
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    state.get(&session(), t, id).await.unwrap().unwrap().data
}

/// Users with a map of free-form metadata beside their scalar members.
const USERS: &str = r##"
openapi: 3.0.0
info: {title: Users, version: "1"}
paths:
  /users/{id}:
    get:
      operationId: getUser
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
    patch:
      operationId: updateUser
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                blocked: {type: boolean}
                given_name: {type: string}
                user_metadata: {type: object, additionalProperties: true}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}
components:
  schemas:
    User:
      type: object
      required: [id]
      properties:
        id: {type: string}
        blocked: {type: boolean}
        given_name: {type: string}
        user_metadata: {type: object, additionalProperties: true}
"##;

/// A `PATCH` naming a key of a map member beside top-level members: the map takes the
/// new key and keeps the rest, with an effect repeating the whole merged map.
#[tokio::test]
async fn a_patched_map_member_keeps_its_new_key() {
    let user = json!({"id": "u1", "blocked": false, "given_name": "Ana", "user_metadata": {"plan": "team"}});
    let merged = json!({"id": "u1", "blocked": true, "given_name": "Anabela", "user_metadata": {"plan": "team", "locale": "de"}});
    let answer = json!({"response": merged, "effects": [
        {"type": "users", "id": "u1", "set": {"blocked": true, "given_name": "Anabela", "user_metadata": {"plan": "team", "locale": "de"}}},
    ]});
    let edit = json!({"blocked": true, "given_name": "Anabela", "user_metadata": {"locale": "de"}});
    let stored = after_update(
        USERS,
        ("users", "u1", user),
        answer,
        ("PATCH", "/users/u1", "application/json", edit),
    )
    .await;
    assert_eq!(
        stored["user_metadata"],
        json!({"plan": "team", "locale": "de"}),
        "{stored}"
    );
    assert_eq!(stored["given_name"], "Anabela", "{stored}");
    assert_eq!(stored["blocked"], true, "{stored}");
}

/// Stores of string settings, updated by JSON merge patch.
const SETTINGS: &str = r##"
openapi: 3.0.0
info: {title: Settings, version: "1"}
paths:
  /stores/{id}:
    get:
      operationId: getStore
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Store"}}}}
    patch:
      operationId: patchStore
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/merge-patch+json:
            schema:
              type: object
              properties:
                data: {type: object, additionalProperties: {type: string, nullable: true}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Store"}}}}
components:
  schemas:
    Store:
      type: object
      required: [id]
      properties:
        id: {type: string}
        data: {type: object, additionalProperties: {type: string}}
"##;

/// A merge patch setting one key of a nested map and removing another: both happen.
#[tokio::test]
async fn a_merge_patch_sets_and_removes_keys_of_a_nested_map() {
    let held = json!({"id": "s1", "data": {"SIZE": "512", "MODE": "x"}});
    let answer = json!({"response": {"id": "s1", "data": {"SIZE": "512", "TTL": "60"}}, "effects": [
        {"type": "stores", "id": "s1", "set": {"data": {"SIZE": "512", "TTL": "60"}}},
    ]});
    let patch = json!({"data": {"TTL": "60", "MODE": null}});
    let stored = after_update(
        SETTINGS,
        ("stores", "s1", held),
        answer,
        ("PATCH", "/stores/s1", "application/merge-patch+json", patch),
    )
    .await;
    assert_eq!(
        stored["data"],
        json!({"SIZE": "512", "TTL": "60"}),
        "{stored}"
    );
}

/// Rows whose cells are a map under `fields`, patched or replaced.
const ROWS: &str = r##"
openapi: 3.0.0
info: {title: Rows, version: "1"}
paths:
  /rows/{id}:
    get:
      operationId: getRow
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Row"}}}}
    patch:
      operationId: patchRow
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {fields: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Row"}}}}
    put:
      operationId: replaceRow
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema: {type: object, properties: {fields: {type: object, additionalProperties: true}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Row"}}}}
components:
  schemas:
    Row:
      type: object
      required: [id]
      properties:
        id: {type: string}
        fields: {type: object, additionalProperties: true}
"##;

fn row() -> Value {
    json!({"id": "r1", "fields": {"Name": "n", "Status": "Todo", "Due date": "2026-01-01", "Quantity": 3}})
}

/// A `PATCH` of one cell: the cell changes, with an effect repeating every cell.
#[tokio::test]
async fn a_patched_cell_of_a_map_member_changes() {
    let cells = json!({"Name": "n", "Status": "Done", "Due date": "2026-01-01", "Quantity": 3});
    let answer = json!({"response": {"id": "r1", "fields": cells}, "effects": [
        {"type": "rows", "id": "r1", "set": {"fields": cells}},
    ]});
    let edit = json!({"fields": {"Status": "Done"}});
    let stored = after_update(
        ROWS,
        ("rows", "r1", row()),
        answer,
        ("PATCH", "/rows/r1", "application/json", edit),
    )
    .await;
    assert_eq!(stored["fields"], cells, "{stored}");
}

/// A `PUT` of the cells replaces them: what the request leaves out is gone, though an
/// effect writes it as cleared.
#[tokio::test]
async fn a_put_replaces_a_map_member_with_the_requests() {
    let answer = json!({"response": {"id": "r1", "fields": {"Name": "n", "Status": "Done"}}, "effects": [
        {"type": "rows", "id": "r1", "set": {"fields": {"Name": "n", "Status": "Done", "Due date": null, "Quantity": null}}},
    ]});
    let edit = json!({"fields": {"Name": "n", "Status": "Done"}});
    let stored = after_update(
        ROWS,
        ("rows", "r1", row()),
        answer,
        ("PUT", "/rows/r1", "application/json", edit),
    )
    .await;
    assert_eq!(
        stored["fields"],
        json!({"Name": "n", "Status": "Done"}),
        "{stored}"
    );
}

/// Incidents whose update wraps the incident's members in a member named after it.
const INCIDENTS: &str = r##"
openapi: 3.0.0
info: {title: Incidents, version: "1"}
paths:
  /incidents/{id}:
    get:
      operationId: getIncident
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Incident"}}}}
    patch:
      operationId: updateIncident
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                incident:
                  type: object
                  properties:
                    status: {type: string}
                    body: {type: string}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Incident"}}}}
components:
  schemas:
    Incident:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        status: {type: string}
        body: {type: string}
"##;

/// A request wrapped one object down sets the record's members through the effect that
/// agrees with it: the answer alone does not carry a wrapped member to the store.
#[tokio::test]
async fn a_wrapped_request_member_reaches_the_record_by_an_agreeing_effect() {
    let held = json!({"id": "i1", "name": "Outage", "status": "investigating", "body": "Looking"});
    let answered =
        json!({"id": "i1", "name": "Outage", "status": "identified", "body": "Found it"});
    let answer = json!({"response": answered, "effects": [
        {"type": "incidents", "id": "i1", "set": {"status": "identified", "body": "Found it"}},
    ]});
    let edit = json!({"incident": {"status": "identified", "body": "Found it"}});
    let stored = after_update(
        INCIDENTS,
        ("incidents", "i1", held),
        answer,
        ("PATCH", "/incidents/i1", "application/json", edit),
    )
    .await;
    assert_eq!(stored["status"], "identified", "{stored}");
    assert_eq!(stored["body"], "Found it", "{stored}");
}

/// The same with an effect contradicting the wrapped request: it does not set the
/// member (as for an unwrapped request the answer leaves out).
#[tokio::test]
async fn a_wrapped_request_member_does_not_take_a_contradicting_effect() {
    let held = json!({"id": "i1", "name": "Outage", "status": "investigating", "body": "Looking"});
    let answer = json!({"response": held, "effects": [
        {"type": "incidents", "id": "i1", "set": {"status": "resolved", "body": "Found it"}},
    ]});
    let edit = json!({"incident": {"status": "identified", "body": "Found it"}});
    let stored = after_update(
        INCIDENTS,
        ("incidents", "i1", held.clone()),
        answer,
        ("PATCH", "/incidents/i1", "application/json", edit),
    )
    .await;
    assert_ne!(stored["status"], "resolved", "{stored}");
    assert_eq!(stored["body"], "Found it", "{stored}");
}

/// Accounts with a map of free-form labels and a declared address, answered on an
/// update by an acknowledgement of another schema, which settles none of them.
const ACCOUNTS: &str = r##"
openapi: 3.0.0
info: {title: Accounts, version: "1"}
paths:
  /accounts/{id}:
    get:
      operationId: getAccount
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Account"}}}}
    patch:
      operationId: patchAccount
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                labels: {type: object, additionalProperties: true}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}
    put:
      operationId: replaceAccount
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                address: {$ref: "#/components/schemas/Address"}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}
components:
  schemas:
    Ack:
      type: object
      properties:
        ok: {type: boolean}
    Address:
      type: object
      properties:
        street: {type: string}
        city: {type: string}
        zip: {type: string}
    Account:
      type: object
      required: [id]
      properties:
        id: {type: string}
        labels: {type: object, additionalProperties: true}
        address: {$ref: "#/components/schemas/Address"}
"##;

/// An acknowledged `PATCH` of one key of a map member, with an effect writing only that
/// key: the map takes it and keeps the keys it held.
#[tokio::test]
async fn an_acknowledged_patch_of_a_map_key_keeps_the_held_keys() {
    let held = json!({"id": "a1", "labels": {"plan": "team"}});
    let answer = json!({"response": {"ok": true}, "effects": [
        {"type": "accounts", "id": "a1", "set": {"labels": {"locale": "de"}}},
    ]});
    let edit = json!({"labels": {"locale": "de"}});
    let stored = after_update(
        ACCOUNTS,
        ("accounts", "a1", held),
        answer,
        ("PATCH", "/accounts/a1", "application/json", edit),
    )
    .await;
    assert_eq!(
        stored["labels"],
        json!({"plan": "team", "locale": "de"}),
        "{stored}"
    );
}

/// The same `PATCH` removing a held key by `null`, with an effect that leaves it out:
/// the key goes and the others stay.
#[tokio::test]
async fn an_acknowledged_patch_removes_a_map_key_sent_as_null() {
    let held = json!({"id": "a1", "labels": {"plan": "team", "tier": "gold"}});
    let answer = json!({"response": {"ok": true}, "effects": [
        {"type": "accounts", "id": "a1", "set": {"labels": {"locale": "de"}}},
    ]});
    let edit = json!({"labels": {"locale": "de", "tier": null}});
    let stored = after_update(
        ACCOUNTS,
        ("accounts", "a1", held),
        answer,
        ("PATCH", "/accounts/a1", "application/json", edit),
    )
    .await;
    assert_eq!(
        stored["labels"],
        json!({"plan": "team", "locale": "de"}),
        "{stored}"
    );
}

/// An acknowledged `PUT` of a declared object, with an effect repeating a member the
/// request left out: the effect says the object is not the request's, and is not
/// taken (as for a map), so the left-out member does not go back on it.
#[tokio::test]
async fn an_acknowledged_put_does_not_keep_what_it_left_out_by_an_effect() {
    let held = json!({"id": "a1", "address": {"street": "Main 1", "city": "Oslo", "zip": "0150"}});
    let answer = json!({"response": {"ok": true}, "effects": [
        {"type": "accounts", "id": "a1", "set": {"address": {"street": "Elm 2", "city": "Bergen", "zip": "0150"}}},
    ]});
    let edit = json!({"address": {"street": "Elm 2", "city": "Bergen"}});
    let stored = after_update(
        ACCOUNTS,
        ("accounts", "a1", held),
        answer,
        ("PUT", "/accounts/a1", "application/json", edit),
    )
    .await;
    assert_ne!(
        stored["address"],
        json!({"street": "Elm 2", "city": "Bergen", "zip": "0150"}),
        "{stored}"
    );
}

/// The same `PUT` with an effect writing just what the request sent: the object is
/// the request's, the member it left out gone.
#[tokio::test]
async fn an_acknowledged_put_with_an_agreeing_effect_replaces_the_object() {
    let held = json!({"id": "a1", "address": {"street": "Main 1", "city": "Oslo", "zip": "0150"}});
    let answer = json!({"response": {"ok": true}, "effects": [
        {"type": "accounts", "id": "a1", "set": {"address": {"street": "Elm 2", "city": "Bergen"}}},
    ]});
    let edit = json!({"address": {"street": "Elm 2", "city": "Bergen"}});
    let stored = after_update(
        ACCOUNTS,
        ("accounts", "a1", held),
        answer,
        ("PUT", "/accounts/a1", "application/json", edit),
    )
    .await;
    assert_eq!(
        stored["address"],
        json!({"street": "Elm 2", "city": "Bergen"}),
        "{stored}"
    );
}
