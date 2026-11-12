//! A held record's reference to a record of a type the contract reads, which no request
//! can set (Plaid's Item `institution_id`: the exchange sends a public token, and no
//! call updates an Item), is no `null` when a read serves the record: the Item names an
//! institution, so the quickstart's `/institutions/get_by_id` with it answers, where a
//! `null` failed the app with a 500.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support;

const CONTRACT: &str = include_str!("../../../../examples/apis/plaid/plaid.openapi.yaml");

fn world() -> Value {
    json!({
        "types": {
            "item": {"identity": {"member": "item_id", "alternates": ["access_token"]}}
        },
        "operations": {
            "itemPublicTokenExchange": {"kind": "create", "type": "item", "answer": {"record": ""}},
            "itemGet": {"kind": "read", "type": "item", "target": {"member": "access_token"},
                        "answer": {"record": "/item"}, "not_found": 400}
        }
    })
}

async fn send(h: &support::DescribedHarness, path: &str, body: Value) -> (String, Value) {
    let (status, headers, body) = h.send("POST", path, Some(body), &[]).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    let source = headers["x-mockingbird-source"]
        .to_str()
        .unwrap_or_default()
        .to_owned();
    (source, body)
}

#[tokio::test]
async fn a_held_items_server_set_reference_names_a_record_on_read() {
    let h = support::described(CONTRACT, world(), vec![json!({}), json!({})]);
    let (_, exchanged) = send(
        &h,
        "/item/public_token/exchange",
        json!({"public_token": "public-sandbox-5c224a01-8314-4491-a06f-39e193d5cddc"}),
    )
    .await;
    let token = exchanged["access_token"].clone();
    let (source, read) = send(&h, "/item/get", json!({"access_token": token})).await;
    assert_eq!(source, "state", "{read}");
    let institution = read["item"]["institution_id"].clone();
    let id = institution.as_str().unwrap_or_default().to_owned();
    assert!(id.starts_with("ins_"), "{read}");
    // Stored: the next read names the same one.
    let (source, again) = send(&h, "/item/get", json!({"access_token": token})).await;
    assert_eq!(source, "state", "{again}");
    assert_eq!(again["item"]["institution_id"], institution, "{again}");
    // The app's next call, with it, answers.
    let (_, found) = send(
        &h,
        "/institutions/get_by_id",
        json!({"institution_id": id, "country_codes": ["US"]}),
    )
    .await;
    assert!(found["institution"].is_object(), "{found}");
}

const DEALS: &str = r##"
openapi: 3.1.0
info: {title: Deals, version: "1"}
paths:
  /orgs/{org_id}:
    get:
      operationId: getOrg
      parameters: [{name: org_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: object, required: [id], properties: {id: {type: string}}}}}
  /deals:
    post:
      operationId: createDeal
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [title], properties: {title: {type: string}, org_id: {type: [string, "null"]}}}}}
      responses:
        "201":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Deal"}}}
  /deals/{id}:
    get:
      operationId: getDeal
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Deal"}}}
components:
  schemas:
    Deal:
      type: object
      required: [id, title]
      properties: {id: {type: string}, title: {type: string}, org_id: {type: [string, "null"]}}
"##;

/// A reference a request can set holds what it set: a `null` sent stays `null`.
#[tokio::test]
async fn a_reference_a_request_sets_stays_as_set() {
    let world = json!({
        "types": {"deal": {"identity": {"member": "id"}}, "org": {"identity": {"member": "id"}}},
        "operations": {
            "createDeal": {"kind": "create", "type": "deal"},
            "getDeal": {"kind": "read", "type": "deal"},
            "getOrg": {"kind": "read", "type": "org"}
        }
    });
    let h = support::described(DEALS, world, Vec::new());
    let (status, _, deal) = h
        .send(
            "POST",
            "/deals",
            Some(json!({"title": "Big", "org_id": null})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{deal}");
    let id = deal["id"].as_str().unwrap_or_default().to_owned();
    let (status, headers, read) = h.send("GET", &format!("/deals/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(headers["x-mockingbird-source"], "state", "{read}");
    assert_eq!(read["org_id"], Value::Null, "{read}");
}

const TICKETS: &str = r##"
openapi: 3.1.0
info: {title: Tickets, version: "1"}
paths:
  /desks/{desk_id}:
    get:
      operationId: getDesk
      parameters: [{name: desk_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: object, required: [desk_id], properties: {desk_id: {type: string}}}}}
  /agents/{agent_id}:
    get:
      operationId: getAgent
      parameters: [{name: agent_id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: object, required: [agent_id], properties: {agent_id: {type: string}}}}}
  /tickets:
    post:
      operationId: createTicket
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [title], properties: {title: {type: string}}}}}
      responses:
        "201":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}
  /tickets/{id}:
    get:
      operationId: getTicket
      parameters: [{name: id, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Ticket"}}}
components:
  schemas:
    Ticket:
      type: object
      required: [id, title]
      properties:
        id: {type: string}
        title: {type: string}
        desk_id: {type: [string, "null"]}
        agent_id: {type: [string, "null"]}
      examples:
        - {id: t_1, title: Printer jam, desk_id: desk_7, agent_id: null}
"##;

/// Of two server-set references on a ticket (desks and agents are never created by the
/// app), the one the contract's example shows set is named; the one it shows `null`
/// stays `null`.
#[tokio::test]
async fn only_a_reference_the_examples_show_set_is_named() {
    let world = json!({
        "types": {
            "ticket": {"identity": {"member": "id"}},
            "desk": {"identity": {"member": "desk_id"}},
            "agent": {"identity": {"member": "agent_id"}}
        },
        "operations": {
            "createTicket": {"kind": "create", "type": "ticket"},
            "getTicket": {"kind": "read", "type": "ticket"},
            "getDesk": {"kind": "read", "type": "desk"},
            "getAgent": {"kind": "read", "type": "agent"}
        }
    });
    let h = support::described(TICKETS, world, vec![json!({})]);
    let (status, _, ticket) = h
        .send(
            "POST",
            "/tickets",
            Some(json!({"title": "Printer jam"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{ticket}");
    // The create's fill left both `null`, as on a fresh record.
    assert_eq!(ticket["desk_id"], Value::Null, "{ticket}");
    let id = ticket["id"].as_str().unwrap_or_default().to_owned();
    let (status, headers, read) = h.send("GET", &format!("/tickets/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(headers["x-mockingbird-source"], "state", "{read}");
    assert!(read["desk_id"].is_string(), "{read}");
    assert_eq!(read["agent_id"], Value::Null, "{read}");
    let (_, _, again) = h.send("GET", &format!("/tickets/{id}"), None, &[]).await;
    assert_eq!(again["desk_id"], read["desk_id"], "{again}");
}
