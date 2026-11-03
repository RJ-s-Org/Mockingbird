#![cfg(test)]
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
            record: None,
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
fn stubs_dangling_ref() {
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
    let (b, warnings) = mb_import::compile_with_warnings(doc.as_bytes()).unwrap();
    assert_eq!(b.defs["Missing"], json!(true));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].code, "dangling_ref");
    assert!(warnings[0].message.contains("Missing"));
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

    // A dangling ref under a property named `x-*` is still checked.
    let mut bad = doc.clone();
    bad["components"]["schemas"]["migration"]["properties"]["x-header"]["$ref"] =
        json!("#/components/schemas/gone");
    let (b, warnings) = mb_import::compile_with_warnings(bad.to_string().as_bytes()).unwrap();
    assert_eq!(b.defs["gone"], json!(true));
    let message = &warnings[0].message;
    assert_eq!(warnings[0].code, "dangling_ref");
    assert!(message.contains("#/components/schemas/gone"), "{message}");
    assert!(message.contains("schema migration"), "{message}");
    assert!(
        message.contains("/components/schemas/migration/properties/x-header"),
        "{message}"
    );
}

/// A dangling ref is reported with its referrer and the JSON pointer of the
/// `$ref` inside it; a ref that is not a component name is still an error.
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
    let (_, warnings) = mb_import::compile_with_warnings(src.as_bytes()).unwrap();
    let message = &warnings[0].message;
    assert!(message.contains("#/components/schemas/Tag"), "{message}");
    assert!(message.contains("operation makeA"), "{message}");
    assert!(message.contains("/properties/tags/items"), "{message}");

    let src = src.replace("schemas/Tag", "schemas/Tag/properties/name");
    match compile(src.as_bytes()) {
        Err(ImportError::Invalid { pointer, message }) => {
            assert!(pointer.ends_with("/Tag/properties/name"), "{pointer}");
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
    let (b, warnings) = mb_import::compile_with_warnings(src.as_bytes()).unwrap();
    assert_eq!(b.defs["Missing"], json!(true));
    assert!(
        warnings
            .iter()
            .any(|w| w.code == "dangling_ref" && w.message.contains("webhook")),
        "{warnings:?}"
    );
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
            record: None,
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
            record: None,
        })
    );
}

/// A minimal OpenAPI 3.0 document with `paths` and `components` spliced in.
fn doc30(paths: serde_json::Value, components: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "openapi": "3.0.3",
        "info": {"title": "t", "version": "1"},
        "paths": paths,
        "components": components,
    }))
    .unwrap()
}

fn codes(op: &mb_mcm::Operation) -> Vec<String> {
    op.outputs
        .iter()
        .map(|o| match o.status {
            StatusSelector::Code(c) => c.to_string(),
            StatusSelector::Default => "default".into(),
        })
        .collect()
}

#[test]
fn status_ranges_expand_to_the_codes_the_operation_does_not_declare() {
    let bytes = doc30(
        json!({
            "/a": {"get": {"operationId": "a", "responses": {
                "4XX": {"description": "client", "content": {"application/json": {"schema": {"type": "object", "properties": {"range": {"type": "string"}}}}}},
                "200": {"description": "ok"},
                "404": {"description": "missing", "content": {"application/json": {"schema": {"type": "object", "properties": {"explicit": {"type": "string"}}}}}},
                "5xx": {"description": "server"},
                "1XX": {"description": "info"}
            }}},
            "/b": {"get": {"operationId": "b", "responses": {"2XX": {"description": "ok", "content": {"application/json": {"schema": {"type": "string"}}}}}}},
            "/c": {"post": {"operationId": "c", "responses": {
                "2XX": {"description": "ok"},
                "201": {"description": "created", "content": {"application/json": {"schema": {"type": "integer"}}}}
            }}}
        }),
        json!({}),
    );
    let (b, warnings) = mb_import::compile_with_warnings(&bytes).unwrap();
    let a = b.operation("a").unwrap();
    let a_codes = codes(a);
    for code in [
        "200", "400", "401", "403", "404", "409", "422", "429", "500", "502", "503",
    ] {
        assert!(a_codes.contains(&code.to_string()), "{code} in {a_codes:?}");
    }
    assert!(!a_codes.iter().any(|c| c.starts_with('1')), "{a_codes:?}");
    // The explicit 404 wins over the range; 400 comes from the range.
    let explicit = a.output_for_status(404).unwrap().schema.as_ref().unwrap();
    assert!(explicit["properties"].get("explicit").is_some());
    let ranged = a.output_for_status(400).unwrap().schema.as_ref().unwrap();
    assert!(ranged["properties"].get("range").is_some());
    assert_eq!(
        a.success_output().unwrap().status,
        StatusSelector::Code(200)
    );
    // A lone 2XX answers 200.
    let b_op = b.operation("b").unwrap();
    assert_eq!(
        b_op.success_output().unwrap().status,
        StatusSelector::Code(200)
    );
    assert_eq!(
        b_op.success_output().unwrap().schema,
        Some(json!({"type": "string"}))
    );
    // An explicit 2xx code keeps 2XX from adding 200.
    let c = b.operation("c").unwrap();
    assert_eq!(codes(c), ["201"]);
    let ranged: Vec<_> = warnings
        .iter()
        .filter(|w| w.code == "status_range_expanded")
        .collect();
    assert_eq!(ranged.len(), 3, "{warnings:?}");
    assert_eq!(ranged[0].operation.as_deref(), Some("a"));
    assert!(ranged[0].message.contains("4XX"), "{}", ranged[0].message);
    assert!(ranged[0].message.contains("1XX"), "{}", ranged[0].message);
    // `compile` gives the same bundle without the warnings.
    assert_eq!(compile(&bytes).unwrap(), b);
}

#[test]
fn a_dangling_component_ref_is_stubbed_with_a_warning() {
    let bytes = doc30(
        json!({"/a": {"get": {"operationId": "a", "responses": {"200": {"description": "ok",
            "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Missing"}}}}}}}}),
        json!({"schemas": {"Pet": {"type": "object", "properties": {"tag": {"$ref": "#/components/schemas/Tag"}}}}}),
    );
    let (b, warnings) = mb_import::compile_with_warnings(&bytes).unwrap();
    assert_eq!(b.defs["Missing"], json!(true));
    assert_eq!(b.defs["Tag"], json!(true));
    let dangling: Vec<_> = warnings
        .iter()
        .filter(|w| w.code == "dangling_ref")
        .collect();
    assert_eq!(dangling.len(), 2, "{warnings:?}");
    assert!(
        dangling
            .iter()
            .any(|w| w.message.contains("#/components/schemas/Missing"))
    );

    // A ref to another file is still refused.
    let bytes = doc30(
        json!({"/a": {"get": {"responses": {"200": {"description": "ok",
            "content": {"application/json": {"schema": {"$ref": "./pet.yaml#/Pet"}}}}}}}}),
        json!({}),
    );
    assert!(matches!(
        compile(&bytes).unwrap_err(),
        ImportError::Invalid { .. }
    ));
}

#[test]
fn openapi_30_x_webhooks_are_read() {
    let bytes = serde_json::to_vec(&json!({
        "openapi": "3.0.3",
        "info": {"title": "t", "version": "1"},
        "paths": {},
        "x-webhooks": {"push": {"post": {"summary": "a push", "requestBody": {"content": {
            "application/json": {"schema": {"$ref": "#/components/schemas/Push"}}}}}}},
        "components": {"schemas": {"Push": {"type": "object", "properties": {"ref": {"type": "string"}}}}}
    }))
    .unwrap();
    let b = compile(&bytes).unwrap();
    assert_eq!(b.webhooks.len(), 1);
    assert_eq!(b.webhooks[0].name, "push");
    assert_eq!(
        b.webhooks[0].payload_schema,
        json!({"$ref": "#/$defs/Push"})
    );
    assert_eq!(b.webhooks[0].summary.as_deref(), Some("a push"));
}

#[test]
fn cookie_params_and_unresolved_headers_warn() {
    let bytes = doc30(
        json!({"/a": {"get": {"operationId": "a",
            "parameters": [{"name": "session", "in": "cookie", "schema": {"type": "string"}}],
            "responses": {"200": {"description": "ok",
                "headers": {"X-Rate": {"$ref": "#/components/headers/Missing"}}}}}}}),
        json!({}),
    );
    let (_, warnings) = mb_import::compile_with_warnings(&bytes).unwrap();
    let codes: Vec<&str> = warnings.iter().map(|w| w.code.as_str()).collect();
    assert!(codes.contains(&"cookie_param_ignored"), "{codes:?}");
    assert!(codes.contains(&"response_header_unresolved"), "{codes:?}");
    let cookie = warnings
        .iter()
        .find(|w| w.code == "cookie_param_ignored")
        .unwrap();
    assert!(cookie.message.contains("session"), "{}", cookie.message);
    assert_eq!(cookie.operation.as_deref(), Some("a"));
}

