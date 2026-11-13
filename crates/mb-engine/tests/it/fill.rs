//! [`Engine::fill`]: the members of a stored record that nothing else decides.

use mb_engine::{FillContext, OperationCall};
use mb_import::compile;
use mb_llm::{LlmError, ResponseFormat};
use serde_json::{Map, Value, json};

use crate::support::scripted;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /intents:
    post:
      operationId: createIntent
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Intent"}
components:
  schemas:
    Intent:
      type: object
      required: [id, status, description, next_action, latest_charge]
      properties:
        id: {type: string}
        status: {type: string}
        description: {type: string, nullable: true}
        next_action:
          nullable: true
          allOf: [{$ref: "#/components/schemas/Action"}]
        latest_charge:
          nullable: true
          anyOf:
            - {type: string}
            - {$ref: "#/components/schemas/Charge"}
        shipping:
          nullable: true
          allOf: [{$ref: "#/components/schemas/Action"}]
        discounts: {type: array, nullable: true, items: {type: string}}
        metadata: {type: object, nullable: true}
        receipt_email: {type: string, nullable: true}
        branding:
          allOf: [{$ref: "#/components/schemas/Action"}]
        excluded: {type: array, items: {$ref: "#/components/schemas/Action"}}
        method_types: {type: array, items: {type: string}}
        items:
          type: object
          properties:
            object: {type: string}
            data: {type: array, items: {$ref: "#/components/schemas/Action"}}
    Action:
      type: object
      required: [type]
      properties:
        type: {type: string}
    Charge:
      type: object
      properties:
        id: {type: string}
"##;

/// A nullable reference to another record that nothing set is null and not asked
/// of the model, and so are optional nullable objects and lists; a required nullable
/// object, the scalars (nullable or not) and the other members are asked, and the
/// model's null for the object is kept (no faker object a client would act on).
#[tokio::test]
async fn a_nullable_reference_or_optional_object_nothing_set_is_null_and_not_asked() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![Ok(
        json!({"status": "requires_payment_method", "description": "Order 42", "next_action": null, "receipt_email": null})
            .to_string(),
    )]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: Some(json!({})),
    };
    let known: Map<String, Value> = json!({"id": "pi_1"}).as_object().cloned().unwrap();
    let missing: Vec<String> = [
        "status",
        "description",
        "next_action",
        "latest_charge",
        "shipping",
        "discounts",
        "metadata",
        "receipt_email",
    ]
    .map(String::from)
    .to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &known,
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &[],
            },
        )
        .await;
    assert_eq!(filled.members["status"], "requires_payment_method");
    assert_eq!(filled.members["description"], "Order 42");
    assert_eq!(filled.members["latest_charge"], Value::Null);
    assert_eq!(filled.members["next_action"], Value::Null);
    for optional in ["shipping", "discounts", "metadata"] {
        assert_eq!(filled.members[optional], Value::Null, "{optional}");
    }
    assert_eq!(filled.members["receipt_email"], Value::Null);
    let sent = provider.requests();
    let ResponseFormat::JsonSchema { schema, .. } = &sent[0].response_format else {
        panic!("no schema sent");
    };
    let mut asked: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    asked.sort();
    assert_eq!(
        asked,
        vec!["description", "next_action", "receipt_email", "status"]
    );
}

/// With only nullable references and optional nullable objects missing, no model
/// call is made.
#[tokio::test]
async fn only_nullable_references_and_optional_objects_ask_no_model() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: None,
    };
    let missing: Vec<String> = ["latest_charge", "shipping", "discounts"]
        .map(String::from)
        .to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &[],
            },
        )
        .await;
    for member in ["latest_charge", "shipping", "discounts"] {
        assert_eq!(filled.members[member], Value::Null, "{member}");
    }
    assert!(provider.requests().is_empty());
}

/// An optional object or list of objects that may not be null is left out: not
/// asked, and no faker value either. A list of scalars is still asked.
#[tokio::test]
async fn an_optional_object_that_may_not_be_null_is_left_out() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![Ok(
        json!({"status": "open", "method_types": ["card"]}).to_string(),
    )]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: None,
    };
    let missing: Vec<String> = ["status", "branding", "excluded", "method_types"]
        .map(String::from)
        .to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &[],
            },
        )
        .await;
    assert_eq!(filled.members["status"], "open");
    assert!(
        !filled.members.contains_key("branding"),
        "{:?}",
        filled.members
    );
    assert!(
        !filled.members.contains_key("excluded"),
        "{:?}",
        filled.members
    );
    let sent = provider.requests();
    let ResponseFormat::JsonSchema { schema, .. } = &sent[0].response_format else {
        panic!("no schema sent");
    };
    assert_eq!(filled.members["method_types"], json!(["card"]));
    let mut asked: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    asked.sort();
    assert_eq!(asked, vec!["method_types", "status"]);
}

