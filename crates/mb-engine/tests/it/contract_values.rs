//! Error bodies the mock builds itself (a `404` for what the session does not hold, a
//! refusal decided from state) take the values the contract names for their members:
//! a `const`, a one-value `enum`, an example, or a value the response's description
//! names in code position. Anything else stays the faker's.

use mb_engine::{Engine, EngineConfig, OperationCall, Source};
use mb_import::compile;
use mb_mcm::Bundle;
use serde_json::Value;

fn offline() -> Engine {
    Engine::new(None, EngineConfig::default())
}

/// One operation, `PUT /things/{id}`, whose `404` and `409` answer `not_found` and
/// `conflict`: the schema of each, and the description of each, as given.
fn contract(not_found: (&str, &str), conflict: (&str, &str)) -> Bundle {
    let src = format!(
        r##"
openapi: 3.1.0
info: {{title: Things, version: "1"}}
paths:
  /things/{{id}}:
    put:
      operationId: putThing
      parameters: [{{name: id, in: path, required: true, schema: {{type: string}}}}]
      requestBody: {{content: {{application/json: {{schema: {{type: object, properties: {{name: {{type: string}}}}}}}}}}}}
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{id: {{type: string}}}}}}}}}}}}
        "404": {{description: "{}", content: {{application/json: {{schema: {}}}}}}}
        "409": {{description: "{}", content: {{application/json: {{schema: {}}}}}}}
"##,
        not_found.1, not_found.0, conflict.1, conflict.0
    );
    compile(src.as_bytes()).unwrap()
}

const PLAIN: &str = r#"{type: object, required: [code, message], properties: {code: {type: string}, message: {type: string}}}"#;

fn not_found_body(bundle: &Bundle) -> Value {
    let op = bundle.operation("putThing").unwrap();
    let out = offline().not_found(bundle, op, &OperationCall::default(), "no t1");
    assert_eq!(out.status, 404);
    out.body.unwrap()
}

fn conflict_body(bundle: &Bundle) -> Value {
    let op = bundle.operation("putThing").unwrap();
    let out = offline()
        .refuse_from_state(bundle, op, &OperationCall::default(), 409)
        .unwrap();
    assert_eq!((out.status, out.source), (409, Source::State));
    out.body.unwrap()
}

#[test]
fn a_const_and_a_single_enum_value_fill_their_members() {
    let bundle = contract(
        (
            r#"{type: object, required: [code, kind], properties: {code: {const: not_found}, kind: {type: string, enum: [missing]}}}"#,
            "Not found.",
        ),
        (PLAIN, "Conflict."),
    );
    let body = not_found_body(&bundle);
    assert_eq!(body["code"], "not_found", "{body}");
    assert_eq!(body["kind"], "missing", "{body}");
}

#[test]
fn an_example_fills_a_member_at_any_depth() {
    let bundle = contract(
        (
            r#"{type: object, required: [error], properties: {error: {type: object, required: [code, status], properties: {code: {type: string, examples: [thing_missing]}, status: {type: integer, examples: [404]}}}}}"#,
            "Not found.",
        ),
        (PLAIN, "Conflict."),
    );
    let body = not_found_body(&bundle);
    assert_eq!(body["error"]["code"], "thing_missing", "{body}");
    assert_eq!(body["error"]["status"], 404, "{body}");
}

#[test]
fn the_description_names_a_member_and_its_value_in_code_position() {
    let bundle = contract(
        (PLAIN, "Not found."),
        (
            r#"{type: object, required: [error], properties: {error: {type: object, required: [code, message], properties: {code: {type: string}, message: {type: string}}}}}"#,
            "The version is stale: `error.code` is `version_conflict`.",
        ),
    );
    let body = conflict_body(&bundle);
    assert_eq!(body["error"]["code"], "version_conflict", "{body}");
    assert_ne!(body["error"]["message"], "version_conflict", "{body}");

    let bundle = contract(
        (PLAIN, "Not found."),
        (
            PLAIN,
            "Answers <code>code</code> <code>stale</code> on a stale write.",
        ),
    );
    assert_eq!(conflict_body(&bundle)["code"], "stale");
}