/// A response that declares only non-JSON media keeps its media type and
/// a schema the data plane can answer from: text and XML keep (or get) a
/// schema, a declared example becomes its `examples`, and
/// `application/octet-stream` gets one only from a declared example.
#[test]
fn non_json_responses_keep_their_media_type_schema_and_example() {
    let doc = br##"
openapi: 3.0.3
info: {title: T, version: "1"}
paths:
  /csv:
    get:
      operationId: csv
      responses:
        "200":
          description: ok
          content:
            text/csv:
              schema: {type: string}
              example: "a,b\n1,2\n"
  /text:
    get:
      operationId: text
      responses:
        "200": {description: ok, content: {text/plain: {}}}
  /xml:
    get:
      operationId: xml
      responses:
        "200":
          description: ok
          content:
            application/xml:
              schema: {$ref: "#/components/schemas/Pet"}
  /bytes:
    get:
      operationId: bytes
      responses:
        "200":
          description: ok
          content:
            application/octet-stream:
              examples: {one: {value: "raw"}}
  /blob:
    get:
      operationId: blob
      responses:
        "200":
          description: ok
          content:
            application/octet-stream:
              schema: {type: string, format: binary}
components:
  schemas:
    Pet: {type: object, properties: {name: {type: string}}}
"##;
    let (b, warnings) = mb_import::compile_with_warnings(doc).unwrap();
    let out = |id: &str| b.operation(id).unwrap().success_output().unwrap().clone();

    let csv = out("csv");
    assert_eq!(csv.content_type.as_deref(), Some("text/csv"));
    assert_eq!(
        csv.schema,
        Some(json!({"type": "string", "examples": ["a,b\n1,2\n"]}))
    );
    let text = out("text");
    assert_eq!(text.content_type.as_deref(), Some("text/plain"));
    assert_eq!(text.schema, Some(json!({"type": "string"})));
    let xml = out("xml");
    assert_eq!(xml.content_type.as_deref(), Some("application/xml"));
    assert_eq!(xml.schema, Some(json!({"$ref": "#/$defs/Pet"})));
    let bytes = out("bytes");
    assert_eq!(
        bytes.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(
        bytes.schema,
        Some(json!({"type": "string", "examples": ["raw"]}))
    );
    let blob = out("blob");
    assert_eq!(
        blob.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(blob.schema, None);
    let empty: Vec<_> = warnings
        .iter()
        .filter(|w| w.code == "response_body_omitted")
        .collect();
    assert_eq!(empty.len(), 1, "{warnings:?}");
    assert_eq!(empty[0].operation.as_deref(), Some("blob"));
}

mod action_routes {
    use super::*;

    const ACTIONS: &str = r##"
openapi: 3.0.0
info: {title: Actions, version: "1"}
paths:
  /v1/payment_intents:
    post:
      operationId: createIntent
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}
  /v1/payment_intents/{intent}:
    get:
      operationId: getIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}
  /v1/payment_intents/{intent}/confirm:
    post:
      operationId: confirmIntent
      parameters: [{name: intent, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Intent"}}}}
  /bookings/{id}:
    get:
      operationId: getBooking
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Booking"}}}}
  /bookings/{bookingId}/cancel:
    post:
      operationId: cancelBooking
      parameters: [{name: bookingId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Cancellation"}}}}
  /captures/{capture_id}:
    get:
      operationId: getCapture
      parameters: [{name: capture_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Capture"}}}}
  /captures/{capture_id}/refund:
    post:
      operationId: refundCapture
      parameters: [{name: capture_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /repos/{number}/close:
    post:
      operationId: closeWithNoDetail
      parameters: [{name: number, in: path, required: true, schema: {type: string}}]
      responses:
        "204": {description: closed}
  /v1/customers/{id}/sources:
    post:
      operationId: addSource
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /repos/{o}/{r}/issues:
    post:
      operationId: createIssue
      parameters:
        - {name: o, in: path, required: true, schema: {type: string}}
        - {name: r, in: path, required: true, schema: {type: string}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /repos/{o}/{r}/merge:
    post:
      operationId: mergeUnderAParam
      parameters:
        - {name: o, in: path, required: true, schema: {type: string}}
        - {name: r, in: path, required: true, schema: {type: string}}
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /things/{id}/note:
    get:
      operationId: getNote
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
    post:
      operationId: postNote
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /things/{id}/item:
    post:
      operationId: postItem
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /things/{id}/item/{item}:
    delete:
      operationId: deleteItem
      parameters:
        - {name: id, in: path, required: true, schema: {type: string}}
        - {name: item, in: path, required: true, schema: {type: string}}
      responses:
        "204": {description: gone}
  /v1/databases/{database_id}/query:
    post:
      operationId: queryDatabase
      parameters: [{name: database_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}
  /chats/{id}/reply:
    post:
      operationId: streamReply
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {text/event-stream: {schema: {$ref: "#/components/schemas/Refund"}}}}
components:
  schemas:
    Intent:
      type: object
      required: [id, status]
      properties: {id: {type: string}, status: {type: string}}
    Booking:
      type: object
      required: [id, status]
      properties: {id: {type: string}, status: {type: string}}
    Cancellation:
      type: object
      required: [bookingId, status]
      properties: {bookingId: {type: string}, status: {type: string}}
    Capture:
      type: object
      required: [id]
      properties: {id: {type: string}, capture_id: {type: string}}
    Refund:
      type: object
      required: [id]
      properties: {id: {type: string}}
"##;

    fn actions() -> Bundle {
        compile(ACTIONS.as_bytes()).unwrap()
    }

    #[test]
    fn confirm_acts_on_its_payment_intent() {
        let b = actions();
        let op = b.operation("confirmIntent").unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Update));
        assert_eq!(op.resource_type.as_deref(), Some("payment_intents"));
        let identity = op.identity.as_ref().unwrap();
        assert_eq!(identity.path_param.as_deref(), Some("intent"));
        assert_eq!(identity.field, "id");
    }

    #[test]
    fn cancel_with_its_own_response_is_other() {
        let b = actions();
        let op = b.operation("cancelBooking").unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Other));
        assert_eq!(op.resource_type.as_deref(), Some("bookings"));
        let identity = op.identity.as_ref().unwrap();
        assert_eq!(identity.path_param.as_deref(), Some("bookingId"));
        // The answer names its booking.
        assert_eq!(identity.field, "bookingId");
    }

    /// `{id}` says nothing about the answer's own `id`, in any spelling: the
    /// identity is the read's.
    #[test]
    fn a_generic_id_param_does_not_name_the_target() {
        for param in ["id", "Id", "ID", "_id"] {
            let contract = format!(
                r##"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /orders/{{{param}}}:
    get:
      operationId: getOrder
      parameters: [{{name: {param}, in: path, required: true, schema: {{type: string}}}}]
      responses:
        "200":
          description: ok
          content: {{application/json: {{schema: {{type: object, required: [id], properties: {{id: {{type: string}}}}}}}}}}
  /orders/{{{param}}}/ship:
    post:
      operationId: shipOrder
      parameters: [{{name: {param}, in: path, required: true, schema: {{type: string}}}}]
      responses:
        "200":
          description: ok
          content: {{application/json: {{schema: {{type: object, properties: {{{param}: {{type: string}}, orderId: {{type: string}}}}}}}}}}
"##
            );
            let b = compile(contract.as_bytes()).unwrap();
            let op = b.operation("shipOrder").unwrap();
            assert_eq!(op.kind, OperationKind::Command(Effect::Other), "{param}");
            let identity = op.identity.as_ref().unwrap();
            assert_eq!(identity.path_param.as_deref(), Some(param), "{param}");
            assert_eq!(identity.field, "id", "{param}");
        }
    }

    #[test]
    fn an_action_without_a_read_or_a_body_still_names_its_target() {
        let b = actions();
        let op = b.operation("closeWithNoDetail").unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Other));
        assert_eq!(op.resource_type.as_deref(), Some("repos"));
        let identity = op.identity.as_ref().unwrap();
        assert_eq!(identity.path_param.as_deref(), Some("number"));
    }

    #[test]
    fn an_other_action_finds_its_target_as_the_read_does() {
        let b = actions();
        let read = b.operation("getCapture").unwrap().identity.clone().unwrap();
        let op = b.operation("refundCapture").unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Other));
        let identity = op.identity.as_ref().unwrap();
        assert_eq!(identity.path_param.as_deref(), Some("capture_id"));
        assert_eq!(identity.field, read.field);
        assert_eq!(identity.path_field, read.path_field);
        assert_eq!(identity.path_field.as_deref(), Some("capture_id"));
    }

    /// A resource composed with `allOf` has the members of its branches, so
    /// its read by path knows what the path names.
    #[test]
    fn an_all_of_resource_has_an_identity() {
        let contract = r##"
openapi: 3.0.0
info: {title: Composed, version: "1"}
paths:
  /offers/{offerId}:
    get:
      operationId: getOffer
      parameters: [{name: offerId, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Offer"}}}}
  /tickets/{ticket_number}:
    get:
      operationId: getTicket
      parameters: [{name: ticket_number, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}}
components:
  schemas:
    Base: {type: object, properties: {id: {type: string}}}
    Summary:
      allOf:
        - $ref: "#/components/schemas/Base"
        - {type: object, properties: {price: {type: string}}}
    Offer:
      allOf:
        - $ref: "#/components/schemas/Summary"
        - {type: object, properties: {taxes: {type: string}}}
    Ticket:
      type: object
      properties: {id: {type: string}}
      allOf:
        - {type: object, properties: {number: {type: string}}}
"##;
        let b = compile(contract.as_bytes()).unwrap();
        let identity = b.operation("getOffer").unwrap().identity.clone().unwrap();
        assert_eq!(identity.path_param.as_deref(), Some("offerId"));
        assert_eq!(identity.field, "id");
        let identity = b.operation("getTicket").unwrap().identity.clone().unwrap();
        assert_eq!(identity.field, "id");
        assert_eq!(identity.path_field.as_deref(), Some("number"));
    }

    #[test]
    fn is_action_tells_an_action_from_a_write_on_the_resource() {
        let b = actions();
        for id in ["confirmIntent", "cancelBooking", "refundCapture"] {
            assert!(b.operation(id).unwrap().is_action(), "{id}");
        }
        for id in ["createIntent", "getIntent", "createIssue"] {
            assert!(!b.operation(id).unwrap().is_action(), "{id}");
        }
    }

    /// Nothing reads what `POST /v1/customers/{id}/sources` answers, and a
    /// `200` does not say it created anything: its spelling does not count.
    #[test]
    fn a_plural_tail_nothing_reads_acts_on_its_parent() {
        let b = actions();
        let op = b.operation("addSource").unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Other));
        assert_eq!(op.resource_type.as_deref(), Some("customers"));
        assert_eq!(
            op.identity.as_ref().unwrap().path_param.as_deref(),
            Some("id")
        );
        assert!(op.is_action());
    }

    #[test]
    fn a_tail_with_a_get_or_detail_route_stays_a_collection() {
        let b = actions();
        for (id, resource) in [
            ("createIssue", "issues"),
            ("postNote", "note"),
            ("postItem", "item"),
        ] {
            let op = b.operation(id).unwrap();
            assert_eq!(op.kind, OperationKind::Command(Effect::Create), "{id}");
            assert_eq!(op.resource_type.as_deref(), Some(resource), "{id}");
        }
    }

    #[test]
    fn a_tail_under_two_params_stays_a_collection() {
        let op = actions();
        let op = op.operation("mergeUnderAParam").unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Create));
        assert_eq!(op.resource_type.as_deref(), Some("merge"));
    }

    #[test]
    fn a_read_verb_tail_stays_a_read() {
        let b = actions();
        let op = b.operation("queryDatabase").unwrap();
        assert_eq!(op.kind, OperationKind::Query);
        assert_eq!(op.resource_type.as_deref(), Some("databases"));
    }

    #[test]
    fn a_streaming_tail_keeps_its_stream() {
        let b = actions();
        let op = b.operation("streamReply").unwrap();
        assert_eq!(op.kind, OperationKind::StreamServer);
        assert_eq!(op.resource_type.as_deref(), Some("reply"));
    }
}