/// An optional object that may not be null is asked of the model when the request
/// sent a member of its name (the model carries the sent items over); one the
/// request did not send stays left out.
#[tokio::test]
async fn an_optional_object_the_request_sent_is_asked() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let items = json!({"object": "list", "data": [{"type": "card"}]});
    let (provider, engine) = scripted(vec![Ok(
        json!({"status": "open", "items": items}).to_string()
    )]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: Some(json!({"items": [{"type": "card"}]})),
    };
    let missing: Vec<String> = ["status", "items", "branding"].map(String::from).to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &[],
            },
        )
        .await;
    assert_eq!(filled.members["items"], items);
    assert!(
        !filled.members.contains_key("branding"),
        "{:?}",
        filled.members
    );
    let sent = provider.requests();
    let ResponseFormat::JsonSchema { schema, .. } = &sent[0].response_format else {
        panic!("no schema sent");
    };
    let mut asked: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    asked.sort();
    assert_eq!(asked, vec!["items", "status"]);
}

/// When the model call fails, an optional object the request sent is absent, not
/// faker items standing for what the client sent.
#[tokio::test]
async fn a_failed_call_leaves_a_sent_optional_object_absent() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (_provider, engine) = scripted(vec![Err(LlmError::Transport("down".into()))]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: Some(json!({"items": [{"type": "card"}]})),
    };
    let missing: Vec<String> = ["status", "items"].map(String::from).to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &[],
            },
        )
        .await;
    assert!(
        filled.members.contains_key("status"),
        "{:?}",
        filled.members
    );
    assert!(
        !filled.members.contains_key("items"),
        "{:?}",
        filled.members
    );
}

/// When the model call fails, a required object that may be null is null, not a
/// faker object a client would act on; a scalar is still the faker's.
#[tokio::test]
async fn a_failed_call_leaves_a_nullable_object_null() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (_provider, engine) = scripted(vec![Err(LlmError::Transport("down".into()))]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: None,
    };
    let missing: Vec<String> = ["status", "next_action"].map(String::from).to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &[],
            },
        )
        .await;
    assert_eq!(filled.members["next_action"], Value::Null);
    assert!(filled.members["status"].is_string(), "{:?}", filled.members);
}

const STRUCTURED: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /messages:
    post:
      operationId: createMessage
      responses:
        "201":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Message"}
components:
  schemas:
    Message:
      type: object
      required: [sid, to, status, segments, sent, created_at, due_date, owner_id, html_url, price, body, meta, replies]
      properties:
        sid: {type: string, pattern: "^SM[0-9a-f]{8}$"}
        to: {type: string, pattern: "^\\+1[0-9]{10}$"}
        status: {type: string, enum: [queued, sent]}
        segments: {type: integer, minimum: 1}
        sent: {type: boolean}
        created_at: {type: string}
        due_date: {type: string}
        owner_id: {type: string}
        ownerId: {type: string}
        html_url: {type: string}
        contact: {type: string, format: email}
        price: {type: number, nullable: true}
        body: {type: string}
        meta:
          type: object
          required: [kind, labels]
          properties:
            kind: {type: string, enum: [a, b]}
            labels: {type: array, items: {type: string}}
        replies: {type: array, items: {type: object, properties: {text: {type: string}}}}
"##;

/// What holds free text: a plain string not named as an id, a link or a moment, and
/// any object or list holding one; ids, enums, numbers, booleans, formatted and
/// patterned strings do not.
#[test]
fn free_text_is_a_plain_string_not_named_for_an_id_a_link_or_a_moment() {
    let bundle = compile(STRUCTURED.as_bytes()).unwrap();
    let op = bundle.operation("createMessage").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let schema = bundle.resolve(&schema).clone();
    let props = schema["properties"].as_object().unwrap();
    let mut free: Vec<&str> = props
        .iter()
        .filter(|(name, member)| mb_engine::free_text(&bundle, name, member))
        .map(|(name, _)| name.as_str())
        .collect();
    free.sort_unstable();
    assert_eq!(free, vec!["body", "meta", "replies"]);
}