#[test]
fn the_description_picks_the_value_it_names_out_of_an_enum() {
    let enumerated = r#"{type: object, required: [code], properties: {code: {type: string, enum: [already_exists, version_conflict, locked]}}}"#;
    let bundle = contract(
        (PLAIN, "Not found."),
        (
            enumerated,
            "Returned with `version_conflict` when the write is stale.",
        ),
    );
    for _ in 0..4 {
        assert_eq!(conflict_body(&bundle)["code"], "version_conflict");
    }
    // Two of its values named: neither is the one.
    let bundle = contract(
        (PLAIN, "Not found."),
        (enumerated, "Either `locked` or `already_exists`."),
    );
    let code = conflict_body(&bundle)["code"].clone();
    assert!(
        ["already_exists", "version_conflict", "locked"].contains(&code.as_str().unwrap()),
        "{code}"
    );
}

#[test]
fn a_member_nothing_names_keeps_the_fakers_value() {
    // A value named with no member before it, and none the member enumerates: the
    // description ties it to nothing.
    let bundle = contract(
        (PLAIN, "Not found."),
        (
            PLAIN,
            "Returned with `version_conflict` when the write is stale.",
        ),
    );
    let body = conflict_body(&bundle);
    assert!(body["code"].is_string(), "{body}");
    assert_ne!(body["code"], "version_conflict", "{body}");
    // Another status's description says nothing of this one.
    let bundle = contract(
        (PLAIN, "`code` is `version_conflict`."),
        (PLAIN, "Conflict."),
    );
    assert_ne!(conflict_body(&bundle)["code"], "version_conflict");
}

#[test]
fn a_value_that_breaks_the_schema_is_not_taken() {
    // The example breaks the member's own pattern: the body stays valid without it.
    let bundle = contract(
        (
            r#"{type: object, required: [code], properties: {code: {type: string, pattern: "^E[0-9]+$", examples: [nope]}}}"#,
            "`code` is `also_nope`.",
        ),
        (PLAIN, "Conflict."),
    );
    let code = not_found_body(&bundle)["code"].clone();
    let code = code.as_str().unwrap();
    assert!(code.starts_with('E') && code != "nope", "{code}");
}

#[test]
fn a_refusal_needs_its_status_declared_by_code() {
    let bundle = contract((PLAIN, "Not found."), (PLAIN, "Conflict."));
    let op = bundle.operation("putThing").unwrap();
    assert!(
        offline()
            .refuse_from_state(&bundle, op, &OperationCall::default(), 412)
            .is_none()
    );
}

/// A shared `Error` used by the `404` and the `409`: its example speaks for neither, the
/// `409`'s description names its code, and the HTTP status the engine reflects stays.
#[test]
fn a_shared_schemas_example_never_overrides_the_status_or_its_description() {
    let src = r##"
openapi: 3.1.0
info: {title: Things, version: "1"}
paths:
  /things/{id}:
    put:
      operationId: putThing
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}
        "404": {description: Not found., content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
        "409": {description: "Stale: `code` is `conflict`.", content: {application/json: {schema: {$ref: "#/components/schemas/Error"}}}}
components:
  schemas:
    Error:
      type: object
      required: [code, status, message]
      properties:
        code: {type: string, examples: [not_found]}
        status: {type: integer, examples: [400]}
        message: {type: string, examples: [Nothing here]}
"##;
    let bundle = compile(src.as_bytes()).unwrap();
    let body = conflict_body(&bundle);
    assert_eq!(body["code"], "conflict", "{body}");
    assert_eq!(body["status"], 409, "{body}");
    assert_ne!(body["message"], "Nothing here", "{body}");
    let body = not_found_body(&bundle);
    assert_ne!(body["code"], "not_found", "a shared example: {body}");
    assert_eq!(body["status"], 404, "{body}");
}

