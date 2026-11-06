//! An update sending a scalar for a member the record holds as an object, the
//! value-of-choice pattern (`{value: <enum>, label: <text>}`): the one member of the
//! object whose schema admits the scalar takes it.

use std::sync::{Arc, Mutex};

use mb_engine::{GenerationContext, OperationCall, Source};
use mb_import::compile;
use mb_state::StoredResource;
use serde_json::{Map, Value, json};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};

use crate::support::scripted;

/// `PATCH /devices/{id}` answering the read's `Device`, its `status` an object whose
/// members are `status`.
fn contract(status: &str) -> String {
    format!(
        r##"
openapi: 3.1.0
info: {{title: T, version: "1"}}
paths:
  /devices/{{deviceId}}:
    parameters: [{{name: deviceId, in: path, required: true, schema: {{type: string}}}}]
    patch:
      operationId: patchDevice
      requestBody:
        required: true
        content: {{application/json: {{schema: {{type: object, additionalProperties: true}}}}}}
      responses:
        "200": {{description: OK, content: {{application/json: {{schema: {{$ref: "#/components/schemas/Device"}}}}}}}}
components:
  schemas:
    Device:
      type: object
      required: [id]
      properties:
        id: {{type: string}}
        name: {{type: string}}
        status: {{$ref: "#/components/schemas/Status"}}
    Status:
      type: object
      required: [value]
      properties: {status}
"##
    )
}

const CHOICE: &str =
    "{value: {type: string, enum: [active, reserved, dhcp]}, label: {type: string}}";

#[derive(Debug)]
struct Settling<'a>(&'a StoredResource);

impl mb_engine::UpdateGuard for Settling<'_> {
    fn settles(&self, _status: u16) -> bool {
        true
    }

    fn keep_identity(&self, mut candidate: Value) -> Value {
        if let Some(id) = self.0.data.get("id") {
            candidate["id"] = id.clone();
        }
        candidate
    }

    fn keeps_owner(&self, _candidate: &Value) -> bool {
        true
    }
}

/// The messages and fields of the events logged, one line each.
#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<String>>>);

struct Line(String);

impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

impl tracing::Subscriber for Events {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = Line(String::new());
        event.record(&mut line);
        if let Ok(mut lines) = self.0.lock() {
            lines.push(line.0);
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// What `PATCH` `request` on the stored device serves when the LLM answers `answer`,
/// and the events it logs.
async fn patched(status: &str, request: Value, answer: Value) -> (Value, Vec<String>) {
    let (body, lines, _) = patched_with(&contract(status), request, answer).await;
    (body, lines)
}

/// [`patched`] over a whole contract, also giving the answer's world gaps.
async fn patched_with(
    contract: &str,
    request: Value,
    answer: Value,
) -> (Value, Vec<String>, Vec<String>) {
    let bundle = compile(contract.as_bytes()).unwrap();
    let op = bundle.operation("patchDevice").unwrap();
    let target = StoredResource {
        resource_type: "devices".into(),
        id: "d1".into(),
        data: json!({"id": "d1", "name": "edge", "status": {"value": "active", "label": "Active"}}),
    };
    let mut path_params = Map::new();
    path_params.insert("deviceId".into(), json!("d1"));
    let call = OperationCall {
        operation_id: "patchDevice".into(),
        path_params,
        body: Some(request),
        ..OperationCall::default()
    };
    let (_p, engine) = scripted(vec![Ok(answer.to_string())]);
    let guard = Settling(&target);
    let context = GenerationContext {
        target: Some(&target),
        update_guard: Some(&guard),
        answer_records: None,
        ..GenerationContext::default()
    };
    let events = Events::default();
    let _default = tracing::subscriber::set_default(events.clone());
    let out = engine.generate(&bundle, op, &call, context).await.unwrap();
    assert_eq!(out.source, Source::Llm, "{:?}", out.fallback_reason);
    let lines = events.0.lock().map(|l| l.clone()).unwrap_or_default();
    (out.body.unwrap(), lines, out.world_gaps)
}

#[tokio::test]
async fn an_answer_applying_the_scalar_to_the_choice_stands() {
    let (body, _) = patched(
        CHOICE,
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "reserved", "label": "Reserved"}}),
    )
    .await;
    assert_eq!(
        body["status"],
        json!({"value": "reserved", "label": "Reserved"}),
        "{body}"
    );
    assert_eq!(body["name"], "edge", "{body}");
}

#[tokio::test]
async fn an_answer_ignoring_the_scalar_still_moves_the_one_member_admitting_it() {
    let (body, _) = patched(
        CHOICE,
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "active"}}),
    )
    .await;
    assert_eq!(body["status"]["value"], "reserved", "{body}");
}

#[tokio::test]
async fn a_scalar_two_members_admit_keeps_the_stored_choice_and_is_logged() {
    let status = "{value: {type: string, enum: [active, reserved, dhcp]}, \
                  previous: {type: string, enum: [active, reserved, dhcp]}, label: {type: string}}";
    let (body, lines) = patched(
        status,
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "active", "previous": "active"}}),
    )
    .await;
    assert_eq!(
        body["status"],
        json!({"value": "active", "label": "Active"}),
        "{body}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("world_gap=\"ambiguous_choice\"")
                && l.contains("patchDevice")
                && l.contains("member=status")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn a_scalar_member_beside_the_choice_is_unaffected() {
    let (body, _) = patched(
        CHOICE,
        json!({"name": "core"}),
        json!({"id": "d1", "name": "core", "status": {"value": "dhcp", "label": "DHCP"}}),
    )
    .await;
    assert_eq!(body["name"], "core", "{body}");
    assert_eq!(
        body["status"],
        json!({"value": "active", "label": "Active"}),
        "{body}"
    );
}