/// With only structured members missing (and free text the request sent under
/// another spelling) no model call is made: the faker writes them, a request value of
/// the same name (case and separators aside) echoed where it fits, and a plain string
/// named for a moment as a date-time or date.
#[tokio::test]
async fn structured_members_are_filled_with_no_model_call() {
    let bundle = compile(STRUCTURED.as_bytes()).unwrap();
    let op = bundle.operation("createMessage").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: Some(json!({"To": "+14155550100", "Body": "down"})),
    };
    let missing: Vec<String> = [
        "sid",
        "to",
        "status",
        "segments",
        "sent",
        "created_at",
        "due_date",
        "owner_id",
        "ownerId",
        "html_url",
        "contact",
        "price",
        "body",
        "replies",
    ]
    .map(String::from)
    .to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &[],
            },
        )
        .await;
    assert!(provider.requests().is_empty());
    let m = &filled.members;
    assert_eq!(m["to"], "+14155550100", "{m:?}");
    // Free text the request sent under another spelling of the name is echoed.
    assert_eq!(m["body"], "down", "{m:?}");
    // A list of objects the request sent nothing for holds none.
    assert_eq!(m["replies"], json!([]), "{m:?}");
    assert!(m["sid"].as_str().unwrap().starts_with("SM"), "{m:?}");
    assert!(["queued", "sent"].contains(&m["status"].as_str().unwrap()));
    assert!(m["segments"].as_i64().unwrap() >= 1);
    assert!(m["sent"].is_boolean());
    let created = m["created_at"].as_str().unwrap();
    assert!(
        chrono::DateTime::parse_from_rfc3339(created).is_ok(),
        "{created}"
    );
    let due = m["due_date"].as_str().unwrap();
    assert!(
        chrono::NaiveDate::parse_from_str(due, "%Y-%m-%d").is_ok(),
        "{due}"
    );
    assert!(m["contact"].as_str().unwrap().contains('@'), "{m:?}");
    for member in ["owner_id", "ownerId", "html_url"] {
        assert!(m[member].is_string(), "{member}: {m:?}");
    }
}

/// With free text missing beside structured members, the model is asked for the
/// free text alone, an object's for its free-text members alone (the faker writes
/// the rest of it).
#[tokio::test]
async fn only_free_text_is_asked_of_the_model() {
    let bundle = compile(STRUCTURED.as_bytes()).unwrap();
    let op = bundle.operation("createMessage").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![Ok(json!({
        "body": "Your check is down.",
        "meta": {"labels": ["ops"]},
        "status": "sent"
    })
    .to_string())]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: None,
    };
    let missing: Vec<String> = ["body", "meta", "status", "created_at"]
        .map(String::from)
        .to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &[],
            },
        )
        .await;
    assert_eq!(filled.members["body"], "Your check is down.");
    assert_eq!(filled.members["meta"]["labels"], json!(["ops"]));
    let sent = provider.requests();
    let ResponseFormat::JsonSchema { schema, .. } = &sent[0].response_format else {
        panic!("no schema sent");
    };
    let mut asked: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    asked.sort();
    // An enum the persona says nothing of is no reason to ask: its first value.
    assert_eq!(asked, vec!["body", "meta"]);
    assert_eq!(filled.members["status"], "queued");
    let meta: Vec<&String> = schema["properties"]["meta"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(meta, vec!["labels"], "{schema}");
    assert!(
        ["a", "b"].contains(&filled.members["meta"]["kind"].as_str().unwrap()),
        "{:?}",
        filled.members
    );
    assert!(filled.members["status"].is_string());
}

const CARDS: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /cards:
    post:
      operationId: createCard
      responses:
        "201":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Card"}
components:
  schemas:
    Card:
      type: object
      required: [id, detail, lines, deleted, closed_at, state]
      properties:
        id: {type: string}
        deleted: {type: boolean}
        closed_at: {type: string, format: date-time, nullable: true}
        state: {type: string, enum: [open, closed]}
        lines: {type: array, items: {type: object, properties: {text: {type: string}}}}
        detail:
          nullable: true
          allOf: [{$ref: "#/components/schemas/Detail"}]
    Detail:
      type: object
      required: [note, kind, pay]
      properties:
        note: {type: string}
        kind: {type: string, enum: [a, b]}
        pay:
          oneOf:
            - {type: object, required: [card], properties: {card: {type: string}}, additionalProperties: false}
            - {type: object, required: [bank], properties: {bank: {type: string}}, additionalProperties: false}
