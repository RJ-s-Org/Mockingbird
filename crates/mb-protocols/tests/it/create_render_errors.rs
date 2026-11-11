//! A create whose answer does not render says why (`render_error`, on the log line and
//! the request record), as a read's does.
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness};

/// Telegram's `sendMessage`: the sent message, answered at `/result`.
const TELEGRAM: &str = r##"
openapi: 3.0.3
info: {title: Telegram, version: "7"}
paths:
  /bot{token}/sendMessage:
    parameters:
      - {name: token, in: path, required: true, schema: {type: string}}
    post:
      operationId: sendMessage
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [chat_id, text]
              properties:
                chat_id: {oneOf: [{type: integer}, {type: string}]}
                message_thread_id: {type: integer, nullable: true}
                text: {type: string, maxLength: 4096}
                parse_mode: {type: string, enum: [HTML, html, Markdown, MarkdownV2]}
      responses:
        "200":
          description: Success
          content:
            application/json:
              schema:
                type: object
                required: [ok, result]
                properties:
                  ok: {type: boolean, enum: [true]}
                  result: {$ref: "#/components/schemas/Message"}
components:
  schemas:
    Message:
      type: object
      required: [message_id, date, chat]
      properties:
        message_id: {type: integer}
        message_thread_id: {type: integer}
        date: {type: integer}
        from:
          type: object
          required: [id, is_bot, first_name]
          properties:
            id: {type: integer}
            is_bot: {type: boolean, enum: [true]}
            first_name: {type: string}
            username: {type: string}
        chat:
          type: object
          required: [id, type]
          properties:
            id: {type: integer}
            type: {type: string, enum: [private, group, supergroup, channel]}
            first_name: {type: string}
            title: {type: string}
        text: {type: string}
        entities: {type: array, items: {type: object, additionalProperties: true}}
"##;

fn send_message(set: Value) -> Value {
    json!({
        "types": {"sendMessage": {"identity": {"member": "message_id"}}},
        "operations": {"sendMessage": {
            "kind": "create", "type": "sendMessage",
            "answer": {"record": "/result"}, "set": set
        }}
    })
}

/// What fill writes for the members nothing set.
fn fill() -> Value {
    json!({
        "date": 1_700_000_000, "text": "hi", "entities": [],
        "chat": {"type": "private", "first_name": "Ada", "title": "t"},
        "from": {"id": 7, "first_name": "Bot", "username": "x"},
        "message_thread_id": 9
    })
}

/// The one request record of `sendMessage`.
fn record_of(h: &DescribedHarness) -> Value {
    let records = h.records();
    let mine: Vec<_> = records
        .iter()
        .filter(|r| r.operation_id.as_deref() == Some("sendMessage"))
        .collect();
    assert_eq!(mine.len(), 1, "{records:?}");
    mine[0].request.clone()
}

/// The trial's world: the message's chat, sender and thread from the request, its id
/// counted per chat.
fn trial_world() -> Value {
    send_message(json!({
        "chat.id": "request.body.chat_id",
        "from.is_bot": "true",
        "from.username": "'HcAlertsBot'",
        "message_id": "seq('chat:' + string(request.body.chat_id))",
        "message_thread_id": "request.body.message_thread_id"
    }))
}

#[tokio::test]
async fn a_null_sent_for_an_optional_member_that_takes_none_is_left_out_and_never_filled() {
    // `message_thread_id` is optional and takes no `null` in the answer's message: the
    // request's `null` is left out (and `set`'s `null` sets nothing), never filled.
    let h = support::described(TELEGRAM, trial_world(), vec![fill()]);
    let sent = json!({"chat_id": 123_456_789, "message_thread_id": null, "parse_mode": "html", "text": "hi"});
    let (status, _, body) = h
        .send("POST", "/bot123:abc/sendMessage", Some(sent), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let request = record_of(&h);
    assert!(request.get("exec_fallback").is_none(), "{request}");
    let result = &body["result"];
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(result["message_id"], 1, "{body}");
    assert_eq!(result["chat"]["id"], 123_456_789, "{body}");
    assert_eq!(result["from"]["username"], "HcAlertsBot", "{body}");
    assert!(result.get("message_thread_id").is_none(), "{body}");
    let asked: Vec<String> = h
        .provider
        .requests()
        .iter()
        .map(|r| format!("{:?}", r.response_format))
        .collect();
    assert!(
        asked.iter().all(|a| !a.contains("message_thread_id")),
        "fill was asked for the member sent as null: {asked:#?}"
    );
}

#[tokio::test]
async fn a_create_whose_answer_does_not_render_says_why_on_the_request_record() {
    // `date` set to text where the message holds an integer: the answer does not
    // render, and the create generates.
    let world = send_message(json!({"date": "'yesterday'"}));
    let generated = json!({"ok": true, "result": {"message_id": 5, "date": 1, "chat": {"id": 1, "type": "private"}}});
    let h = support::described(TELEGRAM, world, vec![fill(), generated]);
    let sent = json!({"chat_id": 1, "text": "hi"});
    let (status, _, body) = h
        .send("POST", "/bot123:abc/sendMessage", Some(sent), &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let request = record_of(&h);
    assert_eq!(request["exec_fallback"], "render_failed", "{request}");
    assert_eq!(request["render_error"], "/result/date type", "{request}");
}