/// What the contract says about a `POST` under `<static>/{param}`, with no
/// word of its path read as English.
mod action_structure {
    use super::*;
    use mb_import::compile_with_warnings;

    const STRUCTURE: &str = r##"
openapi: 3.0.0
info: {title: Structure, version: "1"}
paths:
  /jobs/{id}/process:
    post:
      operationId: process
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /users/{id}/address:
    post:
      operationId: address
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /lists/{id}/entries:
    get:
      operationId: listEntries
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}}
    post:
      operationId: addEntry
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /pages/{id}/entries:
    get:
      operationId: pageEntries
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  data: {type: array, items: {$ref: "#/components/schemas/Item"}}
                  has_more: {type: boolean}
    post:
      operationId: addWrapped
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: object, properties: {data: {$ref: "#/components/schemas/Item"}}}}}
  /sums/{id}/entries:
    get:
      operationId: listSummaries
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/ItemSummary"}}}}}}
    post:
      operationId: addFull
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}}
  /playlists/{id}/tracks:
    get:
      operationId: listTracks
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: object, properties: {items: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}}
    post:
      operationId: addTracks
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Snapshot"}}}}}
  /boards/{id}/labels:
    get:
      operationId: listLabels
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: object, properties: {items: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}}
    post:
      operationId: addLabelled
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Snapshot"}}}}}
  /blobs/{id}/parts:
    get:
      operationId: listParts
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object}}}}}
    post:
      operationId: addPart
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Snapshot"}}}}}
  /blobs/{id}/chunks:
    get:
      operationId: listChunks
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}}
    post:
      operationId: addChunk
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object}}}}}
  /states/{id}/phase:
    get:
      operationId: getPhase
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {state: {type: string}}}}}}}
    post:
      operationId: movePhase
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /issues/{key}/transitions:
    get:
      operationId: listTransitions
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}}
    post:
      operationId: transition
      responses: {"204": {description: done}}
  /docs/{id}/tags:
    post:
      operationId: tagDoc
      responses: {"200": {description: done}}
  /docs/{id}/tags/{tag}:
    delete:
      operationId: untagDoc
      responses: {"204": {description: done}}
  /batches/{id}/run:
    post:
      operationId: runBatch
      responses: {"202": {description: accepted}}
  /folders/{id}/files:
    get:
      operationId: listFiles
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}}
    post:
      operationId: addFile
      responses: {"201": {description: created}}
  /mixed/{id}/things:
    get:
      operationId: listThings
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Item"}}}}}}
    post:
      operationId: addThing
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Item"}}}}
        "204": {description: already there}
  /deep/{id}/y/z:
    post:
      operationId: deepNoBody
      responses: {"204": {description: done}}
  /deeper/{id}/y/z:
    post:
      operationId: deepWithBody
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /repos/{o}/{r}/close:
    post:
      operationId: closeUnderTwoParams
      responses: {"204": {description: done}}
  /tasks/{id}/sync:
    post:
      operationId: syncNoBody
      responses: {"204": {description: done}}
  /tasks/{id}/search:
    post:
      operationId: searchWithBody
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /stars/{id}/star:
    put:
      operationId: putStar
      responses: {"204": {description: done}}
  /blocks/{id}/children:
    patch:
      operationId: patchChildren
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Ack"}}}}}
  /charges/{id}/refund:
    post:
      operationId: refundCharge
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}}
  /refunds/{refund_id}:
    get:
      operationId: getRefund
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}}
  /charges/{id}/dispute:
    post:
      operationId: disputeCharge
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Dispute"}}}}}
  /disputes:
    get:
      operationId: listDisputes
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Dispute"}}}}}}
  /posts/{id}/comments:
    post:
      operationId: comment
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Comment"}}}}}
  /installs/{id}/tokens:
    post:
      operationId: mintToken
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Snapshot"}}}}}
  /orders:
    get:
      operationId: listOrders
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Order"}}}}}}
  /orders/{id}/close:
    post:
      operationId: closeOrder
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /Accounts/{Sid}/Calls.json:
    post:
      operationId: createCall
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Comment"}}}}}
  /Accounts/{Sid}/Calls/{CallSid}.json:
    delete:
      operationId: deleteCall
      responses: {"204": {description: done}}
components:
  schemas:
    Ack: {type: object, properties: {accepted: {type: boolean}}}
    Item: {type: object, properties: {id: {type: string}, name: {type: string}}}
    ItemSummary: {type: object, properties: {id: {type: string}}}
    Snapshot: {type: object, properties: {snapshot_id: {type: string}}}
    Refund: {type: object, properties: {id: {type: string}, amount: {type: integer}}}
    Dispute: {type: object, properties: {id: {type: string}, reason: {type: string}}}
    Comment: {type: object, properties: {id: {type: string}, body: {type: string}}}
    Order: {type: object, properties: {id: {type: string}, status: {type: string}}}