"##;

fn card_call(op: &mb_mcm::Operation, body: Option<Value>) -> OperationCall {
    OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body,
    }
}

/// An object is asked for its free text alone (through a nullable `allOf` of a
/// `$ref`, drawn as an object even where the faker would draw `null`); a union it
/// was asked whole stays as written, and the faker writes the rest (the enum `kind`). A fresh record's
/// boolean is false, its nullable scalar null and, with a call made, its enum the
/// model's.
#[tokio::test]
async fn an_object_is_asked_for_its_free_text_and_keeps_nothing_unasked() {
    let bundle = compile(CARDS.as_bytes()).unwrap();
    let op = bundle.operation("createCard").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = scripted(vec![Ok(json!({
        "detail": {"note": "Paid at the desk.", "pay": {"card": "visa"}},
        "state": "open"
    })
    .to_string())]);
    let call = card_call(op, None);
    let missing: Vec<String> = ["detail", "lines", "deleted", "closed_at", "state"]
        .map(String::from)
        .to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &[],
            },
        )
        .await;
    let m = &filled.members;
    assert_eq!(m["detail"]["note"], "Paid at the desk.", "{m:?}");
    assert!(
        ["a", "b"].contains(&m["detail"]["kind"].as_str().unwrap()),
        "{m:?}"
    );
    assert_eq!(m["detail"]["pay"], json!({"card": "visa"}), "{m:?}");
    assert_eq!(m["lines"], json!([]), "{m:?}");
    assert_eq!(m["deleted"], false, "{m:?}");
    assert_eq!(m["closed_at"], Value::Null, "{m:?}");
    assert_eq!(m["state"], "open", "{m:?}");
    let sent = provider.requests();
    let ResponseFormat::JsonSchema { schema, .. } = &sent[0].response_format else {
        panic!("no schema sent");
    };
    let text = schema.to_string();
    assert!(!text.contains("\"kind\""), "{text}");
    assert!(
        !text.contains("\"lines\"") && !text.contains("\"deleted\""),
        "{text}"
    );
}

/// A list the request sent inside a wrapper (the record the executor read) is asked of
/// the model, which carries the sent items over, not emptied.
#[tokio::test]
async fn a_list_sent_inside_a_wrapper_is_asked() {
    let bundle = compile(CARDS.as_bytes()).unwrap();
    let op = bundle.operation("createCard").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let lines = json!([{"text": "one"}]);
    let (provider, engine) = scripted(vec![Ok(json!({"lines": lines}).to_string())]);
    let call = card_call(op, Some(json!({"card": {"lines": [{"text": "one"}]}})));
    let record = json!({"lines": [{"text": "one"}]});
    let missing: Vec<String> = vec!["lines".into()];
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: record.as_object(),
                caller: &[],
                states: &[],
            },
        )
        .await;
    assert_eq!(filled.members["lines"], lines);
    assert_eq!(provider.requests().len(), 1);
}

const TICKETS: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /repos/{owner}/{repo}/tickets:
    post:
      operationId: createTicket
      parameters:
        - {name: owner, in: path, required: true, schema: {type: string}}
        - {name: repo, in: path, required: true, schema: {type: string}}
      responses:
        "201":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Ticket"}
components:
  schemas:
    Ticket:
      type: object
      required: [author, repository, direction, kind, tier]
      properties:
        author:
          type: object
          required: [login, id]
          properties: {login: {type: string}, id: {type: integer}}
        repository:
          type: object
          required: [owner, name, id]
          properties: {owner: {type: string}, name: {type: string, pattern: "^[a-z]{12}$"}, id: {type: integer}}
        direction: {type: string, enum: [inbound, outbound-api, outbound-call]}
        kind: {type: string, enum: [bug, task], default: task}
        tier: {type: string, enum: [gold, silver]}
"##;

fn ticket_engine(
    persona: Option<&str>,
    script: Vec<Result<String, LlmError>>,
) -> (std::sync::Arc<mb_llm::ScriptedProvider>, mb_engine::Engine) {
    let provider = std::sync::Arc::new(mb_llm::ScriptedProvider::new(script));
    let dyn_provider: std::sync::Arc<dyn mb_llm::Provider> = provider.clone();
    let config = mb_engine::EngineConfig {
        persona: persona.map(str::to_string),
        ..mb_engine::EngineConfig::default()
    };
    (provider, mb_engine::Engine::new(Some(dyn_provider), config))
}

