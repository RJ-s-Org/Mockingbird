//! What a GraphQL create starts a record with that its input leaves out (gh-cli trial
//! run 6, where `gh issue create` came back `CLOSED` with an `id` of `"sable"`): a
//! state the world moves starts where the world or the create's examples say, else
//! where the model picks, never in the first value its enum lists; an `ID` member gets
//! a minted id, never a faker word.
#![cfg(all(feature = "graphql", feature = "http-rest"))]

use serde_json::{Value, json};

use crate::support;

/// Issues keyed by `number`, beside a node `id`, as GitHub's: the state enum lists
/// `CLOSED` first.
const SDL: &str = r#"
enum IssueState { CLOSED OPEN }
enum Priority { HIGH LOW }
type Label { id: ID! name: String! }
type Actor { nodeId: ID! createdAt: String! }
type Issue {
  id: ID!
  number: Int!
  title: String!
  body: String!
  state: IssueState!
  priority: Priority!
  labelId: ID!
  author: Actor!
}
input CreateIssueInput { repositoryId: ID! title: String! body: String! }
type CreateIssuePayload { issue: Issue }
input CreateLabelInput { name: String! }
type CreateLabelPayload { label: Label }
type Query { issue(number: Int!): Issue label(id: ID!): Label }
type Mutation {
  createIssue(input: CreateIssueInput!): CreateIssuePayload
  createLabel(input: CreateLabelInput!): CreateLabelPayload
  closeIssue(number: Int!): Issue
  reopenIssue(number: Int!): Issue
}
"#;

const CREATE: &str = r#"mutation { createIssue(input: {repositoryId: "R_1",
    title: "Router panics", body: "On an empty path."}) {
    issue { id number title state priority labelId author { nodeId } } } }"#;

/// The operations every world here describes, with `more` laid over them.
fn world(types: Value, more: Value) -> Value {
    let mut operations = json!({
        "Query.issue": {"kind": "read", "type": "issue", "target": {"member": "number"}},
        "Query.label": {"kind": "read", "type": "label", "target": {"member": "id"}},
        "Mutation.createIssue": {"kind": "create", "type": "issue", "answer": {"record": "/issue"}},
        "Mutation.createLabel": {"kind": "create", "type": "label", "answer": {"record": "/label"}}
    });
    if let (Some(ops), Value::Object(more)) = (operations.as_object_mut(), more) {
        ops.extend(more);
    }
    json!({"version": 1, "types": types, "operations": operations})
}

/// Closing sets `CLOSED` and reopening `OPEN`: the world moves `state` both ways.
fn both_ways() -> Value {
    json!({
        "Mutation.closeIssue": {"kind": "action", "type": "issue", "target": {"member": "number"},
            "set": {"state": "'CLOSED'"}},
        "Mutation.reopenIssue": {"kind": "action", "type": "issue", "target": {"member": "number"},
            "set": {"state": "'OPEN'"}}
    })
}

fn bundle() -> mb_mcm::Bundle {
    mb_import::compile(SDL.as_bytes()).unwrap()
}

/// [`bundle`], the create's answer showing an `OPEN` issue in its examples.
fn shown_open() -> mb_mcm::Bundle {
    let mut bundle = bundle();
    if let Some(create) = bundle
        .operations
        .iter_mut()
        .find(|op| op.id == "Mutation.createIssue")
    {
        for output in &mut create.outputs {
            if let Some(Value::Object(schema)) = output.schema.as_mut() {
                schema.insert("examples".into(), json!([{"issue": {"state": "OPEN"}}]));
            }
        }
    }
    bundle
}

/// No world description: tier 0 alone, as the trial ran.
fn no_world() -> Value {
    json!({"version": 1})
}

/// The created issue as answered, after checking the executor answered it.
async fn created(h: &support::GraphqlHarness) -> Value {
    let body = h.query(CREATE).await;
    assert!(body.get("errors").is_none(), "{body}");
    assert_eq!(
        body["extensions"]["mockingbird"]["sources"]["createIssue"], "state",
        "{body}"
    );
    body["data"]["createIssue"]["issue"].clone()
}

/// The members the model was asked for, call by call.
fn asked(h: &support::GraphqlHarness) -> Vec<Vec<String>> {
    h.provider
        .requests()
        .iter()
        .map(|r| {
            let mb_llm::ResponseFormat::JsonSchema { schema, .. } = &r.response_format else {
                return Vec::new();
            };
            let mut names: Vec<String> = schema["properties"]
                .as_object()
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();
            names
        })
        .collect()
}

#[tokio::test]
async fn a_create_starts_in_the_lifecycles_initial_state() {
    let types = json!({"issue": {"lifecycle": {"member": "state", "states": ["CLOSED", "OPEN"], "initial": "OPEN"}}});
    let h = support::graphql_described(bundle(), world(types, json!({})), vec![]);
    let issue = created(&h).await;
    assert_eq!(issue["state"], "OPEN", "{issue}");
    assert!(h.provider.requests().is_empty(), "{:?}", asked(&h));
}

/// With no `initial`, the one state no transition moves to is where a record starts.
#[tokio::test]
async fn a_create_starts_in_the_state_no_transition_moves_to() {
    let types = json!({"issue": {"lifecycle": {"member": "state", "states": ["CLOSED", "OPEN"]}}});
    let close = json!({
        "Mutation.closeIssue": {"kind": "action", "type": "issue", "target": {"member": "number"},
            "transition": {"state": "CLOSED"}}
    });
    let h = support::graphql_described(bundle(), world(types, close), vec![]);
    let issue = created(&h).await;
    assert_eq!(issue["state"], "OPEN", "{issue}");
    assert!(h.provider.requests().is_empty(), "{:?}", asked(&h));
}

