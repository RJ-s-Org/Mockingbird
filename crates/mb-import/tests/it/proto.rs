#![cfg(test)]
use mb_import::{ImportError, SourceFile, SourceFormat, compile, compile_files, sniff};
use mb_mcm::{
    Binding, Bundle, Effect, Identity, OperationKind, OutputVariant, Protocol, ProtocolPayload,
    StatusSelector,
};
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};

fn fixture_dir() -> String {
    format!("{}/../../fixtures/proto", env!("CARGO_MANIFEST_DIR"))
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/{name}", fixture_dir())).unwrap()
}

fn route_guide() -> Bundle {
    compile(&read("route_guide.proto")).unwrap()
}

/// The `.proto` files under `fixtures/proto/multi`, with relative paths.
fn multi_files() -> Vec<SourceFile> {
    ["common/v1/money.proto", "orders/v1/orders.proto"]
        .into_iter()
        .map(|path| SourceFile {
            path: path.into(),
            bytes: read(&format!("multi/{path}")),
        })
        .collect()
}

const SCALARS: &str = r#"
syntax = "proto3";
package demo;

import "google/protobuf/any.proto";
import "google/protobuf/duration.proto";
import "google/protobuf/empty.proto";
import "google/protobuf/field_mask.proto";
import "google/protobuf/struct.proto";
import "google/protobuf/timestamp.proto";
import "google/protobuf/wrappers.proto";

service Demo {
  rpc Touch(Scalars) returns (google.protobuf.Empty);
}

enum Color {
  COLOR_UNSPECIFIED = 0;
  RED = 1;
}

message Scalars {
  int32 i32 = 1;
  sint32 s32 = 2;
  sfixed32 sf32 = 3;
  uint32 u32 = 4;
  fixed32 f32 = 5;
  int64 i64 = 6;
  sint64 s64 = 7;
  sfixed64 sf64 = 8;
  uint64 u64 = 9;
  fixed64 f64 = 10;
  float fl = 11;
  double db = 12;
  bool flag = 13;
  string text = 14;
  bytes data = 15;
  Color color = 16;
  repeated string tags = 17;
  map<string, int64> counts = 18;
  optional string nickname = 19;
  oneof contact {
    string email = 20;
    int32 phone_number = 21;
  }
  google.protobuf.Timestamp created_at = 22;
  google.protobuf.Duration ttl = 23;
  google.protobuf.Empty nothing = 24;
  google.protobuf.Struct meta = 25;
  google.protobuf.Value any_value = 26;
  google.protobuf.ListValue list = 27;
  google.protobuf.NullValue null_value = 28;
  google.protobuf.FieldMask mask = 29;
  google.protobuf.Any payload = 30;
  google.protobuf.Int64Value wrapped_count = 31;
  google.protobuf.StringValue wrapped_name = 32;
}
"#;

#[test]
fn route_guide_snapshot() {
    let b = route_guide();
    insta::assert_yaml_snapshot!(b, {
        ".source_digest" => "[digest]",
        ".protocol_payload" => "[descriptor set]",
    });
}

#[test]
fn helloworld_snapshot() {
    let b = compile(&read("helloworld.proto")).unwrap();
    insta::assert_yaml_snapshot!(b, {
        ".source_digest" => "[digest]",
        ".protocol_payload" => "[descriptor set]",
    });
}

#[test]
fn sniffs_proto_text_and_descriptor_sets() {
    assert_eq!(
        sniff(&read("route_guide.proto")).unwrap(),
        SourceFormat::Proto
    );
    assert_eq!(
        sniff(b"service Foo {}\nmessage Bar {}\n").unwrap(),
        SourceFormat::Proto
    );
    let Some(ProtocolPayload::Grpc {
        file_descriptor_set,
    }) = route_guide().protocol_payload
    else {
        panic!("expected a gRPC payload");
    };
    assert_eq!(
        sniff(&file_descriptor_set).unwrap(),
        SourceFormat::FileDescriptorSet
    );
}