fn ticket_call(op: &mb_mcm::Operation) -> OperationCall {
    let mut path_params = Map::new();
    path_params.insert("owner".into(), json!("acme"));
    path_params.insert("repo".into(), json!("backups"));
    OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params,
        query: Map::new(),
        headers: Map::new(),
        body: Some(json!({"title": "down"})),
    }
}

/// A nested member the call's context gives is that value, with no model call: a
/// path parameter by its name (`repository.owner`), a member of who the caller is by
/// its name (`author.login`), though both are plain strings. An enum the persona says
/// nothing of is its `default`, else its first value.
#[tokio::test]
async fn nested_members_come_from_the_path_and_the_caller() {
    let bundle = compile(TICKETS.as_bytes()).unwrap();
    let op = bundle.operation("createTicket").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (provider, engine) = ticket_engine(None, vec![]);
    let call = ticket_call(op);
    let author = mb_engine::consistency::property_schema(&bundle, &schema, "author").unwrap();
    let members = json!({"login": "octocat", "name": "theoctocatxy", "id": 7});
    let caller = [mb_engine::CallerAnswer {
        schema: author,
        members: members.as_object().unwrap().clone(),
    }];
    let missing: Vec<String> = ["author", "repository", "direction", "kind", "tier"]
        .map(String::from)
        .to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &caller,
                states: &[],
            },
        )
        .await;
    assert!(provider.requests().is_empty(), "{:?}", provider.requests());
    let m = &filled.members;
    assert_eq!(m["author"], json!({"login": "octocat", "id": 7}), "{m:?}");
    assert_eq!(m["repository"]["owner"], "acme", "{m:?}");
    // Only an object of the caller's schema takes the caller's members.
    assert_ne!(m["repository"]["name"], "theoctocatxy", "{m:?}");
    assert_ne!(m["repository"]["id"], 7, "{m:?}");
    assert_eq!(m["kind"], "task", "the default: {m:?}");
    assert_eq!(m["direction"], "inbound", "the first value: {m:?}");
    assert_eq!(m["tier"], "gold", "the first value: {m:?}");
}

/// An enum the persona names one value of is that value; one whose member the persona
/// names, but no one value of, is the model's.
#[tokio::test]
async fn an_enum_the_persona_speaks_of_is_resolved_or_asked() {
    let bundle = compile(TICKETS.as_bytes()).unwrap();
    let op = bundle.operation("createTicket").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let persona =
        "Ticket API. New tickets are \"outbound-api\" and get a tier the support plan decides.";
    let (provider, engine) = ticket_engine(
        Some(persona),
        vec![Ok(json!({"tier": "silver"}).to_string())],
    );
    let call = ticket_call(op);
    let missing: Vec<String> = ["direction", "kind", "tier"].map(String::from).to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &[],
            },
        )
        .await;
    let m = &filled.members;
    assert_eq!(m["direction"], "outbound-api", "{m:?}");
    assert_eq!(m["kind"], "task", "{m:?}");
    assert_eq!(m["tier"], "silver", "{m:?}");
    let sent = provider.requests();
    assert_eq!(sent.len(), 1);
    let ResponseFormat::JsonSchema { schema, .. } = &sent[0].response_format else {
        panic!("no schema sent");
    };
    let asked: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    assert_eq!(asked, vec!["tier"]);
}

const STATES: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /issues:
    post:
      operationId: createIssue
      responses:
        "201":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Issue"}
components:
  schemas:
    Issue:
      type: object
      required: [state, phase, stage, kind]
      properties:
        state: {type: string, enum: [CLOSED, OPEN]}
        phase: {type: string, enum: [canceled, requires_payment_method, succeeded]}
        stage: {type: string, enum: [done, open], default: done}
        kind: {type: string, enum: [closed, open]}
"##;

