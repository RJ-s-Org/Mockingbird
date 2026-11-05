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

/// Profiles read by `{username}`, each holding its manager inline: a related record,
/// not the profile's metadata. `extra_paths` and `extra_schemas` add to it.
fn profiles(extra_paths: &str, extra_schemas: &str, manager: &str) -> String {
    format!(
        r##"
openapi: 3.0.0
info: {{title: Profiles, version: "1"}}
paths:
  /profiles:
    post:
      operationId: createProfile
      requestBody:
        content:
          application/json:
            schema: {{$ref: "#/components/schemas/Profile"}}
      responses:
        "201":
          description: made
          content:
            application/json:
              schema: {{$ref: "#/components/schemas/Profile"}}
  /profiles/{{username}}:
    get:
      operationId: readProfile
      parameters:
        - {{name: username, in: path, required: true, schema: {{type: string}}}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {{$ref: "#/components/schemas/Profile"}}
{extra_paths}
components:
  schemas:
    Person:
      type: object
      properties:
{manager}
    Profile:
      type: object
      properties:
        handle: {{type: string}}
        bio: {{type: string}}
        manager: {{$ref: "#/components/schemas/Person"}}
{extra_schemas}
"##
    )
}

const PERSON: &str = "        username: {type: string}\n        email: {type: string}";

fn assert_profiles_unkeyed(contract: &str) {
    let bundle = compile(contract.as_bytes()).unwrap();
    assert_eq!(field(&bundle, "readProfile"), None);
    assert_eq!(field(&bundle, "createProfile"), None);
}

#[test]
fn a_related_record_held_inline_is_not_metadata_that_keys_the_record() {
    // Only profiles hold a `manager`: nothing says it is the record's own metadata.
    assert_profiles_unkeyed(&profiles("", "", PERSON));
}

/// A `Team` read by path that holds a `manager: Person` too.
const TEAMS: &str = r##"
  /teams/{team}:
    get:
      operationId: readTeam
      parameters:
        - {name: team, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Team"}"##;

const TEAM: &str = r##"    Team:
      type: object
      properties:
        title: {type: string}
        manager: {$ref: "#/components/schemas/Person"}"##;

#[test]
fn a_shared_member_that_is_another_type_s_record_is_not_metadata() {
    // Profiles and teams both hold a manager, and a person is a record of its own.
    let people = format!(
        "{TEAMS}\n  /people/{{username}}:\n    get:\n      operationId: readPerson\n      parameters:\n        - {{name: username, in: path, required: true, schema: {{type: string}}}}\n      responses:\n        \"200\":\n          description: ok\n          content:\n            application/json:\n              schema: {{$ref: \"#/components/schemas/Person\"}}"
    );
    assert_profiles_unkeyed(&profiles(&people, TEAM, PERSON));
}

#[test]
fn a_shared_member_with_its_own_id_is_not_metadata() {
    let with_id = format!("        id: {{type: string}}\n{PERSON}");
    assert_profiles_unkeyed(&profiles(TEAMS, TEAM, &with_id));
}

#[test]
fn metadata_shared_by_two_types_keys_the_record() {
    // The shared metadata convention: profiles and teams both hold the same `manager`,
    // which is no record of its own and has no `id`.
    let bundle = compile(profiles(TEAMS, TEAM, PERSON).as_bytes()).unwrap();
    assert_eq!(
        field(&bundle, "readProfile"),
        Some((Some("username".to_owned()), "manager.username".to_owned()))
    );
}