"##;

    fn other(b: &Bundle, id: &str, parent: &str, param: &str) {
        let op = b.operation(id).unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Other), "{id}");
        assert_eq!(op.resource_type.as_deref(), Some(parent), "{id}");
        assert_eq!(
            op.identity.as_ref().and_then(|i| i.path_param.as_deref()),
            Some(param),
            "{id}"
        );
        assert!(op.is_action(), "{id}");
    }

    fn create(b: &Bundle, id: &str, resource: &str) {
        let op = b.operation(id).unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Create), "{id}");
        assert_eq!(op.resource_type.as_deref(), Some(resource), "{id}");
        assert!(!op.is_action(), "{id}");
    }

    fn structure() -> Bundle {
        compile(STRUCTURE.as_bytes()).unwrap()
    }

    /// `process` and `address` end in `s`; that says nothing.
    #[test]
    fn the_spelling_of_a_tail_does_not_count() {
        let b = structure();
        other(&b, "process", "jobs", "id");
        other(&b, "address", "users", "id");
    }

    #[test]
    fn a_get_beside_it_that_lists_its_answer_makes_a_collection() {
        let b = structure();
        create(&b, "addEntry", "entries");
        // In envelopes, and a shorter item with the same identity.
        create(&b, "addWrapped", "entries");
        create(&b, "addFull", "entries");
    }

    #[test]
    fn a_get_beside_it_that_reads_something_else_leaves_an_action() {
        let b = structure();
        other(&b, "movePhase", "states", "id");
        other(&b, "addLabelled", "boards", "id");
    }

    /// An open schema on either side tells them apart from nothing.
    #[test]
    fn a_get_beside_it_that_cannot_be_compared_makes_a_collection() {
        let b = structure();
        create(&b, "addPart", "parts");
        create(&b, "addChunk", "chunks");
    }

    #[test]
    fn a_post_with_no_body_acts_on_its_parent_whatever_is_beside_it() {
        let b = structure();
        other(&b, "transition", "issues", "key");
        other(&b, "tagDoc", "docs", "id");
        other(&b, "runBatch", "batches", "id");
    }

    /// A `201` says it created something, with or without a body; one of
    /// several successes having a body means a client can be told.
    #[test]
    fn a_created_or_a_body_among_the_successes_is_not_a_bodyless_action() {
        let b = structure();
        create(&b, "addFile", "files");
        create(&b, "addThing", "things");
    }

    #[test]
    fn a_static_tail_of_two_segments_needs_no_body_to_act() {
        let b = structure();
        other(&b, "deepNoBody", "deep", "id");
        create(&b, "deepWithBody", "z");
    }

    /// Two parameters in a row name one resource together: the contract does
    /// not say which of them is its identity.
    #[test]
    fn a_tail_under_two_params_stays_as_it_was_without_a_body() {
        create(&structure(), "closeUnderTwoParams", "close");
    }

    #[test]
    fn no_body_beats_a_read_verb() {
        let b = structure();
        other(&b, "syncNoBody", "tasks", "id");
        assert_eq!(
            b.operation("searchWithBody").unwrap().kind,
            OperationKind::Query
        );
    }

    #[test]
    fn put_and_patch_on_a_tail_write_what_their_path_names() {
        let b = structure();
        for (id, resource) in [("putStar", "star"), ("patchChildren", "children")] {
            let op = b.operation(id).unwrap();
            assert_eq!(op.kind, OperationKind::Command(Effect::Update), "{id}");
            assert_eq!(op.resource_type.as_deref(), Some(resource), "{id}");
        }
    }

    /// The created record has to land where its read looks.
    #[test]
    fn an_answer_read_elsewhere_is_created_under_that_read() {
        create(&structure(), "refundCharge", "refunds");
    }

    /// `GET /disputes` lists it, and so might any list of anything.
    #[test]
    fn a_list_alone_does_not_rename_the_type() {
        other(&structure(), "disputeCharge", "charges", "id");
    }

    /// `201` says it made something. With no identity it stores nothing.
    #[test]
    fn a_created_answer_is_a_record_with_or_without_an_identity() {
        let b = structure();
        create(&b, "comment", "comments");
        create(&b, "mintToken", "tokens");
        create(&b, "addTracks", "tracks");
        assert_eq!(b.operation("mintToken").unwrap().identity, None);
    }

    /// Its parent's list is no read elsewhere: the parent is what it acts on.
    #[test]
    fn the_parents_own_list_is_not_a_read_elsewhere() {
        other(&structure(), "closeOrder", "orders", "id");
    }

    /// `Calls.json` and `Calls/{CallSid}.json` are one route with its detail.
    #[test]
    fn a_format_suffix_does_not_hide_a_detail_route() {
        create(&structure(), "createCall", "Calls");
    }

    fn example(rel: &str) -> Bundle {
        let path = format!("{}/../../examples/apis/{rel}", env!("CARGO_MANIFEST_DIR"));
        compile(&std::fs::read(path).unwrap()).unwrap()
    }

    /// `POST .../transitions` answers `204` beside a `GET` of the same path.
    #[test]
    fn jiras_transition_acts_on_its_issue() {
        let b = example("jira/jira.openapi.yaml");
        other(&b, "doTransition", "issue", "issueIdOrKey");
        let identity = b.operation("doTransition").unwrap().identity.clone();
        let read = b.operation("getIssue").unwrap().identity.clone().unwrap();
        let identity = identity.unwrap();
        assert_eq!(identity.field, read.field);
        assert_eq!(identity.path_field, read.path_field);
        assert_eq!(
            b.operation("getTransitions").unwrap().kind,
            OperationKind::Query
        );
        create(&b, "addComment", "comment");
    }

    #[test]
    fn real_contracts_classify_by_structure() {
        let auth0 = example("auth0-management/auth0-management.openapi.yaml");
        other(&auth0, "post_user_roles", "users", "id");
        let spotify = example("spotify/spotify.openapi.yaml");
        create(&spotify, "add-items-to-playlist", "items");
        create(&spotify, "add-tracks-to-playlist", "tracks");
        let github = example("github-rest/github-rest.openapi.yaml");
        create(
            &github,
            "apps/create-installation-access-token",
            "access_tokens",
        );
        create(&github, "issues/create-comment", "comments");
        let paypal = example("paypal/paypal-payments.openapi.yaml");
        create(&paypal, "captures.refund", "refunds");
        let twilio = example("twilio/twilio.openapi.yaml");
        create(&twilio, "CreateCall", "Calls");
        create(&twilio, "CreateMessage", "Messages");
        let discord = example("discord/discord.openapi.yaml");
        create(&discord, "create_message", "messages");
        create(&discord, "create_interaction_response", "callback");
    }

    /// One contract for the shapes the review found, each route on its own
    /// resource so that none reads another.
    fn shaped(routes: &str, schemas: &str) -> Bundle {
        let contract = format!(
            "openapi: 3.0.0\ninfo: {{title: Shapes, version: \"1\"}}\npaths:\n{routes}components:\n  schemas:\n{schemas}"
        );
        compile(contract.as_bytes()).unwrap()
    }

    const NEW_BODY: &str = r##"requestBody: {required: true, content: {application/json: {schema: {type: object, properties: {body: {type: string}}}}}}"##;

    fn body_of(status: &str, schema: &str) -> String {
        format!(
            r##"responses: {{"{status}": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/{schema}"}}}}}}}}}}"##
        )
    }

    fn parent_and_child(parent: &str, tail: &str, status: &str, request: &str) -> Bundle {
        let read = body_of("200", "Node");
        let write = body_of(status, "Node");
        shaped(
            &format!(
                "  /{parent}/{{id}}:\n    get:\n      operationId: read\n      {read}\n  /{parent}/{{id}}/{tail}:\n    post:\n      operationId: subject\n      {request}\n      {write}\n"
            ),
            "    Node: {type: object, properties: {id: {type: string}, text: {type: string}}}\n",
        )
    }

    fn update_of(b: &Bundle, id: &str, parent: &str) {
        let op = b.operation(id).unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Update), "{id}");
        assert_eq!(op.resource_type.as_deref(), Some(parent), "{id}");
        assert!(op.is_action(), "{id}");
    }

    /// An answer of the parent's own schema is an update whatever the status
    /// and the request: nothing in the contract tells a reply from a capture.
    /// The runtime goes by the identity the answer carries.
    #[test]
    fn an_answer_of_the_parents_schema_is_an_update() {
        let required = r##"requestBody: {required: true, content: {application/json: {schema: {type: object, required: [amount], properties: {amount: {type: integer}}}}}}"##;
        let optional = r##"requestBody: {content: {application/json: {schema: {type: object, properties: {body: {type: string}}}}}}"##;
        let one_of = r##"requestBody: {required: true, content: {application/json: {schema: {oneOf: [{type: object, properties: {a: {type: string}}}, {type: object, properties: {b: {type: string}}}]}}}}"##;
        let text = "requestBody: {required: true, content: {text/plain: {schema: {type: string}}}}";
        for request in ["summary: none", NEW_BODY, required, optional, one_of, text] {
            for status in ["200", "201"] {
                for (parent, tail) in [
                    ("comments", "replies"),
                    ("documents", "copies"),
                    ("orders", "capture"),
                ] {
                    update_of(
                        &parent_and_child(parent, tail, status, request),
                        "subject",
                        parent,
                    );
                }
            }
        }
        let paypal = example("paypal/paypal-orders.openapi.yaml");
        for id in ["orders.capture", "orders.authorize", "orders.confirm"] {
            update_of(&paypal, id, "orders");
        }
    }

    /// Two open objects are not the parent's schema by an exact statement.
    #[test]
    fn an_opaque_answer_does_not_overwrite_its_parent() {
        let open = r##"responses: {"200": {description: ok, content: {application/json: {schema: {type: object}}}}}"##;
        let b = shaped(
            &format!(
                "  /jobs/{{id}}:\n    get:\n      operationId: read\n      {open}\n  /jobs/{{id}}/run:\n    post:\n      operationId: subject\n      {open}\n"
            ),
            "    Unused: {type: object}\n",
        );
        other(&b, "subject", "jobs", "id");
    }

    #[test]
    fn action_false_with_a_type_corrects_an_update() {
        let corrected =
            format!("{NEW_BODY}\n      x-mockingbird-resource: {{action: false, type: comments}}");
        let b = parent_and_child("comments", "replies", "200", &corrected);
        create(&b, "subject", "comments");
    }

    /// `Operation` also answers a `DELETE` elsewhere, which serves no record.
    #[test]
    fn a_shared_answer_needs_a_get_by_path_to_rename_the_type() {
        let answer = body_of("200", "Operation");
        let without = format!(
            "  /instances/{{id}}/start:\n    post:\n      operationId: subject\n      {answer}\n  /disks/{{id}}:\n    delete:\n      operationId: dropDisk\n      {answer}\n"
        );
        let schemas = "    Operation: {type: object, properties: {name: {type: string}, done: {type: boolean}}}\n";
        other(&shaped(&without, schemas), "subject", "instances", "id");
        let with = format!(
            "{without}  /operations/{{name}}:\n    get:\n      operationId: getOperation\n      {answer}\n"
        );
        create(&shaped(&with, schemas), "subject", "operations");
    }

    /// Two reads serve it: the contract does not say which it is.
    #[test]
    fn two_reads_serving_an_answer_rename_nothing() {
        let answer = body_of("200", "Rec");
        let b = shaped(
            &format!(
                "  /a/{{id}}/go:\n    post:\n      operationId: subject\n      {answer}\n  /x/{{id}}:\n    get:\n      operationId: x\n      {answer}\n  /y/{{id}}:\n    get:\n      operationId: y\n      {answer}\n"
            ),
            "    Rec: {type: object, properties: {id: {type: string}}}\n",
        );
        other(&b, "subject", "a", "id");
    }

    /// One string member is no envelope: `{snapshot_id}` is not `{id}`.
    #[test]
    fn an_envelope_unwraps_only_to_an_object() {
        let snapshot = r##"responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {snapshot_id: {type: string}}}}}}}"##;
        let id = r##"responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}}"##;
        let b = shaped(
            &format!(
                "  /volumes/{{id}}/snapshots:\n    post:\n      operationId: subject\n      {snapshot}\n  /disks/{{id}}:\n    get:\n      operationId: getDisk\n      {id}\n  /notes/{{id}}:\n    get:\n      operationId: getNote\n      {id}\n  /notes/{{id}}/pin:\n    post:\n      operationId: pin\n      {snapshot}\n"
            ),
            "    Unused: {type: object}\n",
        );
        other(&b, "subject", "volumes", "id");
        other(&b, "pin", "notes", "id");
    }

    /// Neither has a `GET`, so nothing says `Ack` is the order.
    #[test]
    fn an_answer_shared_with_a_delete_does_not_overwrite_its_parent() {
        let ack = body_of("200", "Ack");
        let b = shaped(
            &format!(
                "  /orders/{{id}}:\n    delete:\n      operationId: dropOrder\n      {ack}\n  /orders/{{id}}/ship:\n    post:\n      operationId: subject\n      {ack}\n"
            ),
            "    Ack: {type: object, properties: {ok: {type: boolean}}}\n",
        );
        other(&b, "subject", "orders", "id");
    }

    /// A read that wraps its record serves it just the same.
    #[test]
    fn a_read_side_envelope_counts_at_any_status() {
        let refund = r##"{"$ref": "#/components/schemas/Refund"}"##;
        let b = shaped(
            &format!(
                r##"  /charges/{{id}}/refund:
    post:
      operationId: subject
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {refund}}}}}}}}}
  /refunds/{{id}}:
    get:
      operationId: read
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{data: {refund}}}}}}}}}}}}}
"##
            ),
            "    Refund: {type: object, properties: {id: {type: string}}}\n",
        );
        create(&b, "subject", "refunds");
        assert_eq!(
            b.operation("subject")
                .unwrap()
                .identity
                .as_ref()
                .unwrap()
                .field,
            "id"
        );
    }

    /// Whichever success declares it, `identity` is declared.
    #[test]
    fn the_identity_hint_reads_the_declared_201_too() {
        let contract = r##"
openapi: 3.0.0
info: {title: Ext, version: "1"}
paths:
  /a/{id}/b:
    post:
      operationId: subject
      x-mockingbird-resource: {identity: sku}
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: object, properties: {note: {type: string}}}}}}
        "201": {description: ok, content: {application/json: {schema: {type: object, properties: {sku: {type: string}}}}}}
