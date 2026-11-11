//! A request may send `null` for a member its object does not require, as real APIs
//! commonly treat it as absent (Telegram's `"message_thread_id": null`); `null` in a
//! required member, or a value of another type, is still refused.
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::json;

use crate::support::{self, HarnessOptions};

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Bot, version: "1"}
paths:
  /sendMessage:
    post:
      operationId: sendMessage
      requestBody:
        content:
          application/json:
            schema:
              type: object
              required: [chat_id, text]
              properties:
                chat_id: {type: integer}
                text: {type: string}
                message_thread_id: {type: integer}
                reply: {$ref: "#/components/schemas/Reply"}
                parse_mode: {type: string, enum: [HTML, Markdown]}
                sender: {$ref: "#/components/schemas/Sender"}
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {ok: {type: boolean}}}}}}
components:
  schemas:
    Reply:
      type: object
      required: [message_id]
      properties:
        message_id: {type: integer}
        quote: {type: string}
    Named:
      type: object
      properties:
        name: {type: string}
    Sender:
      allOf:
        - $ref: "#/components/schemas/Named"
        - required: [name]
"##;

async fn status_of(body: serde_json::Value) -> StatusCode {
    let h = support::harness(
        CONTRACT,
        vec![json!({"ok": true})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    h.send("POST", "/sendMessage", Some(body), &[]).await.0
}

#[tokio::test]
async fn null_in_an_optional_member_is_accepted() {
    let base = json!({"chat_id": 1, "text": "hi", "message_thread_id": null});
    assert_eq!(status_of(base).await, StatusCode::OK);
    // Nested, under a `$ref`.
    let nested = json!({"chat_id": 1, "text": "hi", "reply": {"message_id": 3, "quote": null}});
    assert_eq!(status_of(nested).await, StatusCode::OK);
    // A member that is a `$ref`, and one with an `enum`.
    let referenced = json!({"chat_id": 1, "text": "hi", "reply": null, "parse_mode": null});
    assert_eq!(status_of(referenced).await, StatusCode::OK);
}

#[tokio::test]
async fn null_in_a_required_member_or_a_wrong_type_is_refused() {
    let required = json!({"chat_id": 1, "text": null});
    assert_eq!(status_of(required).await, StatusCode::BAD_REQUEST);
    let nested = json!({"chat_id": 1, "text": "hi", "reply": {"message_id": null}});
    assert_eq!(status_of(nested).await, StatusCode::BAD_REQUEST);
    // Required through `allOf`.
    let all_of = json!({"chat_id": 1, "text": "hi", "sender": {"name": null}});
    assert_eq!(status_of(all_of).await, StatusCode::BAD_REQUEST);
    let wrong = json!({"chat_id": 1, "text": "hi", "message_thread_id": "x"});
    assert_eq!(status_of(wrong).await, StatusCode::BAD_REQUEST);
}