#[test]
fn streaming_flags_map_to_kinds() {
    let b = route_guide();
    let ids: Vec<&str> = b.operations.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "RouteGuide.GetFeature",
            "RouteGuide.ListFeatures",
            "RouteGuide.RecordRoute",
            "RouteGuide.RouteChat",
        ]
    );
    let kind = |id: &str| b.operation(id).unwrap().kind.clone();
    assert_eq!(kind("RouteGuide.GetFeature"), OperationKind::Query);
    assert_eq!(kind("RouteGuide.ListFeatures"), OperationKind::StreamServer);
    assert_eq!(kind("RouteGuide.RecordRoute"), OperationKind::StreamClient);
    assert_eq!(kind("RouteGuide.RouteChat"), OperationKind::StreamBidi);

    let get = b.operation("RouteGuide.GetFeature").unwrap();
    assert_eq!(get.protocol, Protocol::Grpc);
    assert_eq!(
        get.binding,
        Binding::Grpc {
            package: "routeguide".into(),
            service: "RouteGuide".into(),
            method: "GetFeature".into(),
        }
    );
    assert_eq!(
        get.binding.grpc_path().as_deref(),
        Some("/routeguide.RouteGuide/GetFeature")
    );
    assert_eq!(
        get.input.body,
        Some(json!({"$ref": "#/$defs/routeguide.Point"}))
    );
    assert!(get.input.body_required);
    assert_eq!(
        get.outputs,
        [OutputVariant {
            status: StatusSelector::Code(200),
            content_type: Some("application/grpc".into()),
            schema: Some(json!({"$ref": "#/$defs/routeguide.Feature"})),
            headers: Default::default(),
        }]
    );
    assert_eq!(get.resource_type.as_deref(), Some("feature"));
    assert!(get.summary.as_deref().unwrap().starts_with("A simple RPC."));
    assert_eq!(get.identity, None);
    assert_eq!(b.title, "routeguide");
    assert_eq!(b.id, "routeguide");
    assert_eq!(b.version, "0");

    let hello = compile(&read("helloworld.proto")).unwrap();
    let say = hello.operation("Greeter.SayHello").unwrap();
    assert_eq!(say.kind, OperationKind::Query);
    assert_eq!(say.summary.as_deref(), Some("Sends a greeting"));
}

#[test]
fn scalar_and_wkt_mapping() {
    let b = compile(SCALARS.as_bytes()).unwrap();
    let i64s = json!({"type": "string", "format": "int64", "pattern": "^-?[0-9]+$"});
    let u64s = json!({"type": "string", "format": "uint64", "pattern": "^[0-9]+$"});
    let i32s = json!({"type": "integer", "minimum": i32::MIN, "maximum": i32::MAX});
    let u32s = json!({"type": "integer", "minimum": 0, "maximum": u32::MAX});
    assert_eq!(
        b.defs["demo.Scalars"],
        json!({
            "type": "object",
            "properties": {
                "i32": i32s, "s32": i32s, "sf32": i32s,
                "u32": u32s, "f32": u32s,
                "i64": i64s, "s64": i64s, "sf64": i64s,
                "u64": u64s, "f64": u64s,
                "fl": {"type": "number"},
                "db": {"type": "number"},
                "flag": {"type": "boolean"},
                "text": {"type": "string"},
                "data": {"type": "string", "format": "byte", "contentEncoding": "base64"},
                "color": {"$ref": "#/$defs/demo.Color"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "counts": {"type": "object", "additionalProperties": i64s},
                "nickname": {"type": "string"},
                "email": {"type": "string"},
                "phoneNumber": i32s,
                "createdAt": {"type": "string", "format": "date-time"},
                "ttl": {
                    "type": "string",
                    "format": "proto-duration",
                    "pattern": "^-?[0-9]+(\\.[0-9]+)?s$"
                },
                "nothing": {"type": "object", "additionalProperties": false},
                "meta": {"type": "object"},
                "anyValue": {},
                "list": {"type": "array"},
                "nullValue": {"type": "null"},
                "mask": {"type": "string"},
                "payload": {
                    "type": "object",
                    "properties": {"@type": {"type": "string"}},
                    "required": ["@type"]
                },
                "wrappedCount": {"anyOf": [i64s, {"type": "null"}]},
                "wrappedName": {"anyOf": [{"type": "string"}, {"type": "null"}]},
            },
            "additionalProperties": false,
            "x-oneof": {"contact": ["email", "phoneNumber"]}
        })
    );
    assert_eq!(
        b.defs["demo.Color"],
        json!({"type": "string", "enum": ["COLOR_UNSPECIFIED", "RED"]})
    );
    // A well-known type used as an RPC message gets a def of its own.
    assert_eq!(
        b.defs["google.protobuf.Empty"],
        json!({"type": "object", "additionalProperties": false})
    );
    // Other well-known types and map entries stay inline.
    let names: Vec<&str> = b.defs.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["demo.Color", "demo.Scalars", "google.protobuf.Empty"]
    );
    let touch = b.operation("Demo.Touch").unwrap();
    assert_eq!(touch.kind, OperationKind::Query);
    assert_eq!(b.title, "demo");
}