"##;
        let (op, warnings) = subject(contract);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(op.identity.unwrap().field, "sku");
    }

    /// Whatever names its identity: the read elsewhere knows it.
    #[test]
    fn a_record_read_elsewhere_takes_its_identity_from_the_read() {
        for (status, schema, param, field) in [
            (
                "201",
                "{shipment_id: {type: string}, carrier: {type: string}}",
                "shipment_id",
                Some("shipment_id"),
            ),
            (
                "200",
                "{shipment_id: {type: string}, carrier: {type: string}}",
                "shipment_id",
                Some("shipment_id"),
            ),
            (
                "201",
                "{key: {type: string}, secret: {type: string}}",
                "key",
                Some("key"),
            ),
            ("201", "{Id: {type: string}}", "id", Some("Id")),
            // `shipment_id` ends in the word `_id` is.
            ("201", "{_id: {type: string}}", "shipment_id", Some("_id")),
            ("201", "{_id: {type: string}}", "tracking", None),
            (
                "201",
                "{data: {type: object}, links: {type: object}}",
                "shipment_id",
                None,
            ),
        ] {
            let write = body_of(status, "Shipment");
            let read = body_of("200", "Shipment");
            let b = shaped(
                &format!(
                    "  /orders/{{id}}/shipments:\n    post:\n      operationId: subject\n      {write}\n  /parcels/{{{param}}}:\n    get:\n      operationId: read\n      {read}\n"
                ),
                &format!("    Shipment: {{type: object, properties: {schema}}}\n"),
            );
            let op = b.operation("subject").unwrap();
            assert_eq!(op.kind, OperationKind::Command(Effect::Create), "{schema}");
            assert_eq!(op.resource_type.as_deref(), Some("parcels"), "{schema}");
            let identity = op.identity.as_ref();
            assert_eq!(identity.map(|i| i.field.as_str()), field, "{schema}");
            assert_eq!(
                identity.and_then(|i| i.path_param.clone()),
                None,
                "{schema}"
            );
        }
    }

    /// Serving it by path is the contract's statement, with no identity name
    /// needed.
    #[test]
    fn a_200_read_elsewhere_needs_no_identity() {
        let b = shaped(
            &format!(
                "  /jobs/{{id}}/run:\n    post:\n      operationId: subject\n      {}\n  /stats/{{day}}:\n    get:\n      operationId: read\n      {}\n",
                body_of("200", "Ack"),
                body_of("200", "Ack"),
            ),
            "    Ack: {type: object, properties: {ok: {type: boolean}}}\n",
        );
        create(&b, "subject", "stats");
    }

    /// Any declared `201` counts, and it says what the identity is.
    #[test]
    fn a_201_beside_another_success_counts() {
        let id = r##"{"$ref": "#/components/schemas/Rec"}"##;
        let b = shaped(
            &format!(
                r##"  /a/{{id}}/both:
    post:
      operationId: both
      responses:
        "200": {{description: ok, content: {{application/json: {{schema: {id}}}}}}}
        "201": {{description: ok, content: {{application/json: {{schema: {id}}}}}}}
  /a/{{id}}/empty:
    post:
      operationId: emptyFirst
      responses:
        "200": {{description: already there}}
        "201": {{description: ok, content: {{application/json: {{schema: {id}}}}}}}
"##
            ),
            "    Rec: {type: object, properties: {id: {type: string}}}\n",
        );
        for (op, resource) in [("both", "both"), ("emptyFirst", "empty")] {
            create(&b, op, resource);
            let identity = b.operation(op).unwrap().identity.clone().unwrap();
            assert_eq!(identity.field, "id", "{op}");
        }
    }

    /// The parent's read wraps what it answers, and a list elsewhere holds
    /// the same schema: it still acts on the parent.
    #[test]
    fn an_answer_the_parent_wraps_is_not_read_elsewhere() {
        let user = r##"{"$ref": "#/components/schemas/User"}"##;
        let b = shaped(
            &format!(
                r##"  /users/{{id}}:
    get:
      operationId: read
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{data: {user}}}}}}}}}}}}}
  /users/{{id}}/suspend:
    post:
      operationId: subject
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {user}}}}}}}}}
  /teams/{{id}}/members:
    get:
      operationId: members
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{type: array, items: {user}}}}}}}}}}}
"##
            ),
            "    User: {type: object, properties: {id: {type: string}}}\n",
        );
        other(&b, "subject", "users", "id");
    }

    #[test]
    fn a_read_elsewhere_counts_through_an_envelope_or_an_all_of() {
        let refund = r##"{"$ref": "#/components/schemas/Refund"}"##;
        let short = r##"{"$ref": "#/components/schemas/RefundCreated"}"##;
        let b = shaped(
            &format!(
                r##"  /a/{{id}}/wrapped:
    post:
      operationId: wrapped
      responses: {{"201": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{data: {refund}}}}}}}}}}}}}
  /b/{{id}}/shorter:
    post:
      operationId: shorter
      responses: {{"201": {{description: ok, content: {{application/json: {{schema: {short}}}}}}}}}
  /c/{{id}}/shorter200:
    post:
      operationId: shorter200
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {short}}}}}}}}}
  /refunds/{{id}}:
    get:
      operationId: read
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {refund}}}}}}}}}
"##
            ),
            r##"    RefundCreated: {type: object, properties: {id: {type: string}, status: {type: string}}}
    Refund:
      allOf:
        - $ref: "#/components/schemas/RefundCreated"
        - {type: object, properties: {amount: {type: integer}}}
