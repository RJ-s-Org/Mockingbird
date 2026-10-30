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
        output_mode: OutputMode::JsonSchema,
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
        output_mode: OutputMode::JsonSchema,
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

fn client(server: &MockServer, output_mode: OutputMode) -> OpenAiCompatible {
    OpenAiCompatible::new(OpenAiConfig {
        base_url: format!("{}/", server.uri()),
        api_key: Some("k".into()),
        model: "m".into(),
        output_mode,
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
    let resp = client(&server, OutputMode::JsonObject)
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
        output_mode: OutputMode::JsonObject,
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
    client(&server, OutputMode::JsonObject)
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
    client(&server, OutputMode::JsonSchema)
        .complete(req(f, Duration::from_secs(2)))
        .await
        .unwrap();
    let b = wire_body(&server).await;
    assert_eq!(b["response_format"]["type"], "json_schema");
    assert_eq!(b["response_format"]["json_schema"]["name"], "n");
    assert_eq!(b["response_format"]["json_schema"]["strict"], false);
}

/// `output_mode: none` sends no `response_format` for any JSON request,
/// and reports neither capability.
#[tokio::test]
async fn output_mode_none_sends_no_response_format() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let provider = client(&server, OutputMode::None);
    assert_eq!(
        provider.capabilities(),
        Capabilities {
            json_schema: false,
            json_mode: false,
        }
    );
    let schema = ResponseFormat::JsonSchema {
        name: "n".into(),
        schema: json!({"type":"object"}),
    };
    for format in [schema, ResponseFormat::JsonObject] {
        provider
            .complete(req(format, Duration::from_secs(2)))
            .await
            .unwrap();
    }
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    for r in reqs {
        let body: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(body.get("response_format").is_none(), "{body}");
    }
}

#[test]
fn output_mode_names_round_trip() {
    for mode in [
        OutputMode::None,
        OutputMode::JsonObject,
        OutputMode::JsonSchema,
    ] {
        assert_eq!(mode.as_str().parse::<OutputMode>(), Ok(mode));
        assert_eq!(
            serde_json::to_value(mode).unwrap(),
            Value::String(mode.to_string())
        );
    }
    assert_eq!(OutputMode::default(), OutputMode::JsonSchema);
    assert_eq!(OutputMode::from_json_schema(true), OutputMode::JsonSchema);
    assert_eq!(OutputMode::from_json_schema(false), OutputMode::JsonObject);
    assert!("strict".parse::<OutputMode>().is_err());
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
    let r = client(&server, OutputMode::JsonObject)
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
    let r = client(&server, OutputMode::JsonObject)
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
    let stream = client(&server, OutputMode::JsonObject)
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
    let metered = client(&server, OutputMode::JsonObject)
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
            completion_tokens: 7,
            reasoning_tokens: 0,
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
    let c = client(&server, OutputMode::JsonObject);
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
    let r = client(&server, OutputMode::JsonObject)
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
    client(&server, OutputMode::JsonObject)
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
    assert!(
        matches!(&items[2], Err(LlmError::Truncated { partial, .. }) if partial.is_empty()),
        "{items:?}"
    );
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
    let resp = client(&server, OutputMode::JsonObject)
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
    let resp = client(&server, OutputMode::JsonObject)
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
    let resp = client(&server, OutputMode::JsonObject)
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
    let resp = client(&server, OutputMode::JsonObject)
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
    let err = client(&server, OutputMode::JsonObject)
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
    let err = client(&server, OutputMode::JsonObject)
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
    let err = client(&server, OutputMode::JsonObject)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap_err();
    let LlmError::Truncated { partial, usage, .. } = &err else {
        panic!("expected Truncated, got {err:?}");
    };
    // The cut text and its tokens come back, for the caller to salvage.
    assert_eq!(partial, "{\"a\": [1, 2");
    assert_eq!(usage.completion_tokens, 3);
    let err = err.to_string();
    assert!(err.contains("max_tokens (100)"), "{err}");
    assert!(
        err.contains("truncated") && err.contains("finish_reason=length"),
        "{err}"
    );
    // A truncated reply is not retried: the same limit would truncate again.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// Reasoning tokens are read from `completion_tokens_details`, and a reply
/// cut short says how many of its tokens went to reasoning.
#[tokio::test]
async fn reasoning_tokens_are_reported() {
    let server = MockServer::start().await;
    let usage = json!({
        "prompt_tokens": 5,
        "completion_tokens": 100,
        "completion_tokens_details": {"reasoning_tokens": 90}
    });
    Mock::given(method("POST"))
        .respond_with(in_turn(vec![
            ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{\"a\":1}"}, "finish_reason": "stop"}],
                "usage": usage,
            })),
            ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{\"a\""}, "finish_reason": "length"}],
                "usage": usage,
            })),
        ]))
        .mount(&server)
        .await;
    let c = client(&server, OutputMode::JsonObject);
    let resp = c
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(
        resp.usage,
        Usage {
            prompt_tokens: 5,
            completion_tokens: 100,
            reasoning_tokens: 90,
        }
    );
    assert_eq!(resp.usage.answer_tokens(), 10);
    let err = c
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("90 of 100 completion tokens spent reasoning"),
        "{err}"
    );
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
        output_mode: OutputMode::JsonObject,
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
    let c = client(&server, OutputMode::JsonObject).with_reasoning_disabled(true);
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
        output_mode: OutputMode::JsonObject,
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