/// A member name in code position is no value, and two values named for one member are
/// no guess.
#[test]
fn member_names_and_several_values_are_no_code() {
    let bundle = contract(
        (PLAIN, "Not found."),
        (
            PLAIN,
            "The `code` and `message` members say what went wrong.",
        ),
    );
    assert_ne!(conflict_body(&bundle)["code"], "message");
    let bundle = contract(
        (PLAIN, "Not found."),
        (PLAIN, "`code` is `stale_version`, or `code` is `locked`."),
    );
    let code = conflict_body(&bundle)["code"].clone();
    assert!(code != "stale_version" && code != "locked", "{code}");
}

/// A singular `example`, where a schema keeps one, counts as `examples` do.
#[test]
fn a_singular_example_counts() {
    let mut bundle = contract((PLAIN, "Not found."), (PLAIN, "Conflict."));
    let op = bundle
        .operations
        .iter_mut()
        .find(|op| op.id == "putThing")
        .unwrap();
    let variant = op
        .outputs
        .iter_mut()
        .find(|o| o.status == mb_mcm::StatusSelector::Code(409))
        .unwrap();
    let schema = variant.schema.as_mut().unwrap();
    schema["properties"]["code"]["example"] = Value::from("held_elsewhere");
    assert_eq!(conflict_body(&bundle)["code"], "held_elsewhere");
}

/// Every value after the member's name to the end of its sentence counts: two are no
/// guess, and a value in the next sentence is not the member's.
#[test]
fn values_after_a_member_count_to_the_end_of_its_sentence() {
    let bundle = contract(
        (PLAIN, "Not found."),
        (PLAIN, "`code` is `version_conflict` or `edit_conflict`."),
    );
    let code = conflict_body(&bundle)["code"].clone();
    assert!(
        code != "version_conflict" && code != "edit_conflict",
        "{code}"
    );
    let bundle = contract(
        (PLAIN, "Not found."),
        (
            PLAIN,
            "Its `code` is `version_conflict`. Retry with `If-Match`.",
        ),
    );
    assert_eq!(conflict_body(&bundle)["code"], "version_conflict");
}

/// The member named as a plain word of the prose counts, and a member typed as a number
/// takes a plain number after it, never the status itself.
#[test]
fn a_member_named_in_prose_takes_the_value_after_it() {
    let bundle = contract(
        (PLAIN, "Not found."),
        (PLAIN, "Refused with the code `name_taken`."),
    );
    assert_eq!(conflict_body(&bundle)["code"], "name_taken");
    let numbered = r#"{type: object, required: [code, message], properties: {code: {type: integer}, message: {type: string}}}"#;
    let bundle = contract(
        (PLAIN, "Not found."),
        (
            numbered,
            "Code 1001 when another thing has the name; 409 otherwise.",
        ),
    );
    assert_eq!(conflict_body(&bundle)["code"], 1001);
    let bundle = contract((PLAIN, "Not found."), (numbered, "The code is 409."));
    assert_ne!(conflict_body(&bundle)["code"], 409, "the status is no code");
    // A string member takes no plain word, and two numbers are no guess.
    let bundle = contract((PLAIN, "Not found."), (PLAIN, "The code says stale."));
    assert_ne!(conflict_body(&bundle)["code"], "says");
    let bundle = contract((PLAIN, "Not found."), (numbered, "Code 7 or code 8."));
    let code = conflict_body(&bundle)["code"].clone();
    assert!(code != 7 && code != 8, "{code}");
}