#[test]
fn multi_file_imports_resolve() {
    let b = compile_files(&multi_files()).unwrap();
    assert!(b.defs.contains_key("common.v1.Money"));
    assert!(b.defs.contains_key("orders.v1.Order"));
    assert_eq!(
        b.defs["orders.v1.Order"]["properties"]["createdAt"],
        json!({"type": "string", "format": "date-time"})
    );
    assert_eq!(b.title, "orders.v1");
    assert_eq!(b.id, "orders-v1");
    let ids: Vec<&str> = b.operations.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "OrderService.GetOrder",
            "OrderService.CreateOrder",
            "OrderService.ListOrders",
        ]
    );
    let get = b.operation("OrderService.GetOrder").unwrap();
    assert_eq!(
        get.identity,
        Some(Identity {
            path_param: None,
            field: "id".into(),
            input_field: Some("orderId".into()),
            path_field: None,
        })
    );
    assert_eq!(get.resource_type.as_deref(), Some("order"));
    assert_eq!(
        b.operation("OrderService.CreateOrder").unwrap().kind,
        OperationKind::Command(Effect::Create)
    );
    assert_eq!(
        b.operation("OrderService.ListOrders").unwrap().kind,
        OperationKind::StreamServer
    );

    // The payload carries the imports, including the well-known types.
    let Some(ProtocolPayload::Grpc {
        file_descriptor_set,
    }) = &b.protocol_payload
    else {
        panic!("expected a gRPC payload");
    };
    let set = prost_types::FileDescriptorSet::decode(file_descriptor_set.as_slice()).unwrap();
    let mut names: Vec<&str> = set.file.iter().map(|f| f.name()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "common/v1/money.proto",
            "google/protobuf/timestamp.proto",
            "orders/v1/orders.proto",
        ]
    );

    // Deterministic: the same files give the same bundle, in any order.
    let mut reversed = multi_files();
    reversed.reverse();
    assert_eq!(compile_files(&reversed).unwrap(), b);
}

