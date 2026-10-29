#![cfg(test)]
#![cfg(feature = "graphql")]

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

/// Sends only the head of a POST declaring a `len`-byte body and returns the
/// answer's status and its lowercased head. The server refuses an oversized body from its
/// `Content-Length` and closes the connection, so a client still uploading the
/// body can get a reset instead of the 413.
async fn post_oversized_head(base: &str, len: usize) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /graphql HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {len}\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("no answer to an oversized request head")
        .unwrap();
    let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head)
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
    let provider = scripted(vec![
        json!({"a": repository("one"), "b": repository("one")}),
    ]);
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
async fn aliases_are_answered_in_one_joint_call() {
    let provider = scripted(vec![json!({"first": user("a"), "second": user("b")})]);
    let base = serve(runtime(Some(provider.clone()))).await;
    let r = query(
        &base,
        "{ first: viewer { login } second: viewer { login name } }",
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.get("errors").is_none(), "{}", r.body);
    assert_eq!(provider.requests().len(), 1);
    let logins: BTreeSet<String> = ["first", "second"]
        .iter()
        .map(|k| r.body["data"][k]["login"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(logins, set(&["a", "b"]));
}

/// Two aliases of one field with different arguments are separate
/// properties of the prompt and get separate values.
#[tokio::test]
async fn aliases_with_different_arguments_get_different_values() {
    let repo = |n: &str| json!({"__typename": "Repository", "name": n});
    let provider = scripted(vec![json!({
        "__typename": "User",
        "login": "octocat",
        "small": [repo("one")],
        "large": [repo("one"), repo("two")],
    })]);
    let base = serve(runtime(Some(provider.clone()))).await;
    let r = query(
        &base,
        "{ viewer { login small: repositories(first: 1) { name } large: repositories(first: 5) { name } } }",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        r.headers["x-mockingbird-source"],
        "llm",
        "{:?}",
        r.headers.get("x-mockingbird-fallback-reason")
    );
    assert_eq!(r.body["data"]["viewer"]["small"], json!([{"name": "one"}]));
    assert_eq!(
        r.body["data"]["viewer"]["large"],
        json!([{"name": "one"}, {"name": "two"}])
    );
    let system = &provider.requests()[0].messages[0].content;
    assert!(
        system.contains(r#"{\"first\":1}"#) && system.contains(r#"{\"first\":5}"#),
        "{system}"
    );
}

/// Aliases with identical arguments share one generated value; the model
/// is not asked for the second one.
#[tokio::test]
async fn aliases_with_identical_arguments_share_one_value() {
    let provider = scripted(vec![json!({
        "__typename": "User",
        "a": [{"__typename": "Repository", "name": "x", "stargazerCount": 7}],
    })]);
    let base = serve(runtime(Some(provider))).await;
    let r = query(
        &base,
        "{ viewer { a: repositories(first: 2) { name } b: repositories(first: 2) { stargazerCount } } }",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.headers["x-mockingbird-source"], "llm");
    assert_eq!(r.body["data"]["viewer"]["a"], json!([{"name": "x"}]));
    assert_eq!(
        r.body["data"]["viewer"]["b"],
        json!([{"stargazerCount": 7}])
    );
}

/// Swapped names in a narrowed read keep their own fields' values.
#[tokio::test]
async fn swapped_aliases_in_a_read_keep_their_fields() {
    let provider = scripted(vec![
        json!({"__typename": "Issue", "title": null, "body": "T"}),
    ]);
    let base = serve(runtime(Some(provider))).await;
    let r = query(&base, r#"{ issue(id: "I_1") { title: body body: title } }"#).await;
    assert_eq!(r.headers["x-mockingbird-source"], "llm", "{}", r.body);
    assert_eq!(r.body["data"]["issue"], json!({"title": null, "body": "T"}));
}

/// A mutation answered for its selection serves swapped aliases, then an
/// aliased read of the stored issue reflects it.
#[tokio::test]
async fn aliases_after_a_mutation_reflect_state() {
    let provider = scripted(vec![
        // Keyed by response key: `title` reads `body`, `body` reads `title`.
        json!({"__typename": "Issue", "id": "i-1", "title": null, "body": "Crash on start"}),
        json!({"__typename": "Issue", "ref": "other", "heading": "Something else"}),
    ]);
    let base = serve(runtime(Some(provider))).await;
    let session = [("x-mockingbird-session", "s1")];
    let created = post(
        &base,
        json!({
            "query": "mutation($in: CreateIssueInput!) { createIssue(input: $in) { id title: body body: title } }",
            "variables": {"in": {"repositoryId": "r-1", "title": "Crash on start"}},
        }),
        &session,
    )
    .await;
    assert!(created.body.get("errors").is_none(), "{}", created.body);
    let made = &created.body["data"]["createIssue"];
    assert_eq!(made["title"], Value::Null);
    assert_eq!(made["body"], "Crash on start");
    let id = made["id"].as_str().unwrap();
    let read = post(
        &base,
        json!({
            "query": "query($id: ID!) { issue(id: $id) { ref: id heading: title } }",
            "variables": {"id": id},
        }),
        &session,
    )
    .await;
    assert!(read.body.get("errors").is_none(), "{}", read.body);
    assert_eq!(
        read.body["data"]["issue"],
        json!({"ref": id, "heading": "Crash on start"})
    );
}

/// A stored issue read with swapped and nested aliases: each key carries
/// its own field of the stored resource.
#[tokio::test]
async fn swapped_aliases_on_a_stored_read_reflect_state() {
    let mut created = issue("i-1", "Crash on start");
    created["body"] = json!("Stack trace");
    let provider = scripted(vec![
        created,
        json!({
            "__typename": "Issue",
            "title": "model body",
            "body": "model title",
            "repository": {"__typename": "Repository", "n": "model", "owner": {"__typename": "User", "name": "model"}},
        }),
    ]);
    let base = serve(runtime(Some(provider))).await;
    let session = [("x-mockingbird-session", "s1")];
    let created = post(
        &base,
        json!({
            "query": "mutation($in: CreateIssueInput!) { createIssue(input: $in) { id } }",
            "variables": {"in": {"repositoryId": "r-1", "title": "Crash on start"}},
        }),
        &session,
    )
    .await;
    assert!(created.body.get("errors").is_none(), "{}", created.body);
    let id = created.body["data"]["createIssue"]["id"].as_str().unwrap();
    let read = post(
        &base,
        json!({
            "query": "query($id: ID!) { issue(id: $id) { title: body body: title repository { n: name owner { name: login } } } }",
            "variables": {"id": id},
        }),
        &session,
    )
    .await;
    assert!(read.body.get("errors").is_none(), "{}", read.body);
    assert_eq!(
        read.body["data"]["issue"],
        json!({
            "title": "Stack trace",
            "body": "Crash on start",
            "repository": {"n": "hello", "owner": {"name": "octocat"}},
        })
    );
}

/// A key the model left out is null, not a sibling's same-named member.
#[tokio::test]
async fn a_missing_alias_is_null() {
    let provider = scripted(vec![json!({"__typename": "Issue", "body": "T"})]);
    let base = serve(runtime(Some(provider))).await;
    let r = query(&base, r#"{ issue(id: "I_1") { title: body body: title } }"#).await;
    assert_eq!(r.headers["x-mockingbird-source"], "llm", "{}", r.body);
    assert_eq!(r.body["data"]["issue"], json!({"title": null, "body": "T"}));
}

/// Identical-argument aliases whose sub-selections give one key different
/// fields are generated separately.
#[tokio::test]
async fn aliases_with_conflicting_sub_selections_are_not_shared() {
    let provider = scripted(vec![json!({
        "__typename": "User",
        "a": [{"__typename": "Repository", "n": "x"}],
        "b": [{"__typename": "Repository", "n": 7}],
    })]);
    let base = serve(runtime(Some(provider))).await;
    let r = query(
        &base,
        "{ viewer { a: repositories(first: 2) { n: name } b: repositories(first: 2) { n: stargazerCount } } }",
    )
    .await;
    assert_eq!(r.headers["x-mockingbird-source"], "llm", "{}", r.body);
    assert_eq!(r.body["data"]["viewer"]["a"], json!([{"n": "x"}]));
    assert_eq!(r.body["data"]["viewer"]["b"], json!([{"n": 7}]));
}

/// One response key naming different fields on different union members.
#[tokio::test]
async fn one_alias_across_union_members() {
    let provider = scripted(vec![json!([
        {"__typename": "User", "label": "octocat"},
        {"__typename": "Issue", "label": "Crash"},
    ])]);
    let base = serve(runtime(Some(provider))).await;
    let r = query(
        &base,
        r#"{ search(query: "x") { ... on Issue { label: title } ... on User { label: login } } }"#,
    )
    .await;
    assert_eq!(r.headers["x-mockingbird-source"], "llm", "{}", r.body);
    assert_eq!(
        r.body["data"]["search"],
        json!([{"label": "octocat"}, {"label": "Crash"}])
    );
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
    // The joint answer and its repair are invalid, so the fields are
    // generated separately: the first gets a valid answer, the second finds
    // the script exhausted and falls back.
    let provider = scripted(vec![json!("bad"), json!("bad"), user("a")]);
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

    let (status, head) = post_oversized_head(&base, 2 * 1024 * 1024).await;
    assert_eq!(status, 413);
    assert!(
        head.contains("\r\ncontent-type: application/problem+json\r\n"),
        "{head}"
    );
}

/// The root fields of one HTTP request are metered one by one but logged
/// as one request.
#[tokio::test]
async fn one_record_per_request() {
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
    assert_eq!(recs.len(), 1);
    let rec = &recs[0];
    assert_eq!(
        rec.operation_id.as_deref(),
        Some("Query.repository,Query.viewer")
    );
    assert_eq!(rec.method, "POST");
    assert_eq!(rec.path, "/graphql");
    assert_eq!(rec.status, 200);
    assert_eq!(rec.session, "s9");
    assert_eq!(rec.source.as_deref(), Some("fallback"));
    assert_eq!(rec.request["variables"], json!({"o": "octocat"}));
    assert_eq!(rec.request["headers"]["x-mockingbird-session"], "s9");
    assert_eq!(
        rec.request["args"]["repository"],
        json!({"owner": "octocat", "name": "n"})
    );
    assert_eq!(rec.request["args"]["viewer"], json!({}));
    assert!(rec.response.as_ref().unwrap()["data"]["viewer"]["login"].is_string());
}

/// A query with several root fields is cached per field like a single
/// one: the identical repeat makes no LLM call.
#[tokio::test]
async fn several_root_fields_are_cached() {
    let provider = scripted(vec![json!({
        "repository": {"__typename": "Repository", "name": "n"},
        "issue": {"__typename": "Issue", "title": "t"},
    })]);
    let rt = MockRuntime::new(
        "github",
        github(),
        Arc::new(Engine::new(
            Some(provider.clone() as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        mb_state::CachingStateStore::wrap(
            Arc::new(MemoryStore::new(Duration::from_secs(3600))),
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
    );
    let base = serve(rt).await;
    let q = r#"{ repository(owner: "o", name: "n") { name } issue(id: "I_1") { title } }"#;
    let first = query(&base, q).await;
    assert_eq!(
        first.headers["x-mockingbird-source"], "llm",
        "{}",
        first.body
    );
    let again = query(&base, q).await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.body["data"], first.body["data"]);
    assert_eq!(again.headers["x-mockingbird-source"], "cache");
    assert_eq!(provider.requests().len(), 1);
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
    let joint: serde_json::Map<String, Value> = (0..20)
        .map(|i| (format!("a{i}"), user(&format!("u{i}"))))
        .collect();
    let provider = scripted(vec![Value::Object(joint)]);
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

    // At the cap, every field runs (in one joint call).
    let r = query(&base, &aliased_viewers(20)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.get("errors").is_none(), "{}", r.body);
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(r.body["data"]["a19"]["login"], "u19");
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

/// A fragment chain `n` levels deep where each fragment spreads the next
/// one twice (2^n leaves), on `on`; `leaf` is the last fragment's body and
/// `root` wraps the first spread.
fn fragment_bomb(n: usize, on: &str, root: &str, leaf: &str) -> String {
    let mut doc = root.replace("{spread}", "...F0");
    for i in 0..n {
        doc.push_str(&format!(
            " fragment F{i} on {on} {{ ...F{next} ...F{next} }}",
            next = i + 1
        ));
    }
    doc.push_str(&format!(" fragment F{n} on {on} {{ {leaf} }}"));
    doc
}

/// Serves a fresh mock on its own two-worker runtime and posts `q` from
/// another thread, giving up after one second. Plain threads and channels
/// keep the clock honest even when every worker is stuck in the request;
/// a stuck server thread is abandoned rather than joined.
fn answer_within_a_second(q: String) -> Option<Answer> {
    let (base_tx, base_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let workers = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        workers.block_on(async move {
            let base = serve(runtime(Some(scripted(vec![])))).await;
            base_tx.send(base).unwrap();
            std::future::pending::<()>().await;
        });
    });
    let base = base_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (answer_tx, answer_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let client = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = answer_tx.send(client.block_on(query(&base, &q)));
    });
    answer_rx.recv_timeout(Duration::from_secs(1)).ok()
}

#[test]
fn root_fragment_bomb_answers_within_a_second() {
    // 32 levels: 2^32 root fields once expanded, about 1.2 KB of query.
    let q = fragment_bomb(32, "Query", "query { {spread} }", "viewer { login }");
    let r = answer_within_a_second(q).expect("no answer within 1 s");
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(
        r.body["errors"][0]["extensions"]["code"], "QUERY_TOO_LARGE",
        "{}",
        r.body
    );
}

#[test]
fn nested_fragment_bomb_answers_within_a_second() {
    // One root field; the bomb sits under it, where the root-field cap
    // does not look. 28 levels stays inside async-graphql's recursion
    // limit (32), which would otherwise refuse it first.
    let q = fragment_bomb(28, "User", "query { viewer { {spread} } }", "login");
    let r = answer_within_a_second(q).expect("no answer within 1 s");
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(
        r.body,
        json!({"errors": [{
            "message": "query too large: more than 10000 selections once fragments are expanded",
            "extensions": {"code": "QUERY_TOO_LARGE"},
        }]})
    );
}

/// A linear fragment chain `n` links long, each fragment spreading the
/// next once (`n + 1` fragments, nested `n + 1` deep), in compact syntax
/// with base-36 names so 30,000 links stay under the 1 MiB body limit.
fn fragment_chain(n: usize, root: &str) -> String {
    fn name(mut i: usize) -> String {
        let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut out = Vec::new();
        loop {
            out.push(digits[i % 36]);
            i /= 36;
            if i == 0 {
                break;
            }
        }
        out.reverse();
        format!("F{}", String::from_utf8(out).unwrap())
    }
    let mut doc = root.replace("{spread}", &format!("...{}", name(0)));
    for i in 0..n {
        doc.push_str(&format!(
            "fragment {} on User{{...{}}}",
            name(i),
            name(i + 1)
        ));
    }
    doc.push_str(&format!("fragment {} on User{{login}}", name(n)));
    doc
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_fragment_chain_is_refused_without_overflowing_the_stack() {
    // 30,000 links: one stack frame pair per link would overflow a 2 MiB
    // Tokio worker stack and abort the process (before auth runs).
    let q = fragment_chain(30_000, "query{viewer{{spread}}}");
    assert!(q.len() < 1 << 20, "{} bytes", q.len());
    let base = serve(runtime(Some(scripted(vec![user("octo")])))).await;
    let r = query(&base, &q).await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(
        r.body["errors"][0]["extensions"]["code"], "QUERY_TOO_LARGE",
        "{}",
        r.body
    );
}

#[tokio::test]
async fn short_fragment_chain_still_runs() {
    let provider = scripted(vec![user("octo")]);
    let base = serve(runtime(Some(provider.clone()))).await;
    let q = fragment_chain(20, "query{viewer{{spread}}}");
    let r = query(&base, &q).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.get("errors").is_none(), "{}", r.body);
    assert!(r.body["data"]["viewer"]["login"].is_string(), "{}", r.body);
}

#[tokio::test]
async fn shared_fragments_below_the_size_bound_still_run() {
    // Eight levels: 256 `login` selections, well inside the bound.
    let provider = scripted(vec![user("octo")]);
    let base = serve(runtime(Some(provider.clone()))).await;
    let q = fragment_bomb(8, "User", "query { viewer { {spread} } }", "login");
    let r = query(&base, &q).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.get("errors").is_none(), "{}", r.body);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_fragment_bomb_refused_before_validation() {
    let records = Arc::new(Records::default());
    let mut rt = runtime(None);
    rt.recorder = Some(records.clone());
    let base = serve(rt).await;
    let mut ws = ws_init(&base, "graphql-transport-ws").await;
    let bomb = fragment_bomb(
        28,
        "Issue",
        r#"subscription { issueAdded(repositoryId: "r-1") { {spread} } }"#,
        "id",
    );
    ws_send(
        &mut ws,
        json!({"type": "subscribe", "id": "1", "payload": {"query": bomb}}),
    )
    .await;
    let refused = ws_recv_for(&mut ws, "error", "1").await;
    assert_eq!(
        refused["payload"][0]["extensions"]["code"], "QUERY_TOO_LARGE",
        "{refused}"
    );
    // The connection stays usable.
    ws_send(
        &mut ws,
        json!({"type": "subscribe", "id": "2", "payload": {"query": SUBSCRIPTION}}),
    )
    .await;
    ws_recv_for(&mut ws, "next", "2").await;
    let recs = records.wait(1).await;
    assert!(recs.iter().any(|r| r.status == 400), "{recs:?}");
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
    assert_eq!(post_oversized_head(&base, 2 * 1024 * 1024).await.0, 413);
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

#[tokio::test]
async fn unknown_path_is_reported_at_the_requested_path() {
    let records = Arc::new(Records::default());
    let mut rt = runtime(None);
    rt.recorder = Some(records.clone());
    let base = serve(rt).await;
    let r = answer(
        reqwest::Client::new()
            .get(format!("{base}/other/path"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, 404);
    let recs = records.wait(1).await;
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!(recs[0].status, 404);
    assert_eq!(recs[0].path, "/other/path");
}

fn starwars_runtime(provider: Option<Arc<ScriptedProvider>>) -> MockRuntime {
    let path = format!(
        "{}/../../fixtures/graphql/starwars.graphql",
        env!("CARGO_MANIFEST_DIR")
    );
    let bundle = Arc::new(compile(&std::fs::read(path).unwrap()).unwrap());
    let dyn_provider = provider.map(|p| p as Arc<dyn Provider>);
    MockRuntime::new(
        "starwars",
        bundle,
        Arc::new(Engine::new(dyn_provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

const EPISODES: [&str; 3] = ["NEWHOPE", "EMPIRE", "JEDI"];

fn assert_episodes(body: &Value) {
    assert!(body.get("errors").is_none(), "{body}");
    let items = body["data"]["hero"]["appearsIn"].as_array().unwrap();
    for item in items {
        assert!(
            item.is_null() || EPISODES.contains(&item.as_str().unwrap_or_default()),
            "{body}"
        );
    }
}

/// `appearsIn: [Episode]!`: faker items are always Episode values (or null).
#[tokio::test]
async fn list_of_enums_from_the_faker_is_valid() {
    let base = serve(starwars_runtime(None)).await;
    for _ in 0..40 {
        let a = query(&base, "{ hero { name appearsIn } }").await;
        assert_eq!(a.status, 200);
        assert_episodes(&a.body);
    }
}

/// An LLM answer with invalid enum items is repaired or replaced by the
/// faker, never served.
#[tokio::test]
async fn list_of_enums_from_the_llm_is_repaired() {
    let bad = json!({"__typename": "Human", "id": "1000", "name": "Luke",
                     "appearsIn": ["A New Hope", "empire", 4], "friends": []});
    let provider = scripted(vec![bad.clone(), bad]);
    let base = serve(starwars_runtime(Some(provider))).await;
    let a = query(&base, "{ hero { name appearsIn } }").await;
    assert_eq!(a.status, 200);
    assert_episodes(&a.body);
}

/// A null item in `[Episode]!` cannot be served as an enum item, so it is
/// treated as invalid output: never passed through.
#[tokio::test]
async fn list_of_enums_with_null_item_is_never_served() {
    let with_null = json!({"__typename": "Human", "id": "1000", "name": "Luke",
                           "appearsIn": ["NEWHOPE", null], "friends": []});
    let provider = scripted(vec![with_null.clone(), with_null]);
    let base = serve(starwars_runtime(Some(provider))).await;
    let a = query(&base, "{ hero { name appearsIn } }").await;
    assert_eq!(a.status, 200);
    assert_episodes(&a.body);
    let items = a.body["data"]["hero"]["appearsIn"].as_array().unwrap();
    assert!(items.iter().all(Value::is_string), "{}", a.body);
}

/// Interface fields implemented covariantly, as GitHub's schema does
/// (`User.email: String!` for `ProfileOwner.email: String`).
const COVARIANT_SDL: &str = r#"
interface Node { id: ID! }
interface ProfileOwner { email: String  owner: Node  friends: [Node]  pick: Item }
union Item = User | Org
type Org implements Node { id: ID! }
type User implements Node & ProfileOwner {
  id: ID!
  email: String!
  owner: User
  friends: [User!]!
  pick: Org
}
type Query { viewer: User  profile: ProfileOwner }
"#;

fn sdl_runtime(sdl: &str, provider: Option<Arc<ScriptedProvider>>) -> MockRuntime {
    let dyn_provider = provider.map(|p| p as Arc<dyn Provider>);
    MockRuntime::new(
        "sdl",
        Arc::new(compile(sdl.as_bytes()).unwrap()),
        Arc::new(Engine::new(dyn_provider, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

#[tokio::test]
async fn covariant_interface_fields_are_served() {
    let user = |id: &str, email: &str| json!({"__typename": "User", "id": id, "email": email, "friends": []});
    let mut viewer = user("u1", "a@example.com");
    viewer["owner"] = user("u0", "o@example.com");
    viewer["friends"] = json!([user("u2", "f@example.com")]);
    viewer["pick"] = json!({"__typename": "Org", "id": "o1"});
    let base = serve(sdl_runtime(COVARIANT_SDL, Some(scripted(vec![viewer])))).await;
    let r = query(
        &base,
        "{ viewer { email owner { id email } friends { id } pick { id } } }",
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.body.get("errors").is_none(), "{}", r.body);
    let v = &r.body["data"]["viewer"];
    assert_eq!(v["email"], "a@example.com");
    assert_eq!(v["owner"], json!({"id": "u0", "email": "o@example.com"}));
    assert_eq!(v["friends"], json!([{"id": "u2"}]));
    assert_eq!(v["pick"], json!({"id": "o1"}));

    // async-graphql 7 cannot register these as declared: a field narrowing
    // nullability takes its interface's type, and the interface leaves out
    // fields that narrow a named type, which keep their declared type.
    let r = query(
        &base,
        r#"{ u: __type(name: "User") { fields { name type { kind name } } }
             p: __type(name: "ProfileOwner") { fields { name } } }"#,
    )
    .await;
    let fields = r.body["data"]["u"]["fields"].as_array().unwrap();
    let field = |name: &str| fields.iter().find(|f| f["name"] == name).unwrap()["type"].clone();
    assert_eq!(field("email"), json!({"kind": "SCALAR", "name": "String"}));
    assert_eq!(field("owner"), json!({"kind": "OBJECT", "name": "User"}));
    assert_eq!(r.body["data"]["p"]["fields"], json!([{"name": "email"}]));
}

#[test]
fn invalid_interface_implementations_are_rejected() {
    for (field, why) in [
        ("email: Int", "a different named type"),
        ("email: [String]", "a list for a scalar"),
    ] {
        let sdl = format!(
            "interface P {{ email: String }}\ntype U implements P {{ {field} }}\ntype Query {{ u: U }}"
        );
        let err = graphql::try_router(Arc::new(sdl_runtime(&sdl, None)))
            .err()
            .unwrap_or_else(|| panic!("{why} was accepted"));
        assert!(err.to_string().contains("U.email"), "{why}: {err}");
    }
    let sdl = "interface P { email: String! }\ntype U implements P { email: String }\ntype Query { u: U }";
    let err = graphql::try_router(Arc::new(sdl_runtime(sdl, None)))
        .err()
        .unwrap();
    assert!(err.to_string().contains("U.email"), "{err}");
}

/// The model is asked only for the selected fields: the prompt carries the
/// narrowed schema (with nested arguments), and an answer holding only
/// those fields is an LLM answer, not a fallback.
#[tokio::test]
async fn generation_follows_the_selection_set() {
    let provider = scripted(vec![json!({
        "__typename": "User",
        "login": "octocat",
        "repositories": [{"__typename": "Repository", "name": "hello"}],
    })]);
    let base = serve(runtime(Some(provider.clone()))).await;
    let a = query(
        &base,
        r#"{ viewer { login ...R } } fragment R on User { repositories(first: 3) { name } }"#,
    )
    .await;
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(
        a.headers["x-mockingbird-source"],
        "llm",
        "{:?}",
        a.headers.get("x-mockingbird-fallback-reason")
    );
    assert_eq!(a.body["data"]["viewer"]["repositories"][0]["name"], "hello");
    let system = &provider.requests()[0].messages[0].content;
    for unselected in ["createdAt", "stargazerCount", "isPrivate"] {
        assert!(!system.contains(unselected), "{unselected} in {system}");
    }
    assert!(
        system.contains(r#"Queried with arguments {\"first\":3}"#),
        "{system}"
    );
}

/// A query's root fields are answered by one LLM call, so they can agree
/// (GitHub's `user(login:)` and `organization(login:)`: one is null).
#[tokio::test]
async fn root_fields_of_one_query_share_one_call() {
    let provider = scripted(vec![json!({
        "repository": {"__typename": "Repository", "name": "n"},
        "issue": null,
    })]);
    let base = serve(runtime(Some(provider.clone()))).await;
    let a = query(
        &base,
        r#"{ repository(owner: "o", name: "n") { name } issue(id: "I_1") { title } }"#,
    )
    .await;
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(a.body["data"]["repository"]["name"], "n");
    assert_eq!(a.body["data"]["issue"], Value::Null);
    assert_eq!(a.headers["x-mockingbird-source"], "llm");
    let system = &provider.requests()[0].messages[0].content;
    assert!(
        system.contains("`repository`") && system.contains("`issue`"),
        "{system}"
    );
}

const STOREFRONT: &str = r#"
type ProductVariant { id: ID! title: String! product: Product! }
type Product { id: ID! title: String! handle: String! variants: [ProductVariant!]! }
type CartLine { id: ID! quantity: Int! merchandise: ProductVariant! }
type Cart { id: ID! checkoutUrl: String! totalQuantity: Int! lines: [CartLine!]! }
type UserError { field: [String!] message: String! }
type CartCreatePayload { cart: Cart userErrors: [UserError!]! }
type CartLinesAddPayload { cart: Cart userErrors: [UserError!]! }
input CartLineInput { merchandiseId: ID! quantity: Int }
input CartInput { lines: [CartLineInput!] }
type Query {
  product(handle: String): Product
  cart(id: ID!): Cart
}
type Mutation {
  cartCreate(input: CartInput): CartCreatePayload
  cartLinesAdd(cartId: ID!, lines: [CartLineInput!]!): CartLinesAddPayload
}
"#;

fn storefront(provider: Arc<ScriptedProvider>) -> MockRuntime {
    MockRuntime::new(
        "shop",
        Arc::new(compile(STOREFRONT.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    )
}

const CART_FIELDS: &str =
    "id checkoutUrl totalQuantity lines { id quantity merchandise { id title product { title } } }";

/// A cart created, read back and added to holds together: `cartCreate`'s
/// cart is stored under its type, `cart(id:)` is served from it, and
/// `cartLinesAdd` is generated knowing the cart and the variant (read
/// earlier with its product), then stored over it.
#[tokio::test]
async fn a_graphql_cart_holds_together() {
    let cart_id = "gid://shopify/Cart/c1?key=0123";
    let variant = json!({"__typename": "ProductVariant", "id": "gid://shopify/ProductVariant/42", "title": "M", "product": {"__typename": "Product", "title": "Fern Tee"}});
    let provider = scripted(vec![
        json!({"__typename": "Product", "id": "gid://shopify/Product/7", "title": "Fern Tee", "handle": "fern-tee",
               "variants": [{"__typename": "ProductVariant", "id": "gid://shopify/ProductVariant/42", "title": "M"}]}),
        json!({"__typename": "CartCreatePayload", "cart": {"__typename": "Cart", "id": cart_id, "checkoutUrl": "https://shop/c1", "totalQuantity": 0, "lines": []},
               "userErrors": []}),
        json!({"__typename": "CartLinesAddPayload", "cart": {"__typename": "Cart", "id": cart_id, "checkoutUrl": "https://shop/c1", "totalQuantity": 1,
                        "lines": [{"__typename": "CartLine", "id": "gid://shopify/CartLine/1", "quantity": 1, "merchandise": variant}]},
               "userErrors": []}),
    ]);
    let base = serve(storefront(provider.clone())).await;
    let session = [("x-mockingbird-session", "s1")];
    let product = post(
        &base,
        json!({"query": r#"{ product(handle: "fern-tee") { id title handle variants { id title } } }"#}),
        &session,
    )
    .await;
    assert!(product.body.get("errors").is_none(), "{}", product.body);

    let created = post(
        &base,
        json!({"query": format!("mutation {{ cartCreate(input: {{lines: []}}) {{ cart {{ {CART_FIELDS} }} userErrors {{ message }} }} }}")}),
        &session,
    )
    .await;
    assert!(created.body.get("errors").is_none(), "{}", created.body);
    assert_eq!(created.body["data"]["cartCreate"]["cart"]["id"], cart_id);

    // Served from the stored cart: no LLM call.
    let read = post(
        &base,
        json!({"query": format!("query($id: ID!) {{ cart(id: $id) {{ {CART_FIELDS} }} }}"), "variables": {"id": cart_id}}),
        &session,
    )
    .await;
    assert_eq!(
        read.headers["x-mockingbird-source"], "state",
        "{}",
        read.body
    );
    assert_eq!(read.body["data"]["cart"]["lines"], json!([]));
    assert_eq!(provider.requests().len(), 2);

    let added = post(
        &base,
        json!({
            "query": format!("mutation($id: ID!, $lines: [CartLineInput!]!) {{ cartLinesAdd(cartId: $id, lines: $lines) {{ cart {{ {CART_FIELDS} }} userErrors {{ message }} }} }}"),
            "variables": {"id": cart_id, "lines": [{"merchandiseId": "gid://shopify/ProductVariant/42", "quantity": 1}]},
        }),
        &session,
    )
    .await;
    assert!(added.body.get("errors").is_none(), "{}", added.body);
    // The prompt carried the stored cart and the variant the line names.
    let prompt: String = provider.requests()[2]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(prompt.contains("https://shop/c1"), "{prompt}");
    let related = prompt
        .split("<related_resources>")
        .nth(1)
        .unwrap_or_default();
    assert!(
        related.contains(r#""id":"gid://shopify/ProductVariant/42""#),
        "{prompt}"
    );
    // The variant was read inside its product, so it knows the product.
    assert!(
        related.contains(r#""product":{"__typename":"Product","handle":"fern-tee","id":"gid://shopify/Product/7","title":"Fern Tee"}"#),
        "{prompt}"
    );

    // The cart now has the line, from state.
    let read = post(
        &base,
        json!({"query": format!("query($id: ID!) {{ cart(id: $id) {{ {CART_FIELDS} }} }}"), "variables": {"id": cart_id}}),
        &session,
    )
    .await;
    assert_eq!(
        read.headers["x-mockingbird-source"], "state",
        "{}",
        read.body
    );
    assert_eq!(read.body["data"]["cart"]["totalQuantity"], 1);
    assert_eq!(
        read.body["data"]["cart"]["lines"][0]["merchandise"]["product"]["title"],
        "Fern Tee"
    );
    assert_eq!(provider.requests().len(), 3);
}

const PAYLOADS: &str = r#"
type User { id: ID! login: String! bio: String company: String location: String }
type Label { id: ID! name: String! color: String! description: String }
type Issue {
  id: ID! number: Int! title: String! body: String url: String! state: String!
  author: User labels: [Label!]! createdAt: String! closedAt: String
}
input CreateIssueInput { repositoryId: ID! title: String! body: String }
type CreateIssuePayload { clientMutationId: String issue: Issue }
type Query { node(id: ID!): Issue }
type Mutation { createIssue(input: CreateIssueInput!): CreateIssuePayload }
"#;

/// A create mutation generates only its selection (GitHub's
/// `createIssue { issue { id url } }`), and the stored issue takes the
/// input's title and body.
#[tokio::test]
async fn a_create_mutation_generates_only_its_selection() {
    let provider = scripted(vec![
        json!({"__typename": "CreateIssuePayload", "issue": {"__typename": "Issue", "id": "I_1", "url": "https://github.com/o/r/issues/1"}}),
    ]);
    let rt = MockRuntime::new(
        "gh",
        Arc::new(compile(PAYLOADS.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider.clone() as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let base = serve(rt).await;
    let session = [("x-mockingbird-session", "s1")];
    let created = post(
        &base,
        json!({
            "query": "mutation($in: CreateIssueInput!) { createIssue(input: $in) { issue { id url } } }",
            "variables": {"in": {"repositoryId": "R_1", "title": "Crash on start", "body": "Trace"}},
        }),
        &session,
    )
    .await;
    assert!(created.body.get("errors").is_none(), "{}", created.body);
    assert_eq!(
        created.body["data"]["createIssue"]["issue"],
        json!({"id": "I_1", "url": "https://github.com/o/r/issues/1"})
    );
    let schema = match &provider.requests()[0].response_format {
        mb_llm::ResponseFormat::JsonSchema { schema, .. } => schema.to_string(),
        other => panic!("{other:?}"),
    };
    for unselected in ["author", "labels", "createdAt", "clientMutationId"] {
        assert!(!schema.contains(unselected), "{unselected} in {schema}");
    }

    let read = post(
        &base,
        json!({"query": r#"{ node(id: "I_1") { id title body url } }"#}),
        &session,
    )
    .await;
    assert_eq!(
        read.headers["x-mockingbird-source"], "state",
        "{}",
        read.body
    );
    assert_eq!(
        read.body["data"]["node"],
        json!({"id": "I_1", "title": "Crash on start", "body": "Trace", "url": "https://github.com/o/r/issues/1"})
    );
}

#[tokio::test]
async fn scenario_headers_are_sent() {
    let mut rt = runtime(Some(scripted(vec![json!(repository("one"))])));
    rt.header_overrides = true;
    rt.scenarios.insert(
        "throttled".into(),
        mb_protocols::ScenarioRuntime {
            spec: mb_engine::ScenarioSpec {
                name: "throttled".into(),
                ..mb_engine::ScenarioSpec::default()
            },
            headers: vec![("X-RateLimit-Remaining".into(), "0".into())],
            ..mb_protocols::ScenarioRuntime::default()
        },
    );
    let base = serve(rt).await;
    let q = json!({"query": r#"{ repository(owner: "o", name: "one") { name } }"#});
    let r = post(&base, q.clone(), &[("x-mockingbird-scenario", "throttled")]).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.headers["x-ratelimit-remaining"], "0");
    let r = post(&base, q, &[]).await;
    assert!(r.headers.get("x-ratelimit-remaining").is_none());
}

const PRICED_STOREFRONT: &str = r#"
type MoneyV2 { amount: String! currencyCode: String! }
type ProductVariant { id: ID! title: String! price: MoneyV2! }
type Product { id: ID! title: String! variants: [ProductVariant!]! }
type CartLineCost { totalAmount: MoneyV2! }
type CartLine { id: ID! quantity: Int! merchandise: ProductVariant! cost: CartLineCost! }
type CartLineEdge { node: CartLine! }
type CartLineConnection { edges: [CartLineEdge!]! }
type CartCost { subtotalAmount: MoneyV2! totalAmount: MoneyV2! }
type Cart { id: ID! totalQuantity: Int! lines: CartLineConnection! cost: CartCost! }
type CartLinesAddPayload { cart: Cart }
input CartLineInput { merchandiseId: ID! quantity: Int }
type Query { product(handle: String): Product }
type Mutation { cartLinesAdd(cartId: ID!, lines: [CartLineInput!]!): CartLinesAddPayload }
"#;

/// A cart line takes the price of the variant it names as the session
/// stored it, and the line and cart totals add up.
#[tokio::test]
async fn cart_costs_come_from_stored_prices() {
    let money =
        |amount: &str| json!({"__typename": "MoneyV2", "amount": amount, "currencyCode": "USD"});
    let provider = scripted(vec![
        json!({"__typename": "Product", "id": "gid://shopify/Product/7", "title": "Fern Tee",
               "variants": [{"__typename": "ProductVariant", "id": "gid://shopify/ProductVariant/42", "title": "M", "price": money("32.00")}]}),
        json!({"__typename": "CartLinesAddPayload", "cart": {"__typename": "Cart", "id": "gid://shopify/Cart/c1", "totalQuantity": 1,
            "lines": {"__typename": "CartLineConnection", "edges": [{"__typename": "CartLineEdge", "node": {"__typename": "CartLine", "id": "gid://shopify/CartLine/1", "quantity": 2,
                "merchandise": {"__typename": "ProductVariant", "id": "gid://shopify/ProductVariant/42", "title": "M", "price": money("28.00")},
                "cost": {"__typename": "CartLineCost", "totalAmount": money("28.00")}}}]},
            "cost": {"__typename": "CartCost", "subtotalAmount": money("28.00"), "totalAmount": money("30.50")}}}),
    ]);
    let rt = MockRuntime::new(
        "shop",
        Arc::new(compile(PRICED_STOREFRONT.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(provider as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let base = serve(rt).await;
    let product = post(
        &base,
        json!({"query": r#"{ product(handle: "fern-tee") { id title variants { id title price { amount currencyCode } } } }"#}),
        &[],
    )
    .await;
    assert!(product.body.get("errors").is_none(), "{}", product.body);
    let added = post(
        &base,
        json!({
            "query": "mutation($lines: [CartLineInput!]!) { cartLinesAdd(cartId: \"gid://shopify/Cart/c1\", lines: $lines) { cart { id totalQuantity lines { edges { node { id quantity merchandise { id title price { amount currencyCode } } cost { totalAmount { amount currencyCode } } } } } cost { subtotalAmount { amount currencyCode } totalAmount { amount currencyCode } } } } }",
            "variables": {"lines": [{"merchandiseId": "gid://shopify/ProductVariant/42", "quantity": 2}]},
        }),
        &[],
    )
    .await;
    assert!(added.body.get("errors").is_none(), "{}", added.body);
    let cart = &added.body["data"]["cartLinesAdd"]["cart"];
    let line = &cart["lines"]["edges"][0]["node"];
    assert_eq!(line["merchandise"]["price"]["amount"], "32.00", "{cart}");
    assert_eq!(line["cost"]["totalAmount"]["amount"], "64.00", "{cart}");
    assert_eq!(cart["cost"]["subtotalAmount"]["amount"], "64.00", "{cart}");
    assert_eq!(cart["cost"]["totalAmount"]["amount"], "64.00", "{cart}");
    assert_eq!(cart["totalQuantity"], 2, "{cart}");
}
