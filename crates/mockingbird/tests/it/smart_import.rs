//! P6 through a built app: a Markdown import answered by a wiremock
//! OpenAI-compatible LLM, reviewed, approved, activated and served.
#![cfg(test)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use mockingbird::config::{App, LlmEnv, ServeArgs, build_app};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MOCKS: &str = "/api/v1/workspaces/default/mocks";

fn petstore_md() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/smart/petstore.md");
    std::fs::read_to_string(p).unwrap()
}

/// The extraction answer for `petstore.md`: three operations whose
/// evidence quotes the document.
fn extraction() -> Value {
    json!({"operations": [
        {"method": "GET", "path": "/pets", "summary": "List pets", "confidence": 0.9,
         "evidence": "Returns all pets, newest first.",
         "parameters": [{"name": "limit", "in": "query", "schema": {"type": "integer"}}],
         "responses": [{"status": 200, "example": [{"id": 1, "name": "Rex", "tag": "dog"}]}]},
        {"method": "GET", "path": "/pets/{petId}", "summary": "Get a pet", "confidence": 0.9,
         "evidence": "Returns the pet with the given id",
         "parameters": [{"name": "petId", "in": "path", "schema": {"type": "integer"}}],
         "responses": [{"status": 200, "example": {"id": 1, "name": "Rex", "tag": "dog"}},
                       {"status": 404, "description": "No such pet"}]},
        {"method": "POST", "path": "/pets", "summary": "Create a pet", "confidence": 0.9,
         "evidence": "Creates a pet. The request body is JSON:",
         "request": {"example": {"name": "Rex", "tag": "dog"}},
         "responses": [{"status": 201, "example": {"id": 3, "name": "Rex", "tag": "dog"}}]}
    ]})
}

fn completion(content: &str) -> Value {
    json!({"choices": [{"message": {"content": content}}],
           "usage": {"prompt_tokens": 1200, "completion_tokens": 300}})
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    format!("http://{addr}")
}

async fn send(req: reqwest::RequestBuilder, expected: u16) -> Value {
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    assert_eq!(status, expected, "{text}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

#[tokio::test]
async fn markdown_import_end_to_end() {
    let llm = MockServer::start().await;
    // Extraction prompts carry the document between the chunk markers.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("<<<DOC-"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completion(&extraction().to_string())),
        )
        .with_priority(1)
        .mount(&llm)
        .await;
    // Anything else (response generation on the data plane) gets a pet.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion(
            &json!([{"id": 1, "name": "Rex", "tag": "dog"}]).to_string(),
        )))
        .with_priority(2)
        .mount(&llm)
        .await;

    let args = ServeArgs {
        host: Some("127.0.0.1".parse().unwrap()),
        port: Some(0),
        llm_timeout: Some("5s".parse().unwrap()),
        ..ServeArgs::default()
    };
    let env = LlmEnv {
        base_url: llm.uri(),
        model: "m".into(),
        api_key: Some("k".into()),
        json_schema: false,
    };
    let app: App = build_app(&args, Some(env)).await.unwrap();
    let data = serve(app.data.clone()).await;
    let admin = serve(app.admin.clone()).await;
    let http = reqwest::Client::new();

    // A mock `docs` without a contract.
    send(
        http.post(format!("{admin}{MOCKS}"))
            .json(&json!({"name": "docs"})),
        201,
    )
    .await;

    // The import job.
    let job = send(
        http.post(format!("{admin}{MOCKS}/docs/imports"))
            .json(&json!({"kind": "markdown", "text": petstore_md()})),
        202,
    )
    .await;
    let job_id = job["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    let job = loop {
        let job = send(
            http.get(format!("{admin}{MOCKS}/docs/imports/{job_id}")),
            200,
        )
        .await;
        match job["status"].as_str().unwrap() {
            "done" => break job,
            "queued" | "running" => {}
            other => panic!("job ended {other}: {job}"),
        }
        assert!(Instant::now() < deadline, "job not done in 10 s: {job}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let version = job["result_version"].as_i64().unwrap();
    let contract = format!("{admin}{MOCKS}/docs/contracts/{version}");

    // The draft: three operations, each with provenance from the document.
    let draft = send(http.get(format!("{contract}/draft")), 200).await;
    assert_eq!(draft["status"], "draft");
    let ops = draft["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 3, "{draft}");
    let normalized: String = petstore_md()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for op in ops {
        let provenance = op["provenance"].as_array().unwrap();
        assert!(!provenance.is_empty(), "{op}");
        for p in provenance {
            let excerpt = p["excerpt"].as_str().unwrap();
            assert!(normalized.contains(excerpt), "{excerpt:?} not in the doc");
        }
    }

    // An edit.
    let mut doc = draft["document"].clone();
    doc["paths"]["/pets"]["get"]["summary"] = json!("List every pet");
    let edited = send(
        http.put(format!("{contract}/draft"))
            .json(&json!({"document": doc})),
        200,
    )
    .await;
    assert_eq!(edited["meta"]["edited"], true);
    assert_eq!(
        edited["document"]["paths"]["/pets"]["get"]["summary"],
        "List every pet"
    );

    // Approve, then activate.
    let approved = send(
        http.post(format!("{contract}/approve"))
            .json(&json!({ "activate": false })),
        200,
    )
    .await;
    assert_eq!(approved["status"], "approved");
    assert_eq!(approved["imported"], true);
    send(
        http.post(format!("{contract}/activate")).json(&json!({})),
        200,
    )
    .await;

    // The data plane serves the imported contract.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = http.get(format!("{data}/docs/pets")).send().await.unwrap();
        if r.status() == 200 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "GET /docs/pets: {} {}",
            r.status(),
            r.text().await.unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The audit log.
    let events = send(
        http.get(format!("{admin}/api/v1/workspaces/default/audit?limit=100")),
        200,
    )
    .await;
    let actions: Vec<&str> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    for action in [
        "import.create",
        "import.complete",
        "contract.draft_update",
        "contract.approve",
        "contract.activate",
    ] {
        assert!(actions.contains(&action), "{action} missing: {actions:?}");
    }
}
