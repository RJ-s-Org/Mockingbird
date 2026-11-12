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
    // Fill's value (an integer, the faker's) stands.
    assert!(body["number"].is_i64(), "fill's value stands: {body}");
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
    // The value its member refuses is dropped, and noted.
    let request = record_of(&h, "renameMember");
    let pointers: Vec<Value> = request["expr_failed"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| f["pointer"].clone())
        .collect();
    assert_eq!(
        pointers,
        vec![json!("/operations/renameMember/changes/number")],
        "{request}"
    );
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

/// Messages sent with a server token in a header the contract's security scheme
/// names.
const TOKENED: &str = r##"
openapi: 3.0.0
info: {title: Mail, version: "1"}
components:
  securitySchemes:
    serverToken: {type: apiKey, in: header, name: X-Server-Token}
  schemas:
    Sent:
      type: object
      required: [id, to]
      properties: {id: {type: string}, to: {type: string}}
    Error:
      type: object
      required: [error]
      properties:
        error:
          type: object
          required: [code, message]
          properties: {code: {type: string}, message: {type: string}}
security: [{serverToken: []}]
paths:
  /messages:
    post:
      operationId: sendMessage
      parameters: [{name: X-Server-Token, in: header, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {to: {type: string}}}}}}
      responses:
        "201": {description: sent, content: {application/json: {schema: {$ref: "#/components/schemas/Sent"}}}}
        "401": {description: bad token, content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
"##;

#[tokio::test]
async fn a_refusal_reads_a_credential_header_that_neither_the_record_nor_the_model_sees() {
    // The first entry fails on every call: its failure is noted without the value.
    // The second states no message, so the model writes its body: the prompt of a
    // call carrying the token.
    let world = json!({"operations": {"sendMessage": {"kind": "create", "refuse": [
        {"when": "int(request.headers['x-server-token']) > 0", "status": 401, "code": "0"},
        {"when": "request.headers['x-server-token'] != 'good-secret'", "status": 401,
         "code": "10"}
    ]}}});
    let h = support::described(TOKENED, world, vec![json!({"id": "s1", "to": "a@x.io"})]);
    let (status, _, body) = h
        .send(
            "POST",
            "/messages",
            Some(json!({"to": "a@x.io"})),
            &[("x-server-token", "wrong")],
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "10", "{body}");

    let (status, _, body) = h
        .send(
            "POST",
            "/messages",
            Some(json!({"to": "a@x.io"})),
            &[("x-server-token", "good-secret")],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let records = h.records();
    assert_eq!(records.len(), 2, "{records:?}");
    let records = format!("{records:?}");
    assert!(records.contains("expr_failed"), "{records}");
    assert!(!records.contains("good-secret"), "{records}");
    assert!(!records.contains("wrong"), "{records}");
    let requests = h.provider.requests();
    assert!(!requests.is_empty(), "the refusal's body is the model's");
    let prompts = format!("{requests:?}");
    assert!(!prompts.contains("good-secret"), "{prompts}");
    assert!(!prompts.contains("wrong"), "{prompts}");
}

#[tokio::test]
async fn a_refusal_stating_its_code_and_message_asks_no_model() {
    let world = json!({"operations": {"addMember": {"kind": "create", "refuse": [
        {"when": "request.body.amount > 1000", "status": 422,
         "code": "amount_too_large", "message": "amount is over 1000"},
        {"when": "request.body.amount < 0", "status": 422, "code": "amount_negative"}
    ]}}});
    let h = members(world, vec![]);
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "A", "amount": 5000})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "amount_too_large", "{body}");
    assert_eq!(body["error"]["message"], "amount is over 1000", "{body}");
    assert!(
        h.provider.requests().is_empty(),
        "{:?}",
        h.provider.requests()
    );
    assert_eq!(
        h.records()[0].source.as_deref(),
        Some("state"),
        "{:?}",
        h.records()
    );

    // With no message the model still writes the body's text.
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "A", "amount": -5})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "amount_negative", "{body}");
    assert_eq!(
        h.provider.requests().len(),
        1,
        "{:?}",
        h.provider.requests()
    );
}

