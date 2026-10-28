#![allow(clippy::unwrap_used)]

use mb_import::{ImportError, SourceFormat, compile, sniff};
use mb_mcm::{
    Binding, Bundle, Effect, GraphqlOperationType, Identity, OperationKind, OutputVariant,
    ProtocolPayload, StatusSelector,
};
use serde_json::json;

fn read(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../fixtures/graphql/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(path).unwrap()
}

fn github() -> Bundle {
    compile(&read("github.graphql")).unwrap()
}

#[test]
fn github_sdl_snapshot() {
    insta::assert_yaml_snapshot!(github(), {".source_digest" => "[digest]"});
}

#[test]
fn starwars_introspection_snapshot() {
    let b = compile(&read("starwars-introspection.json")).unwrap();
    insta::assert_yaml_snapshot!(b, {".source_digest" => "[digest]"});
}

#[test]
fn introspection_and_equivalent_sdl_compile_identically() {
    let intro = compile(&read("starwars-introspection.json")).unwrap();
    let sdl = compile(&read("starwars.graphql")).unwrap();
    assert_eq!(intro.operations, sdl.operations);
    assert_eq!(intro.defs, sdl.defs);
    assert_eq!(intro.title, "The Star Wars API");
    assert_eq!(intro.id, "the-star-wars-api");
    assert_eq!(sdl.title, "GraphQL API");
    assert_eq!(sdl.id, "graphql-api");
    assert_eq!(sdl.version, "0");
    // The rendered SDL is itself a valid source that compiles the same way.
    let Some(ProtocolPayload::Graphql { sdl: rendered }) = &intro.protocol_payload else {
        panic!("expected a GraphQL payload");
    };
    let again = compile(rendered.as_bytes()).unwrap();
    assert_eq!(again.operations, sdl.operations);
    assert_eq!(again.defs, sdl.defs);
    // Without the `data` envelope too.
    let doc: serde_json::Value =
        serde_json::from_slice(&read("starwars-introspection.json")).unwrap();
    let bare = serde_json::to_vec(&doc["data"]).unwrap();
    assert_eq!(compile(&bare).unwrap().operations, sdl.operations);
}

#[test]
fn root_fields_become_operations() {
    let b = github();
    let ids: Vec<&str> = b.operations.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "Query.viewer",
            "Query.repository",
            "Query.issue",
            "Query.search",
            "Mutation.createIssue",
            "Mutation.updateIssue",
            "Mutation.deleteIssue",
            "Subscription.issueAdded",
        ]
    );
    let kind = |id: &str| b.operation(id).unwrap().kind.clone();
    assert_eq!(kind("Query.repository"), OperationKind::Query);
    assert_eq!(
        kind("Mutation.createIssue"),
        OperationKind::Command(Effect::Create)
    );
    assert_eq!(
        kind("Mutation.updateIssue"),
        OperationKind::Command(Effect::Update)
    );
    assert_eq!(
        kind("Mutation.deleteIssue"),
        OperationKind::Command(Effect::Delete)
    );
    assert_eq!(kind("Subscription.issueAdded"), OperationKind::StreamServer);

    let issue = b.operation("Query.issue").unwrap();
    assert_eq!(
        issue.binding,
        Binding::Graphql {
            operation_type: GraphqlOperationType::Query,
            field: "issue".into()
        }
    );
    assert_eq!(
        issue.identity,
        Some(Identity {
            path_param: None,
            field: "id".into(),
            input_field: Some("id".into()),
            path_field: None,
        })
    );
    assert_eq!(issue.resource_type.as_deref(), Some("issue"));
    assert_eq!(
        issue.summary.as_deref(),
        Some("Lookup an issue by its Node ID.")
    );
    assert_eq!(
        issue.semantic_hints,
        ["An Issue is a place to discuss ideas, enhancements, tasks, and bugs."]
    );
    assert_eq!(
        issue.input.body,
        Some(json!({
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
            "additionalProperties": false
        }))
    );
    assert!(issue.input.body_required);
    assert_eq!(
        issue.outputs,
        [OutputVariant {
            status: StatusSelector::Code(200),
            content_type: Some("application/json".into()),
            schema: Some(json!({"anyOf": [{"$ref": "#/$defs/Issue"}, {"type": "null"}]})),
            headers: Default::default(),
        }]
    );

    // `<resource>Id` is the input field when there is no `id` argument.
    let added = b.operation("Subscription.issueAdded").unwrap();
    assert_eq!(
        added
            .identity
            .as_ref()
            .and_then(|i| i.input_field.as_deref()),
        None
    );
    let viewer = b.operation("Query.viewer").unwrap();
    assert_eq!(viewer.resource_type.as_deref(), Some("user"));
    assert!(!viewer.input.body_required);
    let search = b.operation("Query.search").unwrap();
    assert_eq!(search.resource_type, None);
    assert_eq!(search.identity, None);
    assert_eq!(
        b.protocol_payload,
        Some(ProtocolPayload::Graphql {
            sdl: String::from_utf8(read("github.graphql")).unwrap()
        })
    );
    // `extend type` fields are merged into the base type.
    assert!(b.defs["Issue"]["properties"].get("labels").is_some());
}