/// A create of things whose `sku` is unique, its `400` and `409` sharing one `Error`;
/// `info` is the API-level description.
fn shop(info: &str) -> Bundle {
    let src = format!(
        r##"
openapi: 3.1.0
info:
  title: Shop
  version: "1"
  description: "{info}"
paths:
  /things:
    post:
      operationId: createThing
      requestBody: {{content: {{application/json: {{schema: {{type: object, properties: {{sku: {{type: string}}, name: {{type: string}}}}}}}}}}}}
      responses:
        "201": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{id: {{type: string}}}}}}}}}}}}
        "400": {{description: "`bad_param` or `taken`.", content: {{application/json: {{schema: {{$ref: "#/components/schemas/Error"}}}}}}}}
        "409": {{description: Conflict., content: {{application/json: {{schema: {{$ref: "#/components/schemas/Error"}}}}}}}}
components:
  schemas:
    Error:
      type: object
      required: [code, message]
      properties: {{code: {{type: string}}, message: {{type: string}}}}
"##
    );
    compile(src.as_bytes()).unwrap()
}

fn repeat_body(bundle: &Bundle, status: u16, member: &str) -> Value {
    let op = bundle.operation("createThing").unwrap();
    let answered = mb_engine::GeneratedResponse {
        status: 201,
        content_type: None,
        body: None,
        source: Source::Llm,
        usage: Default::default(),
        fallback_reason: None,
        effects: Vec::new(),
        effect_outcomes: Vec::new(),
        dropped_effects: 0,
        new_record: false,
        patched: None,
    };
    let out = offline()
        .refuse_repeated_value(
            bundle,
            op,
            &OperationCall::default(),
            (status, member),
            &answered,
        )
        .unwrap();
    assert_eq!(out.status, status);
    out.body.unwrap()
}

/// The API-level description gives a repeat's refusal its code when one sentence holds
/// the status, names the repeated member, and names the code after the error's member.
#[test]
fn the_api_description_names_a_repeats_code() {
    let info = "A thing's `sku` is unique: a create with an SKU another has is refused with 400 and the code `sku_taken`. An unknown id answers 404 with the code `missing`.";
    let bundle = shop(info);
    for _ in 0..4 {
        assert_eq!(repeat_body(&bundle, 400, "sku")["code"], "sku_taken");
    }
    // Another status's refusal, another member's repeat, or no repeat at all: nothing.
    assert_ne!(repeat_body(&bundle, 409, "sku")["code"], "sku_taken");
    assert_ne!(repeat_body(&bundle, 400, "name")["code"], "sku_taken");
    let op = bundle.operation("createThing").unwrap();
    let body = offline()
        .refuse_from_state(&bundle, op, &OperationCall::default(), 400)
        .unwrap()
        .body
        .unwrap();
    assert_ne!(body["code"], "sku_taken", "{body}");
    // The status named in another sentence than the member's ties nothing.
    let bundle =
        shop("A thing's `sku` is unique, and the code is `sku_taken`. Refusals answer 400.");
    assert_ne!(repeat_body(&bundle, 400, "sku")["code"], "sku_taken");
    // Nor does a sentence that names no type the create makes.
    let bundle = shop(
        "A gizmo's `sku` is unique: one with a taken SKU is refused with 400 and the code `sku_taken`.",
    );
    assert_ne!(repeat_body(&bundle, 400, "sku")["code"], "sku_taken");
}