/// A model that rejects `reasoning: {enabled: false}` is asked with
/// minimal effort instead, and the client remembers that.
#[tokio::test]
async fn a_rejected_reasoning_switch_steps_down_to_minimal_effort() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|r: &Request| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v["reasoning"] == json!({"enabled": false})
        })
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Reasoning is mandatory for this endpoint and cannot be disabled."}}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let c = client(&server, OutputMode::JsonObject).with_reasoning_disabled(true);
    for _ in 0..2 {
        let resp = c
            .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(resp.content, "{\"a\":1}");
    }
    let reqs = server.received_requests().await.unwrap();
    let sent: Vec<Value> = reqs
        .iter()
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["reasoning"].clone())
        .collect();
    // Rejected once; minimal effort from then on.
    assert_eq!(
        sent,
        vec![
            json!({"enabled": false}),
            json!({"effort": "minimal"}),
            json!({"effort": "minimal"}),
        ]
    );
}

fn schema_format(name: &str) -> ResponseFormat {
    ResponseFormat::JsonSchema {
        name: name.to_string(),
        schema: json!({"type": "array", "items": {"type": "string", "pattern": "^a$"}}),
    }
}

/// A model can loop forever in an unbounded string while a schema constrains
/// it: free text is sent with a limit, and everything else as written.
#[tokio::test]
async fn free_text_strings_are_sent_with_a_length_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let format = ResponseFormat::JsonSchema {
        name: "n".to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "note": {"type": ["string", "null"]},
                "code": {"type": "string", "pattern": "^a$"},
                "kind": {"type": "string", "enum": ["a"]},
                "short": {"type": "string", "maxLength": 5},
                "when": {"type": "string", "format": "date"},
                "type": {"type": "integer"},
                "fixed": {"const": {"type": "string"}},
                "long": {"type": "string", "minLength": 3000},
                "blob": {"type": "string", "contentEncoding": "base64"}
            },
            "$defs": {"Text": {"type": "string"}}
        }),
    };
    client(&server, OutputMode::JsonSchema)
        .complete(req(format, Duration::from_secs(2)))
        .await
        .unwrap();
    let reqs = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let schema = &sent["response_format"]["json_schema"]["schema"];
    let limit = json!(MAX_FREE_TEXT_CHARS);
    let props = &schema["properties"];
    assert_eq!(props["name"]["maxLength"], limit);
    assert_eq!(props["note"]["maxLength"], limit);
    assert_eq!(schema["$defs"]["Text"]["maxLength"], limit);
    assert_eq!(props["short"]["maxLength"], json!(5));
    for unchanged in ["code", "kind", "when", "type", "long", "blob"] {
        assert!(props[unchanged].get("maxLength").is_none(), "{unchanged}");
    }
    assert_eq!(props["fixed"], json!({"const": {"type": "string"}}));
}

/// OpenAI refuses a schema whose root is not an object. That schema is then
/// asked for in JSON mode, without failing the call, and others are unaffected.
#[tokio::test]
async fn a_rejected_schema_falls_back_to_json_mode_and_is_remembered() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|r: &Request| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v["response_format"]["json_schema"]["name"] == "bad"
        })
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Invalid schema for response_format 'bad': schema must be a JSON Schema of 'type: "object"'"}}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let c = client(&server, OutputMode::JsonSchema);
    for name in ["bad", "bad", "good"] {
        let resp = c
            .complete(req(schema_format(name), Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(resp.content, "{\"a\":1}");
    }
    let reqs = server.received_requests().await.unwrap();
    let sent: Vec<String> = reqs
        .iter()
        .map(|r| {
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            v["response_format"]["type"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(
        sent,
        ["json_schema", "json_object", "json_object", "json_schema"]
    );
}

/// A call in flight when another records the rejection still falls back.
#[tokio::test]
async fn concurrent_rejections_of_a_schema_all_fall_back() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|r: &Request| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v["response_format"]["type"] == "json_schema"
        })
        .respond_with(
            ResponseTemplate::new(400)
                .set_delay(Duration::from_millis(100))
                .set_body_string(r#"{"error":{"message":"Invalid schema for response_format"}}"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let c = client(&server, OutputMode::JsonSchema);
    let (a, b) = tokio::join!(
        c.complete(req(schema_format("bad"), Duration::from_secs(2))),
        c.complete(req(schema_format("bad"), Duration::from_secs(2))),
    );
    assert!(a.is_ok() && b.is_ok(), "{a:?} {b:?}");
}

/// JSON mode being refused too is not retried, and costs no extra requests.
#[tokio::test]
async fn a_rejection_of_json_mode_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"'messages' must contain the word 'json' to use response_format"}}"#,
        ))
        .mount(&server)
        .await;
    let c = client(&server, OutputMode::JsonSchema);
    for _ in 0..2 {
        assert!(
            c.complete(req(schema_format("n"), Duration::from_secs(2)))
                .await
                .is_err()
        );
    }
    // Schema then JSON mode, then JSON mode alone.
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

