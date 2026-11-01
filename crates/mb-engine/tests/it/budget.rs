//! The output budget and persona formats, through `Engine::generate`.

use std::sync::Arc;

use mb_engine::{Engine, EngineConfig, GenerationContext, OperationCall, Source};
use mb_import::compile;
use mb_llm::{Provider, ResponseFormat, ScriptedProvider};
use mb_mcm::Bundle;
use serde_json::{Map, Value, json};

/// A search envelope whose items each carry a heavy required repository.
fn search() -> Bundle {
    let urls: Vec<String> = (0..30).map(|i| format!("url_{i}")).collect();
    let props: Map<String, Value> = urls
        .iter()
        .map(|u| (u.clone(), json!({"type": "string", "format": "uri"})))
        .collect();
    let doc = json!({
        "openapi": "3.1.0",
        "info": {"title": "S", "version": "1"},
        "paths": {"/search/commits": {"get": {
            "operationId": "searchCommits",
            "parameters": [{"name": "q", "in": "query", "schema": {"type": "string"}}],
            "responses": {"200": {"description": "OK", "content": {"application/json": {"schema": {
                "type": "object",
                "required": ["total_count", "items"],
                "properties": {
                    "total_count": {"type": "integer"},
                    "items": {"type": "array", "items": {"$ref": "#/components/schemas/Item"}}
                }
            }}}}}
        }}},
        "components": {"schemas": {
            "Item": {
                "type": "object",
                "required": ["sha", "message", "repository"],
                "properties": {
                    "sha": {"type": "string"},
                    "message": {"type": "string"},
                    "repository": {"$ref": "#/components/schemas/Repo"}
                }
            },
            "Repo": {"type": "object", "required": urls, "properties": props}
        }}
    });
    compile(doc.to_string().as_bytes()).unwrap()
}

fn engine(answers: Vec<Value>, config: EngineConfig) -> (Arc<ScriptedProvider>, Engine) {
    let provider = Arc::new(ScriptedProvider::new(
        answers.into_iter().map(|a| Ok(a.to_string())).collect(),
    ));
    let dyn_provider: Arc<dyn Provider> = provider.clone();
    (provider, Engine::new(Some(dyn_provider), config))
}

fn ctx() -> GenerationContext<'static> {
    GenerationContext::default()
}

