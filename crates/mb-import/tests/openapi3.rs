#![allow(clippy::unwrap_used)]

use mb_import::{ImportError, compile};
use mb_mcm::{Bundle, Effect, Identity, OperationKind, StatusSelector};
use serde_json::json;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../fixtures/openapi/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(path).unwrap()
}

fn orders30() -> Bundle {
    compile(&fixture("orders-3.0.yaml")).unwrap()
}

#[test]
fn compiles_orders_30() {
    let b = orders30();
    assert_eq!(b.operations.len(), 5);
    let get = b.operation("getOrder").unwrap();
    assert_eq!(get.kind, OperationKind::Query);
    assert_eq!(get.resource_type.as_deref(), Some("orders"));
    assert_eq!(
        get.identity,
        Some(Identity {
            path_param: Some("orderId".into()),
            field: "id".into(),
            input_field: None,
            path_field: None,
        })
    );
    let create = b.operation("createOrder").unwrap();
    assert_eq!(create.kind, OperationKind::Command(Effect::Create));
    assert_eq!(create.identity.as_ref().unwrap().path_param, None);
    let del = b.operation("deleteOrder").unwrap();
    let out = del.success_output().unwrap();
    assert_eq!(out.status, StatusSelector::Code(204));
    assert_eq!(out.schema, None);
}

#[test]
fn normalizes_nullable_30() {
    let b = orders30();
    assert_eq!(
        b.defs["Order"]["properties"]["note"]["type"],
        json!(["string", "null"])
    );
}

#[test]
fn compiles_orders_31_equivalently() {
    let a = orders30();
    let c = compile(&fixture("orders-3.1.yaml")).unwrap();
    assert_eq!(a.operations, c.operations);
    assert_eq!(a.defs, c.defs);
}

#[test]
fn compiles_recursive_schema() {
    let b = orders30();
    assert_eq!(
        b.defs["Category"]["properties"]["parent"],
        json!({"$ref": "#/$defs/Category"})
    );
}