/// A rejection is about one schema: a narrower selection under the same name
/// is still asked for with its own.
#[tokio::test]
async fn a_rejection_is_remembered_per_schema_not_per_name() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|r: &Request| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v["response_format"]["json_schema"]["schema"]["type"] == "array"
        })
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(r#"{"error":{"message":"Invalid schema for response_format"}}"#),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let c = client(&server, OutputMode::JsonSchema);
    c.complete(req(schema_format("same"), Duration::from_secs(2)))
        .await
        .unwrap();
    let other = ResponseFormat::JsonSchema {
        name: "same".to_string(),
        schema: json!({"type": "object"}),
    };
    c.complete(req(other, Duration::from_secs(2)))
        .await
        .unwrap();
    let reqs = server.received_requests().await.unwrap();
    let last: Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
    assert_eq!(last["response_format"]["type"], "json_schema");
}

/// Some routes lose the answer in schema mode: an empty one is tried once in
/// JSON mode, for this call only.
#[tokio::test]
async fn an_empty_answer_under_a_schema_is_retried_in_json_mode() {
    let server = MockServer::start().await;
    let empty = json!({"choices": [{"message": {"content": ""}, "finish_reason": "stop"}]});
    Mock::given(method("POST"))
        .and(|r: &Request| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v["response_format"]["type"] == "json_schema"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(empty))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let c = client(&server, OutputMode::JsonSchema);
    for _ in 0..2 {
        let resp = c
            .complete(req(schema_format("n"), Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(resp.content, "{\"a\":1}");
    }
    let reqs = server.received_requests().await.unwrap();
    let sent: Vec<Value> = reqs
        .iter()
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["response_format"]["type"].clone()
        })
        .collect();
    assert_eq!(
        sent,
        [
            json!("json_schema"),
            json!("json_object"),
            json!("json_schema")
        ]
    );
}

/// Any other 400 is the caller's to see, and is not retried.
#[tokio::test]
async fn an_unrelated_rejection_is_not_retried_in_json_mode() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
        ))
        .mount(&server)
        .await;
    let c = client(&server, OutputMode::JsonSchema);
    assert!(
        c.complete(req(schema_format("n"), Duration::from_secs(2)))
            .await
            .is_err()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// When minimal effort is rejected too, `reasoning` is no longer sent.
#[tokio::test]
async fn a_rejected_reasoning_setting_is_dropped() {
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
    let c = client(&server, OutputMode::JsonObject).with_reasoning_disabled(true);
    for _ in 0..2 {
        let resp = c
            .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(resp.content, "{\"a\":1}");
    }
    // Off, then minimal effort, rejected once each; then never sent again.
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn other_endpoints_get_no_reasoning_switch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    client(&server, OutputMode::JsonObject)
        .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    assert!(wire_body(&server).await.get("reasoning").is_none());
}

/// Two calls rejected for the same setting step it down once: the client
/// goes to minimal effort, not past it.
#[tokio::test]
async fn concurrent_rejections_step_down_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|r: &Request| {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            v["reasoning"] == json!({"enabled": false})
        })
        .respond_with(
            ResponseTemplate::new(400)
                .set_delay(Duration::from_millis(200))
                .set_body_string(
                    r#"{"error":{"message":"Reasoning is mandatory for this endpoint and cannot be disabled."}}"#,
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;
    let c = client(&server, OutputMode::JsonObject).with_reasoning_disabled(true);
    let (a, b) = tokio::join!(
        c.complete(req(ResponseFormat::JsonObject, Duration::from_secs(2))),
        c.complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
    );
    a.unwrap();
    b.unwrap();
    c.complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
        .await
        .unwrap();
    let reqs = server.received_requests().await.unwrap();
    let last: Value = serde_json::from_slice(&reqs[reqs.len() - 1].body).unwrap();
    assert_eq!(last["reasoning"], json!({"effort": "minimal"}));
    assert!(
        reqs.iter().all(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["reasoning"] != Value::Null
        }),
        "never stepped past minimal effort"
    );
}

/// An error that only mentions reasoning is not a rejection of the
/// reasoning parameter: nothing steps down.
#[tokio::test]
async fn an_unrelated_400_mentioning_reasoning_keeps_the_setting() {
    for message in [
        "This model's maximum context length is 8192 tokens (including reasoning). Please reduce the length of the messages.",
        "Invalid schema for response_format: property 'effort' has no type",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(json!({"error": {"message": message}}).to_string()),
            )
            .mount(&server)
            .await;
        let c = client(&server, OutputMode::JsonObject).with_reasoning_disabled(true);
        for _ in 0..2 {
            let _ = c
                .complete(req(ResponseFormat::JsonObject, Duration::from_secs(2)))
                .await;
        }
        let sent: Vec<Value> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["reasoning"].clone())
            .collect();
        assert_eq!(sent, vec![json!({"enabled": false}); 2], "{message}");
    }
}