#[test]
fn missing_import_is_invalid() {
    let only_orders: Vec<SourceFile> = multi_files()
        .into_iter()
        .filter(|f| f.path.starts_with("orders/"))
        .collect();
    match compile_files(&only_orders) {
        Err(ImportError::Invalid { pointer, message }) => {
            assert_eq!(pointer, "orders/v1/orders.proto");
            assert_eq!(
                message,
                "import \"common/v1/money.proto\" not found; upload a .zip or .tar.gz containing it"
            );
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn syntax_errors_carry_locations() {
    let err = compile(b"syntax = \"proto3\";\nmessage Broken {\n  int32 x = ;\n}\n").unwrap_err();
    match err {
        ImportError::Parse(msg) => assert!(msg.starts_with("main.proto:3:"), "{msg}"),
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[test]
fn compile_files_without_proto_falls_back_to_single_file() {
    let one = [SourceFile {
        path: "route_guide.txt".into(),
        bytes: read("route_guide.proto"),
    }];
    assert_eq!(
        compile_files(&one).unwrap().operations,
        route_guide().operations
    );
    let two = [
        SourceFile {
            path: "a.txt".into(),
            bytes: b"a".to_vec(),
        },
        SourceFile {
            path: "b.txt".into(),
            bytes: b"b".to_vec(),
        },
    ];
    assert!(matches!(
        compile_files(&two),
        Err(ImportError::Unsupported(_))
    ));
}

#[test]
fn file_descriptor_set_compiles_same_as_source() {
    let set = protox::compile(["route_guide.proto"], [fixture_dir()]).unwrap();
    let bytes = set.encode_to_vec();
    assert_eq!(sniff(&bytes).unwrap(), SourceFormat::FileDescriptorSet);
    let from_set = compile(&bytes).unwrap();
    let from_source = route_guide();
    assert_eq!(from_set.operations, from_source.operations);
    assert_eq!(from_set.defs, from_source.defs);
    assert_eq!(from_set.title, from_source.title);
}

fn validate(bundle: &Bundle, message: &str, instance: &Value) {
    let schema = bundle.with_reachable_defs(&json!({"$ref": format!("#/$defs/{message}")}));
    let validator = jsonschema::draft202012::new(&schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{instance} -> {errors:?}");
}

fn pool(bundle: &Bundle) -> DescriptorPool {
    let Some(ProtocolPayload::Grpc {
        file_descriptor_set,
    }) = &bundle.protocol_payload
    else {
        panic!("expected a gRPC payload");
    };
    DescriptorPool::decode(file_descriptor_set.as_slice()).unwrap()
}

/// Parse `json` into `message` with prost-reflect, then serialize it back
/// with the default options.
fn round_trip(pool: &DescriptorPool, message: &str, json: Value) -> Value {
    let desc = pool.get_message_by_name(message).unwrap();
    let msg = DynamicMessage::deserialize(desc, json).unwrap();
    serde_json::to_value(&msg).unwrap()
}

#[test]
fn prost_reflect_json_validates_against_schema() {
    let rg = route_guide();
    let feature = round_trip(
        &pool(&rg),
        "routeguide.Feature",
        json!({"name": "Summit", "location": {"latitude": 409146138, "longitude": -746188906}}),
    );
    assert_eq!(feature["location"]["latitude"], json!(409146138));
    validate(&rg, "routeguide.Feature", &feature);

    let demo = compile(SCALARS.as_bytes()).unwrap();
    let scalars = round_trip(
        &pool(&demo),
        "demo.Scalars",
        json!({
            "i32": -7,
            "u32": 7,
            "i64": "-9007199254740993",
            "u64": "18446744073709551615",
            "fl": 1.5,
            "flag": true,
            "text": "hi",
            "data": "aGVsbG8=",
            "color": "RED",
            "tags": ["a", "b"],
            "counts": {"x": "3", "y": "-4"},
            "nickname": "nick",
            "phoneNumber": 5,
            "createdAt": "2024-01-02T03:04:05Z",
            "ttl": "1.500s",
            "nothing": {},
            "meta": {"k": [1, "two"]},
            "anyValue": {"nested": true},
            "list": [1, null],
            "nullValue": null,
            "mask": "a.b,c",
            "payload": {
                "@type": "type.googleapis.com/google.protobuf.Duration",
                "value": "2s"
            },
            "wrappedCount": "12",
            "wrappedName": "w"
        }),
    );
    assert_eq!(scalars["i64"], json!("-9007199254740993"));
    assert_eq!(scalars["data"], json!("aGVsbG8="));
    assert_eq!(scalars["color"], json!("RED"));
    assert_eq!(scalars["createdAt"], json!("2024-01-02T03:04:05Z"));
    assert_eq!(scalars["counts"]["x"], json!("3"));
    validate(&demo, "demo.Scalars", &scalars);
}