"##,
        );
        for id in ["wrapped", "shorter", "shorter200"] {
            create(&b, id, "refunds");
        }
    }

    fn with_ext(path: &str, method: &str, status: &str, ext: &str) -> String {
        format!(
            r##"
openapi: 3.0.0
info: {{title: Ext, version: "1"}}
paths:
  /things/{{id}}:
    get:
      operationId: getThing
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Thing"}}}}}}}}}}
  {path}:
    {method}:
      operationId: subject
      x-mockingbird-resource: {ext}
      responses: {status}
components:
  schemas:
    Thing: {{type: object, properties: {{id: {{type: string}}, sku: {{type: string}}}}}}
"##
        )
    }

    const THING: &str = r##"{"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Thing"}}}}}"##;
    const NOTE: &str = r##"{"200": {description: ok, content: {application/json: {schema: {type: object, properties: {note: {type: string}}}}}}}"##;

    fn subject(contract: &str) -> (mb_mcm::Operation, Vec<String>) {
        let (b, warnings) = compile_with_warnings(contract.as_bytes()).unwrap();
        let op = b.operation("subject").unwrap().clone();
        let codes = warnings
            .into_iter()
            .filter(|w| w.code == "invalid_resource_extension")
            .map(|w| {
                assert_eq!(w.operation.as_deref(), Some("subject"));
                assert!(w.message.contains("x-mockingbird-resource"), "{w:?}");
                w.message
            })
            .collect();
        (op, codes)
    }

    /// The tail says nothing English here: it would be read as a read.
    #[test]
    fn the_extension_makes_a_read_verb_an_action() {
        let c = with_ext("/things/{id}/sync", "post", NOTE, "{action: true}");
        let (op, warnings) = subject(&c);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(op.kind, OperationKind::Command(Effect::Other));
        assert_eq!(op.resource_type.as_deref(), Some("things"));
        assert!(op.is_action());
    }

    #[test]
    fn the_extension_makes_a_read_verb_a_write_of_its_own() {
        let c = with_ext("/things/{id}/history", "post", NOTE, "{action: false}");
        let (op, warnings) = subject(&c);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(op.kind, OperationKind::Command(Effect::Create));
        assert_eq!(op.resource_type.as_deref(), Some("history"));
        assert!(!op.is_action());
    }

    #[test]
    fn the_extension_overrides_what_inference_decides() {
        // Inference: an action. Declared: a collection.
        let c = with_ext("/things/{id}/note", "post", NOTE, "{action: false}");
        let (op, _) = subject(&c);
        assert_eq!(op.kind, OperationKind::Command(Effect::Create));
        assert_eq!(op.resource_type.as_deref(), Some("note"));
        // Inference: a create with no parent. Declared: an action, deep down.
        let c = with_ext("/things/{id}/a/b", "post", THING, "{action: true}");
        let (op, warnings) = subject(&c);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(op.kind, OperationKind::Command(Effect::Update));
        assert_eq!(op.resource_type.as_deref(), Some("things"));
        // PUT is left alone by inference, not by the contract's author.
        let c = with_ext(
            "/things/{id}/star",
            "put",
            r#"{"204": {description: ok}}"#,
            "{action: true}",
        );
        let (op, warnings) = subject(&c);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(op.kind, OperationKind::Command(Effect::Other));
        assert_eq!(op.resource_type.as_deref(), Some("things"));
    }

    #[test]
    fn the_extension_names_the_type_and_the_identity() {
        let c = with_ext(
            "/parts/{id}",
            "put",
            THING,
            "{type: widgets, identity: sku}",
        );
        let (op, warnings) = subject(&c);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(op.kind, OperationKind::Command(Effect::Update));
        assert_eq!(op.resource_type.as_deref(), Some("widgets"));
        let identity = op.identity.unwrap();
        assert_eq!(identity.field, "sku");
        assert_eq!(identity.path_param.as_deref(), Some("id"));
        // On an action, the type and the identity of what it acts on.
        let c = with_ext(
            "/things/{id}/ship",
            "post",
            NOTE,
            "{action: true, type: widgets, identity: sku}",
        );
        let (op, warnings) = subject(&c);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(op.kind, OperationKind::Command(Effect::Other));
        assert_eq!(op.resource_type.as_deref(), Some("widgets"));
        let identity = op.identity.unwrap();
        assert_eq!(identity.field, "sku");
        assert_eq!(identity.path_param.as_deref(), Some("id"));
    }

    #[test]
    fn an_invalid_extension_warns_and_inference_stands() {
        let inferred = with_ext("/things/{id}/note", "post", NOTE, "{}");
        let (expected, warnings) = subject(&inferred);
        assert!(warnings.is_empty(), "{warnings:?}");
        for ext in [
            "true",
            "[a]",
            "{type: 1}",
            "{type: ''}",
            "{identity: 1}",
            "{identity: ''}",
            "{identity: nowhere}",
            "{action: 'yes'}",
            "{resource: things}",
            "{type: 'two words'}",
            "{type: 'a/b'}",
            "{type: '{id}'}",
        ] {
            let c = with_ext("/things/{id}/note", "post", NOTE, ext);
            let (op, warnings) = subject(&c);
            assert_eq!(warnings.len(), 1, "{ext}: {warnings:?}");
            assert_eq!(op.kind, expected.kind, "{ext}");
            assert_eq!(op.resource_type, expected.resource_type, "{ext}");
            assert_eq!(op.identity, expected.identity, "{ext}");
        }
    }

    #[test]
    fn an_impossible_action_warns_and_inference_stands() {
        let c = with_ext("/standalone", "post", NOTE, "{action: true}");
        let (op, warnings) = subject(&c);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(op.kind, OperationKind::Command(Effect::Create));
        assert_eq!(op.resource_type.as_deref(), Some("standalone"));
        let c = with_ext("/things/{id}/view", "get", NOTE, "{action: true}");
        let (op, warnings) = subject(&c);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(op.kind, OperationKind::Query);
        let streams =
            r#"{"200": {description: ok, content: {text/event-stream: {schema: {type: object}}}}}"#;
        let c = with_ext("/things/{id}/reply", "post", streams, "{action: true}");
        let (op, warnings) = subject(&c);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(op.kind, OperationKind::StreamServer);
    }

    #[test]
    fn an_action_false_with_nothing_to_override_warns() {
        for (path, method) in [
            ("/things/{id}/view", "get"),
            ("/things/{id}/star", "put"),
            ("/parts/{id}", "post"),
        ] {
            let c = with_ext(path, method, NOTE, "{action: false}");
            let (_, warnings) = subject(&c);
            assert_eq!(warnings.len(), 1, "{path}: {warnings:?}");
        }
        // Stating what inference already decides is not ignored.
        let c = with_ext("/standalone", "post", NOTE, "{action: false}");
        let (op, warnings) = subject(&c);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(op.kind, OperationKind::Command(Effect::Create));
        let streams =
            r#"{"200": {description: ok, content: {text/event-stream: {schema: {type: object}}}}}"#;
        let c = with_ext("/things/{id}/reply", "post", streams, "{action: false}");
        let (_, warnings) = subject(&c);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    /// A route is judged by the reads as declared, wherever they sort.
    #[test]
    fn a_route_lands_where_a_read_declares_its_type() {
        let contract = r##"
openapi: 3.0.0
info: {title: Ext, version: "1"}
paths:
  /a/{id}/refund:
    post:
      operationId: subject
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}}
  /z/{refund_id}:
    get:
      operationId: getRefund
      x-mockingbird-resource: {type: refunds}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Refund"}}}}}
components:
  schemas:
    Refund: {type: object, properties: {id: {type: string}}}
"##;
        let (op, warnings) = subject(contract);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(op.kind, OperationKind::Command(Effect::Create));
        assert_eq!(op.resource_type.as_deref(), Some("refunds"));
    }

    /// One bad member does not cost the good ones.
    #[test]
    fn a_valid_member_beside_an_invalid_one_holds() {
        let c = with_ext(
            "/things/{id}/note",
            "post",
            NOTE,
            "{type: widgets, action: 'yes'}",
        );
        let (op, warnings) = subject(&c);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(op.resource_type.as_deref(), Some("widgets"));
    }
}

fn op<'a>(bundle: &'a mb_mcm::Bundle, id: &str) -> &'a mb_mcm::Operation {
    bundle.operation(id).unwrap()
}

/// A path by id over `$ref`s the routes share: one user, its lookups, and what
/// must not be taken for either.
const SHARED_SCHEMAS: &str = r##"
openapi: 3.0.0
info: {title: Shared, version: "1"}
paths:
  /users:
    post:
      operationId: createUser
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}}
  /users/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getUser
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}}
    delete:
      operationId: deleteUser
      responses: {"204": {description: gone}}
  /users-by-email:
    get:
      operationId: usersByEmail
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/User"}}}}}}
  /people-page:
    get:
      operationId: peoplePage
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {total: {type: integer}, people: {type: array, items: {$ref: "#/components/schemas/User"}}}}}}}}
  /teams/{team}/members:
    parameters: [{name: team, in: path, required: true, schema: {type: string}}]
    get:
      operationId: teamMembers
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/User"}}}}}}
  /me:
    get:
      operationId: me
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}}
  /admins:
    get:
      operationId: listAdmins
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/User"}}}}}}
    post:
      operationId: createAdmin
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Other"}}}}}
  /hinted:
    get:
      operationId: hinted
      x-mockingbird-resource: {type: chosen}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/User"}}}}}}
  /tokens:
    post:
      operationId: createToken
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {token_id: {type: string}}}}}}}
components:
  schemas:
    User:
      type: object
      properties:
        user_id: {type: string}
        email: {type: string}
    Other:
      type: object
      properties:
        other: {type: string}
"##;

#[test]
fn a_member_named_for_its_own_type_identifies_a_type_read_by_path() {
    let bundle = compile(SHARED_SCHEMAS.as_bytes()).unwrap();
    let read = op(&bundle, "getUser").identity.as_ref().unwrap();
    assert_eq!(read.field, "user_id");
    assert_eq!(read.path_param.as_deref(), Some("id"));
    let create = op(&bundle, "createUser").identity.as_ref().unwrap();
    assert_eq!(create.field, "user_id");
    assert_eq!(create.path_param, None);
    // No route reads a token by path: its answer is no record to keep.
    assert_eq!(op(&bundle, "createToken").identity, None);
}

#[test]
fn a_lookup_answering_a_types_items_reads_as_that_type() {
    let bundle = compile(SHARED_SCHEMAS.as_bytes()).unwrap();
    let type_of = |id: &str| op(&bundle, id).resource_type.as_deref();
    assert_eq!(type_of("usersByEmail"), Some("users"));
    assert_eq!(type_of("peoplePage"), Some("users"));
    // Under a parent, or beside a route of its own type, items that are exactly a
    // type's records are that type's.
    assert_eq!(type_of("teamMembers"), Some("users"));
    assert_eq!(type_of("listAdmins"), Some("users"));
    assert_eq!(type_of("createAdmin"), Some("admins"));
    // Not a list: one record, which may not be stored as any.
    assert_eq!(type_of("me"), Some("me"));
    assert_eq!(type_of("hinted"), Some("chosen"));
}

#[test]
fn a_lookup_of_two_types_stays_its_own() {
    let contract = SHARED_SCHEMAS.replace(
        "  /tokens:\n",
        r##"  /staff/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getStaff
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/User"}}}}}
  /tokens:
"##,
    );
    let bundle = compile(contract.as_bytes()).unwrap();
    assert_eq!(
        op(&bundle, "usersByEmail").resource_type.as_deref(),
        Some("users-by-email")
    );
}

#[test]
fn the_auth0_example_reads_one_user_on_every_route() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/apis/auth0-management/auth0-management.openapi.yaml"
    );
    let bundle = compile(&std::fs::read(path).unwrap()).unwrap();
    for id in ["post_users", "get_users_by_id", "patch_users_by_id"] {
        let identity = op(&bundle, id).identity.as_ref().unwrap();
        assert_eq!(identity.field, "user_id", "{id}");
    }
    assert_eq!(
        op(&bundle, "get_users-by-email").resource_type.as_deref(),
        Some("users")
    );
    // An access token is no record.
    assert_eq!(op(&bundle, "oauth_token").identity, None);
}

