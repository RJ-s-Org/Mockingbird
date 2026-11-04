//! A model that loops until `max_tokens`, over a real client and a streamed
//! reply: the call is stopped early, retried once, and an answer that still
//! loops falls back with a reason saying so.

use std::sync::Arc;

use mb_engine::{Engine, EngineConfig, GenerationContext, OperationCall, Source};
use mb_llm::{OpenAiCompatible, OpenAiConfig, OutputMode, Provider};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use serde_json::{Map, Value, json};
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::method};

/// A small contract of its own: one gadget by id.
fn gadgets() -> mb_mcm::Bundle {
    let doc = json!({
        "openapi": "3.0.3",
        "info": {"title": "gadgets", "version": "1"},
        "paths": {"/gadgets/{gadgetId}": {"get": {
            "operationId": "getGadget",
            "parameters": [{"name": "gadgetId", "in": "path", "required": true,
                            "schema": {"type": "string"}}],
            "responses": {"200": {"description": "ok", "content": {"application/json": {
                "schema": {"$ref": "#/components/schemas/Gadget"}}}}}
        }}},
        "components": {"schemas": {"Gadget": {
            "type": "object",
            "required": ["id", "label"],
            "properties": {
                "id": {"type": "string"},
                "label": {"type": "string"},
                "events": {"type": "array", "items": {"type": "object", "properties": {
                    "id": {"type": "string"}}}}
            }
        }}}
    });
    mb_import::compile(doc.to_string().as_bytes()).unwrap()
}

/// One content delta per item of `deltas`, then `finish` with the usage
/// the provider reports (`completion` tokens).
fn sse_of(deltas: &[String], finish: &str, completion: u32) -> ResponseTemplate {
    let mut body = String::new();
    for d in deltas {
        let chunk = json!({"provider": "Loopy", "choices": [{"delta": {"content": d}}]});
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    let last = json!({"provider": "Loopy", "choices": [{"delta": {}, "finish_reason": finish}],
                      "usage": {"prompt_tokens": 40, "completion_tokens": completion}});
    body.push_str(&format!("data: {last}\n\ndata: [DONE]\n\n"));
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// The recorded loop: one line, again and again, about 64 KiB of it.
fn looping() -> ResponseTemplate {
    let mut deltas = vec![r#"{"id": "g1", "label": "Gadget", "events": [{"#.to_string()];
    deltas.extend(std::iter::repeat_n(
        "\n\"id\": \"2026-11-04T18:29:32Z\",".to_string(),
        2000,
    ));
    sse_of(&deltas, "length", 16_384)
}

fn answering() -> ResponseTemplate {
    sse_of(
        &[r#"{"id": "g1", "label": "Gadget"}"#.to_string()],
        "stop",
        9,
    )
}

/// The engine over a real client of `server`, its answer to `getGadget`,
/// and what [`mb_llm::CUT_REPLIES_METRIC`] counted, by reason.
fn served(
    responses: Vec<ResponseTemplate>,
) -> (mb_engine::GeneratedResponse, usize, Vec<(String, u64)>) {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let (out, calls) = metrics::with_local_recorder(&recorder, || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = MockServer::start().await;
            let n = std::sync::atomic::AtomicUsize::new(0);
            Mock::given(method("POST"))
                .respond_with(move |_: &Request| {
                    let i = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    responses[i.min(responses.len() - 1)].clone()
                })
                .mount(&server)
                .await;
            let provider = OpenAiCompatible::new(OpenAiConfig {
                base_url: format!("{}/", server.uri()),
                api_key: None,
                model: "m".into(),
                output_mode: OutputMode::JsonSchema,
                headers: Vec::new(),
                extra_body: serde_json::Map::new(),
                http: Default::default(),
            })
            .unwrap();
            let engine = Engine::new(
                Some(Arc::new(provider) as Arc<dyn Provider>),
                EngineConfig::default(),
            );
            let bundle = gadgets();
            let mut path_params = Map::new();
            path_params.insert("gadgetId".into(), json!("g1"));
            let call = OperationCall {
                operation_id: "getGadget".into(),
                path_params,
                query: Map::new(),
                headers: Map::new(),
                body: None,
            };
            let out = engine
                .generate(
                    &bundle,
                    bundle.operation("getGadget").unwrap(),
                    &call,
                    GenerationContext::default(),
                )
                .await
                .unwrap();
            let sent: Vec<Value> = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|r| serde_json::from_slice(&r.body).unwrap())
                .collect();
            assert!(sent.iter().all(|b| b["stream"] == true));
            (out, sent.len())
        })
    });
    let counted = snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(key, ..)| key.key().name() == mb_llm::CUT_REPLIES_METRIC)
        .map(|(key, _, _, value)| {
            let reason = key
                .key()
                .labels()
                .find(|l| l.key() == "reason")
                .map(|l| l.value().to_string())
                .unwrap_or_default();
            let DebugValue::Counter(n) = value else {
                panic!("not a counter");
            };
            (reason, n)
        })
        .collect();
    (out, calls, counted)
}

/// A loop is stopped and the same request sent once more, which answers.
#[test]
fn a_looping_answer_is_stopped_and_the_retry_serves() {
    let (out, calls, counted) = served(vec![looping(), answering()]);
    assert_eq!(out.source, Source::Llm, "{:?}", out.fallback_reason);
    assert_eq!(out.body, Some(json!({"id": "g1", "label": "Gadget"})));
    assert_eq!(calls, 2);
    // The stopped attempt is charged (estimated: it never reached its usage
    // chunk), far below the 16,384 tokens it was heading for.
    assert!(
        out.usage.completion_tokens > 8000 && out.usage.completion_tokens < 9500,
        "{:?}",
        out.usage
    );
    assert_eq!(counted, vec![("repetition".to_string(), 1)]);
}

/// A retry that loops too falls back, at once: no repair round is asked
/// for the looped text, and no third call is made.
#[test]
fn an_answer_that_keeps_looping_falls_back_with_the_reason() {
    let (out, calls, counted) = served(vec![looping()]);
    assert_eq!(out.source, Source::Fallback);
    assert_eq!(calls, 2);
    let reason = out.fallback_reason.unwrap_or_default();
    assert!(
        reason.starts_with("LLM output repeated itself: provider Loopy, aborted after about "),
        "{reason}"
    );
    assert!(
        out.usage.completion_tokens > 16_000 && out.usage.completion_tokens < 19_000,
        "{:?}",
        out.usage
    );
    assert_eq!(counted, vec![("repetition".to_string(), 2)]);
}
