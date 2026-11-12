//! The first read of a record the session does not hold: built locally as a create
//! builds its record, the model asked only for its free text, links and booleans, then
//! stored, so a second read answers the same record; a held record is answered as held.
#![cfg(feature = "http-rest")]

use serde_json::{Value, json};

use crate::support::{self, HarnessOptions};

/// One read of a widget by id: text, links, flags, enums, ids, numbers, moments and
/// another record ([`contract`] makes it large).
const CONTRACT: &str = r##"{
  "openapi": "3.1.0",
  "info": {"title": "widgets", "version": "1"},
  "paths": {"/widgets/{widget_id}": {"get": {
    "operationId": "getWidget",
    "parameters": [{"name": "widget_id", "in": "path", "required": true, "schema": {"type": "string"}}],
    "responses": {"200": {"description": "ok", "content": {"application/json": {
      "schema": {"$ref": "#/components/schemas/Widget"}
    }}}}
  }}},
  "components": {"schemas": {
    "Widget": {"type": "object",
      "required": ["id", "node_id", "title", "description", "archived", "html_url",
                   "priority", "count", "created_at", "closed_at", "maker", "reviewer"],
      "properties": {
        "id": {"type": "string"},
        "node_id": {"type": "string", "examples": ["W_kgDOAB12cd34"]},
        "global_id": {"type": "string", "examples": ["MDEwOlJlcG9zaXRvcnkxMjk2MjY5"]},
        "title": {"type": "string"},
        "description": {"type": ["string", "null"]},
        "archived": {"type": "boolean"},
        "html_url": {"type": "string", "format": "uri"},
        "priority": {"type": "string", "enum": ["high", "low"], "default": "low"},
        "count": {"type": "integer", "minimum": 0},
        "weight": {"type": "number"},
        "created_at": {"type": "string", "format": "date-time"},
        "closed_at": {"type": ["string", "null"], "format": "date-time"},
        "maker": {"type": "object", "required": ["login", "site_admin", "avatar_url"],
          "properties": {
            "login": {"type": "string"},
            "site_admin": {"type": "boolean"},
            "avatar_url": {"type": "string", "format": "uri"},
            "followers": {"type": "integer"}
          }},
        "reviewer": {"anyOf": [{"$ref": "#/components/schemas/Person"}, {"type": "null"}]}
      }},
    "Person": {"type": "object", "required": ["id", "login"],
      "properties": {"id": {"type": "integer"}, "login": {"type": "string"}}}
  }}
}"##;

/// Moments and counts the record holds besides, so that its answer is large: about the
/// size of GitHub's `issues/get`.
const MOMENTS: usize = 80;
const COUNTS: usize = 40;

/// [`CONTRACT`] with [`MOMENTS`] required moments and [`COUNTS`] required counts more.
fn contract() -> String {
    let mut doc: Value = serde_json::from_str(CONTRACT).unwrap();
    let widget = &mut doc["components"]["schemas"]["Widget"];
    for i in 0..MOMENTS {
        let name = format!("moment_{i}_at");
        widget["properties"][&name] = json!({"type": "string", "format": "date-time"});
        widget["required"].as_array_mut().unwrap().push(json!(name));
    }
    for i in 0..COUNTS {
        let name = format!("count_{i}");
        widget["properties"][&name] = json!({"type": "integer", "minimum": 0});
        widget["required"].as_array_mut().unwrap().push(json!(name));
    }
    doc.to_string()
}

/// What the model writes for the members it is asked for.
fn written() -> Value {
    json!({
        "title": "Brass hinge",
        "description": "A hinge for the garden gate.",
        "archived": true,
        "html_url": "https://widgets.example.test/widgets/w42",
        "maker": {"login": "ada", "site_admin": false, "avatar_url": "https://widgets.example.test/ada.png"}
    })
}

/// The top-level members each model call asked for.
fn asked(h: &support::DescribedHarness) -> Vec<Vec<String>> {
    h.provider
        .requests()
        .iter()
        .map(|r| {
            let mb_llm::ResponseFormat::JsonSchema { schema, .. } = &r.response_format else {
                return Vec::new();
            };
            // A generated read's format is a `$ref` into its `$defs`.
            let root = match schema["$ref"].as_str() {
                Some(at) => schema.pointer(at.trim_start_matches('#')).unwrap_or(schema),
                None => schema,
            };
            let mut names: Vec<String> = root["properties"]
                .as_object()
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();
            names
        })
        .collect()
}

