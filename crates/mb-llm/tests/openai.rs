#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mb_llm::*;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn req(format: ResponseFormat, timeout: Duration) -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage {
            role: Role::User,
            content: "hi".into(),
        }],
        response_format: format,
        temperature: 0.2,
        max_tokens: 100,
        timeout,
    }
}

fn client(server: &MockServer, json_schema: bool) -> OpenAiCompatible {
    OpenAiCompatible::new(OpenAiConfig {
        base_url: format!("{}/", server.uri()),
        api_key: Some("k".into()),
        model: "m".into(),
        json_schema,
    })
    .unwrap()
}

fn ok_body() -> Value {
    json!({"choices":[{"message":{"content":"{\"a\":1}"}}],"usage":{"prompt_tokens":5,"completion_tokens":3}})
}

async fn wire_body(server: &MockServer) -> Value {
    let reqs = server.received_requests().await.unwrap();
    serde_json::from_slice(&reqs[0].body).unwrap()
}

#[tokio::test]
async fn sends_json_object_and_parses_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer k"))
        .and(|r: &Request| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v["model"] == "m" && v["response_format"]["type"] == "json_object"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let resp = client(&server, false)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(resp.content, "{\"a\":1}");
    assert_eq!(resp.usage.prompt_tokens, 5);
    assert_eq!(resp.usage.completion_tokens, 3);
}

#[tokio::test]
async fn downgrades_json_schema_when_unsupported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let f = ResponseFormat::JsonSchema {
        name: "n".into(),
        schema: json!({"type":"object"}),
    };
    client(&server, false)
        .complete(req(f, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(
        wire_body(&server).await["response_format"]["type"],
        "json_object"
    );
}

#[tokio::test]
async fn uses_json_schema_when_supported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let f = ResponseFormat::JsonSchema {
        name: "n".into(),
        schema: json!({"type":"object"}),
    };
    client(&server, true)
        .complete(req(f, Duration::from_secs(2)))
        .await
        .unwrap();
    let b = wire_body(&server).await;
    assert_eq!(b["response_format"]["type"], "json_schema");
    assert_eq!(b["response_format"]["json_schema"]["name"], "n");
}

#[tokio::test]
async fn times_out() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_body())
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&server)
        .await;
    let r = client(&server, false)
        .complete(req(ResponseFormat::Text, Duration::from_millis(100)))
        .await;
    assert!(matches!(r, Err(LlmError::Timeout)));
}

#[tokio::test]
async fn maps_http_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .mount(&server)
        .await;
    let r = client(&server, false)
        .complete(req(ResponseFormat::Text, Duration::from_secs(2)))
        .await;
    assert!(matches!(r, Err(LlmError::Http { status: 429, .. })));
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn scripted_returns_in_order() {
    let p = ScriptedProvider::new(vec![Ok("one".into()), Ok("two".into())]);
    let a = p
        .complete(req(ResponseFormat::Text, Duration::from_secs(1)))
        .await
        .unwrap();
    let b = p
        .complete(req(ResponseFormat::Text, Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!((a.content.as_str(), b.content.as_str()), ("one", "two"));
    assert_eq!(p.requests().len(), 2);
}
