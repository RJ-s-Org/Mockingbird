#![cfg(test)]
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

#[test]
fn config_debug_redacts_the_api_key_and_header_values() {
    let key = "sk-s3ntinel-api-key";
    let config = OpenAiConfig {
        base_url: "https://api.example/v1/".into(),
        api_key: Some(key.into()),
        model: "m".into(),
        json_schema: true,
        headers: vec![(
            "cf-aig-authorization".into(),
            "Bearer s3ntinel-gateway-token".into(),
        )],
        extra_body: serde_json::Map::new(),
        http: HttpOptions::default(),
    };
    let shown = format!("{config:?}");
    assert!(!shown.contains(key), "api key leaked: {shown}");
    assert!(
        !shown.contains("s3ntinel-gateway-token"),
        "header value leaked: {shown}"
    );
    assert!(
        shown.contains("cf-aig-authorization"),
        "header name kept: {shown}"
    );
    assert!(
        shown.contains("https://api.example/v1/"),
        "base_url kept: {shown}"
    );
}

fn config_with_secrets() -> OpenAiConfig {
    OpenAiConfig {
        base_url: "https://bob:s3ntinel-base-pass@llm.example/v1".into(),
        api_key: Some("sk-s3ntinel-api-key".into()),
        model: "m".into(),
        json_schema: true,
        headers: vec![("api-key".into(), "s3ntinel-header".into())],
        extra_body: serde_json::Map::new(),
        http: HttpOptions {
            extra_roots_pem: Vec::new(),
            proxy: Some("http://alice:s3ntinel-proxy-pass@proxy.example:3128".into()),
            no_proxy: vec!["internal.example".into()],
        },
    }
}

#[test]
fn http_options_debug_redacts_proxy_credentials() {
    let shown = format!("{:?}", config_with_secrets().http);
    assert!(
        !shown.contains("s3ntinel"),
        "proxy password leaked: {shown}"
    );
    assert!(!shown.contains("alice"), "proxy user leaked: {shown}");
    assert!(
        shown.contains("http://<redacted>@proxy.example:3128"),
        "proxy host kept: {shown}"
    );
    assert!(shown.contains("internal.example"), "no_proxy kept: {shown}");
}

#[test]
fn provider_debug_shows_no_secret() {
    let config = config_with_secrets();
    let shown = format!("{config:?}");
    assert!(!shown.contains("s3ntinel"), "secret leaked: {shown}");
    assert!(shown.contains("llm.example"), "base_url host kept: {shown}");
    let provider = OpenAiCompatible::new(config).unwrap();
    let shown = format!("{provider:?}");
    assert!(!shown.contains("s3ntinel"), "secret leaked: {shown}");
    assert!(shown.starts_with("OpenAiCompatible"), "{shown}");
}

#[test]
fn invalid_proxy_error_hides_its_credentials() {
    let opts = HttpOptions {
        proxy: Some("http://alice:s3ntinel-pass@[bad".into()),
        ..HttpOptions::default()
    };
    let err = http_client(&opts, None).unwrap_err().to_string();
    assert!(err.contains("invalid proxy"), "{err}");
    assert!(!err.contains("s3ntinel"), "proxy password leaked: {err}");
}

