//! Lifecycles, soft deletes and `not_found` from a world description: a described
//! soft delete keeps the record, readable in its deleted state, and out of the default
//! list on every path that lists held records (unless the list asks for deleted ones);
//! a create starts in the described initial state; a soft-deleted record's unique
//! values are free again; and a read of a hard-deleted record answers the described
//! `not_found` status. Without a description, deletes and missing reads answer as
//! before.
#![cfg(test)]
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness};

/// Spaces with a status member, listed with a page size and a cursor, a deleted-ones
/// switch and a name filter; notes with a boolean deleted flag; apps paged by `Range`.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Spaces, version: "1"}
paths:
  /spaces:
    get:
      operationId: listSpaces
      parameters:
        - {name: limit, in: query, schema: {type: integer}}
        - {name: cursor, in: query, schema: {type: string}}
        - {name: showDeleted, in: query, schema: {type: boolean}}
        - {name: displayName, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [spaces]
                properties:
                  spaces: {type: array, items: {$ref: "#/components/schemas/Space"}}
                  cursor: {type: string}
    post:
      operationId: createSpace
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/SpaceInput"}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Space"}}}}
        "409": {description: taken, content: {application/json: {schema: {$ref: "#/components/schemas/Problem"}}}}
  /spaces/{spaceId}:
    parameters: [{name: spaceId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getSpace
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Space"}}}}}
    delete:
      operationId: deleteSpace
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Space"}}}}}
  /notes:
    post:
      operationId: createNote
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
  /notes/{noteId}:
    parameters: [{name: noteId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getNote
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Note"}}}}}
    delete:
      operationId: deleteNote
      responses: {"204": {description: gone}}
  /apps:
    post:
      operationId: createApp
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/App"}}}}}
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
    parameters: [{name: appId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getApp
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/App"}}}}}
    delete:
      operationId: deleteApp
      responses: {"204": {description: gone}}
components:
  schemas:
    Space:
      type: object
      required: [id]
      properties:
        id: {type: string}
        displayName: {type: string}
        state: {type: string, enum: [ACTIVE, PENDING, DELETE_REQUESTED]}
    SpaceInput:
      type: object
      properties: {displayName: {type: string}}
    Note:
      type: object
      required: [id]
      properties:
        id: {type: string}
        title: {type: string}
        is_deleted: {type: boolean}
    App:
      type: object
      required: [id]
      properties:
        id: {type: string}
        name: {type: string}
        status: {type: string}
    Problem:
      type: object
      properties: {message: {type: string}}
"##;

/// Widgets whose read declares two client errors, so no heuristic picks one for a
/// missing record.
const WIDGETS: &str = r##"
openapi: 3.0.0
info: {title: Widgets, version: "1"}
paths:
  /widgets:
    post:
      operationId: createWidget
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}}
  /widgets/{widgetId}:
    parameters: [{name: widgetId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getWidget
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
        "400": {description: bad, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
        "422": {description: unprocessable, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
    delete:
      operationId: deleteWidget
      responses: {"204": {description: gone}}
components:
  schemas:
    Widget:
      type: object
      required: [id]
      properties: {id: {type: string}, name: {type: string}}
    Error:
      type: object
      required: [ErrorCode, Message]
      properties:
        ErrorCode: {type: string, enum: [NOT_FOUND, INVALID]}
        Message: {type: string}
      additionalProperties: false
"##;

fn space(id: &str, name: &str, state: &str) -> Value {
    json!({"id": id, "displayName": name, "state": state})
}

fn spaces(items: Vec<Value>) -> Value {
    json!({"spaces": items})
}

fn ids(body: &Value) -> Vec<&str> {
    let items = body
        .get("spaces")
        .and_then(Value::as_array)
        .or_else(|| body.as_array());
    items
        .map(|items| items.iter().filter_map(|i| i["id"].as_str()).collect())
        .unwrap_or_default()
}

/// Spaces soft-delete into `DELETE_REQUESTED`, listed again with `showDeleted=true`.
fn soft_spaces() -> Value {
    json!({"types": {"spaces": {"lifecycle": {
        "member": "state",
        "states": ["ACTIVE", "PENDING", "DELETE_REQUESTED"],
        "soft_delete": {
            "state": "DELETE_REQUESTED",
            "listed_with": {"param": "showDeleted", "value": true}
        }
    }}}})
}

async fn create_space(h: &DescribedHarness, name: &str) {
    let (status, _, body) = h
        .send("POST", "/spaces", Some(json!({"displayName": name})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

async fn delete(h: &DescribedHarness, uri: &str) {
    let (status, _, body) = h.send("DELETE", uri, None, &[]).await;
    assert!(status.is_success(), "{uri}: {status} {body}");
}

async fn get(h: &DescribedHarness, uri: &str) -> Value {
    let (status, _, body) = h.send("GET", uri, None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

#[tokio::test]
async fn a_soft_delete_keeps_the_record_readable_in_its_state() {
    let h = support::described(
        CONTRACT,
        soft_spaces(),
        vec![space("s1", "Docs", "ACTIVE"), space("s1", "Docs", "ACTIVE")],
    );
    create_space(&h, "Docs").await;
    delete(&h, "/spaces/s1").await;
    let body = get(&h, "/spaces/s1").await;
    assert_eq!(body["state"], "DELETE_REQUESTED", "{body}");
    assert_eq!(body["displayName"], "Docs");
    let stored = h.stored("spaces", "s1").await.unwrap();
    assert_eq!(stored["state"], "DELETE_REQUESTED");
    // The read is answered from state: no model call of its own.
    assert_eq!(h.provider.requests().len(), 2);
}

#[tokio::test]
async fn a_hard_delete_without_a_lifecycle_is_as_today() {
    let h = support::described(
        CONTRACT,
        json!({}),
        vec![space("s1", "Docs", "ACTIVE"), space("s1", "Docs", "ACTIVE")],
    );
    create_space(&h, "Docs").await;
    delete(&h, "/spaces/s1").await;
    assert!(h.stored("spaces", "s1").await.is_none());
    let (status, _, _) = h.send("GET", "/spaces/s1", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_soft_deleted_record_leaves_the_default_list() {
    let h = support::described(
        CONTRACT,
        soft_spaces(),
        vec![
            space("s1", "Docs", "ACTIVE"),
            space("s2", "Notes", "ACTIVE"),
            space("s1", "Docs", "ACTIVE"),
            spaces(vec![
                space("s1", "Docs", "ACTIVE"),
                space("s2", "Notes", "ACTIVE"),
            ]),
        ],
    );
    create_space(&h, "Docs").await;
    create_space(&h, "Notes").await;
    delete(&h, "/spaces/s1").await;
    let body = get(&h, "/spaces").await;
    assert_eq!(ids(&body), ["s2"], "{body}");
}

#[tokio::test]
async fn listed_with_lists_it_again() {
    let h = support::described(
        CONTRACT,
        soft_spaces(),
        vec![
            space("s1", "Docs", "ACTIVE"),
            space("s2", "Notes", "ACTIVE"),
            space("s1", "Docs", "ACTIVE"),
            spaces(vec![
                space("s1", "Docs", "ACTIVE"),
                space("s2", "Notes", "ACTIVE"),
            ]),
        ],
    );
    create_space(&h, "Docs").await;
    create_space(&h, "Notes").await;
    delete(&h, "/spaces/s1").await;
    let body = get(&h, "/spaces?showDeleted=true").await;
    assert_eq!(ids(&body), ["s1", "s2"], "{body}");
    assert_eq!(body["spaces"][0]["state"], "DELETE_REQUESTED", "{body}");
}

#[tokio::test]
async fn a_soft_deleted_record_is_not_added_back_by_the_held_record_paths() {
    // `listSpaces` describes nothing; the lifecycle of its type alone excludes.
    let h = support::described(
        CONTRACT,
        soft_spaces(),
        vec![
            space("s1", "Docs", "ACTIVE"),
            space("s2", "Notes", "ACTIVE"),
            space("s1", "Docs", "ACTIVE"),
            // A full page of one with no cursor: the session's held records are its rest.
            spaces(vec![space("g1", "Other", "ACTIVE")]),
            // A list naming the soft-deleted record as the model last saw it, under a
            // name filter.
            spaces(vec![
                space("s1", "Notes", "ACTIVE"),
                space("s2", "Notes", "ACTIVE"),
                space("g2", "Else", "ACTIVE"),
            ]),
        ],
    );
    create_space(&h, "Docs").await;
    create_space(&h, "Notes").await;
    delete(&h, "/spaces/s1").await;

    // stored_rest: the rest of the full page holds `s2`, not `s1`.
    let first = get(&h, "/spaces?limit=1").await;
    assert_eq!(ids(&first), ["g1"], "{first}");
    let token = first["cursor"]
        .as_str()
        .expect("a minted cursor")
        .to_string();
    let rest = get(&h, &format!("/spaces?limit=1&cursor={token}")).await;
    assert_eq!(ids(&rest), ["s2"], "{rest}");
    assert!(
        rest.get("cursor").is_none_or(|c| c.is_null() || c == ""),
        "{rest}"
    );

    // overlay_stored shows `s1` as stored, deleted, and narrow drops it; the
    // name-matched filter still drops `g2`.
    let named = get(&h, "/spaces?displayName=Notes").await;
    assert_eq!(ids(&named), ["s2"], "{named}");
}

#[tokio::test]
async fn a_record_soft_deleted_after_the_first_page_is_not_on_the_next() {
    let h = support::described(
        CONTRACT,
        soft_spaces(),
        vec![
            spaces(vec![
                space("s1", "A", "ACTIVE"),
                space("s2", "B", "ACTIVE"),
                space("s3", "C", "ACTIVE"),
                space("s4", "D", "ACTIVE"),
            ]),
            space("s3", "C", "ACTIVE"),
        ],
    );
    let first = get(&h, "/spaces?limit=2").await;
    assert_eq!(ids(&first), ["s1", "s2"], "{first}");
    let token = first["cursor"]
        .as_str()
        .expect("a minted cursor")
        .to_string();
    delete(&h, "/spaces/s3").await;
    let next = get(&h, &format!("/spaces?limit=2&cursor={token}")).await;
    assert_eq!(ids(&next), ["s4"], "{next}");
    assert_eq!(h.provider.requests().len(), 2);
}

#[tokio::test]
async fn a_range_paged_list_leaves_soft_deleted_records_out() {
    let world = json!({"types": {"apps": {"lifecycle": {
        "member": "status",
        "soft_delete": {"state": "archived"}
    }}}});
    let h = support::described(
        CONTRACT,
        world,
        vec![
            json!({"id": "a1", "name": "alpha", "status": "live"}),
            json!({"id": "a2", "name": "beta", "status": "live"}),
            json!({"id": "a3", "name": "gamma", "status": "live"}),
            json!([]),
        ],
    );
    for name in ["alpha", "beta", "gamma"] {
        let (status, _, _) = h
            .send("POST", "/apps", Some(json!({"name": name})), &[])
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    delete(&h, "/apps/a2").await;
    assert_eq!(h.stored("apps", "a2").await.unwrap()["status"], "archived");
    let (status, _, body) = h
        .send("GET", "/apps", None, &[("Range", "name ..; max=10")])
        .await;
    assert!(status.is_success(), "{status} {body}");
    assert_eq!(ids(&body), ["a1", "a3"], "{body}");
}

#[tokio::test]
async fn a_boolean_soft_delete_state_is_stored_as_true() {
    let world = json!({"types": {"notes": {"lifecycle": {
        "member": "is_deleted",
        "soft_delete": {"state": "true"}
    }}}});
    let h = support::described(
        CONTRACT,
        world,
        vec![json!({"id": "n1", "title": "t", "is_deleted": false})],
    );
    let (status, _, _) = h
        .send("POST", "/notes", Some(json!({"title": "t"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);
    delete(&h, "/notes/n1").await;
    let stored = h.stored("notes", "n1").await.unwrap();
    assert_eq!(stored["is_deleted"], json!(true), "{stored}");
    let body = get(&h, "/notes/n1").await;
    assert_eq!(body["is_deleted"], json!(true), "{body}");
}

#[tokio::test]
async fn a_create_starts_in_the_initial_state() {
    let mut world = soft_spaces();
    world["types"]["spaces"]["lifecycle"]["initial"] = json!("ACTIVE");
    let h = support::described(CONTRACT, world, vec![space("s1", "Docs", "PENDING")]);
    let (status, _, body) = h
        .send("POST", "/spaces", Some(json!({"displayName": "Docs"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["state"], "ACTIVE", "{body}");
    assert_eq!(h.stored("spaces", "s1").await.unwrap()["state"], "ACTIVE");
    let read = get(&h, "/spaces/s1").await;
    assert_eq!(read["state"], "ACTIVE", "{read}");
}

#[tokio::test]
async fn a_soft_deleted_records_unique_values_are_free() {
    let mut world = soft_spaces();
    world["types"]["spaces"]["unique"] = json!([{"members": ["displayName"], "conflict": 409}]);
    let h = support::described(
        CONTRACT,
        world,
        vec![
            space("s1", "Docs", "ACTIVE"),
            space("s1", "Docs", "ACTIVE"),
            space("s2", "Docs", "ACTIVE"),
        ],
    );
    create_space(&h, "Docs").await;
    delete(&h, "/spaces/s1").await;
    let (status, _, body) = h
        .send("POST", "/spaces", Some(json!({"displayName": "Docs"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(h.stored("spaces", "s2").await.is_some());
}

#[tokio::test]
async fn a_hard_deleted_read_answers_the_described_not_found_status() {
    let script = || vec![json!({"id": "w1", "name": "a"})];
    // No description: two client errors, neither picked, the problem+json 404.
    let plain = support::described(WIDGETS, json!({}), script());
    plain
        .send("POST", "/widgets", Some(json!({"name": "a"})), &[])
        .await;
    delete(&plain, "/widgets/w1").await;
    let (status, _, _) = plain.send("GET", "/widgets/w1", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let h = support::described(
        WIDGETS,
        json!({"operations": {"getWidget": {"not_found": 422}}}),
        script(),
    );
    h.send("POST", "/widgets", Some(json!({"name": "a"})), &[])
        .await;
    delete(&h, "/widgets/w1").await;
    let (status, _, body) = h.send("GET", "/widgets/w1", None, &[]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["ErrorCode"].is_string(), "{body}");
    assert!(body["Message"].is_string(), "{body}");
    assert_eq!(
        body.as_object().map(serde_json::Map::len),
        Some(2),
        "{body}"
    );
}
