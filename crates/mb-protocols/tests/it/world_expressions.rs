//! World expressions at runtime: an operation's `set` (and `changes`, read as `set`)
//! on the record a create, update or action stores, and its `refuse` before anything is
//! stored or generated; what failed, and why the executor went on to generation, on the
//! request record.
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness};

/// Members added to `/members` and read by id, renamed by an action; lists read by id.
/// A refusal's body nests its code and message under `error`.
const MEMBERS: &str = r##"
openapi: 3.0.0
info: {title: Members, version: "1"}
paths:
  /members:
    post:
      operationId: addMember
      parameters: [{name: X-Actor, in: header, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {email: {type: string}, name: {type: string}, list_id: {type: string}, amount: {type: integer}, number: {type: string}}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Member"}}}}
        "409": {description: taken, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
        "422": {description: invalid, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
  /members/{id}:
    get:
      operationId: getMember
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Member"}}}}}
  /members/{id}/rename:
    post:
      operationId: renameMember
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {new_name: {type: string}, label: {type: string}, meta: {type: object, properties: {"0": {type: string}}}, tags: {type: array, items: {type: string}}, note: {type: string, nullable: true}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Member"}}}}}
  /lists/{id}:
    get:
      operationId: getList
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/List"}}}}}
components:
  schemas:
    Member:
      type: object
      required: [id, email, name]
      properties:
        id: {type: string}
        email: {type: string}
        name: {type: string}
        list_id: {type: string}
        currency: {type: string}
        label: {type: string}
        tier: {type: string}
        number: {type: integer}
        code: {type: string}
        note: {}
    List:
      type: object
      required: [id, currency]
      properties: {id: {type: string}, currency: {type: string}}
    Error:
      type: object
      required: [error]
      properties:
        error:
          type: object
          required: [code, message]
          properties: {code: {type: string}, message: {type: string}}
"##;

/// What fill answers for a member's missing members: a value for every member, so
/// one the description sets would show if fill wrote it.
fn fill_decoys() -> Value {
    json!({
        "list_id": "decoy", "currency": "XXX", "label": "decoy", "tier": "decoy",
        "number": 999, "code": "decoy", "note": "decoy"
    })
}

fn members(world: Value, script: Vec<Value>) -> DescribedHarness {
    support::described(MEMBERS, world, script)
}

fn add_member(set: Value) -> Value {
    json!({"operations": {"addMember": {"kind": "create", "set": set}}})
}

/// The one request record of `operation`.
fn record_of(h: &DescribedHarness, operation: &str) -> Value {
    let records = h.records();
    let mine: Vec<_> = records
        .iter()
        .filter(|r| r.operation_id.as_deref() == Some(operation))
        .collect();
    assert_eq!(mine.len(), 1, "{records:?}");
    mine[0].request.clone()
}

/// That the provider was asked for no member: only, at most, a refusal's error body.
fn assert_no_record_generated(h: &DescribedHarness) {
    let asked: Vec<String> = h
        .provider
        .requests()
        .iter()
        .map(|r| format!("{:?}", r.response_format))
        .collect();
    assert!(
        asked.iter().all(|a| !a.contains("email")),
        "a refusal generates no record: {asked:#?}"
    );
}

#[tokio::test]
async fn set_writes_members_from_the_request_a_held_record_a_template_a_hash_and_a_condition() {
    let h = members(
        add_member(json!({
            "id": "md5(lower(request.body.email))",
            "currency": "held('lists', request.body.list_id).currency",
            "label": "'member:' + request.body.name + '@' + request.headers['x-actor']",
            "tier": "request.body.amount > 100 ? 'gold' : 'basic'",
            "number": "request.body.amount * 2.0",
            "code": "request.body.amount"
        })),
        vec![fill_decoys()],
    );
    h.hold("lists", &[json!({"id": "l1", "currency": "EUR"})])
        .await;
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(
                json!({"email": "Ada@Example.com", "name": "Ada", "list_id": "l1", "amount": 150}),
            ),
            &[("x-actor", "ops")],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = "3e3417d7ef77d5932a6734b916515ed5";
    let want = json!({
        "id": id,
        "email": "Ada@Example.com",
        "name": "Ada",
        "list_id": "l1",
        "amount": null,
        "currency": "EUR",
        "label": "member:Ada@ops",
        "tier": "gold",
        // An integral double into an integer member is an integer; an integer into a
        // string member is its text.
        "number": 300,
        "code": "150"
    });
    for (member, value) in want.as_object().unwrap() {
        if value.is_null() {
            continue;
        }
        assert_eq!(&body[member], value, "{member}: {body}");
    }
    let stored = h
        .stored("members", id)
        .await
        .expect("stored under the set id");
    assert_eq!(stored["currency"], "EUR");
    assert_eq!(stored["number"], 300);
    assert!(record_of(&h, "addMember").get("expr_failed").is_none());
}

#[tokio::test]
async fn a_create_s_set_reads_the_record_as_built_its_set_id_first() {
    // `label` comes before `id` in the description; the id is still evaluated first.
    let h = members(
        add_member(json!({
            "label": "'https://x/' + record.id",
            "id": "'m_' + lower(request.body.name)",
            "code": "record.email"
        })),
        vec![fill_decoys()],
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "Ada"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], "m_ada", "{body}");
    assert_eq!(body["label"], "https://x/m_ada", "{body}");
    assert_eq!(body["code"], "a@x.io", "{body}");
    let stored = h.stored("members", "m_ada").await.expect("stored");
    assert_eq!(stored["label"], "https://x/m_ada");
    assert!(record_of(&h, "addMember").get("expr_failed").is_none());
}

#[tokio::test]
async fn a_create_s_set_reads_the_id_the_executor_minted() {
    let h = members(
        add_member(json!({"label": "'https://x/' + record.id"})),
        vec![fill_decoys()],
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "Ada"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().expect("an id").to_owned();
    assert_eq!(body["label"], format!("https://x/{id}"), "{body}");
    let stored = h.stored("members", &id).await.expect("stored");
    assert_eq!(stored["label"], format!("https://x/{id}"));
    assert!(record_of(&h, "addMember").get("expr_failed").is_none());
}

#[tokio::test]
async fn a_create_the_executor_gives_up_on_reads_the_generated_record_s_id() {
    // `number` sent as text that is no integer: the executor's record is refused and
    // the create is generated, under the generated id.
    let generated = json!({"id": "m-gen", "email": "a@x.io", "name": "Ada", "number": 1});
    let h = members(
        add_member(json!({"label": "'https://x/' + record.id"})),
        vec![fill_decoys(), generated],
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "Ada", "number": "abc"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let request = record_of(&h, "addMember");
    assert_eq!(request["exec_fallback"], "record_invalid", "{request}");
    assert_eq!(request["render_error"], "/number type", "{request}");
    let id = body["id"].as_str().expect("an id").to_owned();
    assert_eq!(body["label"], format!("https://x/{id}"), "{body}");
}

#[tokio::test]
async fn a_member_set_names_is_never_filled() {
    let h = members(
        add_member(json!({"tier": "request.body.amount > 100 ? 'gold' : 'basic'"})),
        vec![fill_decoys()],
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "A", "amount": 5})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["tier"], "basic", "{body}");
    // What fill wrote stands where nothing is set.
    assert_eq!(body["label"], "decoy", "{body}");
    let asked: Vec<String> = h
        .provider
        .requests()
        .iter()
        .map(|r| format!("{:?}", r.response_format))
        .collect();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        !asked[0].contains("\"tier\""),
        "fill was asked for a set member: {asked:?}"
    );
}

#[tokio::test]
async fn a_failed_expression_leaves_the_member_to_fill_and_is_on_the_request_record() {
    let h = members(
        add_member(json!({"number": "int(request.body.name)", "tier": "'basic'"})),
        vec![fill_decoys()],
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "Ada"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["number"], 999, "fill's value stands: {body}");
    assert_eq!(body["tier"], "basic", "{body}");
    let request = record_of(&h, "addMember");
    let failed = request["expr_failed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(failed.len(), 1, "{request}");
    assert_eq!(failed[0]["pointer"], "/operations/addMember/set/number");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("line 1, column"),
        "{request}"
    );
}

#[tokio::test]
async fn refusals_answer_in_order_with_code_and_message_in_a_nested_error() {
    let world = json!({"operations": {"addMember": {"kind": "create", "refuse": [
        {
            "when": "request.body.amount > 1000",
            "status": 422,
            "code": "amount_too_large",
            "message": {"expr": "'amount ' + string(request.body.amount) + ' is over 1000'"}
        },
        {
            "when": "exists('members', lower(it.email) == lower(request.body.email))",
            "status": 409,
            "code": "member_exists",
            "message": "this email is a member already"
        }
    ]}}});
    // No script: the error body is the faker's, with the refusal's facts over it.
    let h = members(world, vec![]);
    h.hold(
        "members",
        &[json!({"id": "m1", "email": "ada@x.io", "name": "Ada"})],
    )
    .await;
    // Both hold: the first in order answers.
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "ADA@x.io", "name": "A", "amount": 5000})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "amount_too_large", "{body}");
    assert_eq!(
        body["error"]["message"], "amount 5000 is over 1000",
        "{body}"
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "ADA@x.io", "name": "A", "amount": 5})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "member_exists", "{body}");
    assert_eq!(
        body["error"]["message"], "this email is a member already",
        "{body}"
    );
    assert_eq!(h.stored_ids("members").await, vec!["m1".to_string()]);
    assert_no_record_generated(&h);
    // Neither holds: the create goes on.
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "bob@x.io", "name": "Bob", "amount": 5})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_changes_world_sets_as_before_through_set() {
    let world = json!({"operations": {"renameMember": {
        "kind": "action",
        "target": {"path_param": "id"},
        "changes": {"name": {"from_request": "/new_name"}, "tier": {"value": "gold"}, "label": {"from_request": "/label"}}
    }}});
    let h = members(world, vec![]);
    let held = json!({"id": "m1", "email": "a@x.io", "name": "Ada", "label": "kept"});
    h.hold("members", std::slice::from_ref(&held)).await;
    let (status, _, body) = h
        .send(
            "POST",
            "/members/m1/rename",
            Some(json!({"new_name": "Grace"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let want =
        json!({"id": "m1", "email": "a@x.io", "name": "Grace", "label": "kept", "tier": "gold"});
    assert_eq!(body, want);
    assert_eq!(h.stored("members", "m1").await, Some(want));
    assert!(h.provider.requests().is_empty());
    // A missing request member sets nothing, and is no failure.
    assert!(record_of(&h, "renameMember").get("expr_failed").is_none());
}

#[tokio::test]
async fn why_the_executor_generated_is_on_the_request_record() {
    let h = members(
        json!({}),
        vec![json!({"id": "m9", "email": "a@x.io", "name": "A"})],
    );
    let (status, _, body) = h.send("GET", "/members/m9", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let request = record_of(&h, "getMember");
    assert_eq!(request["exec_fallback"], "not_held", "{request}");
    assert!(request.get("exec_fallback_on").is_none(), "{request}");
}

#[tokio::test]
async fn a_create_s_changes_set_nothing_as_before() {
    let world = json!({"operations": {"addMember": {
        "kind": "create",
        "changes": {"tier": {"value": "gold"}},
        "set": {"label": "'set'"}
    }}});
    let h = members(world, vec![fill_decoys()]);
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "A"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["label"], "set", "{body}");
    assert_eq!(
        body["tier"], "decoy",
        "changes never applied to a create: {body}"
    );
}

#[tokio::test]
async fn a_changes_world_keeps_its_pointer_and_value_semantics() {
    let world = json!({"operations": {"renameMember": {
        "kind": "action",
        "target": {"path_param": "id"},
        "changes": {
            // A digit token names an object's member as well as an array's item.
            "name": {"from_request": "/meta/0"},
            // `01` is no array index: it reads nothing.
            "label": {"from_request": "/tags/01"},
            // An explicit `null` sent is set.
            "note": {"from_request": "/note"},
            // A fixed value is never converted to its member's type.
            "number": {"value": "5"}
        }
    }}});
    let h = members(world, vec![]);
    let held = json!({"id": "m1", "email": "a@x.io", "name": "Ada", "label": "kept", "note": "n"});
    h.hold("members", std::slice::from_ref(&held)).await;
    let (status, _, body) = h
        .send(
            "POST",
            "/members/m1/rename",
            Some(json!({"meta": {"0": "Grace"}, "tags": ["t0", "t1"], "note": null})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let want =
        json!({"id": "m1", "email": "a@x.io", "name": "Grace", "label": "kept", "note": null});
    assert_eq!(body, want);
    assert_eq!(h.stored("members", "m1").await, Some(want));
    assert!(record_of(&h, "renameMember").get("expr_failed").is_none());
}

#[tokio::test]
async fn a_refusal_holds_on_a_call_the_executor_does_not_take() {
    let world = json!({"operations": {"addMember": {"kind": "create", "refuse": [
        {"when": "request.body.amount > 1000", "status": 422, "code": "amount_too_large"}
    ]}}});
    let h = members(world, vec![]);
    // A seeded call generates rather than executing.
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "A", "amount": 5000})),
            &[("x-mockingbird-seed", "7")],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "amount_too_large", "{body}");
    assert!(h.stored_ids("members").await.is_empty());
    assert_no_record_generated(&h);
}

#[tokio::test]
async fn a_create_that_generates_still_applies_set() {
    let h = members(
        add_member(json!({"id": "md5(lower(request.body.email))", "tier": "'gold'"})),
        vec![json!({"id": "gen1", "email": "a@x.io", "name": "A", "tier": "basic"})],
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "A@x.io", "name": "A"})),
            &[("x-mockingbird-seed", "7")],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = "a43c564056190dca51f9f4a6fae36c30";
    assert_eq!(body["id"], id, "{body}");
    assert_eq!(body["tier"], "gold", "{body}");
    let stored = h
        .stored("members", id)
        .await
        .expect("stored under the set id");
    assert_eq!(stored["tier"], "gold");
}