/// [`MEMBERS`] with its `Error` schema replaced by `error` (YAML at that indent).
fn members_erring(error: &str, world: Value) -> DescribedHarness {
    let (head, _) = MEMBERS
        .split_once("    Error:\n")
        .expect("MEMBERS declares Error last");
    support::described(&format!("{head}    Error:\n{error}"), world, vec![])
}

fn refuse_large(code: &str) -> Value {
    json!({"operations": {"addMember": {"kind": "create", "refuse": [
        {"when": "request.body.amount > 1000", "status": 422,
         "code": code, "message": "amount is over 1000"}
    ]}}})
}

async fn send_large(h: &DescribedHarness) -> Value {
    let (status, _, body) = h
        .send(
            "POST",
            "/members",
            Some(json!({"email": "a@x.io", "name": "A", "amount": 5000})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    body
}

#[tokio::test]
async fn a_stated_refusal_whose_code_its_schema_cannot_hold_still_asks_the_model() {
    // An integer code member: the stated `amount_too_large` is not placed.
    let h = members_erring(
        "      type: object
      required: [error]
      properties:
        error:
          type: object
          required: [code, message]
          properties: {code: {type: integer}, message: {type: string}}
",
        refuse_large("amount_too_large"),
    );
    let body = send_large(&h).await;
    assert_eq!(body["error"]["message"], "amount is over 1000", "{body}");
    assert_eq!(
        h.provider.requests().len(),
        1,
        "{:?}",
        h.provider.requests()
    );

    // With no code member at all, the same (`hint` is free text the model would
    // write: with none, no model is ever asked for this schema).
    let h = members_erring(
        "      type: object
      required: [message, hint]
      properties: {message: {type: string}, hint: {type: string}}
",
        refuse_large("amount_too_large"),
    );
    let body = send_large(&h).await;
    assert_eq!(body["message"], "amount is over 1000", "{body}");
    assert_eq!(
        h.provider.requests().len(),
        1,
        "{:?}",
        h.provider.requests()
    );
}

#[tokio::test]
async fn a_stated_refusal_leaving_other_free_text_still_asks_the_model() {
    // JSON:API's `title` beside `code` and `detail` is the faker's word unless the
    // model writes it.
    let errors = |item: &str| {
        format!(
            "      type: object
      required: [errors]
      properties:
        errors:
          type: array
          items: {{type: object, properties: {item}}}
"
        )
    };
    let h = members_erring(
        &errors("{code: {type: string}, title: {type: string}, detail: {type: string}}"),
        refuse_large("amount_too_large"),
    );
    let body = send_large(&h).await;
    assert_eq!(body["errors"][0]["code"], "amount_too_large", "{body}");
    assert_eq!(
        h.provider.requests().len(),
        1,
        "{:?}",
        h.provider.requests()
    );

    // With nothing but the code and the message, the faker's body stands.
    let h = members_erring(
        &errors("{code: {type: string}, detail: {type: string}}"),
        refuse_large("amount_too_large"),
    );
    let body = send_large(&h).await;
    assert_eq!(body["errors"][0]["code"], "amount_too_large", "{body}");
    assert_eq!(body["errors"][0]["detail"], "amount is over 1000", "{body}");
    assert!(
        h.provider.requests().is_empty(),
        "{:?}",
        h.provider.requests()
    );
}

#[tokio::test]
async fn a_refusal_whose_message_is_blank_still_asks_the_model() {
    let world = json!({"operations": {"addMember": {"kind": "create", "refuse": [
        {"when": "request.body.amount > 1000", "status": 422,
         "code": "amount_too_large", "message": "   "}
    ]}}});
    let h = members(world, vec![]);
    let body = send_large(&h).await;
    assert_eq!(body["error"]["code"], "amount_too_large", "{body}");
    assert_ne!(
        body["error"]["message"], "   ",
        "a blank message is never laid: {body}"
    );
    assert_eq!(
        h.provider.requests().len(),
        1,
        "{:?}",
        h.provider.requests()
    );
}

#[tokio::test]
async fn a_stated_refusal_whose_message_its_schema_refuses_still_asks_the_model() {
    // The message member's pattern refuses the stated message: only the code lands.
    let h = members_erring(
        "      type: object
      required: [error]
      properties:
        error:
          type: object
          required: [code, message]
          properties: {code: {type: string}, message: {type: string, pattern: '^x+$'}}
",
        refuse_large("amount_too_large"),
    );
    let body = send_large(&h).await;
    assert_eq!(body["error"]["code"], "amount_too_large", "{body}");
    assert_ne!(body["error"]["message"], "amount is over 1000", "{body}");
    assert_eq!(
        h.provider.requests().len(),
        1,
        "{:?}",
        h.provider.requests()
    );
}

/// Issues created under a repository and read by number (trimmed from a GitHub
/// contract): the author is an object with required members, `closed_at` nullable.
const ISSUES: &str = r##"
openapi: 3.0.3
info: {title: Issues, version: "1"}
paths:
  /repos/{owner}/{repo}/issues:
    parameters:
      - {name: owner, in: path, required: true, schema: {type: string}}
      - {name: repo, in: path, required: true, schema: {type: string}}
    post:
      operationId: issuesCreate
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [title]
              properties:
                title: {type: string}
                body: {type: string}
      responses:
        "201": {description: Created, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
  /repos/{owner}/{repo}/issues/{issue_number}:
    get:
      operationId: issuesGet
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: issue_number, in: path, required: true, schema: {type: integer}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
components:
  schemas:
    Issue:
      type: object
      required: [id, node_id, number, title, state, url, repository_url, user, comments, created_at]
      properties:
        id: {type: integer}
        node_id: {type: string}
        number: {type: integer}
        title: {type: string}
        body: {type: string, nullable: true}
        state: {type: string, enum: [open, closed]}
        locked: {type: boolean}
        url: {type: string}
        repository_url: {type: string}
        user:
          type: object
          required: [login, id, type]
          properties:
            login: {type: string}
            id: {type: integer}
            type: {type: string, enum: [Bot, User]}
        comments: {type: integer}
        created_at: {type: string, format: date-time}
        closed_at: {type: string, nullable: true}
"##;

fn issues_create(set: Value) -> Value {
    json!({
        "types": {"issues": {"identity": {"member": "id"}}},
        "operations": {"issuesCreate": {"kind": "create", "type": "issues", "set": set}}
    })
}

/// What fill answers for an issue: decoys for every member `set` writes.
fn issue_fill() -> Value {
    json!({
        "node_id": "n", "url": "u", "created_at": "2026-01-01T00:00:00Z",
        "user": {"id": 1, "login": "octo", "type": "User"},
        "body": "decoy", "comments": 999, "number": 999, "repository_url": "decoy", "closed_at": "decoy",
        "locked": true, "state": "closed"
    })
}

#[tokio::test]
async fn a_create_s_set_with_a_nested_member_is_in_its_answer_and_its_stored_record() {
    // `user.type` leaves `user` without the members fill writes next: no reason to
    // drop what the other entries set. A `null` on a nullable member is set.
    let world = issues_create(json!({
        "comments": "0",
        "number": "seq('issues:' + request.path.owner + '/' + request.path.repo)",
        "repository_url": "'https://api.github.com/repos/' + request.path.owner + '/' + request.path.repo",
        "closed_at": "null",
        "locked": "false",
        "state": "'open'",
        "title": "request.body.title",
        "body": "request.body.body",
        "user.type": "'Bot'"
    }));
    let h = support::described(ISSUES, world, vec![issue_fill(), issue_fill()]);
    for n in 1..=2 {
        let (status, headers, body) = h
            .send(
                "POST",
                "/repos/acme/backups/issues",
                Some(json!({"title": "down", "body": "b"})),
                &[],
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(headers["x-mockingbird-source"], "state", "{body}");
        assert_eq!(body["number"], n, "{body}");
        assert_eq!(body["comments"], 0, "{body}");
        assert_eq!(
            body["repository_url"], "https://api.github.com/repos/acme/backups",
            "{body}"
        );
        assert_eq!(body["closed_at"], Value::Null, "{body}");
        assert_eq!(body["locked"], false, "{body}");
        assert_eq!(body["state"], "open", "{body}");
        assert_eq!(body["user"]["type"], "Bot", "{body}");
        assert_eq!(
            body["user"]["login"], "octo",
            "fill completes the rest: {body}"
        );
        let id = body["id"].to_string();
        let stored = h.stored("issues", &id).await.unwrap_or_default();
        for member in ["number", "comments", "repository_url", "closed_at", "user"] {
            assert_eq!(stored[member], body[member], "{member}: {stored}");
        }
    }
}

#[tokio::test]
async fn a_set_value_its_member_refuses_is_on_the_request_record() {
    let world = issues_create(json!({"comments": "'many'", "locked": "false"}));
    let h = support::described(ISSUES, world, vec![issue_fill()]);
    let (status, _, body) = h
        .send(
            "POST",
            "/repos/acme/backups/issues",
            Some(json!({"title": "down"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["locked"], false, "{body}");
    let request = record_of(&h, "issuesCreate");
    let failed = request["expr_failed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(failed.len(), 1, "{request}");
    assert_eq!(
        failed[0]["pointer"],
        "/operations/issuesCreate/set/comments"
    );
}

#[tokio::test]
async fn a_null_a_create_s_set_gives_a_member_that_refuses_it_sets_nothing() {
    // `node_id` is no nullable member: fill completes it, and nothing is noted.
    let h = support::described(
        ISSUES,
        issues_create(json!({"node_id": "null", "comments": "0"})),
        vec![issue_fill()],
    );
    let (status, _, body) = h
        .send(
            "POST",
            "/repos/acme/backups/issues",
            Some(json!({"title": "down"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // An id-like string, the faker's.
    assert!(body["node_id"].is_string(), "{body}");
    assert_eq!(body["comments"], 0, "{body}");
    let request = record_of(&h, "issuesCreate");
    assert!(request.get("expr_failed").is_none(), "{request}");
}

/// Things read by id, holding at most three members: each of `a` and `b` fits its
/// member, but a record holding both does not fit the type.
const THINGS: &str = r##"
openapi: 3.0.3
info: {title: Things, version: "1"}
paths:
  /things:
    post:
      operationId: createThing
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [name], properties: {name: {type: string}}}}}
      responses:
        "201": {description: Created, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}
  /things/{id}:
    get:
      operationId: getThing
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}
components:
  schemas:
    Thing:
      type: object
      required: [id, name]
      maxProperties: 3
      properties:
        id: {type: string}
        name: {type: string}
        a: {type: string}
        b: {type: string}
"##;

#[tokio::test]
async fn a_create_whose_set_breaks_the_finished_record_is_generated_and_its_set_noted() {
    // The executor's record breaks the type's reads once built: it is never stored,
    // the create is generated, and on the generated record the set, which breaks it
    // too, is dropped whole and noted.
    let world =
        json!({"operations": {"createThing": {"kind": "create", "set": {"a": "'x'", "b": "'y'"}}}});
    let generated = json!({"id": "t1", "name": "lamp"});
    let h = support::described(THINGS, world, vec![generated.clone()]);
    let (status, _, body) = h
        .send("POST", "/things", Some(json!({"name": "lamp"})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body, generated, "{body}");
    let request = record_of(&h, "createThing");
    assert_eq!(request["exec_fallback"], "record_invalid", "{request}");
    assert_eq!(request["render_error"], "<root> maxProperties", "{request}");
    let failed = request["expr_failed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(failed.len(), 1, "{request}");
    assert_eq!(failed[0]["pointer"], "/operations/createThing", "{request}");
    let error = failed[0]["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("<root>") && error.contains("maxProperties"),
        "{request}"
    );
    // Only the generated record is stored, as it fits.
    let ids = h.stored_ids("things").await;
    assert_eq!(ids, vec!["t1".to_string()], "{ids:?}");
    assert_eq!(h.stored("things", "t1").await, Some(generated));
}
