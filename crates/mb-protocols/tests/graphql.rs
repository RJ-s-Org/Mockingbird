#![cfg(feature = "graphql")]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_mcm::Bundle;
use mb_protocols::{MockRuntime, RequestRecord, RequestRecorder, graphql};
use mb_state::MemoryStore;
use serde_json::{Value, json};

fn github() -> Arc<Bundle> {
    let path = format!(
        "{}/../../fixtures/graphql/github.graphql",
        env!("CARGO_MANIFEST_DIR")
    );
    Arc::new(compile(&std::fs::read(path).unwrap()).unwrap())
}

fn runtime(provider: Option<Arc<ScriptedProvider>>) -> MockRuntime {
    let dyn_provider = provider.map(|p| p as Arc<dyn Provider>);
    MockRuntime::new(
        "github",
        github(),
        Arc::new(Engine::new(dyn_provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

fn scripted(script: Vec<Value>) -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ))
}

/// Serves `rt` on an ephemeral port; returns its base URL.
async fn serve(rt: MockRuntime) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = graphql::try_router(Arc::new(rt)).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

struct Answer {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Value,
}

async fn answer(resp: reqwest::Response) -> Answer {
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Answer {
        status,
        headers,
        body,
    }
}

async fn post(base: &str, body: Value, headers: &[(&str, &str)]) -> Answer {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/graphql"))
        .header("content-type", "application/json")
        .body(body.to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    answer(req.send().await.unwrap()).await
}

fn get_url(base: &str, params: &[(&str, &str)]) -> reqwest::Url {
    reqwest::Url::parse_with_params(&format!("{base}/graphql"), params).unwrap()
}

async fn query(base: &str, q: &str) -> Answer {
    post(base, json!({ "query": q }), &[]).await
}

fn user(login: &str) -> Value {
    json!({
        "__typename": "User",
        "id": format!("u-{login}"),
        "login": login,
        "name": null,
        "createdAt": "2024-01-01T00:00:00Z",
        "repositories": [],
    })
}

fn repository(name: &str) -> Value {
    json!({
        "__typename": "Repository",
        "id": format!("r-{name}"),
        "name": name,
        "owner": user("octocat"),
        "isPrivate": false,
        "stargazerCount": 42,
        "issues": [],
    })
}

fn issue(id: &str, title: &str) -> Value {
    json!({
        "__typename": "Issue",
        "id": id,
        "number": 7,
        "title": title,
        "body": null,
        "state": "OPEN",
        "author": null,
        "repository": repository("hello"),
        "createdAt": "2024-01-01T00:00:00Z",
        "labels": ["bug"],
    })
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[derive(Default)]
struct Records(Mutex<Vec<RequestRecord>>);

impl RequestRecorder for Records {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

#[tokio::test]
async fn query_with_variables_fragments_aliases() {
    let provider = scripted(vec![repository("one"), repository("one")]);
    let base = serve(runtime(Some(provider))).await;
    let q = r#"
        query Q($owner: String!, $name: String!) {
          a: repository(owner: $owner, name: $name) { ...Repo }
          b: repository(owner: "x", name: "y") { name stargazerCount }
        }
        fragment Repo on Repository { id name owner { login } }
    "#;
    let r = post(
        &base,
        json!({"query": q, "variables": {"owner": "octocat", "name": "one"}, "operationName": "Q"}),
        &[],
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.get("errors").is_none(), "{}", r.body);
    let data = &r.body["data"];
    assert_eq!(keys(&data["a"]), set(&["id", "name", "owner"]));
    assert_eq!(keys(&data["a"]["owner"]), set(&["login"]));
    assert_eq!(data["a"]["owner"]["login"], "octocat");
    assert_eq!(keys(&data["b"]), set(&["name", "stargazerCount"]));
    assert_eq!(data["b"]["stargazerCount"], 42);
}

#[tokio::test]
async fn one_engine_call_per_root_field() {
    let provider = scripted(vec![user("a"), user("b")]);
    let base = serve(runtime(Some(provider.clone()))).await;
    let r = query(
        &base,
        "{ first: viewer { login } second: viewer { login name } }",
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.get("errors").is_none(), "{}", r.body);
    assert_eq!(provider.requests().len(), 2);
    let logins: BTreeSet<String> = ["first", "second"]
        .iter()
        .map(|k| r.body["data"][k]["login"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(logins, set(&["a", "b"]));
}

#[tokio::test]
async fn mutation_then_query_reflects_state() {
    let provider = scripted(vec![
        issue("i-1", "Crash on start"),
        issue("i-1", "Something else"),
    ]);
    let base = serve(runtime(Some(provider))).await;
    let session = [("x-mockingbird-session", "s1")];
    let created = post(
        &base,
        json!({
            "query": "mutation($in: CreateIssueInput!) { createIssue(input: $in) { id title } }",
            "variables": {"in": {"repositoryId": "r-1", "title": "Crash on start"}},
        }),
        &session,
    )
    .await;
    assert_eq!(created.status, 200);
    assert!(created.body.get("errors").is_none(), "{}", created.body);
    let made = &created.body["data"]["createIssue"];
    let id = made["id"].as_str().unwrap();
    let title = made["title"].as_str().unwrap();

    let read = post(
        &base,
        json!({"query": "query($id: ID!) { issue(id: $id) { id title } }", "variables": {"id": id}}),
        &session,
    )
    .await;
    assert!(read.body.get("errors").is_none(), "{}", read.body);
    assert_eq!(read.body["data"]["issue"]["id"], id);
    assert_eq!(read.body["data"]["issue"]["title"], title);
}

#[tokio::test]
async fn introspection_returns_schema() {
    let base = serve(runtime(None)).await;
    let r = query(
        &base,
        "{ __schema { queryType { name } mutationType { name } types { name } } }",
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.get("errors").is_none(), "{}", r.body);
    let schema = &r.body["data"]["__schema"];
    assert_eq!(schema["queryType"]["name"], "Query");
    assert_eq!(schema["mutationType"]["name"], "Mutation");
    let names: BTreeSet<String> = schema["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for want in [
        "Repository",
        "Issue",
        "User",
        "Node",
        "SearchResult",
        "IssueState",
        "CreateIssueInput",
        "DateTime",
    ] {
        assert!(names.contains(want), "missing {want}");
    }
}

#[tokio::test]
async fn introspection_disabled_by_setting() {
    let mut rt = runtime(None);
    rt.protocols.graphql_introspection = false;
    let base = serve(rt).await;
    let r = query(&base, "{ __schema { queryType { name } } }").await;
    assert_eq!(r.status, 200);
    assert!(
        !r.body["errors"].as_array().unwrap().is_empty(),
        "{}",
        r.body
    );
    assert!(r.body["data"]["__schema"].is_null());
}

#[tokio::test]
async fn forced_status_maps_to_error_code() {
    let provider = scripted(vec![]);
    let base = serve(runtime(Some(provider.clone()))).await;
    let r = post(
        &base,
        json!({"query": r#"{ repository(owner: "a", name: "b") { name } }"#}),
        &[("x-mockingbird-status", "404")],
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["data"], json!({"repository": null}), "{}", r.body);
    let error = &r.body["errors"][0];
    assert_eq!(error["extensions"]["code"], "NOT_FOUND");
    assert_eq!(error["extensions"]["status"], 404);
    assert_eq!(error["path"], json!(["repository"]), "{}", r.body);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn query_depth_and_complexity_limits() {
    let provider = scripted(vec![]);
    let mut rt = runtime(Some(provider.clone()));
    // The root-field cap would refuse the wide query first.
    rt.protocols.graphql_max_root_fields = 1000;
    let base = serve(rt).await;

    // viewer, then 16 more levels: 17 deep.
    let deep = format!(
        "{{ viewer {{ {} login {} }} }}",
        "repositories { owner { ".repeat(8),
        "} } ".repeat(8)
    );
    let r = query(&base, &deep).await;
    assert_eq!(r.status, 200);
    let message = r.body["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("deep"), "{message}");

    // 501 aliased `viewer { login }`: complexity 1002.
    let wide: String = (0..501)
        .map(|i| format!("v{i}: viewer {{ login }} "))
        .collect();
    let r = query(&base, &format!("{{ {wide} }}")).await;
    assert_eq!(r.status, 200);
    let message = r.body["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("complex"), "{message}");

    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn sources_in_extensions_and_header() {
    // The first engine call gets a valid answer; the second finds the
    // script exhausted and falls back.
    let provider = scripted(vec![user("a")]);
    let base = serve(runtime(Some(provider))).await;
    let r = query(&base, "{ x: viewer { login } y: viewer { login } }").await;
    assert_eq!(r.status, 200);
    assert!(r.body.get("errors").is_none(), "{}", r.body);
    let sources = &r.body["extensions"]["mockingbird"]["sources"];
    assert_eq!(keys(sources), set(&["x", "y"]));
    let values: BTreeSet<String> = ["x", "y"]
        .iter()
        .map(|k| sources[k].as_str().unwrap().to_string())
        .collect();
    assert_eq!(values, set(&["llm", "fallback"]));
    assert_eq!(r.headers["x-mockingbird-source"], "fallback");
    assert_eq!(r.headers["content-type"], "application/json");
}

#[tokio::test]
async fn get_query_supported_and_get_mutation_rejected() {
    let base = serve(runtime(None)).await;
    let client = reqwest::Client::new();
    let r = answer(
        client
            .get(get_url(
                &base,
                &[
                    ("query", "query V { viewer { login } }"),
                    ("operationName", "V"),
                ],
            ))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body["data"]["viewer"]["login"].is_string(), "{}", r.body);

    let r = answer(
        client
            .get(get_url(
                &base,
                &[("query", r#"mutation { deleteIssue(id: "i-1") { id } }"#)],
            ))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, 405);
    assert_eq!(r.headers["content-type"], "application/problem+json");
    assert_eq!(r.body["status"], 405);
}

#[tokio::test]
async fn invalid_query_returns_graphql_errors() {
    let base = serve(runtime(None)).await;
    for q in ["{ viewer { login ", "{ nope }"] {
        let r = query(&base, q).await;
        assert_eq!(r.status, 200, "{q}");
        assert!(r.body["errors"][0]["message"].is_string(), "{}", r.body);
    }
}

#[tokio::test]
async fn raw_graphql_body_accepted() {
    let base = serve(runtime(None)).await;
    let r = answer(
        reqwest::Client::new()
            .post(format!("{base}/graphql"))
            .header("content-type", "application/graphql")
            .body("{ viewer { login } }")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body["data"]["viewer"]["login"].is_string(), "{}", r.body);
}

#[tokio::test]
async fn http_errors_are_problems() {
    let base = serve(runtime(None)).await;
    let client = reqwest::Client::new();

    let r = answer(client.get(format!("{base}/other")).send().await.unwrap()).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.headers["content-type"], "application/problem+json");

    let r = answer(client.put(format!("{base}/graphql")).send().await.unwrap()).await;
    assert_eq!(r.status, 405);
    assert_eq!(r.headers["content-type"], "application/problem+json");

    let r = answer(
        client
            .post(format!("{base}/graphql"))
            .header("content-type", "application/json")
            .body("not json")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.headers["content-type"], "application/problem+json");

    let big =
        json!({"query": "{ viewer { login } }", "variables": {"pad": "x".repeat(1024 * 1024)}});
    let r = post(&base, big, &[]).await;
    assert_eq!(r.status, 413);
    assert_eq!(r.headers["content-type"], "application/problem+json");
}

#[tokio::test]
async fn one_report_per_root_field() {
    let records = Arc::new(Records::default());
    let mut rt = runtime(None);
    rt.recorder = Some(records.clone());
    let base = serve(rt).await;
    let r = post(
        &base,
        json!({
            "query": "query($o: String!) { viewer { login } repository(owner: $o, name: \"n\") { name } }",
            "variables": {"o": "octocat"},
        }),
        &[("x-mockingbird-session", "s9")],
    )
    .await;
    assert_eq!(r.status, 200);
    let recs = records.0.lock().unwrap();
    assert_eq!(recs.len(), 2);
    let mut ops: Vec<&str> = recs
        .iter()
        .map(|r| r.operation_id.as_deref().unwrap())
        .collect();
    ops.sort_unstable();
    assert_eq!(ops, ["Query.repository", "Query.viewer"]);
    for rec in recs.iter() {
        assert_eq!(rec.method, "POST");
        assert_eq!(rec.path, "/graphql");
        assert_eq!(rec.status, 200);
        assert_eq!(rec.session, "s9");
        assert_eq!(rec.source.as_deref(), Some("fallback"));
        assert_eq!(rec.request["variables"], json!({"o": "octocat"}));
        assert_eq!(rec.request["headers"]["x-mockingbird-session"], "s9");
    }
    let repo = recs
        .iter()
        .find(|r| r.operation_id.as_deref() == Some("Query.repository"))
        .unwrap();
    assert_eq!(
        repo.request["args"],
        json!({"owner": "octocat", "name": "n"})
    );
}

#[tokio::test]
async fn missing_sdl_is_an_error() {
    let path = format!(
        "{}/../../fixtures/openapi/orders-3.0.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let bundle = Arc::new(compile(&std::fs::read(path).unwrap()).unwrap());
    let rt = MockRuntime::new(
        "orders",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    assert!(matches!(
        graphql::try_router(Arc::new(rt)),
        Err(graphql::GraphqlError::MissingSdl)
    ));
}

impl Records {
    /// Waits up to 2 s for at least `n` records and returns them all.
    async fn wait(&self, n: usize) -> Vec<RequestRecord> {
        for _ in 0..100 {
            let got = self.0.lock().unwrap().clone();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.0.lock().unwrap().clone()
    }
}

/// `{ a0: viewer { login } … }` with `n` aliases.
fn aliased_viewers(n: usize) -> String {
    let fields: Vec<String> = (0..n)
        .map(|i| format!("a{i}: viewer {{ login }}"))
        .collect();
    format!("{{ {} }}", fields.join(" "))
}

#[tokio::test]
async fn root_field_cap_rejects_before_engine() {
    let provider = scripted((0..21).map(|i| user(&format!("u{i}"))).collect());
    let records = Arc::new(Records::default());
    let mut rt = runtime(Some(provider.clone()));
    rt.recorder = Some(records.clone());
    let base = serve(rt).await;

    let r = query(&base, &aliased_viewers(21)).await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(
        r.body,
        json!({"errors": [{
            "message": "too many root fields: 21 (maximum 20)",
            "extensions": {"code": "TOO_MANY_ROOT_FIELDS"},
        }]})
    );
    assert!(provider.requests().is_empty());
    let recs = records.wait(1).await;
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!(recs[0].status, 400);
    assert_eq!(recs[0].path, "/graphql");

    // At the cap, every field runs.
    let r = query(&base, &aliased_viewers(20)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.get("errors").is_none(), "{}", r.body);
    assert_eq!(provider.requests().len(), 20);
}

#[tokio::test]
async fn fragment_spread_counts_toward_cap() {
    let mut rt = runtime(None);
    rt.protocols.graphql_max_root_fields = 2;
    let base = serve(rt).await;

    let spread = "query { ...Two c: viewer { login } } \
                  fragment Two on Query { a: viewer { login } b: viewer { login } }";
    let r = query(&base, spread).await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(
        r.body["errors"][0]["message"],
        "too many root fields: 3 (maximum 2)"
    );
    assert_eq!(
        r.body["errors"][0]["extensions"]["code"],
        "TOO_MANY_ROOT_FIELDS"
    );

    let inline = "{ ... on Query { a: viewer { login } b: viewer { login } } c: viewer { login } }";
    let r = query(&base, inline).await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(
        r.body["errors"][0]["message"],
        "too many root fields: 3 (maximum 2)"
    );

    // Only the selected operation counts.
    let named = json!({
        "query": "query Small { viewer { login } } query Big { a: viewer { login } b: viewer { login } c: viewer { login } }",
        "operationName": "Small",
    });
    let r = post(&base, named, &[]).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body["data"]["viewer"]["login"].is_string(), "{}", r.body);
}

// ------------------------------------------------------- WebSocket caps

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const SUBSCRIPTION: &str = r#"subscription { issueAdded(repositoryId: "r-1") { id title } }"#;

fn ws_request(
    base: &str,
    protocol: &str,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let url = format!("{}/graphql", base.replacen("http://", "ws://", 1));
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("sec-websocket-protocol", protocol.parse().unwrap());
    req
}

async fn ws_send(ws: &mut Ws, msg: Value) {
    use futures::SinkExt;
    ws.send(tokio_tungstenite::tungstenite::Message::text(
        msg.to_string(),
    ))
    .await
    .unwrap();
}

/// The next text message as JSON, skipping pings and pongs.
async fn ws_recv(ws: &mut Ws) -> Value {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out")
            .unwrap()
            .unwrap();
        match msg {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// The next message of `ty` for operation `id`, skipping the others.
async fn ws_recv_for(ws: &mut Ws, ty: &str, id: &str) -> Value {
    loop {
        let msg = ws_recv(ws).await;
        if msg["type"] == ty && msg["id"] == id {
            return msg;
        }
    }
}

/// An initialised connection speaking `protocol`.
async fn ws_init(base: &str, protocol: &str) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_request(base, protocol))
        .await
        .unwrap();
    ws_send(
        &mut ws,
        json!({"type": "connection_init", "payload": {"x-mockingbird-stream-items": "1"}}),
    )
    .await;
    assert_eq!(ws_recv(&mut ws).await["type"], "connection_ack");
    ws
}

#[tokio::test]
async fn subscription_cap_per_connection() {
    // (subprotocol, subscribe type, stop type, item type)
    let protocols = [
        ("graphql-transport-ws", "subscribe", "complete", "next"),
        ("graphql-ws", "start", "stop", "data"),
    ];
    for (protocol, subscribe, stop, item) in protocols {
        let records = Arc::new(Records::default());
        let mut rt = runtime(None);
        rt.protocols.graphql_max_subscriptions_per_connection = 2;
        rt.recorder = Some(records.clone());
        let base = serve(rt).await;
        let mut ws = ws_init(&base, protocol).await;
        let start =
            |id: &str| json!({"type": subscribe, "id": id, "payload": {"query": SUBSCRIPTION}});
        for id in ["1", "2"] {
            ws_send(&mut ws, start(id)).await;
            ws_recv_for(&mut ws, item, id).await;
        }
        ws_send(&mut ws, start("3")).await;
        let refused = ws_recv_for(&mut ws, "error", "3").await;
        let message = match protocol {
            "graphql-transport-ws" => &refused["payload"][0]["message"],
            _ => &refused["payload"]["message"],
        };
        assert_eq!(
            message, "too many subscriptions on this connection (maximum 2)",
            "{protocol}: {refused}"
        );
        // The connection stays open: stopping one makes room for another.
        ws_send(&mut ws, json!({"type": stop, "id": "1"})).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        ws_send(&mut ws, start("4")).await;
        ws_recv_for(&mut ws, item, "4").await;
        let recs = records.wait(2).await;
        assert!(
            recs.iter()
                .any(|r| r.status == 429 && r.operation_id.is_none()),
            "{protocol}: {recs:?}"
        );
    }
}

#[tokio::test]
async fn connection_cap_per_mock_503() {
    let records = Arc::new(Records::default());
    let mut rt = runtime(None);
    rt.protocols.graphql_max_connections_per_mock = 2;
    rt.recorder = Some(records.clone());
    let base = serve(rt).await;
    let first = ws_init(&base, "graphql-transport-ws").await;
    let _second = ws_init(&base, "graphql-transport-ws").await;

    let err = tokio_tungstenite::connect_async(ws_request(&base, "graphql-transport-ws"))
        .await
        .unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(resp) = err else {
        panic!("expected an HTTP refusal, got {err:?}");
    };
    assert_eq!(resp.status().as_u16(), 503);
    assert_eq!(resp.headers()["retry-after"], "1");
    assert_eq!(resp.headers()["content-type"], "application/problem+json");
    let body: Value = serde_json::from_slice(resp.body().as_deref().unwrap()).unwrap();
    assert_eq!(body["detail"], "too many GraphQL WebSocket connections");
    let recs = records.wait(1).await;
    assert!(recs.iter().any(|r| r.status == 503), "{recs:?}");

    // Closing a connection releases its slot.
    drop(first);
    let mut reopened = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok((ws, _)) =
            tokio_tungstenite::connect_async(ws_request(&base, "graphql-transport-ws")).await
        {
            reopened = Some(ws);
            break;
        }
    }
    assert!(reopened.is_some(), "the slot was not released");
}

/// The GitHub bundle with every operation requiring a bearer token.
fn bearer_github() -> Arc<Bundle> {
    let mut bundle = (*github()).clone();
    for op in &mut bundle.operations {
        op.security = vec![mb_mcm::SecurityRequirement {
            schemes: vec![mb_mcm::SecurityScheme::Bearer],
        }];
    }
    Arc::new(bundle)
}

#[tokio::test]
async fn rejections_are_recorded() {
    use mb_protocols::{DataPlaneAuth, DataPlaneAuthMode};
    let client = reqwest::Client::new();
    let recorded = || {
        let records = Arc::new(Records::default());
        let mut rt = runtime(None);
        rt.recorder = Some(records.clone());
        (records, rt)
    };

    // 400: an unparseable body.
    let (records, rt) = recorded();
    let base = serve(rt).await;
    let r = answer(
        client
            .post(format!("{base}/graphql"))
            .header("content-type", "application/json")
            .body("not json")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, 400);
    let recs = records.wait(1).await;
    assert_eq!(recs.len(), 1, "400: {recs:?}");
    assert_eq!(recs[0].status, 400);

    // 405: an unsupported method, and a mutation over GET.
    let (records, rt) = recorded();
    let base = serve(rt).await;
    let r = answer(client.put(format!("{base}/graphql")).send().await.unwrap()).await;
    assert_eq!(r.status, 405);
    let url = get_url(&base, &[("query", "mutation { a }")]);
    let r = answer(client.get(url).send().await.unwrap()).await;
    assert_eq!(r.status, 405);
    let recs = records.wait(2).await;
    assert_eq!(recs.len(), 2, "405: {recs:?}");
    assert!(recs.iter().all(|r| r.status == 405), "{recs:?}");

    // 413: an oversized body.
    let (records, rt) = recorded();
    let base = serve(rt).await;
    let big =
        json!({"query": "{ viewer { login } }", "variables": {"pad": "x".repeat(1024 * 1024)}});
    assert_eq!(post(&base, big, &[]).await.status, 413);
    let recs = records.wait(1).await;
    assert_eq!(recs.len(), 1, "413: {recs:?}");
    assert_eq!(recs[0].status, 413);

    // 401, api_key mode over HTTP: no key.
    let (records, mut rt) = recorded();
    rt.set_data_plane_auth(DataPlaneAuth {
        mode: DataPlaneAuthMode::ApiKey,
        header: "x-api-key".into(),
    });
    let base = serve(rt).await;
    assert_eq!(query(&base, "{ viewer { login } }").await.status, 401);
    let recs = records.wait(1).await;
    assert_eq!(recs.len(), 1, "401 api_key: {recs:?}");
    assert_eq!(recs[0].status, 401);

    // 401, passthrough mode over HTTP: no bearer token.
    let records = Arc::new(Records::default());
    let mut rt = MockRuntime::new(
        "github",
        bearer_github(),
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    rt.recorder = Some(records.clone());
    rt.set_data_plane_auth(DataPlaneAuth {
        mode: DataPlaneAuthMode::PassthroughValidate,
        header: "x-api-key".into(),
    });
    let base = serve(rt).await;
    let r = query(&base, "{ viewer { login } }").await;
    assert_eq!(
        r.body["errors"][0]["extensions"]["code"], "UNAUTHENTICATED",
        "{}",
        r.body
    );
    let recs = records.wait(1).await;
    assert_eq!(recs.len(), 1, "401 passthrough: {recs:?}");
    assert_eq!(recs[0].status, 401);

    // 401 over WebSocket: `connection_init` without a key.
    let (records, mut rt) = recorded();
    rt.set_data_plane_auth(DataPlaneAuth {
        mode: DataPlaneAuthMode::ApiKey,
        header: "x-api-key".into(),
    });
    let base = serve(rt).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_request(&base, "graphql-ws"))
        .await
        .unwrap();
    ws_send(&mut ws, json!({"type": "connection_init", "payload": {}})).await;
    assert_eq!(ws_recv(&mut ws).await["type"], "connection_error");
    let recs = records.wait(1).await;
    assert_eq!(recs.len(), 1, "401 ws: {recs:?}");
    assert_eq!(recs[0].status, 401);
}