/// `contract` with `/{resource}/{param}` over `member`, and a create.
fn by_path(resource: &str, param: &str, member: &str) -> Bundle {
    let contract = format!(
        r##"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /{resource}:
    post:
      operationId: create
      responses: {{"201": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/R"}}}}}}}}}}
  /{resource}/{{{param}}}:
    parameters: [{{name: {param}, in: path, required: true, schema: {{type: string}}}}]
    get:
      operationId: read
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/R"}}}}}}}}}}
components:
  schemas:
    R:
      type: object
      properties:
        {member}: {{type: string}}
        name: {{type: string}}
"##
    );
    compile(contract.as_bytes()).unwrap()
}

#[test]
fn the_type_name_rule_needs_a_parameter_that_says_it_is_an_id() {
    for param in ["id", "userId", "user_id", "_id"] {
        let bundle = by_path("users", param, "user_id");
        let read = op(&bundle, "read").identity.as_ref();
        assert_eq!(read.map(|i| i.field.as_str()), Some("user_id"), "{param}");
        let create = op(&bundle, "create").identity.as_ref();
        assert_eq!(create.map(|i| i.field.as_str()), Some("user_id"), "{param}");
    }
    // A handle is not the key `user_id` holds, on the read or for what a create stores.
    let bundle = by_path("users", "handle", "user_id");
    assert_eq!(op(&bundle, "read").identity, None);
    assert_eq!(op(&bundle, "create").identity, None);
}

#[test]
fn the_type_name_rule_reads_plurals_as_the_budget_does() {
    for (resource, member) in [
        ("categories", "category_id"),
        ("categories", "categoryId"),
        ("addresses", "address_id"),
        ("users", "userId"),
    ] {
        let bundle = by_path(resource, "id", member);
        let read = op(&bundle, "read").identity.as_ref();
        assert_eq!(read.map(|i| i.field.as_str()), Some(member), "{resource}");
    }
    // Another type's key is not this one's.
    assert_eq!(
        op(&by_path("users", "id", "order_id"), "read").identity,
        None
    );
}

/// `/search` listing `arrays`, and two types read by path.
fn search_over(arrays: &str) -> Bundle {
    let contract = format!(
        r##"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /search:
    get:
      operationId: search
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{{arrays}}}}}}}}}}}}}
  /tickets/{{id}}:
    parameters: [{{name: id, in: path, required: true, schema: {{type: string}}}}]
    get:
      operationId: getTicket
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Ticket"}}}}}}}}}}
components:
  schemas:
    Ticket: {{type: object, properties: {{id: {{type: string}}, subject: {{type: string}}}}}}
    Hit: {{type: object, properties: {{id: {{type: string}}, label: {{type: string}}}}}}
"##
    );
    compile(contract.as_bytes()).unwrap()
}

#[test]
fn a_lookup_is_retyped_only_if_every_listed_array_holds_the_type() {
    let tickets = r##"tickets: {type: array, items: {$ref: "#/components/schemas/Ticket"}}"##;
    let hits = r##"users: {type: array, items: {$ref: "#/components/schemas/Hit"}}"##;
    let type_of = |arrays: &str| {
        let bundle = search_over(arrays);
        op(&bundle, "search").resource_type.clone()
    };
    assert_eq!(type_of(tickets).as_deref(), Some("tickets"));
    let both = format!(
        "{tickets}, also: {{type: array, items: {{$ref: \"#/components/schemas/Ticket\"}}}}"
    );
    assert_eq!(type_of(&both).as_deref(), Some("tickets"));
    // Strings are no records, and say nothing of the type.
    let words = format!("{tickets}, tags: {{type: array, items: {{type: string}}}}");
    assert_eq!(type_of(&words).as_deref(), Some("tickets"));
    // Another shape beside it, or items that could be anything, stays as it was.
    assert_eq!(
        type_of(&format!("{hits}, {tickets}")).as_deref(),
        Some("search")
    );
    let opaque = format!("{tickets}, other: {{type: array, items: {{type: object}}}}");
    assert_eq!(type_of(&opaque).as_deref(), Some("search"));
}

/// `/bounces/{param}` answering an array of `member` items, beside a delete.
fn array_by(param: &str, member: &str, required: bool) -> Bundle {
    let required = if required {
        format!("required: [{member}]")
    } else {
        "required: []".to_owned()
    };
    let contract = format!(
        r##"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /bounces/{{{param}}}:
    parameters: [{{name: {param}, in: path, required: true, schema: {{type: string}}}}]
    get:
      operationId: read
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{type: array, items: {{$ref: "#/components/schemas/B"}}}}}}}}}}}}
    delete:
      operationId: remove
      responses: {{"204": {{description: gone}}}}
components:
  schemas:
    B:
      type: object
      {required}
      properties:
        {member}: {{type: string}}
        reason: {{type: string}}
"##
    );
    compile(contract.as_bytes()).unwrap()
}

#[test]
fn a_path_parameter_naming_a_required_item_member_is_the_identity() {
    for (param, member) in [("email", "email"), ("emails", "email"), ("Email", "email")] {
        let bundle = array_by(param, member, true);
        assert_eq!(
            op(&bundle, "read").identity,
            Some(Identity {
                path_param: Some(param.into()),
                field: member.into(),
                input_field: None,
                path_field: None,
                record: Some(mb_mcm::RecordAt::Item),
            }),
            "{param}"
        );
        let remove = op(&bundle, "remove").identity.as_ref();
        assert_eq!(remove.map(|i| i.field.as_str()), Some(member), "{param}");
        assert_eq!(remove.and_then(|i| i.record.as_ref()), None, "{param}");
    }
    // A member that may be missing, or one the parameter does not name, is not it.
    assert_eq!(
        op(&array_by("email", "email", false), "read").identity,
        None
    );
    assert_eq!(
        op(&array_by("address", "email", true), "read").identity,
        None
    );
}

/// A read answering `{ticket: Ticket}` and a write answering `write`.
fn enveloped(write: &str) -> Bundle {
    let contract = format!(
        r##"
openapi: 3.0.0
info: {{title: T, version: "1"}}
paths:
  /tickets:
    post:
      operationId: create
      responses: {{"201": {{description: ok, content: {{application/json: {{schema: {write}}}}}}}}}
  /tickets/{{ticket_id}}:
    parameters: [{{name: ticket_id, in: path, required: true, schema: {{type: integer}}}}]
    get:
      operationId: read
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{type: object, properties: {{ticket: {{$ref: "#/components/schemas/Ticket"}}}}}}}}}}}}}}
    put:
      operationId: update
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {write}}}}}}}}}
  /tickets/{{ticket_id}}/solve:
    parameters: [{{name: ticket_id, in: path, required: true, schema: {{type: integer}}}}]
    post:
      operationId: solve
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{type: object, additionalProperties: false, properties: {{ticket: {{$ref: "#/components/schemas/Ticket"}}}}}}}}}}}}}}
components:
  schemas:
    Ticket: {{type: object, properties: {{id: {{type: integer}}, subject: {{type: string}}}}}}
    Audit: {{type: object, properties: {{id: {{type: integer}}, ticket_id: {{type: integer}}}}}}
"##
    );
    compile(contract.as_bytes()).unwrap()
}

#[test]
fn a_one_record_envelope_is_seen_through() {
    let in_ticket = Some(mb_mcm::RecordAt::Member("ticket".into()));
    let with_audit = r##"{type: object, properties: {ticket: {$ref: "#/components/schemas/Ticket"}, audit: {$ref: "#/components/schemas/Audit"}}}"##;
    let bundle = enveloped(with_audit);
    assert_eq!(
        op(&bundle, "read").identity,
        Some(Identity {
            path_param: Some("ticket_id".into()),
            field: "id".into(),
            input_field: None,
            path_field: None,
            record: in_ticket.clone(),
        })
    );
    for id in ["create", "update"] {
        let identity = op(&bundle, id).identity.as_ref();
        assert_eq!(identity.map(|i| i.field.as_str()), Some("id"), "{id}");
        assert_eq!(identity.and_then(|i| i.record.clone()), in_ticket, "{id}");
    }
    // An action answering the record in its envelope updates it.
    let solve = op(&bundle, "solve");
    assert_eq!(solve.kind, OperationKind::Command(Effect::Update));
    assert_eq!(solve.resource_type.as_deref(), Some("tickets"));
    assert_eq!(
        solve.identity.as_ref().and_then(|i| i.record.clone()),
        in_ticket
    );

    // Two members of the record's schema: nothing says which it is.
    let twice = r##"{type: object, properties: {ticket: {$ref: "#/components/schemas/Ticket"}, merged: {$ref: "#/components/schemas/Ticket"}}}"##;
    let bundle = enveloped(twice);
    assert_eq!(op(&bundle, "create").identity, None);
    assert_eq!(op(&bundle, "update").identity, None);
    assert!(op(&bundle, "read").identity.is_some());
}

/// An envelope with another member beside a record no read answers is not opened.
#[test]
fn an_envelope_beside_another_member_needs_the_types_schema() {
    let contract = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /users:
    post:
      operationId: create
      responses: {"201": {description: ok, content: {application/json: {schema: {type: object, properties: {data: {$ref: "#/components/schemas/User"}, links: {$ref: "#/components/schemas/Links"}}}}}}}
components:
  schemas:
    User: {type: object, properties: {id: {type: string}}}
    Links: {type: object, properties: {self: {type: string}}}
"##;
    let bundle = compile(contract.as_bytes()).unwrap();
    assert_eq!(op(&bundle, "create").identity, None);
}