/// A generated create whose `set` identity names a record the session holds is not
/// stored over it: the held record keeps what it had.
#[tokio::test]
async fn a_generated_create_never_stores_over_a_held_record_its_set_identity_names() {
    let h = members(
        add_member(json!({"id": "md5(lower(request.body.email))"})),
        vec![json!({"id": "gen1", "email": "a@x.io", "name": "New"})],
    );
    let id = "a43c564056190dca51f9f4a6fae36c30";
    h.hold(
        "members",
        &[json!({"id": id, "email": "a@x.io", "name": "Orig"})],
    )
    .await;
    let (_, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "A@x.io", "name": "New"})),
            &[("x-mockingbird-seed", "7")],
        )
        .await;
    let stored = h.stored("members", id).await.expect("the held record");
    assert_eq!(stored["name"], "Orig", "{body}");
}

#[tokio::test]
async fn held_finds_a_record_past_the_first_page_of_the_type() {
    let h = members(
        add_member(json!({
            "currency": "held('lists', 'l0').currency",
            "label": "held('lists', 'l599').currency"
        })),
        vec![fill_decoys()],
    );
    let lists: Vec<Value> = (0..600)
        .map(|i| json!({"id": format!("l{i}"), "currency": format!("C{i}")}))
        .collect();
    h.hold("lists", &lists).await;
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "A"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["currency"], "C0", "{body}");
    assert_eq!(body["label"], "C599", "{body}");
}

#[tokio::test]
async fn a_refusal_whose_when_fails_does_not_fire_and_is_on_the_request_record() {
    let world = json!({"operations": {"addMember": {"kind": "create", "refuse": [
        {"when": "request.body.name > 5", "status": 422, "code": "bad"}
    ]}}});
    let h = members(world, vec![fill_decoys()]);
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "A"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let request = record_of(&h, "addMember");
    let failed = request["expr_failed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(failed.len(), 1, "{request}");
    assert_eq!(failed[0]["pointer"], "/operations/addMember/refuse/0/when");
}

#[tokio::test]
async fn an_alternate_key_set_writes_is_refused_when_another_record_holds_it() {
    let world = json!({
        "types": {"members": {"identity": {"member": "id", "alternates": ["code"]}}},
        "operations": {"addMember": {"kind": "create", "set": {"code": "lower(request.body.name)"}}}
    });
    let h = members(world, vec![fill_decoys()]);
    h.hold(
        "members",
        &[json!({"id": "m1", "email": "ada@x.io", "name": "Ada", "code": "ada"})],
    )
    .await;
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "other@x.io", "name": "ADA"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(h.stored_ids("members").await, vec!["m1".to_string()]);
}