/// Whether `answer` validates against the read's whole answer schema.
fn validates(h: &support::DescribedHarness, answer: &Value) -> bool {
    let bundle = h.rt.bundle();
    let op = bundle
        .operations
        .iter()
        .find(|op| op.id == "getWidget")
        .unwrap();
    let schema = op.success_output().unwrap().schema.as_ref().unwrap();
    mb_engine::SchemaValidator::for_output(&bundle.with_reachable_defs(schema))
        .unwrap()
        .is_valid(answer)
}

#[tokio::test]
async fn a_first_read_asks_the_model_for_text_links_and_flags_alone() {
    let h = support::harness(
        &contract(),
        vec![written()],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let bundle = h.rt.bundle();
    let schema = bundle.operations[0]
        .success_output()
        .unwrap()
        .schema
        .as_ref()
        .unwrap();
    let estimate = mb_engine::budget::estimated_tokens(&bundle.with_reachable_defs(schema));
    assert!(estimate.unwrap() > 1000.0, "a large record: {estimate:?}");
    let (status, _, body) = h.send("GET", "/widgets/w42", None, &[]).await;
    assert_eq!(status, 200, "{body}");

    // One call, for the text, the links and the flags; the maker asked for its own.
    assert_eq!(
        asked(&h),
        vec![vec![
            "archived".to_string(),
            "description".to_string(),
            "html_url".to_string(),
            "maker".to_string(),
            "title".to_string(),
        ]],
        "{body}"
    );
    let request = &h.provider.requests()[0];
    let mb_llm::ResponseFormat::JsonSchema { schema, .. } = &request.response_format else {
        panic!("no schema format");
    };
    let mut maker: Vec<&String> = schema["properties"]["maker"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    maker.sort();
    assert_eq!(maker, ["avatar_url", "login", "site_admin"], "{schema}");

    // What the model wrote, the path's id, a minted node id, the enum's default.
    assert_eq!(body["title"], "Brass hinge");
    assert_eq!(body["archived"], true);
    assert_eq!(body["maker"]["login"], "ada");
    assert_eq!(body["id"], "w42");
    // Ids in the shape the contract's examples show: their prefix and length, or with
    // none to share, their characters and length.
    let node_id = body["node_id"].as_str().unwrap();
    assert_ne!(node_id, "W_kgDOAB12cd34");
    assert!(
        node_id.starts_with("W_") && node_id.len() == "W_kgDOAB12cd34".len(),
        "{node_id}"
    );
    let global_id = body["global_id"].as_str().unwrap();
    assert!(
        global_id.len() == 28 && global_id.chars().all(|c| c.is_ascii_alphanumeric()),
        "{global_id}"
    );
    assert_eq!(body["priority"], "low");
    assert!(body["count"].is_u64(), "{body}");
    assert!(body["created_at"].is_string(), "{body}");
    assert!(body["moment_7_at"].is_string(), "{body}");
    // Another record nothing names is none.
    assert_eq!(body["reviewer"], Value::Null);
    // A moment that may not have come yet has not.
    assert_eq!(body["closed_at"], Value::Null);
    assert!(validates(&h, &body), "{body}");
}

#[tokio::test]
async fn a_second_read_answers_the_same_record() {
    let h = support::harness(
        &contract(),
        vec![written()],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (_, _, first) = h.send("GET", "/widgets/w42", None, &[]).await;
    let (status, _, second) = h.send("GET", "/widgets/w42", None, &[]).await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(first, second);
    assert_eq!(
        h.provider.requests().len(),
        1,
        "the second read asks nothing"
    );
}

#[tokio::test]
async fn a_held_record_keeps_its_stored_values() {
    let h = support::harness(
        &contract(),
        vec![written()],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let mut held = json!({
        "id": "w7", "node_id": "W_kgDOHELD0001", "global_id": "MDEwOlJlcG9zaXRvcnkxMjk2MjY5",
        "title": "Kept title",
        "description": null, "archived": false,
        "html_url": "https://widgets.example.test/widgets/w7", "priority": "high",
        "count": 3, "created_at": "2026-01-02T03:04:05Z", "closed_at": null,
        "maker": {"login": "grace", "site_admin": true, "avatar_url": "https://widgets.example.test/g.png"},
        "reviewer": {"id": 9, "login": "lin"}
    });
    for i in 0..MOMENTS {
        held[format!("moment_{i}_at")] = json!("2026-02-03T04:05:06Z");
    }
    for i in 0..COUNTS {
        held[format!("count_{i}")] = json!(i);
    }
    h.hold("widgets", std::slice::from_ref(&held)).await;
    let (status, _, body) = h.send("GET", "/widgets/w7", None, &[]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, held);
    assert!(h.provider.requests().is_empty(), "nothing is filled");

    // Another record's first read leaves it as stored.
    h.send("GET", "/widgets/w42", None, &[]).await;
    let (_, _, again) = h.send("GET", "/widgets/w7", None, &[]).await;
    assert_eq!(again, held);
}

#[tokio::test]
async fn a_small_record_is_still_generated_whole() {
    let h = support::harness(CONTRACT, vec![], &HarnessOptions::default(), |rt| rt);
    h.send("GET", "/widgets/w42", None, &[]).await;
    let asked = asked(&h);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].iter().any(|m| m == "count") && asked[0].iter().any(|m| m == "id"),
        "the whole record: {asked:?}"
    );
}

#[tokio::test]
async fn a_read_whose_query_only_shows_the_record_is_filled_locally() {
    // A single-record read names its record by the path whatever its query; a query
    // that only says how it is shown is no other record.
    let h = support::harness(
        &contract(),
        vec![written()],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h.send("GET", "/widgets/w42?view=full", None, &[]).await;
    assert_eq!(status, 200, "{body}");
    let asked = asked(&h);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(!asked[0].iter().any(|m| m == "count_3"), "{asked:?}");
    assert_eq!(body["title"], "Brass hinge");
}

#[tokio::test]
async fn a_model_that_writes_nothing_stores_nothing() {
    // The fill's answer holds none of what it asked: the read generates (the script
    // has nothing more, so the faker answers), and nothing is stored.
    let h = support::harness(
        &contract(),
        vec![json!({})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, headers, body) = h.send("GET", "/widgets/w42", None, &[]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(headers["x-mockingbird-source"], "fallback");
    let calls = h.provider.requests().len();
    assert!(calls >= 2, "the fill, then the generation: {calls}");
    h.send("GET", "/widgets/w42", None, &[]).await;
    assert!(
        h.provider.requests().len() > calls,
        "the second read is not answered from state"
    );
}

/// `contract` served with `persona`, its model answering `script`.
fn with_persona(
    contract: &str,
    persona: &str,
    script: Vec<Value>,
) -> (axum::Router, std::sync::Arc<mb_llm::ScriptedProvider>) {
    use std::sync::Arc;
    let provider = Arc::new(mb_llm::ScriptedProvider::new(
        script.into_iter().map(|v| Ok(v.to_string())).collect(),
    ));
    let engine = mb_engine::Engine::new(
        Some(Arc::clone(&provider) as Arc<dyn mb_llm::Provider>),
        mb_engine::EngineConfig {
            persona: Some(persona.to_owned()),
            ..mb_engine::EngineConfig::default()
        },
    );
    let rt = Arc::new(mb_protocols::MockRuntime::new(
        "widgets",
        Arc::new(mb_import::compile(contract.as_bytes()).unwrap()),
        Arc::new(engine),
        Arc::new(mb_state::MemoryStore::new(std::time::Duration::from_secs(
            60,
        ))),
    ));
    (mb_protocols::http_rest::router(rt), provider)
}

#[tokio::test]
async fn an_id_the_persona_names_is_the_models() {
    let mut answer = written();
    answer["node_id"] = json!("W_kgDOPersona01");
    let (router, provider) = with_persona(
        &contract(),
        "Widgets of a hardware shop. node_id values use the W_kgDO prefix.",
        vec![answer],
    );
    let req = axum::http::Request::builder()
        .uri("/widgets/w42")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, _, body) = support::send_to(&router, req).await;
    assert_eq!(status, 200, "{body}");
    let requests = provider.requests();
    let mb_llm::ResponseFormat::JsonSchema { schema, .. } = &requests[0].response_format else {
        panic!("no schema format");
    };
    assert!(schema["properties"].get("node_id").is_some(), "{schema}");
    assert!(schema["properties"].get("global_id").is_none(), "{schema}");
    assert_eq!(body["node_id"], "W_kgDOPersona01");
}

/// Pull requests read by number below a repository, and created there.
const PULLS: &str = r##"{
  "openapi": "3.1.0",
  "info": {"title": "pulls", "version": "1"},
  "paths": {
    "/repos/{owner}/{repo}/pulls": {"post": {
      "operationId": "createPull",
      "parameters": [
        {"name": "owner", "in": "path", "required": true, "schema": {"type": "string"}},
        {"name": "repo", "in": "path", "required": true, "schema": {"type": "string"}}
      ],
      "requestBody": {"content": {"application/json": {"schema": {
        "type": "object", "required": ["title"], "properties": {"title": {"type": "string"}}
      }}}},
      "responses": {"201": {"description": "ok", "content": {"application/json": {
        "schema": {"$ref": "#/components/schemas/Pull"}
      }}}}
    }},
    "/repos/{owner}/{repo}/pulls/{pull_number}": {"get": {
      "operationId": "getPull",
      "parameters": [
        {"name": "owner", "in": "path", "required": true, "schema": {"type": "string"}},
        {"name": "repo", "in": "path", "required": true, "schema": {"type": "string"}},
        {"name": "pull_number", "in": "path", "required": true, "schema": {"type": "integer"}}
      ],
      "responses": {"200": {"description": "ok", "content": {"application/json": {
        "schema": {"$ref": "#/components/schemas/Pull"}
      }}}}
    }}
  },
  "components": {"schemas": {
    "Pull": {"type": "object", "required": ["id", "number", "title", "state", "url"],
      "properties": {
        "id": {"type": "integer"},
        "node_id": {"type": "string"},
        "url": {"type": "string", "format": "uri"},
        "number": {"type": "integer"},
        "title": {"type": "string"},
        "state": {"type": "string", "enum": ["open", "closed"]}
      }}
  }}
}"##;

/// [`PULLS`] with twice [`MOMENTS`] required moments more, so a read of a pull is
/// large.
fn pulls() -> String {
    let mut doc: Value = serde_json::from_str(PULLS).unwrap();
    let pull = &mut doc["components"]["schemas"]["Pull"];
    for i in 0..2 * MOMENTS {
        let name = format!("moment_{i}_at");
        pull["properties"][&name] = json!({"type": "string", "format": "date-time"});
        pull["required"].as_array_mut().unwrap().push(json!(name));
    }
    doc.to_string()
}

#[tokio::test]
async fn a_create_after_a_first_read_takes_the_next_number() {
    let h = support::harness(
        &pulls(),
        vec![
            json!({"title": "Router panics", "url": "https://api.example.test/repos/octo/hello/pulls/9"}),
            json!({"title": "Fix"}),
            json!({"title": "Elsewhere"}),
        ],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, read) = h.send("GET", "/repos/octo/hello/pulls/9", None, &[]).await;
    assert_eq!(status, 200, "{read}");
    assert_eq!(read["number"], 9);
    assert_eq!(
        h.provider.requests().len(),
        1,
        "the read was filled locally"
    );
    let (status, _, created) = h
        .send(
            "POST",
            "/repos/octo/hello/pulls",
            Some(json!({"title": "Fix"})),
            &[],
        )
        .await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["number"], 10, "{created}");
    // Another repository's pull requests are numbered on their own.
    let (status, _, other) = h
        .send(
            "POST",
            "/repos/octo/other/pulls",
            Some(json!({"title": "Elsewhere"})),
            &[],
        )
        .await;
    assert_eq!(status, 201, "{other}");
    assert_eq!(other["number"], 1, "{other}");
}

#[cfg(feature = "graphql")]
#[tokio::test]
async fn a_graphql_create_numbers_its_record_after_the_held_ones() {
    const SDL: &str = r#"
type PullRequest { id: ID! number: Int! title: String! }
input CreatePullRequestInput { repositoryId: ID! title: String! }
type CreatePullRequestPayload { pullRequest: PullRequest }
type Query { node(id: ID!): PullRequest }
type Mutation { createPullRequest(input: CreatePullRequestInput!): CreatePullRequestPayload }
"#;
    let h = support::graphql_described(
        mb_import::compile(SDL.as_bytes()).unwrap(),
        json!({"version": 1}),
        vec![],
    );
    let session = mb_state::SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    };
    let held = mb_state::StoredResource {
        resource_type: "pullrequest".into(),
        id: "PR_9".into(),
        data: json!({"id": "PR_9", "number": 9, "title": "Earlier"}),
    };
    h.rt.state.upsert(&session, held).await.unwrap();
    // A number held as text counts too.
    let held = mb_state::StoredResource {
        resource_type: "pullrequest".into(),
        id: "PR_11".into(),
        data: json!({"id": "PR_11", "number": "11", "title": "Imported"}),
    };
    h.rt.state.upsert(&session, held).await.unwrap();
    let body = h
        .query(
            r#"mutation { createPullRequest(input: {repositoryId: "R_1", title: "Fix"}) {
                pullRequest { id number title } } }"#,
        )
        .await;
    assert_eq!(
        body["data"]["createPullRequest"]["pullRequest"]["number"], 12,
        "{body}"
    );
}