/// A query route under another resource, and a search beside a route on its type.
fn lists() -> Bundle {
    let contract = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /pages/{page_id}:
    parameters: [{name: page_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getPage
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Page"}}}}}
  /sources/{source_id}:
    parameters: [{name: source_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getSource
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Source"}}}}}
  /sources/{source_id}/query:
    parameters: [{name: source_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: query
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {results: {type: array, items: {$ref: "#/components/schemas/Page"}}}}}}}}
  /sources/{source_id}/summaries:
    parameters: [{name: source_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: summaries
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/PageSummary"}}}}}}
  /search/jql:
    get:
      operationId: searchGet
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Found"}}}}}
    post:
      operationId: searchPost
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Found"}}}}}
  /mixed:
    get:
      operationId: mixed
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {pages: {type: array, items: {$ref: "#/components/schemas/Page"}}, sources: {type: array, items: {$ref: "#/components/schemas/Source"}}}}}}}}
components:
  schemas:
    Page: {type: object, properties: {id: {type: string}, title: {type: string}}}
    PageSummary:
      allOf:
        - $ref: "#/components/schemas/Page"
        - {type: object, properties: {score: {type: number}}}
    Source: {type: object, properties: {id: {type: string}, name: {type: string}}}
    Found: {type: object, properties: {issues: {type: array, items: {$ref: "#/components/schemas/Page"}}}}
"##;
    compile(contract.as_bytes()).unwrap()
}

#[test]
fn a_list_is_typed_by_its_items_when_they_are_exactly_a_types_record() {
    let bundle = lists();
    let type_of = |id: &str| op(&bundle, id).resource_type.clone();
    assert_eq!(type_of("query").as_deref(), Some("pages"));
    assert_eq!(type_of("searchGet").as_deref(), Some("pages"));
    assert_eq!(op(&bundle, "searchGet").kind, OperationKind::Query);
    // A write is not a list, whatever it answers.
    assert_eq!(type_of("searchPost").as_deref(), Some("jql"));
    // A longer form is not exactly the record, and two types are not one.
    assert_eq!(type_of("summaries").as_deref(), Some("summaries"));
    assert_eq!(type_of("mixed").as_deref(), Some("mixed"));
    assert_eq!(type_of("getSource").as_deref(), Some("sources"));
}

/// An item with an `id` beside the member the path names still goes by that member,
/// on the `GET` and the `DELETE` alike.
#[test]
fn an_items_id_does_not_take_the_place_of_the_member_the_path_names() {
    let contract = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /bounces/{email}:
    parameters: [{name: email, in: path, required: true, schema: {type: string}}]
    get:
      operationId: read
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/B"}}}}}}
    delete:
      operationId: remove
      responses: {"204": {description: gone}}
components:
  schemas:
    B: {type: object, required: [email], properties: {id: {type: integer}, email: {type: string}}}
"##;
    let bundle = compile(contract.as_bytes()).unwrap();
    let read = op(&bundle, "read").identity.clone().unwrap();
    assert_eq!((read.field.as_str(), read.path_field), ("email", None));
    let remove = op(&bundle, "remove").identity.clone().unwrap();
    assert_eq!((remove.field.as_str(), remove.path_field), ("email", None));
}

/// Asana's and GitLab's shapes: records keyed by `gid` with no `id`, in a `{data}`
/// envelope, and issues read by `iid` beside their `id`.
const KEYS_FROM_PARAMS: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /tasks:
    post:
      operationId: createTask
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/TaskData"}}}}}
  /tasks/{task_gid}:
    parameters: [{name: task_gid, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTask
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/TaskData"}}}}}
    put:
      operationId: updateTask
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/TaskData"}}}}}
  /projects/{id}/issues:
    parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
    post:
      operationId: createIssue
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}}
  /projects/{id}/issues/{issue_iid}:
    parameters:
      - {name: id, in: path, required: true, schema: {type: integer}}
      - {name: issue_iid, in: path, required: true, schema: {type: integer}}
    get:
      operationId: getIssue
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}}
  /boards/{board_kind}:
    parameters: [{name: board_kind, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBoard
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Board"}}}}}
components:
  schemas:
    TaskData: {type: object, properties: {data: {$ref: "#/components/schemas/Task"}}}
    Task:
      type: object
      properties:
        gid: {type: string}
        name: {type: string}
        project: {type: object, properties: {gid: {type: string}}}
    Issue: {type: object, properties: {id: {type: integer}, iid: {type: integer}, title: {type: string}}}
    Board: {type: object, properties: {name: {type: string}, kind: {type: object, properties: {label: {type: string}}}}}
"##;

#[test]
fn a_path_parameter_names_its_key_by_its_trailing_words() {
    let bundle = compile(KEYS_FROM_PARAMS.as_bytes()).unwrap();
    let by_gid = |path_param: Option<&str>| Identity {
        path_param: path_param.map(str::to_owned),
        field: "gid".into(),
        input_field: None,
        path_field: None,
        record: Some(mb_mcm::RecordAt::Member("data".into())),
    };
    assert_eq!(
        op(&bundle, "getTask").identity,
        Some(by_gid(Some("task_gid")))
    );
    assert_eq!(
        op(&bundle, "updateTask").identity,
        Some(by_gid(Some("task_gid")))
    );
    // A create goes by the key its type's read by path uses.
    assert_eq!(op(&bundle, "createTask").identity, Some(by_gid(None)));
    assert_eq!(
        op(&bundle, "getIssue").identity,
        Some(Identity {
            path_param: Some("issue_iid".into()),
            field: "id".into(),
            input_field: None,
            path_field: Some("iid".into()),
            record: None,
        })
    );
    let create = op(&bundle, "createIssue").identity.clone().unwrap();
    assert_eq!((create.path_param, create.field.as_str()), (None, "id"));
    // An object member is no key, whatever the parameter's words say.
    assert_eq!(op(&bundle, "getBoard").identity, None);
}

/// A create of a type no route reads by path names no key in the paths: a `sid` or a
/// `uuid` is still its identity, so a list sees what it made.
#[test]
fn a_create_with_no_read_by_path_still_goes_by_a_sid() {
    let contract = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /calls:
    post:
      operationId: createCall
      responses: {"201": {description: ok, content: {application/json: {schema: {type: object, properties: {sid: {type: string}, to: {type: string}}}}}}}
"##;
    let bundle = compile(contract.as_bytes()).unwrap();
    let identity = op(&bundle, "createCall").identity.clone().unwrap();
    assert_eq!(
        (identity.path_param, identity.field.as_str()),
        (None, "sid")
    );
}

/// Freshdesk's and Trello's shapes: notes and replies created under a ticket are its
/// conversations, and a comment created under a card is one of its actions. A subtask
/// created under a task is a task, as the read by path serves it.
const TYPED_BY_RECORD: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /tickets/{id}/notes:
    parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
    post:
      operationId: addNote
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Conversation"}}}}}
  /tickets/{id}/reply:
    parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
    post:
      operationId: reply
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Conversation"}}}}}
  /tickets/{id}/conversations:
    parameters: [{name: id, in: path, required: true, schema: {type: integer}}]
    get:
      operationId: listConversations
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Conversation"}}}}}}
  /cards/{id}/actions:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getActions
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Action"}}}}}}
  /cards/{id}/actions/comments:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: addComment
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Action"}}}}}
  /tasks/{task_id}:
    parameters: [{name: task_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getTask
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}}
  /tasks/{task_id}/subtasks:
    parameters: [{name: task_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getSubtasks
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/TaskCompact"}}}}}}
    post:
      operationId: createSubtask
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Task"}}}}}
  /posts/{id}/likes:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: like
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Event"}}}}}
  /feed:
    get:
      operationId: feed
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Event"}}}}}}
  /audit:
    get:
      operationId: audit
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Event"}}}}}}
components:
  schemas:
    Conversation: {type: object, properties: {id: {type: integer}, body: {type: string}, ticket_id: {type: integer}}}
    Action: {type: object, properties: {id: {type: string}, type: {type: string}, data: {type: object}}}
    Task: {type: object, properties: {id: {type: string}, name: {type: string}, notes: {type: string}}}
    TaskCompact: {type: object, properties: {id: {type: string}, name: {type: string}}}
    Event: {type: object, properties: {id: {type: string}, kind: {type: string}}}
"##;

#[test]
fn a_created_record_takes_the_type_that_reads_its_schema() {
    let bundle = compile(TYPED_BY_RECORD.as_bytes()).unwrap();
    let type_of = |id: &str| op(&bundle, id).resource_type.clone();
    for id in ["addNote", "reply", "listConversations"] {
        assert_eq!(type_of(id).as_deref(), Some("conversations"), "{id}");
    }
    for id in ["addComment", "getActions"] {
        assert_eq!(type_of(id).as_deref(), Some("actions"), "{id}");
    }
    // The read by path serves exactly what the create answers.
    assert_eq!(type_of("createSubtask").as_deref(), Some("tasks"));
    assert_eq!(type_of("getSubtasks").as_deref(), Some("subtasks"));
    // Lists of two other types are not one type.
    assert_eq!(type_of("like").as_deref(), Some("likes"));
    let note = op(&bundle, "addNote");
    assert_eq!(note.kind, OperationKind::Command(Effect::Create));
    let identity = note.identity.clone().unwrap();
    assert_eq!((identity.path_param, identity.field.as_str()), (None, "id"));
}

#[test]
fn a_version_segment_is_never_a_type() {
    let contract = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /3.0/search-members:
    get:
      operationId: searchMembers
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {total: {type: integer}}}}}}}
  /v2/query:
    post:
      operationId: query
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {total: {type: integer}}}}}}}
  /2024-01-01/accounts/get:
    post:
      operationId: getAccounts
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {total: {type: integer}}}}}}}
  /1/search:
    get:
      operationId: search
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {total: {type: integer}}}}}}}
  /users/v1beta2:
    get:
      operationId: users
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {total: {type: integer}}}}}}}
"##;
    let bundle = compile(contract.as_bytes()).unwrap();
    let type_of = |id: &str| op(&bundle, id).resource_type.clone();
    assert_eq!(type_of("searchMembers").as_deref(), Some("search-members"));
    assert_eq!(type_of("query").as_deref(), Some("query"));
    assert_eq!(type_of("getAccounts").as_deref(), Some("accounts"));
    assert_eq!(type_of("search").as_deref(), Some("search"));
    assert_eq!(type_of("users").as_deref(), Some("users"));
}