fn client(server: &MockServer, json_schema: bool) -> OpenAiCompatible {
    OpenAiCompatible::new(OpenAiConfig {
        base_url: format!("{}/", server.uri()),
        api_key: Some("k".into()),
        model: "m".into(),
        json_schema,
        headers: Vec::new(),
        extra_body: serde_json::Map::new(),
        http: HttpOptions::default(),
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
async fn sends_extra_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer k"))
        .and(header("cf-aig-authorization", "Bearer gw"))
        .and(header("x-title", "Mockingbird"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: server.uri(),
        api_key: Some("k".into()),
        model: "m".into(),
        json_schema: false,
        headers: vec![
            ("cf-aig-authorization".into(), "Bearer gw".into()),
            ("x-title".into(), "Mockingbird".into()),
        ],
        extra_body: serde_json::Map::new(),
        http: HttpOptions::default(),
    })
    .unwrap();
    let resp = provider
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(resp.content, "{\"a\":1}");
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

#[tokio::test]
async fn openai_stream_yields_deltas() {
    use futures::StreamExt;

    let server = MockServer::start().await;
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"a\\\"\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\":1}\\n\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&server)
        .await;
    let stream = client(&server, false)
        .stream(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    let deltas: Vec<String> = stream.map(|d| d.unwrap()).collect().await;
    assert_eq!(deltas, vec!["{\"a\"".to_string(), ":1}\n".to_string()]);
    assert_eq!(wire_body(&server).await["stream"], true);
}

/// Streams ask for usage (`stream_options.include_usage`), and the final
/// usage chunk (no choices) is reported once the stream has ended.
#[tokio::test]
async fn openai_stream_reports_the_final_usage_chunk() {
    use futures::StreamExt;

    let server = MockServer::start().await;
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"a\\\":1}\\n\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&server)
        .await;
    let metered = client(&server, false)
        .stream_metered(req(ResponseFormat::Text, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(
        metered.usage.get(),
        None,
        "nothing is reported before the end"
    );
    let deltas: Vec<String> = metered.text.map(|d| d.unwrap()).collect().await;
    assert_eq!(deltas, vec!["{\"a\":1}\n".to_string()]);
    assert_eq!(
        metered.usage.get(),
        Some(Usage {
            prompt_tokens: 12,
            completion_tokens: 7
        })
    );
    assert_eq!(
        wire_body(&server).await["stream_options"],
        json!({"include_usage": true})
    );
}

/// An endpoint that rejects `stream_options` is asked once more without
/// it, and then never again; its streams report no usage.
#[tokio::test]
async fn a_rejected_stream_usage_option_is_dropped() {
    use futures::StreamExt;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|r: &Request| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v.get("stream_options").is_some()
        })
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let c = client(&server, false);
    for _ in 0..2 {
        let metered = c
            .stream_metered(req(ResponseFormat::Text, Duration::from_secs(2)))
            .await
            .unwrap();
        let deltas: Vec<String> = metered.text.map(|d| d.unwrap()).collect().await;
        assert_eq!(deltas, vec!["x".to_string()]);
        assert_eq!(metered.usage.get(), None);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn openai_stream_http_error_before_first_delta() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let r = client(&server, false)
        .stream(req(ResponseFormat::Text, Duration::from_secs(2)))
        .await;
    assert!(matches!(r, Err(LlmError::Http { status: 500, .. })));
}

/// The items of a stream request answered (200) with `sse`.
async fn stream_items(sse: &'static str) -> Vec<Result<String, LlmError>> {
    use futures::StreamExt;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&server)
        .await;
    client(&server, false)
        .stream(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap()
        .collect()
        .await
}

#[tokio::test]
async fn openai_stream_error_event_before_content_is_an_error() {
    let items = stream_items(concat!(
        r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#,
        "\n\n",
        r#"data: {"error":{"code":503,"message":"overloaded"}}"#,
        "\n\n",
        "data: [DONE]\n\n",
    ))
    .await;
    assert_eq!(items.len(), 1, "{items:?}");
    match &items[0] {
        Err(LlmError::Http { status: 503, body }) => assert!(body.contains("overloaded")),
        other => panic!("expected HTTP 503, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_stream_error_mid_stream_follows_the_content() {
    let items = stream_items(concat!(
        r#"data: {"choices":[{"delta":{"content":"{\"a\""}}]}"#,
        "\n\n",
        r#"data: {"provider":"P","choices":[{"delta":{},"finish_reason":"error","error":{"code":502,"message":"upstream died"}}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    ))
    .await;
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0].as_ref().unwrap(), "{\"a\"");
    match &items[1] {
        Err(LlmError::Http { status: 502, body }) => {
            assert!(body.contains("provider P") && body.contains("upstream died"));
        }
        other => panic!("expected HTTP 502, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_stream_finish_length_is_truncated() {
    let items = stream_items(concat!(
        r#"data: {"choices":[{"delta":{"content":"{\"a\""}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"content":":"},"finish_reason":"length"}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    ))
    .await;
    assert_eq!(items.len(), 3, "{items:?}");
    assert_eq!(items[0].as_ref().unwrap(), "{\"a\"");
    assert_eq!(items[1].as_ref().unwrap(), ":");
    assert!(matches!(items[2], Err(LlmError::Truncated(_))), "{items:?}");
}

#[tokio::test]
async fn openai_stream_keepalive_and_stop_chunks_stay_ignored() {
    let items = stream_items(concat!(
        r#"data: {"choices":[]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"content":"x"}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"error":null}"#,
        "\n\n",
        "data: [DONE]\n\n",
    ))
    .await;
    let items: Vec<String> = items.into_iter().map(Result::unwrap).collect();
    assert_eq!(items, vec!["x"]);
}

fn reply(message: Value, finish_reason: &str) -> Value {
    json!({
        "provider": "SomeProvider",
        "choices": [{"message": message, "finish_reason": finish_reason}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3}
    })
}

/// Responds with each body in turn; the last one repeats.
fn in_turn(bodies: Vec<ResponseTemplate>) -> impl wiremock::Respond {
    let calls = std::sync::atomic::AtomicUsize::new(0);
    move |_: &Request| {
        let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        bodies[n.min(bodies.len() - 1)].clone()
    }
}

#[tokio::test]
async fn reasoning_only_reply_uses_the_json_in_reasoning() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply(
            json!({"content": null, "reasoning": "The answer is {\"a\":1}"}),
            "stop",
        )))
        .mount(&server)
        .await;
    let resp = client(&server, false)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(resp.content, "{\"a\":1}");
}

#[tokio::test]
async fn reasoning_only_reply_uses_the_last_whole_value_not_a_nested_one() {
    let server = MockServer::start().await;
    let reasoning = "Draft: {\"id\":\"x\"}. Final: {\"id\":\"pi_1\",\"types\":[\"card\"],\"opts\":{\"card\":{}}}";
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply(
            json!({"content": null, "reasoning": reasoning}),
            "stop",
        )))
        .mount(&server)
        .await;
    let resp = client(&server, false)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(
        resp.content,
        "{\"id\":\"pi_1\",\"types\":[\"card\"],\"opts\":{\"card\":{}}}"
    );
}

#[tokio::test]
async fn in_band_error_is_retried_once() {
    let server = MockServer::start().await;
    let failed = json!({
        "provider": "Flaky",
        "choices": [{"message": {"content": ""}, "finish_reason": "error",
                     "error": {"code": 502, "message": "upstream died"}}]
    });
    Mock::given(method("POST"))
        .respond_with(in_turn(vec![
            ResponseTemplate::new(200).set_body_json(failed),
            ResponseTemplate::new(200).set_body_json(ok_body()),
        ]))
        .mount(&server)
        .await;
    let resp = client(&server, false)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(resp.content, "{\"a\":1}");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn server_error_is_retried_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(in_turn(vec![
            ResponseTemplate::new(502).set_body_string("bad gateway"),
            ResponseTemplate::new(200).set_body_json(ok_body()),
        ]))
        .mount(&server)
        .await;
    let resp = client(&server, false)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(resp.content, "{\"a\":1}");
}

#[tokio::test]
async fn empty_reply_error_names_the_provider_and_finish_reason() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply(
            json!({"content": null, "reasoning": "thinking…"}),
            "stop",
        )))
        .mount(&server)
        .await;
    let err = client(&server, false)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("SomeProvider"), "{err}");
    assert!(err.contains("finish_reason stop"), "{err}");
    // Retried once before giving up.
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

/// Seen with OpenRouter's DigitalOcean route in JSON Schema mode: the answer
/// is lost and the reasoning only plans it, quoting fragments.
#[tokio::test]
async fn reasoning_that_only_plans_the_answer_is_empty() {
    let server = MockServer::start().await;
    let reasoning = "payment_method_types: [\"card\"] from the form. amount_details: {} has no required fields. Output the JSON.";
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply(
            json!({"content": null, "reasoning": reasoning}),
            "stop",
        )))
        .mount(&server)
        .await;
    let err = client(&server, false)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("reasoning without JSON"), "{err}");
}