fn format_schema(provider: &ScriptedProvider) -> Value {
    match &provider.requests()[0].response_format {
        ResponseFormat::JsonSchema { schema, .. } => schema.clone(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_answer_over_budget_is_written_short_and_filled() {
    let bundle = search();
    let op = bundle.operation("searchCommits").unwrap();
    let config = EngineConfig {
        output_budget: 150,
        ..EngineConfig::default()
    };
    let answer =
        json!({"total_count": 1234, "items": [{"sha": "a1b2", "message": "Fix the build"}]});
    let (provider, engine) = engine(vec![answer], config);
    let out = engine
        .generate(&bundle, op, &OperationCall::default(), ctx())
        .await
        .unwrap();
    // The model was asked for one item without its repository...
    let written = format_schema(&provider);
    assert_eq!(written["properties"]["items"]["maxItems"], 1, "{written}");
    let item = &written["$defs"]["Item"];
    assert!(item["properties"].get("repository").is_none(), "{item}");
    // ...and the answer is its own, completed from the full schema.
    assert_eq!(out.source, Source::Llm, "{:?}", out.fallback_reason);
    let body = out.body.unwrap();
    assert_eq!(body["total_count"], 1234);
    assert_eq!(body["items"][0]["message"], "Fix the build");
    assert!(
        body["items"][0]["repository"]["url_7"].is_string(),
        "{body}"
    );
}

#[tokio::test]
async fn an_answer_within_budget_is_not_shaped() {
    let bundle = search();
    let op = bundle.operation("searchCommits").unwrap();
    let config = EngineConfig {
        output_budget: 0,
        ..EngineConfig::default()
    };
    let (provider, engine) = engine(vec![json!({"total_count": 0, "items": []})], config);
    engine
        .generate(&bundle, op, &OperationCall::default(), ctx())
        .await
        .unwrap();
    let written = format_schema(&provider);
    assert!(
        written["properties"]["items"].get("maxItems").is_none(),
        "{written}"
    );
}

#[tokio::test]
async fn persona_formats_are_kept() {
    let doc = json!({
        "openapi": "3.1.0",
        "info": {"title": "Stripe", "version": "1"},
        "paths": {"/v1/payment_intents": {"post": {
            "operationId": "PostPaymentIntents",
            "requestBody": {"content": {"application/json": {"schema": {
                "type": "object", "properties": {"amount": {"type": "integer"}}
            }}}},
            "responses": {"200": {"description": "OK", "content": {"application/json": {"schema": {
                "type": "object",
                "required": ["id", "client_secret", "amount"],
                "properties": {
                    "id": {"type": "string"},
                    "client_secret": {"type": "string"},
                    "amount": {"type": "integer"}
                }
            }}}}}
        }}}
    });
    let bundle = compile(doc.to_string().as_bytes()).unwrap();
    let op = bundle.operation("PostPaymentIntents").unwrap();
    let config = EngineConfig {
        persona: Some(
            "Ids use Stripe prefixes (pi_, cus_) followed by 24 random alphanumeric chars. \
             client_secret = \"<id>_secret_<24 random chars>\""
                .into(),
        ),
        ..EngineConfig::default()
    };
    let answer = json!({"id": "pi_3QxYzAbCdEfGhIjKlMnOpQrS", "client_secret": "pi_3QxYzAbCdEfGhIjKlMnOpQrS_secret_abc", "amount": 5999});
    let (_provider, engine) = engine(vec![answer], config);
    let call = OperationCall {
        body: Some(json!({"amount": 5999})),
        ..OperationCall::default()
    };
    let out = engine.generate(&bundle, op, &call, ctx()).await.unwrap();
    let body = out.body.unwrap();
    let secret = body["client_secret"].as_str().unwrap();
    assert!(
        secret.starts_with("pi_3QxYzAbCdEfGhIjKlMnOpQrS_secret_abc"),
        "{secret}"
    );
    assert_eq!(
        secret.len(),
        "pi_3QxYzAbCdEfGhIjKlMnOpQrS_secret_".len() + 24,
        "{secret}"
    );
    assert_eq!(body["amount"], 5999);
}

/// Spotify's search, as the blind benchmark sent it: `type=track,artist` names
/// `tracks` and `artists`, and five of each fit once the rest has gone.
#[tokio::test]
async fn a_search_keeps_the_types_it_names_at_its_limit() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/apis/spotify/spotify.openapi.yaml"
    );
    let bundle = compile(&std::fs::read(path).unwrap()).unwrap();
    let op = bundle.operation("search").unwrap();
    let call = OperationCall {
        query: serde_json::from_value(json!({"q": "love", "type": "track,artist", "limit": "5"}))
            .unwrap(),
        ..OperationCall::default()
    };
    let (provider, engine) = engine(vec![json!({})], EngineConfig::default());
    engine.generate(&bundle, op, &call, ctx()).await.unwrap();
    let written = format_schema(&provider);
    let props = written["properties"].as_object().unwrap();
    assert!(props.contains_key("tracks"), "{}", written["properties"]);
    assert!(props.contains_key("artists"), "{}", written["properties"]);
    assert!(!props.contains_key("albums"), "{}", written["properties"]);
    for def in ["PagingTrackObject", "PagingArtistObject"] {
        // A page is its `Paging` members and, in an `allOf`, its `items`.
        let parts = written["$defs"][def]["allOf"].as_array().unwrap();
        let items = parts
            .iter()
            .find_map(|part| part["properties"].get("items"))
            .unwrap();
        assert_eq!(items["type"], "array", "{def}: {items}");
        assert_eq!(items["maxItems"], 5, "{def}: {items}");
    }
}

/// A page of things heavy enough to shape, with `query` its parameters, and beside
/// the page's items what no page size counts: a list of a lighter object, and words.
fn paged_things(query: &str, items: &str) -> Bundle {
    let detail: String = (0..30)
        .map(|i| format!("f{i}: {{type: string}}, "))
        .collect();
    let contract = format!(
        r##"
openapi: 3.0.0
info: {{title: Things, version: "1"}}
paths:
  /things:
    get:
      operationId: listThings
      parameters: [{query}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [things, warnings, tags]
                properties:
                  things: {{type: array, {items} items: {{$ref: "#/components/schemas/Thing"}}}}
                  warnings: {{type: array, maxItems: 500, items: {{type: object, required: [code], properties: {{code: {{type: string}}}}}}}}
                  tags: {{type: array, items: {{type: string}}}}
components:
  schemas:
    Thing:
      type: object
      required: [id]
      properties:
        id: {{type: string}}
        detail: {{type: object, properties: {{ {detail} }}}}
"##
    );
    compile(contract.as_bytes()).unwrap()
}

/// The schema a model is sent for `query` on [`paged_things`].
async fn things_written(bundle: &Bundle, query: Value) -> Value {
    let op = bundle.operation("listThings").unwrap();
    let call = OperationCall {
        query: serde_json::from_value(query).unwrap(),
        ..OperationCall::default()
    };
    let (provider, engine) = engine(vec![json!({})], EngineConfig::default());
    engine.generate(bundle, op, &call, ctx()).await.unwrap();
    format_schema(&provider)
}

/// No API's own word for a page size is special: a name holding one of the words
/// says so, in whatever case.
#[tokio::test]
async fn a_page_size_goes_by_any_name_for_it() {
    for name in [
        "limit",
        "per_page",
        "perPage",
        "pageSize",
        "page-size",
        "first",
        "$top",
    ] {
        let bundle = paged_things(
            &format!("{{name: \"{name}\", in: query, schema: {{type: integer}}}}"),
            "",
        );
        let written = things_written(&bundle, json!({name: "40"})).await;
        assert_eq!(written["properties"]["things"]["maxItems"], 40, "{name}");
    }
}

/// A word is no page size unless it can be: a parameter declared as text, or one
/// whose name holds none of the words, says nothing of the page.
#[tokio::test]
async fn other_parameters_are_no_page_size() {
    for (name, kind) in [
        ("limit", "string"),
        ("first_name", "string"),
        ("offset", "integer"),
    ] {
        let bundle = paged_things(
            &format!("{{name: {name}, in: query, schema: {{type: {kind}}}}}"),
            "",
        );
        let written = things_written(&bundle, json!({name: "40"})).await;
        assert_ne!(written["properties"]["things"]["maxItems"], 40, "{name}");
    }
}

/// The size applies to the page's items, not to every array outside another.
#[tokio::test]
async fn a_page_size_holds_only_for_the_items_of_the_page() {
    let bundle = paged_things("{name: limit, in: query, schema: {type: integer}}", "");
    let written = things_written(&bundle, json!({"limit": "40"})).await;
    let properties = &written["properties"];
    assert_eq!(properties["things"]["maxItems"], 40, "{written}");
    assert_ne!(properties["warnings"]["maxItems"], 40, "{written}");
    assert_ne!(properties["tags"]["maxItems"], 40, "{written}");
}

/// The contract says better than a name can: a parameter whose `maximum` is an array's
/// `maxItems` is that array's page size, whatever it is called.
#[tokio::test]
async fn a_maximum_matching_max_items_names_the_page() {
    let bundle = paged_things(
        "{name: amount, in: query, schema: {type: integer, maximum: 100}}",
        "maxItems: 100,",
    );
    let written = things_written(&bundle, json!({"amount": "40"})).await;
    assert_eq!(written["properties"]["things"]["maxItems"], 40, "{written}");
}
