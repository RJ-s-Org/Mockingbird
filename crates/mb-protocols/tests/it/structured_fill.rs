//! A create fills what its request did not send with no model call when none of it is
//! free text (ids, enums, numbers, booleans, formatted, patterned or timestamp
//! strings, objects of nothing else), and asks the model for the free text alone.
use axum::http::StatusCode;
use mb_llm::ResponseFormat;
use serde_json::{Value, json};

use crate::support;

const CONTRACT: &str = r##"
openapi: 3.1.0
info: {title: Alerts, version: "1"}
paths:
  /alerts:
    post:
      operationId: createAlert
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [Message]
              properties:
                Message: {type: string, pattern: "^[A-Z].*$"}
                summary: {type: string}
      responses:
        "201":
          description: created
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Alert"}
  /alerts/{id}:
    get:
      operationId: getAlert
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Alert"}
components:
  schemas:
    Alert:
      type: object
      required: [id, message, priority, count, acked, created_at, owner_id, html_url, sla, routing]
      properties:
        id: {type: string, format: uuid}
        message: {type: string, pattern: "^[A-Z].*$"}
        summary: {type: string}
        priority: {type: string, enum: [P1, P2, P3]}
        count: {type: integer, minimum: 0}
        acked: {type: boolean}
        created_at: {type: string}
        owner_id: {type: string}
        html_url: {type: string}
        contact: {type: string, format: email}
        sla:
          type: object
          required: [seconds, tier]
          properties:
            seconds: {type: number}
            tier: {type: string, enum: [gold, silver]}
        routing:
          type: object
          required: [team_id, note]
          properties:
            team_id: {type: string}
            note: {type: string}
"##;

fn world() -> Value {
    json!({
        "types": {"alert": {"identity": {"member": "id"}}},
        "operations": {
            "createAlert": {"kind": "create", "type": "alert"},
            "getAlert": {"kind": "read", "type": "alert", "target": {"path_param": "id"}}
        }
    })
}

/// The members a fill call's structured output asked for.
fn asked(h: &support::DescribedHarness) -> Vec<Vec<String>> {
    h.provider
        .requests()
        .iter()
        .map(|r| {
            let ResponseFormat::JsonSchema { schema, .. } = &r.response_format else {
                return Vec::new();
            };
            let mut names: Vec<String> = schema["properties"]
                .as_object()
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();
            names
        })
        .collect()
}

/// Every missing member is structured, the free-text one (`routing.note`) aside, which
/// the request's `summary` does not cover: the model is asked for `routing` alone.
/// With `routing` sent whole, nothing is free text and no model call is made: the
/// record answers valid, echoes `Message` into `message`, and reads back the same.
#[tokio::test]
async fn a_create_missing_only_structured_members_makes_no_model_call() {
    let h = support::described(CONTRACT, world(), vec![]);
    let (status, headers, body) = h
        .send(
            "POST",
            "/alerts",
            Some(json!({
                "Message": "Backup is down",
                "summary": "nightly",
                "routing": {"team_id": "t1", "note": "pager"}
            })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(headers["x-mockingbird-source"], "state", "{body}");
    assert!(h.provider.requests().is_empty(), "{:?}", asked(&h));
    assert_eq!(body["message"], "Backup is down", "{body}");
    assert!(
        ["P1", "P2", "P3"].contains(&body["priority"].as_str().unwrap_or_default()),
        "{body}"
    );
    assert!(body["count"].as_i64().is_some_and(|c| c >= 0), "{body}");
    assert!(body["acked"].is_boolean(), "{body}");
    let created = body["created_at"].as_str().unwrap_or_default();
    assert!(
        chrono::DateTime::parse_from_rfc3339(created).is_ok(),
        "{body}"
    );
    assert!(body["sla"]["seconds"].is_number(), "{body}");
    assert!(
        body["owner_id"].is_string() && body["html_url"].is_string(),
        "{body}"
    );

    let id = body["id"].as_str().unwrap_or_default().to_owned();
    let (status, headers, read) = h.send("GET", &format!("/alerts/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(headers["x-mockingbird-source"], "state", "{read}");
    assert_eq!(read, body);
}

/// A missing member holding free text is still filled by the model, which is asked
/// for it alone: the structured members beside it are the faker's.
#[tokio::test]
async fn a_create_missing_free_text_asks_the_model_for_it_alone() {
    let fill = json!({"routing": {"note": "Page the on-call engineer."}, "priority": "P2"});
    let h = support::described(CONTRACT, world(), vec![fill]);
    let (status, _, body) = h
        .send(
            "POST",
            "/alerts",
            Some(json!({"Message": "Backup is down", "summary": "nightly"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // `routing` is asked for its free text alone, and the enum rides along.
    assert_eq!(
        asked(&h),
        vec![vec!["priority".to_string(), "routing".to_string()]]
    );
    assert_eq!(body["priority"], "P2", "{body}");
    assert!(body["routing"]["team_id"].is_string(), "{body}");
    assert_eq!(
        body["routing"]["note"], "Page the on-call engineer.",
        "{body}"
    );
    assert!(
        body["priority"].is_string() && body["sla"].is_object(),
        "{body}"
    );

    let id = body["id"].as_str().unwrap_or_default().to_owned();
    let stored = h.stored("alert", &id).await.unwrap_or_default();
    assert_eq!(stored["routing"]["note"], "Page the on-call engineer.");
}