#[tokio::test]
async fn truncated_reply_says_to_raise_max_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(reply(json!({"content": "{\"a\": [1, 2"}), "length")),
        )
        .mount(&server)
        .await;
    let err = client(&server, false)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("max_tokens (100)"), "{err}");
    // A truncated reply is not retried: the same limit would truncate again.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn merges_extra_body_under_its_own_fields() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let extra = json!({"provider": {"order": ["atlas-cloud"]}, "model": "ignored"});
    let provider = OpenAiCompatible::new(OpenAiConfig {
        base_url: server.uri(),
        api_key: None,
        model: "m".into(),
        json_schema: false,
        headers: Vec::new(),
        extra_body: extra.as_object().unwrap().clone(),
        http: HttpOptions::default(),
    })
    .unwrap();
    provider
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    let body = wire_body(&server).await;
    assert_eq!(body["provider"]["order"][0], "atlas-cloud");
    assert_eq!(body["model"], "m", "the client's own fields win");
}

#[tokio::test]
async fn reasoning_off_is_sent_unless_extra_body_sets_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let c = client(&server, false).with_reasoning_disabled(true);
    c.complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(
        wire_body(&server).await["reasoning"],
        json!({"enabled": false})
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let c = OpenAiCompatible::new(OpenAiConfig {
        base_url: server.uri(),
        api_key: None,
        model: "m".into(),
        json_schema: false,
        headers: Vec::new(),
        extra_body: json!({"reasoning": {"effort": "high"}})
            .as_object()
            .unwrap()
            .clone(),
        http: HttpOptions::default(),
    })
    .unwrap()
    .with_reasoning_disabled(true);
    c.complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(
        wire_body(&server).await["reasoning"],
        json!({"effort": "high"})
    );
}

#[tokio::test]
async fn a_rejected_reasoning_switch_is_dropped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|r: &Request| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v.get("reasoning").is_some()
        })
        .respond_with(
            ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"Reasoning is mandatory for this endpoint"}}"#,
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let c = client(&server, false).with_reasoning_disabled(true);
    for _ in 0..2 {
        let resp = c
            .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(resp.content, "{\"a\":1}");
    }
    // Rejected once, then never sent again.
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn other_endpoints_get_no_reasoning_switch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    client(&server, false)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert!(wire_body(&server).await.get("reasoning").is_none());
}