/// What ties a repeat to a refusal strongly enough to enforce it: a 409 whose own
/// description names the member, or another status whose own description names it in
/// code position and names no other member of the request and no other status. No
/// sentence of the API description ties one, whatever it says.
#[test]
fn a_refusal_is_tied_to_a_repeat_by_its_own_description_only() {
    use mb_engine::guards::refusal_tied_to;
    let tied = |bundle: &Bundle, member: &str| {
        refusal_tied_to(bundle, bundle.operation("createThing").unwrap(), member)
    };
    let set = |bundle: &mut Bundle, code: u16, description: &str| {
        let op = bundle
            .operations
            .iter_mut()
            .find(|op| op.id == "createThing")
            .unwrap();
        let variant = op
            .outputs
            .iter_mut()
            .find(|o| o.status == mb_mcm::StatusSelector::Code(code))
            .unwrap();
        variant.description = Some(description.into());
    };
    let mut bundle = shop("A thing's `sku` is unique: a taken one is refused with 409.");
    assert_eq!(tied(&bundle, "sku"), None, "an API sentence ties nothing");
    set(&mut bundle, 409, "A thing with this SKU exists.");
    assert_eq!(tied(&bundle, "sku"), Some(409));
    assert_eq!(tied(&bundle, "name"), None);
    // Without the 409, the 400 is offered: in code position, alone, it ties.
    let op = bundle
        .operations
        .iter_mut()
        .find(|op| op.id == "createThing")
        .unwrap();
    op.outputs
        .retain(|o| o.status != mb_mcm::StatusSelector::Code(409));
    for (description, expected) in [
        ("Another thing has this `sku`.", Some(400)),
        ("The SKU is too long.", None),
        ("The `sku` holds at most 300 characters.", None),
        (
            "Another thing has this `sku`, or the `name` is empty.",
            None,
        ),
        ("Another thing has this `sku`; 422 for anything else.", None),
    ] {
        set(&mut bundle, 400, description);
        assert_eq!(tied(&bundle, "sku"), expected, "{description}");
    }
}

/// A sentence of the API description naming another status too, or another member of
/// the request, gives a repeat's refusal no code.
#[test]
fn an_api_sentence_of_two_statuses_or_members_names_no_code() {
    for info in [
        "A thing with a taken `sku` is refused with 400 and the code `sku_taken`, a missing one with 404.",
        "A thing with a taken `sku` or `name` is refused with 400 and the code `sku_taken`.",
    ] {
        let bundle = shop(info);
        assert_ne!(
            repeat_body(&bundle, 400, "sku")["code"],
            "sku_taken",
            "{info}"
        );
    }
}

/// A member whose own description lists codes takes the one the status's description
/// names; naming two, or none of them, keeps the faker's value.
#[test]
fn a_member_s_own_description_lists_the_codes_the_status_picks_from() {
    let listed = r#"{type: object, required: [code, version], properties: {code: {type: string, description: "Such as `ConcurrentModification`, `ResourceNotFound` or `InvalidInput`."}, version: {type: integer, description: "With `ConcurrentModification`: the current version."}}}"#;
    let bundle = contract(
        (listed, "No resource has the id (`ResourceNotFound`)."),
        (
            listed,
            "The version sent is not the current one (`ConcurrentModification`); nothing changed.",
        ),
    );
    for _ in 0..4 {
        assert_eq!(conflict_body(&bundle)["code"], "ConcurrentModification");
        assert_eq!(not_found_body(&bundle)["code"], "ResourceNotFound");
    }
    assert!(conflict_body(&bundle)["version"].is_i64());
    let bundle = contract(
        (listed, "Either `ResourceNotFound` or `InvalidInput`."),
        (listed, "Conflict."),
    );
    let code = not_found_body(&bundle)["code"].clone();
    assert!(
        code != "ResourceNotFound" && code != "InvalidInput",
        "{code}"
    );
}

/// Where the prose names the repeated request member, it speaks of the request: a
/// member of the error body of that name takes no value from it.
#[test]
fn the_repeated_member_named_in_prose_is_the_requests() {
    let src = r##"
openapi: 3.1.0
info: {title: Shop, version: "1", description: "A thing's `name` is unique: a taken name is refused with 409 `name_in_use`."}
paths:
  /things:
    post:
      operationId: createThing
      requestBody: {content: {application/json: {schema: {type: object, properties: {name: {type: string}}}}}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}
        "409": {description: A thing with this name exists (code `name_in_use`)., content: {application/json: {schema: {type: object, required: [code, conflict], properties: {code: {type: string}, conflict: {type: object, required: [name], properties: {name: {type: string}}}}}}}}
"##;
    let bundle = compile(src.as_bytes()).unwrap();
    let body = repeat_body(&bundle, 409, "name");
    assert_eq!(body["code"], "name_in_use", "{body}");
    assert_ne!(body["conflict"]["name"], "name_in_use", "{body}");
}
