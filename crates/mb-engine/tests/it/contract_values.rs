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
    assert!(code != "version_conflict" && code != "edit_conflict", "{code}");
    let bundle = contract(
        (PLAIN, "Not found."),
        (PLAIN, "Its `code` is `version_conflict`. Retry with `If-Match`."),
    );
    assert_eq!(conflict_body(&bundle)["code"], "version_conflict");
}
