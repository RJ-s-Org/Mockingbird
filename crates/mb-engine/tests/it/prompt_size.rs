//! A generated read of a large answer whose sub-schemas repeat: its prompt stays
//! bounded, and the answer is still checked against the contract's whole schema.

use mb_engine::{GenerationContext, OperationCall, SchemaValidator, Source};
use mb_llm::ResponseFormat;
use serde_json::{Map, Value, json};

use crate::support::scripted;

/// Most bytes a read's prompt may send: its messages and its response format.
const PROMPT_BOUND: usize = 20 * 1024;

/// Member `name` of a link: a described URL with two examples.
fn link(name: &str) -> (String, Value) {
    (
        name.to_string(),
        json!({
            "type": "string",
            "description": format!("The {name} API URL template, expanded by the client."),
            "examples": [
                format!("https://api.example.test/repos/octo/hello/{name}{{/id}}"),
                format!("https://api.example.test/repos/octo/world/{name}{{/id}}")
            ]
        }),
    )
}

/// `count` described links named `<prefix>_<i>_url`, and their names.
fn links(prefix: &str, count: usize) -> (Map<String, Value>, Vec<String>) {
    let props: Map<String, Value> = (0..count)
        .map(|i| link(&format!("{prefix}_{i}_url")))
        .collect();
    let names = props.keys().cloned().collect();
    (props, names)
}

/// A repository answer: 60 links, flags, and a user four times over (owner,
/// organization, an id-or-object `template_owner`, a nullable `parent`'s owner), each
/// user 25 links of its own.
fn contract() -> mb_mcm::Bundle {
    let (mut user, user_required) = links("user", 25);
    user.insert(
        "login".into(),
        json!({"type": "string", "examples": ["octocat"]}),
    );
    let mut user_required = user_required;
    user_required.push("login".into());
    let (mut repo, mut repo_required) = links("repo", 60);
    for flag in ["private", "archived", "fork", "has_issues"] {
        repo.insert(
            flag.into(),
            json!({"type": "boolean", "description": format!("Whether the repository is {flag}."), "examples": [false]}),
        );
        repo_required.push(flag.into());
    }
    repo.insert(
        "full_name".into(),
        json!({"type": "string", "examples": ["octo/hello"]}),
    );
    repo.insert("owner".into(), json!({"$ref": "#/components/schemas/User"}));
    repo.insert(
        "organization".into(),
        json!({"anyOf": [{"$ref": "#/components/schemas/User"}, {"type": "null"}]}),
    );
    repo.insert(
        "template_owner".into(),
        json!({"anyOf": [{"type": "string"}, {"$ref": "#/components/schemas/User"}]}),
    );
    repo.insert(
        "parent".into(),
        json!({"anyOf": [{"$ref": "#/components/schemas/Parent"}, {"type": "null"}]}),
    );
    repo_required.extend(["full_name", "owner"].map(String::from));
    let doc = json!({
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
            "Parent": {"type": "object", "required": ["full_name", "owner"], "properties": {
                "full_name": {"type": "string"},
                "owner": {"$ref": "#/components/schemas/User"}
            }},
            "Repo": {"type": "object", "required": repo_required, "properties": repo}
        }}
    });
    mb_import::compile(doc.to_string().as_bytes()).unwrap()
}

/// A valid answer: every required member, each link a URL.
fn answer() -> Value {
    let user: Map<String, Value> = (0..25)
        .map(|i| {
            (
                format!("user_{i}_url"),
                json!(format!("https://api.example.test/users/octo/{i}")),
            )
        })
        .chain([("login".to_string(), json!("octo"))])
        .collect();
    let mut repo: Map<String, Value> = (0..60)
        .map(|i| {
            (
                format!("repo_{i}_url"),
                json!(format!("https://api.example.test/repos/octo/hello/{i}")),
            )
        })
        .collect();
    for flag in ["private", "archived", "fork"] {
        repo.insert(flag.into(), json!(false));
    }
    repo.insert("has_issues".into(), json!(true));
    repo.insert("full_name".into(), json!("octo/hello"));
    repo.insert("owner".into(), Value::Object(user));
    repo.insert("organization".into(), Value::Null);
    repo.insert("template_owner".into(), json!("octo"));
    repo.insert("parent".into(), Value::Null);
    Value::Object(repo)
}

fn ctx() -> GenerationContext<'static> {
    GenerationContext {
        related: &[],
        target: None,
        forced_status: None,
        scenario: None,
        response_schema: None,
        earlier_requests: &[],
        earlier_own_body: None,
        earlier_answers: &[],
        earlier_writes: &[],
        earlier_origins: &[],
        served_cursors: &[],
        created_since: &[],
        alongside: &[],
        same_values: &[],
        changes_by_effects: false,
        update_guard: None,
        answer_records: None,
        restates: None,
        shown_target: None,
    }
}

#[tokio::test]
async fn a_large_repeated_read_schema_stays_bounded_and_the_answer_still_validates() {
    let bundle = contract();
    let op = bundle.operation("getRepo").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let full = bundle.with_reachable_defs(&schema);
    // The contract's whole answer schema alone is over the bound.
    assert!(
        full.to_string().len() > PROMPT_BOUND,
        "{}",
        full.to_string().len()
    );
    let answer = answer();
    let validator = SchemaValidator::new(&full).unwrap();
    assert!(validator.is_valid(&answer));

    let (provider, engine) = scripted(vec![Ok(answer.to_string())]);
    let mut path_params = Map::new();
    path_params.insert("owner".into(), json!("octo"));
    path_params.insert("repo".into(), json!("hello"));
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: "getRepo".into(),
        path_params,
        query: Map::new(),
        headers: Map::new(),
        body: None,
    };
    let out = engine.generate(&bundle, op, &call, ctx()).await.unwrap();
    assert_eq!(out.source, Source::Llm);
    let body = out.body.unwrap();
    assert!(validator.is_valid(&body), "{body}");
    assert_eq!(body["owner"]["login"], "octo");

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let ResponseFormat::JsonSchema { schema: format, .. } = &request.response_format else {
        panic!(
            "a read is asked for its schema: {:?}",
            request.response_format
        );
    };
    let format = format.to_string();
    let messages: usize = request.messages.iter().map(|m| m.content.len()).sum();
    assert!(
        messages + format.len() <= PROMPT_BOUND,
        "{messages} bytes of messages and {} of format",
        format.len()
    );
    // The format repeats no annotation the system message carries.
    assert!(!format.contains("\"description\""), "{format}");
    assert!(!format.contains("\"examples\""), "{format}");
    // The answer's own members, and the user a required member holds, are still
    // shown whole.
    let system = &request.messages[0].content;
    assert!(system.contains("\"login\""), "{system}");
    assert!(system.contains("repo_59_url"), "{system}");
}