/// Where the world moves the state both ways, the state the create's answer shows in
/// the contract's examples is where a record starts.
#[tokio::test]
async fn a_create_starts_in_the_state_its_examples_show() {
    let h = support::graphql_described(shown_open(), world(json!({}), both_ways()), vec![]);
    let issue = created(&h).await;
    assert_eq!(issue["state"], "OPEN", "{issue}");
    assert!(h.provider.requests().is_empty(), "{:?}", asked(&h));
}

/// Where nothing says where the state starts, the model picks it: never the first
/// value the enum lists.
#[tokio::test]
async fn a_create_asks_the_model_where_nothing_says_where_its_state_starts() {
    let h = support::graphql_described(
        bundle(),
        world(json!({}), both_ways()),
        vec![json!({"state": "OPEN"})],
    );
    let issue = created(&h).await;
    assert_eq!(asked(&h), vec![vec!["state".to_string()]]);
    assert_eq!(issue["state"], "OPEN", "{issue}");
}

/// With no world, as the trial ran, a member the contract names as a state is the
/// model's to pick, never its first listed value; an enum named as no state keeps
/// fill's order (here its first value).
#[tokio::test]
async fn with_no_world_a_state_named_enum_is_asked_of_the_model() {
    let h = support::graphql_described(bundle(), no_world(), vec![json!({"state": "OPEN"})]);
    let issue = created(&h).await;
    assert_eq!(asked(&h), vec![vec!["state".to_string()]]);
    assert_eq!(issue["state"], "OPEN", "{issue}");
    assert_eq!(issue["priority"], "HIGH", "{issue}");
}

/// With no world, the state the create's answer shows in the contract's examples is
/// where a record starts, with no model call.
#[tokio::test]
async fn with_no_world_a_create_starts_in_the_state_its_examples_show() {
    let h = support::graphql_described(shown_open(), no_world(), vec![]);
    let issue = created(&h).await;
    assert_eq!(issue["state"], "OPEN", "{issue}");
    assert!(h.provider.requests().is_empty(), "{:?}", asked(&h));
}

/// An `ID` member nothing sent is minted (letters and digits, never a faker word), in
/// the form the session's records show where they show one; a reference id names a
/// record the session holds.
#[tokio::test]
async fn an_id_member_is_minted_and_a_reference_names_a_held_record() {
    let h = support::graphql_described(bundle(), world(json!({}), json!({})), vec![]);
    let issue = created(&h).await;
    let id = issue["id"].as_str().unwrap_or_default();
    assert_eq!(id.len(), 20, "{issue}");
    assert!(id.chars().all(|c| c.is_ascii_alphanumeric()), "{issue}");
    // An id inside an object the faker draws is minted too.
    let node_id = issue["author"]["nodeId"].as_str().unwrap_or_default();
    assert_eq!(node_id.len(), 20, "{issue}");
    assert!(
        node_id.chars().all(|c| c.is_ascii_alphanumeric()),
        "{issue}"
    );

    let h = support::graphql_described(bundle(), world(json!({}), json!({})), vec![]);
    let session = mb_state::SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    };
    let label = json!({"id": "LA_kwDOlabel1", "name": "bug"});
    let issues = [
        json!({"id": "I_kwDOAbCdEf12", "number": 1, "title": "a", "body": "b", "state": "OPEN", "labelId": "LA_kwDOlabel1"}),
        json!({"id": "I_kwDOGhIjKl34", "number": 2, "title": "c", "body": "d", "state": "OPEN", "labelId": "LA_kwDOlabel1"}),
    ];
    let held = [("label", "LA_kwDOlabel1", label)]
        .into_iter()
        .chain(issues.iter().map(|i| ("issue", "", i.clone())));
    for (t, key, data) in held {
        let id = match key {
            "" => data["number"].to_string(),
            key => key.to_string(),
        };
        let resource = mb_state::StoredResource {
            resource_type: t.to_string(),
            id,
            data,
        };
        h.rt.state.upsert(&session, resource).await.unwrap();
    }
    let issue = created(&h).await;
    let id = issue["id"].as_str().unwrap_or_default();
    assert!(id.starts_with("I_") && id.len() > 2, "{issue}");
    assert!(
        !["I_kwDOAbCdEf12", "I_kwDOGhIjKl34"].contains(&id),
        "{issue}"
    );
    assert_eq!(issue["labelId"], "LA_kwDOlabel1", "{issue}");
    assert_eq!(h.held("issue").await.len(), 3);
}

/// A record keyed by an `ID` the contract shows no form of is minted letters and
/// digits, never a faker word, and held under it.
#[tokio::test]
async fn a_record_id_nothing_shows_the_form_of_is_minted_letters_and_digits() {
    let h = support::graphql_described(bundle(), world(json!({}), json!({})), vec![]);
    let body = h
        .query(r#"mutation { createLabel(input: {name: "bug"}) { label { id name } } }"#)
        .await;
    assert!(body.get("errors").is_none(), "{body}");
    assert_eq!(
        body["extensions"]["mockingbird"]["sources"]["createLabel"], "state",
        "{body}"
    );
    let label = &body["data"]["createLabel"]["label"];
    let id = label["id"].as_str().unwrap_or_default();
    assert_eq!(id.len(), 20, "{label}");
    assert!(id.chars().all(|c| c.is_ascii_alphanumeric()), "{label}");
    let held = h.held("label").await;
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["id"], id, "{held:?}");
}