/// A repository read as GitHub's REST API answers one: dozens of links (some of a URL
/// format), an owner of its own links and an unformatted `starred_at`, a nullable
/// license, moments and counts.
fn repository() -> String {
    let mut user = serde_json::Map::new();
    for name in ["avatar_url", "html_url", "url"] {
        user.insert(name.into(), json!({"type": "string", "format": "uri"}));
    }
    for name in ["followers_url", "gists_url", "repos_url", "events_url"] {
        user.insert(name.into(), json!({"type": "string"}));
    }
    user.insert("login".into(), json!({"type": "string"}));
    user.insert(
        "node_id".into(),
        json!({"type": "string", "examples": ["MDQ6VXNlcjE="]}),
    );
    user.insert("starred_at".into(), json!({"type": "string"}));
    user.insert("site_admin".into(), json!({"type": "boolean"}));
    let user_required: Vec<&String> = user.keys().collect();
    let mut repo = serde_json::Map::new();
    let mut required = vec![];
    for i in 0..40 {
        let name = format!("link_{i}_url");
        repo.insert(name.clone(), json!({"type": "string"}));
        required.push(name);
    }
    for (name, schema) in [
        ("id", json!({"type": "integer"})),
        (
            "node_id",
            json!({"type": "string", "examples": ["MDEwOlJlcG9zaXRvcnkxMjk2MjY5"]}),
        ),
        ("name", json!({"type": "string"})),
        ("full_name", json!({"type": "string"})),
        ("html_url", json!({"type": "string", "format": "uri"})),
        (
            "homepage",
            json!({"type": ["string", "null"], "format": "uri"}),
        ),
        ("private", json!({"type": "boolean"})),
        (
            "created_at",
            json!({"type": "string", "format": "date-time"}),
        ),
        (
            "pushed_at",
            json!({"type": ["string", "null"], "format": "date-time"}),
        ),
        ("starred_at", json!({"type": "string"})),
        ("stargazers_count", json!({"type": "integer"})),
        ("owner", json!({"$ref": "#/components/schemas/User"})),
        (
            "license",
            json!({"anyOf": [{"$ref": "#/components/schemas/License"}, {"type": "null"}]}),
        ),
    ] {
        repo.insert(name.into(), schema);
        required.push(name.into());
    }
    json!({
        "openapi": "3.1.0",
        "info": {"title": "repos", "version": "1"},
        "paths": {"/repos/{owner}/{repo}": {"get": {
            "operationId": "getRepo",
            "parameters": [
                {"name": "owner", "in": "path", "required": true, "schema": {"type": "string"}},
                {"name": "repo", "in": "path", "required": true, "schema": {"type": "string"}}
            ],
            "responses": {"200": {"description": "ok", "content": {"application/json": {
                "schema": {"$ref": "#/components/schemas/Repo"}
            }}}}
        }}},
        "components": {"schemas": {
            "User": {"type": "object", "required": user_required, "properties": user},
            "License": {"type": "object", "required": ["key", "name"], "properties": {
                "key": {"type": "string"},
                "name": {"type": "string"},
                "html_url": {"type": "string", "format": "uri"}
            }},
            "Repo": {"type": "object", "required": required, "properties": repo}
        }}
    })
    .to_string()
}

