#![cfg(test)]
//! A record keyed by a member of its object metadata (`info.label`), read by a path
//! parameter that names that member.
use mb_import::compile;
use mb_mcm::Bundle;

const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Shelves, version: "1"}
paths:
  /racks:
    post:
      operationId: createRack
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Rack"}
      responses:
        "201":
          description: made
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Rack"}
  /racks/{rack}:
    get:
      operationId: readRack
      parameters:
        - {name: rack, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Rack"}
  /racks/{rack}/gadgets:
    parameters:
      - {name: rack, in: path, required: true, schema: {type: string}}
    get:
      operationId: listGadgets
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  kind: {type: string}
                  items:
                    type: array
                    items: {$ref: "#/components/schemas/Gadget"}
    post:
      operationId: createGadget
      requestBody:
        content:
          application/json:
            schema: {$ref: "#/components/schemas/Gadget"}
      responses:
        "201":
          description: made
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Gadget"}
  /racks/{rack}/gadgets/{label}:
    parameters:
      - {name: rack, in: path, required: true, schema: {type: string}}
      - {name: label, in: path, required: true, schema: {type: string}}
    get:
      operationId: readGadget
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Gadget"}
    patch:
      operationId: patchGadget
      requestBody:
        content:
          application/merge-patch+json:
            schema: {type: object, additionalProperties: true}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Gadget"}
    delete:
      operationId: deleteGadget
      responses:
        "204": {description: gone}
components:
  schemas:
    Info:
      type: object
      properties:
        label: {type: string}
        rack: {type: string, readOnly: true}
        serial: {type: string, format: uuid, readOnly: true}
        revision: {type: string}
        tags:
          type: object
          additionalProperties: {type: string}
    Rack:
      type: object
      properties:
        kind: {type: string}
        info: {$ref: "#/components/schemas/Info"}
    Gadget:
      type: object
      properties:
        kind: {type: string}
        info: {$ref: "#/components/schemas/Info"}
        settings:
          type: object
          additionalProperties: {type: string}
"##;

fn field(bundle: &Bundle, op: &str) -> Option<(Option<String>, String)> {
    let identity = bundle.operation(op)?.identity.clone()?;
    Some((identity.path_param, identity.field))
}

#[test]
fn a_parameter_naming_a_member_of_the_metadata_keys_the_record_by_its_path() {
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    let by_label = Some((Some("label".to_owned()), "info.label".to_owned()));
    for op in ["readGadget", "patchGadget", "deleteGadget"] {
        assert_eq!(field(&bundle, op), by_label, "{op}");
    }
    assert_eq!(
        field(&bundle, "createGadget"),
        Some((None, "info.label".to_owned()))
    );
}

#[test]
fn a_parameter_naming_its_own_collection_names_no_metadata_member() {
    // `/racks/{rack}` names the rack itself: the metadata's `rack` is the scope a
    // gadget refers to, not the rack's key.
    let bundle = compile(CONTRACT.as_bytes()).unwrap();
    assert_eq!(field(&bundle, "readRack"), None);
    assert_eq!(field(&bundle, "createRack"), None);
}

#[test]
fn a_plain_identity_serializes_as_it_always_has() {
    let plain = mb_mcm::Identity {
        path_param: Some("id".into()),
        field: "id".into(),
        input_field: None,
        path_field: None,
        record: None,
    };
    let text = serde_json::to_value(&plain).unwrap();
    assert_eq!(text, serde_json::json!({"path_param": "id", "field": "id"}));
    let nested: mb_mcm::Identity =
        serde_json::from_value(serde_json::json!({"path_param": "label", "field": "info.label"}))
            .unwrap();
    assert_eq!(nested.field, "info.label");
}
