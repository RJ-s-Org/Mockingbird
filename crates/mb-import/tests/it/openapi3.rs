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

    /// A `201` whose request supplies the record makes a child of the
    /// parent's shape, under the parent's type so that its `GET` reads it.
    #[test]
    fn a_created_child_of_the_parents_shape_does_not_replace_it() {
        for (parent, tail) in [
            ("comments", "replies"),
            ("documents", "copies"),
            ("folders", "folders"),
        ] {
            let b = parent_and_child(parent, tail, "201", NEW_BODY);
            create(&b, "subject", parent);
            let identity = b.operation("subject").unwrap().identity.clone().unwrap();
            assert_eq!(identity.field, "id", "{parent}");
            assert_eq!(identity.path_param, None, "{parent}");
        }
    }

    /// PayPal's capture and authorize: a `201` answering the order itself,
    /// with no request that could be a new one. A tail's word cannot tell
    /// them from a reply, the request can.
    #[test]
    fn a_created_answer_of_the_parents_shape_with_no_content_replaces_it() {
        let optional = "requestBody: {content: {application/json: {schema: {type: object}}}}";
        for request in ["summary: none", optional] {
            let b = parent_and_child("orders", "capture", "201", request);
            let op = b.operation("subject").unwrap();
            assert_eq!(op.kind, OperationKind::Command(Effect::Update), "{request}");
            assert_eq!(op.resource_type.as_deref(), Some("orders"), "{request}");
            assert!(op.is_action(), "{request}");
        }
        let paypal = example("paypal/paypal-orders.openapi.yaml");
        for id in ["orders.capture", "orders.authorize", "orders.confirm"] {
            let op = paypal.operation(id).unwrap();
            assert_eq!(op.kind, OperationKind::Command(Effect::Update), "{id}");
            assert_eq!(op.resource_type.as_deref(), Some("orders"), "{id}");
        }
    }

    #[test]
    fn a_200_of_the_parents_shape_is_an_update_until_corrected() {
        let b = parent_and_child("comments", "replies", "200", NEW_BODY);
        let op = b.operation("subject").unwrap();
        assert_eq!(op.kind, OperationKind::Command(Effect::Update));
        assert_eq!(op.resource_type.as_deref(), Some("comments"));
        let corrected =
            format!("{NEW_BODY}\n      x-mockingbird-resource: {{action: false, type: comments}}");
        let b = parent_and_child("comments", "replies", "200", &corrected);
        create(&b, "subject", "comments");
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
            ("201", "{_id: {type: string}}", "shipment_id", None),
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

    /// A `200` needs the read to say what identifies it: a shared answer
    /// with no identity says nothing about being that record.
    #[test]
    fn a_200_read_elsewhere_with_no_identity_stays_an_action() {
        let b = shaped(
            &format!(
                "  /jobs/{{id}}/run:\n    post:\n      operationId: subject\n      {}\n  /stats/{{day}}:\n    get:\n      operationId: read\n      {}\n",
                body_of("200", "Ack"),
                body_of("200", "Ack"),
            ),
            "    Ack: {type: object, properties: {ok: {type: boolean}}}\n",
        );
        other(&b, "subject", "jobs", "id");
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

    /// With no `GET`, a `PATCH` or an answering `DELETE` says the record is
    /// there and what it looks like.
    #[test]
    fn a_patch_or_delete_is_evidence_of_a_record_when_no_get_is() {
        let task = r##"{"$ref": "#/components/schemas/Task"}"##;
        let answering = format!(
            r##"responses: {{"200": {{description: ok, content: {{application/json: {{schema: {task}}}}}}}}}"##
        );
        let b = shaped(
            &format!(
                r##"  /tasks/{{id}}:
    patch:
      operationId: patchTask
      {answering}
  /tasks/{{id}}/complete:
    post:
      operationId: complete
      {answering}
  /projects/{{id}}/tasks:
    post:
      operationId: addTask
      {answering}
  /jobs/{{id}}:
    delete:
      operationId: deleteJob
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Job"}}}}}}}}}}
  /jobs/{{id}}/stop:
    post:
      operationId: stop
      responses: {{"200": {{description: ok, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Job"}}}}}}}}}}
"##
            ),
            "    Task: {type: object, properties: {id: {type: string}, done: {type: boolean}}}\n    Job: {type: object, properties: {id: {type: string}, state: {type: string}}}\n",
        );
        for id in ["complete", "stop"] {
            assert_eq!(
                b.operation(id).unwrap().kind,
                OperationKind::Command(Effect::Update),
                "{id}"
            );
        }
        create(&b, "addTask", "tasks");
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