/// Every string of `value` at a member that holds a link or a moment, with its path.
fn formatted(value: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(members) => {
            for (key, inner) in members {
                let at = format!("{path}.{key}");
                match inner {
                    Value::String(text)
                        if key.ends_with("url") || key.ends_with("_at") || key.ends_with("_id") =>
                    {
                        out.push((at, text.clone()));
                    }
                    other => formatted(other, &at, out),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                formatted(item, path, out);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn no_faker_word_stands_in_a_formatted_member() {
    // The model writes some of what it is asked and leaves the rest out.
    let h = support::harness(
        &repository(),
        vec![json!({
            "name": "hello", "full_name": "octo/hello",
            "html_url": "https://github.com/octo/hello",
            "owner": {"login": "octo", "site_admin": false}
        })],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let (status, _, body) = h.send("GET", "/repos/octo/hello", None, &[]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(h.provider.requests().len(), 1, "filled locally: {body}");
    let mut found = Vec::new();
    formatted(&body, "", &mut found);
    assert!(found.len() > 40, "{found:?}");
    let word = |text: &str| text.chars().all(|c| c.is_ascii_lowercase());
    for (at, text) in &found {
        assert!(!word(text), "a faker word at {at}: {text}");
        if at.ends_with("_at") {
            assert!(
                text.len() >= 20 && text.as_bytes()[4] == b'-',
                "{at}: {text}"
            );
        }
        if at.ends_with("url") {
            assert!(text.contains("://"), "{at}: {text}");
        }
    }
    assert!(body["pushed_at"].is_null(), "{body}");
    // What the model left out of the owner it was asked for is still there, a URL.
    assert!(
        body["owner"]["avatar_url"]
            .as_str()
            .is_some_and(|url| url.contains("://")),
        "{body}"
    );
    let node_id = body["node_id"].as_str().unwrap();
    assert_eq!(node_id.len(), 28, "{node_id}");
}

#[tokio::test]
async fn a_persona_saying_ids_asks_for_no_id() {
    let (router, provider) = with_persona(&contract(), "Use realistic IDs.", vec![written()]);
    let req = axum::http::Request::builder()
        .uri("/widgets/w42")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, _, body) = support::send_to(&router, req).await;
    assert_eq!(status, 200, "{body}");
    let requests = provider.requests();
    let mb_llm::ResponseFormat::JsonSchema { schema, .. } = &requests[0].response_format else {
        panic!("no schema format");
    };
    let mut asked: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    asked.sort();
    assert_eq!(
        asked,
        ["archived", "description", "html_url", "maker", "title"],
        "{schema}"
    );
    assert!(
        body["node_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("W_"))
    );
}

#[tokio::test]
async fn a_create_has_the_model_write_an_id_the_persona_names() {
    let (router, provider) = with_persona(
        &pulls(),
        "Pull requests of a router project. node_id values use the PR_kwDO prefix.",
        vec![json!({"title": "Fix", "state": "open", "node_id": "PR_kwDOabc12345"})],
    );
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/repos/octo/hello/pulls")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(json!({"title": "Fix"}).to_string()))
        .unwrap();
    let (status, _, body) = support::send_to(&router, req).await;
    assert_eq!(status, 201, "{body}");
    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "{body}");
    let mb_llm::ResponseFormat::JsonSchema { schema, .. } = &requests[0].response_format else {
        panic!("no schema format");
    };
    assert!(schema["properties"].get("node_id").is_some(), "{schema}");
    assert_eq!(body["node_id"], "PR_kwDOabc12345");
}