/// A state nothing gave a start, when no model writes it, starts at the value that
/// reads as a start (`OPEN` though `CLOSED` is listed first, a `requires_*` value), or
/// at the contract's `default`; an enum that is no state keeps its first value.
#[tokio::test]
async fn a_state_no_model_wrote_starts_at_the_value_reading_as_a_start() {
    let bundle = compile(STATES.as_bytes()).unwrap();
    let op = bundle.operation("createIssue").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let (_provider, engine) = scripted(vec![Err(LlmError::Transport("down".into()))]);
    let call = OperationCall {
        credentials: Default::default(),
        operation_id: op.id.clone(),
        path_params: Map::new(),
        query: Map::new(),
        headers: Map::new(),
        body: None,
    };
    let missing: Vec<String> = ["state", "phase", "stage", "kind"]
        .map(String::from)
        .to_vec();
    let states: Vec<String> = ["state", "phase", "stage"].map(String::from).to_vec();
    let filled = engine
        .fill(
            &bundle,
            &schema,
            &Map::new(),
            &missing,
            FillContext {
                op,
                call: &call,
                sent: None,
                caller: &[],
                states: &states,
            },
        )
        .await;
    assert_eq!(filled.members["state"], "OPEN", "{:?}", filled.members);
    assert_eq!(filled.members["phase"], "requires_payment_method");
    assert_eq!(filled.members["stage"], "done");
    assert_eq!(filled.members["kind"], "closed");
}

const REQUIRING: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /intents:
    post:
      operationId: createIntent
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                amount: {type: integer}
                payment_method: {type: string}
                confirm: {type: boolean}
                capture_method: {type: string}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Intent"}
  /wrapped:
    post:
      operationId: createWrapped
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                input: {$ref: "#/components/schemas/IntentInput"}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Intent"}
components:
  schemas:
    IntentInput:
      allOf:
        - type: object
          properties:
            payment_method: {type: string}
        - type: object
          properties:
            confirm: {type: boolean}
    Intent:
      type: object
      required: [status, review]
      properties:
        status:
          type: string
          enum: [canceled, processing, requires_action, requires_capture, requires_confirmation, requires_payment_method, succeeded]
        review: {type: string, enum: [merged, opened, closed]}
"##;

/// Of several `requires_*` starts, a state no model wrote starts at the one naming a
/// request member the create did not send, a missing value before a missing flag;
/// `opened` reads as a start.
#[tokio::test]
async fn a_requiring_start_names_what_the_create_did_not_send() {
    let bundle = compile(REQUIRING.as_bytes()).unwrap();
    let op = bundle.operation("createIntent").unwrap();
    let schema = op.success_output().unwrap().schema.clone().unwrap();
    let states: Vec<String> = ["status", "review"].map(String::from).to_vec();
    let missing = states.clone();
    let mut started = Vec::new();
    for body in [
        json!({"amount": 5}),
        json!({"amount": 5, "payment_method": "pm_1"}),
        // A flag sent `false` is not sent.
        json!({"amount": 5, "payment_method": "pm_1", "confirm": false}),
    ] {
        let (_provider, engine) = scripted(vec![Err(LlmError::Transport("down".into()))]);
        let call = OperationCall {
            credentials: Default::default(),
            operation_id: op.id.clone(),
            path_params: Map::new(),
            query: Map::new(),
            headers: Map::new(),
            body: Some(body),
        };
        let filled = engine
            .fill(
                &bundle,
                &schema,
                &Map::new(),
                &missing,
                FillContext {
                    op,
                    call: &call,
                    sent: None,
                    caller: &[],
                    states: &states,
                },
            )
            .await;
        assert_eq!(filled.members["review"], "opened", "{:?}", filled.members);
        started.push(filled.members["status"].clone());
    }
    assert_eq!(
        started,
        [
            json!("requires_payment_method"),
            json!("requires_confirmation"),
            json!("requires_confirmation")
        ]
    );

    // A request wrapping the record (a GraphQL `input`), its schema an `allOf`.
    let op = bundle.operation("createWrapped").unwrap();
    let mut started = Vec::new();
    for input in [json!({}), json!({"payment_method": "pm_1"})] {
        let (_provider, engine) = scripted(vec![Err(LlmError::Transport("down".into()))]);
        let call = OperationCall {
            credentials: Default::default(),
            operation_id: op.id.clone(),
            path_params: Map::new(),
            query: Map::new(),
            headers: Map::new(),
            body: Some(json!({"input": input})),
        };
        let sent = input.as_object().cloned().unwrap_or_default();
        let filled = engine
            .fill(
                &bundle,
                &schema,
                &Map::new(),
                &missing,
                FillContext {
                    op,
                    call: &call,
                    sent: Some(&sent),
                    caller: &[],
                    states: &states,
                },
            )
            .await;
        started.push(filled.members["status"].clone());
    }
    assert_eq!(
        started,
        [
            json!("requires_payment_method"),
            json!("requires_confirmation")
        ]
    );
}