#[test]
fn resource_id_argument_is_the_input_field() {
    let sdl = br#"
      type Order { id: ID! total: Float }
      type Query { order(orderId: ID!): Order }
      schema { query: Query }
    "#;
    let b = compile(sdl).unwrap();
    let op = b.operation("Query.order").unwrap();
    assert_eq!(
        op.identity,
        Some(Identity {
            path_param: None,
            field: "id".into(),
            input_field: Some("orderId".into()),
            path_field: None,
        })
    );
}

#[test]
fn schema_block_renames_root_types() {
    let sdl = br#"
      schema { query: RootQuery mutation: RootMutation }
      type RootQuery { ping: String }
      type RootMutation { addThing(name: String!): Boolean }
      type Query { ignored: Int }
    "#;
    let b = compile(sdl).unwrap();
    let ids: Vec<&str> = b.operations.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["Query.ping", "Mutation.addThing"]);
    let add = b.operation("Mutation.addThing").unwrap();
    assert_eq!(add.kind, OperationKind::Command(Effect::Create));
    assert_eq!(add.resource_type.as_deref(), Some("thing"));
    assert_eq!(add.identity, None);
}

#[test]
fn scalars_enums_lists_nonnull_map() {
    let sdl = br#"
      scalar Url
      enum Color { RED GREEN }
      "A thing."
      type Thing @cached {
        i: Int!
        f: Float
        s: String!
        id: ID!
        b: Boolean
        "Some ints."
        l: [Int]!
        u: Url
        c: Color!
      }
      input Filter { color: Color! limit: Int = 5 }
      type Query { thing(n: Int!, tags: [String!], filter: Filter = {color: RED}): [Thing!] }
    "#;
    let b = compile(sdl).unwrap();
    let int = json!({"type": "integer", "minimum": -2147483648_i64, "maximum": 2147483647});
    let nil = json!({"type": "null"});
    assert_eq!(
        serde_json::Value::Object(b.defs.clone()),
        json!({
            "Color": {"type": "string", "enum": ["RED", "GREEN"]},
            "Filter": {
                "type": "object",
                "properties": {
                    "color": {"$ref": "#/$defs/Color"},
                    "limit": {"anyOf": [int, nil]}
                },
                "required": ["color"],
                "additionalProperties": false
            },
            "Query": {
                "type": "object",
                "properties": {
                    "__typename": {"const": "Query"},
                    "thing": {"anyOf": [
                        {"type": "array", "items": {"$ref": "#/$defs/Thing"}},
                        nil
                    ]}
                },
                "required": ["__typename"],
                "additionalProperties": false
            },
            "Thing": {
                "description": "A thing.",
                "type": "object",
                "properties": {
                    "__typename": {"const": "Thing"},
                    "i": int,
                    "f": {"anyOf": [{"type": "number"}, nil]},
                    "s": {"type": "string"},
                    "id": {"type": "string"},
                    "b": {"anyOf": [{"type": "boolean"}, nil]},
                    "l": {
                        "description": "Some ints.",
                        "type": "array",
                        "items": {"anyOf": [int, nil]}
                    },
                    "u": {"anyOf": [{"$ref": "#/$defs/Url"}, nil]},
                    "c": {"$ref": "#/$defs/Color"}
                },
                "required": ["__typename", "i", "s", "id", "l", "c"],
                "additionalProperties": false
            },
            "Url": {"description": "GraphQL scalar Url"}
        })
    );
    let op = b.operation("Query.thing").unwrap();
    assert_eq!(
        op.input.body,
        Some(json!({
            "type": "object",
            "properties": {
                "n": int,
                "tags": {"anyOf": [{"type": "array", "items": {"type": "string"}}, nil]},
                "filter": {"anyOf": [{"$ref": "#/$defs/Filter"}, nil]}
            },
            "required": ["n"],
            "additionalProperties": false
        }))
    );
    assert!(op.input.body_required);
    // A list of objects has no single identity; the resource is still the item type.
    assert_eq!(op.resource_type.as_deref(), Some("thing"));
    assert_eq!(op.identity, None);
}