#[test]
fn rejects_dangling_ref() {
    let doc = r##"
openapi: 3.0.3
info: {title: T, version: "1"}
paths:
  /x:
    get:
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Missing'
"##;
    match compile(doc.as_bytes()) {
        Err(ImportError::Invalid { pointer, .. }) => assert!(pointer.contains("Missing")),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn rejects_unknown_format() {
    let r = compile(br#"{"asyncapi":"4.0.0"}"#);
    assert!(matches!(r, Err(ImportError::Unsupported(_))));
}

#[test]
fn orders_30_snapshot() {
    let b = orders30();
    insta::assert_yaml_snapshot!(b, {".source_digest" => "[digest]"});
}

fn oas(paths: &str) -> String {
    format!("openapi: 3.0.3\ninfo: {{title: T, version: '1'}}\npaths:\n{paths}")
}

#[test]
fn rejects_duplicate_operation_ids() {
    let src = oas(r#"
  /a:
    get: {operationId: dup, responses: {"200": {description: OK}}}
    post: {operationId: dup, responses: {"200": {description: OK}}}
"#);
    match compile(src.as_bytes()) {
        Err(ImportError::Invalid { pointer, message }) => {
            assert_eq!(pointer, "/paths/~1a/post/operationId");
            assert!(message.contains("dup"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn disambiguates_generated_operation_ids() {
    let src = oas(r#"
  /a-b:
    get: {responses: {"200": {description: OK}}}
  /a_b:
    get: {responses: {"200": {description: OK}}}
  /x:
    get: {responses: {"200": {description: OK}}}
  /y:
    get: {operationId: get_x, responses: {"200": {description: OK}}}
"#);
    let b = compile(src.as_bytes()).unwrap();
    let mut ids: Vec<&str> = b.operations.iter().map(|o| o.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["get_a_b", "get_a_b_2", "get_x", "get_x_2"]);
    // The explicit operationId keeps its name.
    let y = b.operation("get_x").unwrap();
    assert!(matches!(&y.binding, mb_mcm::Binding::Http { path, .. } if path == "/y"));
}

#[test]
fn imports_request_body_required() {
    let b = orders30();
    assert!(b.operation("createOrder").unwrap().input.body_required);
    assert!(!b.operation("getOrder").unwrap().input.body_required);
    let src = oas(r#"
  /a:
    post:
      requestBody:
        content: {application/json: {schema: {type: object}}}
      responses: {"200": {description: OK}}
"#);
    let b = compile(src.as_bytes()).unwrap();
    let op = &b.operations[0];
    assert!(op.input.body.is_some());
    assert!(!op.input.body_required);
}

#[test]
fn parses_unquoted_integer_status_keys() {
    let text = String::from_utf8(fixture("orders-3.0.yaml")).unwrap();
    let unquoted = unquote_status_keys(&text);
    assert_ne!(unquoted, text);
    let b = compile(unquoted.as_bytes()).unwrap();
    assert_eq!(b.operations.len(), 5);
}

fn unquote_status_keys(text: &str) -> String {
    let mut out = text.to_string();
    for code in ["200", "201", "204", "400", "404", "409", "422", "500"] {
        out = out.replace(&format!("\"{code}\":"), &format!("{code}:"));
    }
    out
}

#[test]
fn malformed_yaml_is_parse_error() {
    let r = compile(b"openapi: 3.0.0\npaths: [unclosed\n  - : {{{\n\t bad: *missing_anchor\n");
    assert!(matches!(r, Err(ImportError::Parse(_))), "got {r:?}");
}

#[test]
fn openapi_event_stream_compiles_to_stream_server() {
    let b = compile(&fixture("events-3.1.yaml")).unwrap();
    let stream = b.operation("streamTicks").unwrap();
    assert_eq!(stream.kind, OperationKind::StreamServer);
    let out = stream.success_output().unwrap();
    assert_eq!(out.content_type.as_deref(), Some("text/event-stream"));
    assert_eq!(
        stream.stream_item_schema(),
        Some(&json!({"$ref": "#/$defs/Tick", "x-mockingbird-sse-event": "tick"}))
    );
    assert_eq!(stream.resource_type.as_deref(), Some("ticks"));
    // The error variant stays a plain JSON response.
    let err = stream.output_for_status(503).unwrap();
    assert_eq!(err.content_type.as_deref(), Some("application/json"));
    assert_eq!(err.schema, Some(json!({"$ref": "#/$defs/Error"})));

    let create = b.operation("createTick").unwrap();
    assert_eq!(create.kind, OperationKind::Command(Effect::Create));
    assert_eq!(create.resource_type.as_deref(), Some("ticks"));
    assert_eq!(
        create.success_output().unwrap().schema,
        Some(json!({"$ref": "#/$defs/Tick"}))
    );
}

#[test]
fn event_stream_item_schema_extension_and_plain_schema() {
    let src = oas(r#"
  /a:
    get:
      responses:
        "200":
          description: OK
          content:
            text/event-stream:
              schema: {type: string}
              x-mockingbird-item-schema:
                type: object
                properties: {n: {type: integer}}
  /b:
    get:
      responses:
        "200":
          description: OK
          content:
            text/event-stream; charset=utf-8:
              schema: {type: object, properties: {m: {type: string}}}
"#);
    let b = compile(src.as_bytes()).unwrap();
    let a = b.operation("get_a").unwrap();
    assert_eq!(a.kind, OperationKind::StreamServer);
    assert_eq!(
        a.stream_item_schema(),
        Some(&json!({"type": "object", "properties": {"n": {"type": "integer"}}}))
    );
    let bb = b.operation("get_b").unwrap();
    assert_eq!(bb.kind, OperationKind::StreamServer);
    assert_eq!(
        bb.success_output().unwrap().content_type.as_deref(),
        Some("text/event-stream")
    );
    assert_eq!(
        bb.stream_item_schema(),
        Some(&json!({"type": "object", "properties": {"m": {"type": "string"}}}))
    );
}

/// Vendor extensions are annotations: `$ref`s inside them (Stripe's
/// `x-expansionResources`, GitHub's `x-github-breaking-changes`) are neither
/// rewritten nor checked, while a property merely named `x-*` still is.
#[test]
fn refs_inside_vendor_extensions_are_ignored() {
    let doc = json!({
        "openapi": "3.0.0",
        "info": {"title": "T", "version": "1"},
        "paths": {},
        "components": {"schemas": {
            "file": {"type": "object", "properties": {"id": {"type": "string"}}},
            "simple-repository": {"type": "object"},
            "account_branding_settings": {
                "type": "object",
                "properties": {"icon": {
                    "anyOf": [{"type": "string"}, {"$ref": "#/components/schemas/file"}],
                    "x-expansionResources": {"oneOf": [{"$ref": "#/components/schemas/file"}]}
                }}
            },
            "migration": {
                "type": "object",
                "properties": {"x-header": {"$ref": "#/components/schemas/file"}},
                "x-github-breaking-changes": [{"patch": [{"op": "add", "value": {
                    "type": "array", "items": {"$ref": "#/components/schemas/simple-repository"}
                }}]}]
            }
        }}
    });
    let b = compile(doc.to_string().as_bytes()).unwrap();
    let icon = &b.defs["account_branding_settings"]["properties"]["icon"];
    assert_eq!(icon["anyOf"][1], json!({"$ref": "#/$defs/file"}));
    assert_eq!(
        b.defs["migration"]["properties"]["x-header"],
        json!({"$ref": "#/$defs/file"})
    );

    // A dangling ref under a property named `x-*` is still an error.
    let mut bad = doc.clone();
    bad["components"]["schemas"]["migration"]["properties"]["x-header"]["$ref"] =
        json!("#/components/schemas/gone");
    match compile(bad.to_string().as_bytes()) {
        Err(ImportError::Invalid { pointer, message }) => {
            assert!(pointer.ends_with("/gone"), "{pointer}");
            assert!(message.contains("schema migration"), "{message}");
            assert!(
                message.contains("/components/schemas/migration/properties/x-header"),
                "{message}"
            );
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

/// A dangling ref is reported with its referrer and the JSON pointer of the
/// `$ref` inside it.
#[test]
fn dangling_ref_names_referrer_and_location() {
    let src = oas(r#"
  /a:
    post:
      operationId: makeA
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                tags: {type: array, items: {$ref: '#/components/schemas/Tag'}}
      responses: {"200": {description: OK}}
"#);
    match compile(src.as_bytes()) {
        Err(ImportError::Invalid { pointer, message }) => {
            assert!(pointer.ends_with("/Tag"), "{pointer}");
            assert!(message.contains("operation makeA"), "{message}");
            assert!(message.contains("/properties/tags/items"), "{message}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn form_and_multipart_bodies_are_compiled() {
    let src = oas(r#"
  /form:
    post:
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema: {type: object, properties: {a: {type: integer}}}
      responses: {"200": {description: OK}}
  /multi:
    post:
      requestBody:
        content:
          multipart/form-data:
            schema: {type: object, properties: {f: {type: string, format: binary}}}
      responses: {"200": {description: OK}}
  /both:
    post:
      requestBody:
        content:
          application/x-www-form-urlencoded: {schema: {type: object}}
          application/json: {schema: {type: object, properties: {j: {type: string}}}}
      responses: {"200": {description: OK}}
"#);
    let b = compile(src.as_bytes()).unwrap();
    let body = |id: &str| b.operation(id).unwrap().input.body.clone().unwrap();
    assert_eq!(
        body("post_form"),
        json!({"type": "object", "properties": {"a": {"type": "integer"}},
               "x-content-type": "application/x-www-form-urlencoded"})
    );
    assert_eq!(body("post_multi")["x-content-type"], "multipart/form-data");
    // JSON stays preferred, untagged.
    assert_eq!(body("post_both").get("x-content-type"), None);
}

#[test]
fn compile_with_warnings_flags_schemas_over_the_prompt_budget() {
    let bytes = fixture("huge-response.json");
    let (bundle, warnings) = mb_import::compile_with_warnings(&bytes).unwrap();
    assert_eq!(bundle.operations.len(), 2);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let w = &warnings[0];
    assert_eq!(w.code, "schema_too_large");
    assert_eq!(w.operation.as_deref(), Some("getBig"));
    assert!(w.size.unwrap() > mb_import::PROMPT_SCHEMA_BUDGET as u64);
    assert!(
        w.message.contains("getBig") && w.message.contains("trimmed"),
        "{}",
        w.message
    );
    assert_eq!(mb_import::compile(&bytes).unwrap(), bundle);
    let small = fixture("orders-3.0.yaml");
    assert!(
        mb_import::compile_with_warnings(&small)
            .unwrap()
            .1
            .is_empty()
    );
}

#[test]
fn compiles_31_webhooks_and_callbacks() {
    let b = compile(&fixture("webhooks-3.1.yaml")).unwrap();
    assert_eq!(b.webhooks.len(), 2);
    let hook = b.webhooks.iter().find(|w| w.name == "pet.created").unwrap();
    assert_eq!(hook.operation, None);
    assert_eq!(hook.summary.as_deref(), Some("A pet was created"));
    assert_eq!(hook.payload_schema, json!({"$ref": "#/$defs/PetEvent"}));
    let cb = b
        .webhooks
        .iter()
        .find(|w| w.name == "createPet.onAdopted")
        .unwrap();
    assert_eq!(cb.operation.as_deref(), Some("createPet"));
    assert_eq!(
        cb.payload_schema["properties"]["adopted"]["const"],
        json!(true)
    );
    // Callback first, then name match.
    assert_eq!(
        b.webhook_for("createPet", "pet.created").unwrap().name,
        "createPet.onAdopted"
    );
    assert_eq!(
        b.webhook_for("other", "PET.CREATED").unwrap().name,
        "pet.created"
    );
    assert!(b.webhook_for("other", "pet.deleted").is_none());
}

#[test]
fn contracts_without_webhooks_serialise_unchanged() {
    let b = orders30();
    assert!(b.webhooks.is_empty());
    assert!(serde_json::to_value(&b).unwrap().get("webhooks").is_none());
}

#[test]
fn webhook_payload_refs_are_checked() {
    let src = std::str::from_utf8(&fixture("webhooks-3.1.yaml"))
        .unwrap()
        .replace(
            "$ref: \"#/components/schemas/PetEvent\"",
            "$ref: \"#/components/schemas/Missing\"",
        );
    assert!(matches!(
        compile(src.as_bytes()),
        Err(ImportError::Invalid { .. })
    ));
}

#[test]
fn response_headers_are_declared() {
    let b = compile(
        br##"
openapi: 3.0.3
info: {title: t, version: "1"}
components:
  headers:
    Remaining: {schema: {type: integer}, description: requests left}
paths:
  /things:
    get:
      operationId: listThings
      responses:
        "200":
          description: ok
          headers:
            X-RateLimit-Remaining: {$ref: "#/components/headers/Remaining"}
          content: {application/json: {schema: {type: array, items: {type: string}}}}
        "429":
          description: slow down
          headers:
            Retry-After: {schema: {type: integer}}
"##,
    )
    .unwrap();
    let op = b.operation("listThings").unwrap();
    let ok = op.output_for_status(200).unwrap();
    assert_eq!(
        ok.headers.get("X-RateLimit-Remaining"),
        Some(&json!({"type": "integer"}))
    );
    let limited = op.output_for_status(429).unwrap();
    assert_eq!(
        limited.headers.get("Retry-After"),
        Some(&json!({"type": "integer"}))
    );
}

#[test]
fn path_ids_name_their_response_field() {
    let b = compile(
        br##"
openapi: 3.0.3
info: {title: t, version: "1"}
components:
  schemas:
    Issue:
      type: object
      properties: {id: {type: integer}, number: {type: integer}, key: {type: string}}
    Message:
      type: object
      properties: {sid: {type: string}, body: {type: string}}
paths:
  /repos/{owner}/{repo}/issues/{issue_number}:
    get:
      operationId: getIssue
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: issue_number, in: path, required: true, schema: {type: integer}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
  /rest/issue/{issueIdOrKey}:
    get:
      operationId: getJiraIssue
      parameters:
        - {name: issueIdOrKey, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
  /Accounts/{AccountSid}/Messages.json:
    post:
      operationId: CreateMessage
      parameters:
        - {name: AccountSid, in: path, required: true, schema: {type: string}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Message"}}}}
    get:
      operationId: ListMessage
      parameters:
        - {name: AccountSid, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {messages: {type: array, items: {$ref: "#/components/schemas/Message"}}}}}}}
  /Accounts/{AccountSid}/Messages/{Sid}.json:
    get:
      operationId: FetchMessage
      parameters:
        - {name: AccountSid, in: path, required: true, schema: {type: string}}
        - {name: Sid, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Message"}}}}
"##,
    )
    .unwrap();
    let issue = b.operation("getIssue").unwrap();
    assert_eq!(
        issue.identity,
        Some(Identity {
            path_param: Some("issue_number".into()),
            field: "id".into(),
            input_field: None,
            path_field: Some("number".into()),
        })
    );
    let jira = b
        .operation("getJiraIssue")
        .unwrap()
        .identity
        .clone()
        .unwrap();
    assert_eq!(jira.path_field.as_deref(), Some("key"));

    for id in ["CreateMessage", "ListMessage", "FetchMessage"] {
        let op = b.operation(id).unwrap();
        assert_eq!(op.resource_type.as_deref(), Some("Messages"), "{id}");
    }
    let create = b
        .operation("CreateMessage")
        .unwrap()
        .identity
        .clone()
        .unwrap();
    assert_eq!((create.path_param, create.field.as_str()), (None, "sid"));
    let fetch = b.operation("FetchMessage").unwrap();
    assert_eq!(fetch.kind, OperationKind::Query);
    assert_eq!(
        fetch.identity,
        Some(Identity {
            path_param: Some("Sid".into()),
            field: "sid".into(),
            input_field: None,
            path_field: None,
        })
    );
}