/// The contract of [`contract`] with `Status` as `schema`, a whole YAML mapping.
fn contract_with_status(schema: &str) -> String {
    contract(CHOICE).replace(
        &format!(
            "    Status:\n      type: object\n      required: [value]\n      properties: {CHOICE}\n"
        ),
        &format!("    Status:\n{schema}\n"),
    )
}

#[tokio::test]
async fn an_ignored_scalar_drops_the_optional_sibling_the_old_value_set() {
    let (body, _) = patched(
        CHOICE,
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "active", "label": "Active"}}),
    )
    .await;
    assert_eq!(body["status"], json!({"value": "reserved"}), "{body}");
}

const PAIRED_PROPERTIES: &str = "      type: object
      required: [value, label]
      properties:
        value: {type: string, enum: [active, reserved, dhcp]}
        label: {type: string}";

#[tokio::test]
async fn an_ignored_scalar_takes_the_sibling_an_enum_of_objects_pairs_with_it() {
    let schema = format!(
        "{PAIRED_PROPERTIES}
      enum:
        - {{value: active, label: Active}}
        - {{value: reserved, label: Reserved}}
        - {{value: dhcp, label: DHCP}}"
    );
    let (body, _, gaps) = patched_with(
        &contract_with_status(&schema),
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "active", "label": "Active"}}),
    )
    .await;
    assert_eq!(
        body["status"],
        json!({"value": "reserved", "label": "Reserved"}),
        "{body}"
    );
    assert!(gaps.is_empty(), "{gaps:?}");
}

#[tokio::test]
async fn an_ignored_scalar_takes_the_sibling_a_const_branch_pairs_with_it() {
    let schema = format!(
        "{PAIRED_PROPERTIES}
      oneOf:
        - properties: {{value: {{const: active}}, label: {{const: Active}}}}
        - properties: {{value: {{const: reserved}}, label: {{const: Reserved}}}}
        - properties: {{value: {{const: dhcp}}, label: {{const: DHCP}}}}"
    );
    let (body, _, _) = patched_with(
        &contract_with_status(&schema),
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "active", "label": "Active"}}),
    )
    .await;
    assert_eq!(
        body["status"],
        json!({"value": "reserved", "label": "Reserved"}),
        "{body}"
    );
}

#[tokio::test]
async fn an_ignored_scalar_takes_the_sibling_an_example_pairs_with_it() {
    let schema = format!(
        "{PAIRED_PROPERTIES}
      examples:
        - {{value: active, label: Active}}
        - {{value: reserved, label: Reserved}}"
    );
    let (body, _, _) = patched_with(
        &contract_with_status(&schema),
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "active", "label": "Active"}}),
    )
    .await;
    assert_eq!(
        body["status"],
        json!({"value": "reserved", "label": "Reserved"}),
        "{body}"
    );
}

#[tokio::test]
async fn an_ignored_scalar_with_an_unpaired_required_sibling_keeps_the_stored_choice() {
    let (body, lines, gaps) = patched_with(
        &contract_with_status(PAIRED_PROPERTIES),
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "active", "label": "Active"}}),
    )
    .await;
    assert_eq!(
        body["status"],
        json!({"value": "active", "label": "Active"}),
        "{body}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("world_gap=\"unapplied_choice\"")
                && l.contains("patchDevice")
                && l.contains("member=status")),
        "{lines:?}"
    );
    assert_eq!(gaps, ["unapplied_choice"]);
}

#[tokio::test]
async fn a_member_that_is_a_union_of_a_scalar_and_an_object_switches_branch() {
    let contract = contract(CHOICE).replace(
        "status: {$ref: \"#/components/schemas/Status\"}",
        "status: {oneOf: [{type: string}, {$ref: \"#/components/schemas/Status\"}]}",
    );
    let (body, lines, gaps) = patched_with(
        &contract,
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "active", "label": "Active"}}),
    )
    .await;
    assert_eq!(body["status"], "reserved", "{body}");
    assert!(!lines.iter().any(|l| l.contains("world_gap")), "{lines:?}");
    assert!(gaps.is_empty(), "{gaps:?}");
}

#[tokio::test]
async fn a_member_whose_const_is_the_scalar_takes_it() {
    let schema = "      type: object
      properties: {value: {type: string, const: reserved}, label: {type: string}}";
    let (body, _, _) = patched_with(
        &contract_with_status(schema),
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"label": "Active"}}),
    )
    .await;
    assert_eq!(body["status"], json!({"value": "reserved"}), "{body}");
}

#[tokio::test]
async fn a_member_whose_examples_list_the_scalar_takes_it() {
    let (body, _) = patched(
        "{value: {type: string, examples: [active, reserved]}, label: {type: string}}",
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "active", "label": "Active"}}),
    )
    .await;
    assert_eq!(body["status"], json!({"value": "reserved"}), "{body}");
}

#[tokio::test]
async fn with_nothing_declared_the_member_the_answer_echoes_takes_the_scalar() {
    let (body, _) = patched(
        "{value: {type: string}, label: {type: string}}",
        json!({"status": "reserved"}),
        json!({"id": "d1", "name": "edge", "status": {"value": "reserved", "label": "Reserved"}}),
    )
    .await;
    assert_eq!(
        body["status"],
        json!({"value": "reserved", "label": "Reserved"}),
        "{body}"
    );
}