#[test]
fn recursive_types_use_defs() {
    let b = github();
    assert_eq!(
        b.defs["User"]["properties"]["repositories"],
        json!({"description": "A list of repositories that the user owns.",
               "type": "array", "items": {"$ref": "#/$defs/Repository"}})
    );
    assert_eq!(
        b.defs["Repository"]["properties"]["owner"],
        json!({"description": "The User owner of the repository.", "$ref": "#/$defs/User"})
    );
    assert_eq!(
        b.defs["Node"],
        json!({"description": "An object with an ID.", "oneOf": [
            {"$ref": "#/$defs/Issue"}, {"$ref": "#/$defs/Repository"}, {"$ref": "#/$defs/User"}
        ]})
    );
    assert_eq!(
        b.defs["SearchResult"]["oneOf"],
        json!([{"$ref": "#/$defs/Issue"}, {"$ref": "#/$defs/Repository"}, {"$ref": "#/$defs/User"}])
    );
    assert_eq!(
        b.defs["User"]["properties"]["__typename"],
        json!({"const": "User"})
    );
}

#[test]
fn invalid_sdl_reports_location() {
    let err = compile(b"type Query {\n  a: Int\n  b: \n}\n").unwrap_err();
    let ImportError::Parse(msg) = err else {
        panic!("expected a parse error, got {err:?}");
    };
    assert!(msg.starts_with("line 4, column 1: "), "{msg}");

    let err = compile(b"type Mutation { a: Int }").unwrap_err();
    assert!(
        matches!(&err, ImportError::Invalid { pointer, .. } if pointer == "/schema"),
        "{err:?}"
    );

    let err = compile(b"type Query { a: Missing }").unwrap_err();
    assert!(
        matches!(&err, ImportError::Invalid { pointer, .. } if pointer == "/types/Query/fields/a"),
        "{err:?}"
    );
    let err = compile(b"type Query { a(x: Nope): Int }").unwrap_err();
    assert!(
        matches!(&err, ImportError::Invalid { pointer, .. } if pointer == "/types/Query/fields/a"),
        "{err:?}"
    );
    let err = compile(b"schema { query: Root } type Query { a: Int }").unwrap_err();
    assert!(
        matches!(&err, ImportError::Invalid { pointer, .. } if pointer == "/schema"),
        "{err:?}"
    );
}

#[test]
fn sniff_detects_sdl_and_introspection() {
    assert_eq!(
        sniff(&read("github.graphql")).unwrap(),
        SourceFormat::GraphqlSdl
    );
    assert_eq!(
        sniff(&read("starwars.graphql")).unwrap(),
        SourceFormat::GraphqlSdl
    );
    assert_eq!(
        sniff(&read("starwars-introspection.json")).unwrap(),
        SourceFormat::GraphqlIntrospection
    );
    assert_eq!(
        sniff(br#"{"__schema": {"types": []}}"#).unwrap(),
        SourceFormat::GraphqlIntrospection
    );
    assert_eq!(
        sniff(b"schema { query: Root }\ntype Root { a: Int }").unwrap(),
        SourceFormat::GraphqlSdl
    );
    assert_eq!(
        sniff(br#"{"openapi": "3.1.0", "info": {}}"#).unwrap(),
        SourceFormat::OpenApi3
    );
    assert_eq!(
        sniff(b"swagger: '2.0'\ninfo: {title: T}\n").unwrap(),
        SourceFormat::Swagger2
    );
    assert!(matches!(
        sniff(b"just some words"),
        Err(ImportError::Unsupported(_) | ImportError::Parse(_))
    ));
}

/// A null item of a list of enums, objects or lists cannot be served by the
/// GraphQL adapter, so output schemas require those items; scalar items and
/// input values keep GraphQL's nullability.
#[test]
fn output_list_items_that_cannot_be_null_are_required() {
    let sdl = br#"
      enum Color { RED GREEN }
      type Thing { c: [Color]!, cc: [[Color]], t: [Thing], s: [String]! }
      type Query { things(colors: [Color]): [Thing] }
    "#;
    let b = compile(sdl).unwrap();
    let nil = json!({"type": "null"});
    let color = json!({"$ref": "#/$defs/Color"});
    let thing = &b.defs["Thing"]["properties"];
    assert_eq!(thing["c"], json!({"type": "array", "items": color}));
    assert_eq!(
        thing["cc"],
        json!({"anyOf": [
            {"type": "array", "items": {"type": "array", "items": color}},
            nil
        ]})
    );
    assert_eq!(
        thing["t"],
        json!({"anyOf": [{"type": "array", "items": {"$ref": "#/$defs/Thing"}}, nil]})
    );
    assert_eq!(
        thing["s"],
        json!({"type": "array", "items": {"anyOf": [{"type": "string"}, nil]}})
    );
    let op = b.operation("Query.things").unwrap();
    assert_eq!(
        op.outputs[0].schema,
        Some(json!({"anyOf": [{"type": "array", "items": {"$ref": "#/$defs/Thing"}}, nil]}))
    );
    // Input: a null item is a valid argument.
    assert_eq!(
        op.input.body.as_ref().unwrap()["properties"]["colors"],
        json!({"anyOf": [{"type": "array", "items": {"anyOf": [color, nil]}}, nil]})
    );
}
