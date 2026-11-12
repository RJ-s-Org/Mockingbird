//! A create of a type no route reads (twilio-cli trial: Verify's
//! `POST /v2/Services/{ServiceSid}/VerificationCheck`, with no read, list or update of
//! its own) keeps what the request sent and the path names, as a create of a type a
//! route reads does (`POST …/Verifications`). The identity was minted from the reads
//! alone, so with none it was a faker word (`"meadow"`) where the answer's `sid`
//! declares `^VE[0-9a-fA-F]{32}$`: the record did not render, and the answer was the
//! faker's acknowledgement, with `to: "alpha"`, another service's `service_sid` and a
//! `status` the model never chose. The id is now minted as the create's answer declares
//! it, the only place a type read by no route declares it.
#![cfg(feature = "http-rest")]

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support;

/// Services, their verifications (read by id) and verification checks (created only,
/// answering `200` beside `201` as Twilio's contract does), sent as forms. The records'
/// definitions are named as Twilio names them, never for the type (a definition named
/// for a type no route reads is read as its scalar members).
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Verify, version: "2"}
paths:
  /v2/Services:
    post:
      operationId: CreateService
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema:
              type: object
              properties: {FriendlyName: {type: string}}
      responses:
        "201":
          description: Created
          content: {application/json: {schema: {$ref: "#/components/schemas/Service"}}}
  /v2/Services/{Sid}:
    get:
      operationId: FetchService
      parameters: [{name: Sid, in: path, required: true, schema: {type: string, pattern: "^VA[0-9a-fA-F]{32}$"}}]
      responses:
        "200":
          description: OK
          content: {application/json: {schema: {$ref: "#/components/schemas/Service"}}}
  /v2/Services/{ServiceSid}/Verifications:
    post:
      operationId: CreateVerification
      parameters: [{name: ServiceSid, in: path, required: true, schema: {type: string, pattern: "^VA[0-9a-fA-F]{32}$"}}]
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema:
              type: object
              required: [To, Channel]
              properties: {To: {type: string}, Channel: {type: string}}
      responses:
        "201":
          description: Created
          content: {application/json: {schema: {$ref: "#/components/schemas/verify.v2.service.verification"}}}
  /v2/Services/{ServiceSid}/Verifications/{Sid}:
    get:
      operationId: FetchVerification
      parameters:
        - {name: ServiceSid, in: path, required: true, schema: {type: string, pattern: "^VA[0-9a-fA-F]{32}$"}}
        - {name: Sid, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: OK
          content: {application/json: {schema: {$ref: "#/components/schemas/verify.v2.service.verification"}}}
  /v2/Services/{ServiceSid}/VerificationCheck:
    post:
      operationId: CreateVerificationCheck
      parameters: [{name: ServiceSid, in: path, required: true, schema: {type: string, pattern: "^VA[0-9a-fA-F]{32}$"}}]
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema:
              type: object
              properties: {Code: {type: string}, To: {type: string}, VerificationSid: {type: string}}
      responses:
        "201":
          description: Created
          content: {application/json: {schema: {$ref: "#/components/schemas/verify.v2.service.verification_check"}}}
        "200":
          description: OK
          content: {application/json: {schema: {$ref: "#/components/schemas/verify.v2.service.verification_check"}}}
components:
  schemas:
    Service:
      type: object
      properties:
        sid: {type: string, nullable: true, pattern: "^VA[0-9a-fA-F]{32}$", example: VAaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa}
        friendly_name: {type: string, nullable: true}
    verify.v2.service.verification:
      type: object
      properties:
        sid: {type: string, nullable: true, pattern: "^VE[0-9a-fA-F]{32}$", example: VEaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa}
        service_sid: {type: string, nullable: true, pattern: "^VA[0-9a-fA-F]{32}$"}
        to: {type: string, nullable: true}
        channel: {type: string, nullable: true, enum: [sms, call, email]}
        status: {type: string, nullable: true, description: "Can be: `pending`, `approved` or `canceled`."}
    verify.v2.service.verification_check:
      type: object
      properties:
        sid:
          type: string
          minLength: 34
          maxLength: 34
          pattern: "^VE[0-9a-fA-F]{32}$"
          nullable: true
          example: VEaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
        service_sid: {type: string, minLength: 34, maxLength: 34, pattern: "^VA[0-9a-fA-F]{32}$", nullable: true}
        to: {type: string, nullable: true}
        channel: {type: string, nullable: true, enum: [sms, call, email]}
        status: {type: string, nullable: true, description: "Can be: `pending`, `approved` or `canceled`."}
        valid: {type: boolean, nullable: true}
"##;

/// The trial's world: the check is a create of a type named by `sid`, with nothing
/// else described for it.
fn world() -> Value {
    json!({
        "version": 1,
        "types": {"VerificationCheck": {"identity": {"member": "sid"}}},
        "operations": {
            "CreateVerificationCheck": {"kind": "create", "type": "VerificationCheck"}
        }
    })
}

const TO: &str = "+15558675310";

/// Sends `form` to `uri` as a form; answers the status and JSON body.
async fn post_form(h: &support::DescribedHarness, uri: &str, form: &str) -> (StatusCode, Value) {
    let router = mb_protocols::http_rest::router(Arc::clone(&h.rt));
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(form.to_owned()))
        .unwrap();
    let (status, _, body) = support::send_to(&router, req).await;
    (status, body)
}

/// Whether `sid` is `prefix` and 32 hex digits.
fn sid_of(sid: &Value, prefix: &str) -> bool {
    sid.as_str().is_some_and(|s| {
        s.len() == 34 && s.starts_with(prefix) && s[2..].chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// The service's sid, made at run time: a literal one trips secret scanners.
fn service_sid() -> String {
    format!("VA{}", "0".repeat(32))
}

#[tokio::test]
async fn a_create_of_a_type_no_route_reads_keeps_what_was_sent_and_the_path_names() {
    // The model fills what nothing sent: it picks the status from its description.
    let fill = json!({"status": "approved"});
    let h = support::described(CONTRACT, world(), vec![fill]);
    let service = service_sid();
    let (status, check) = post_form(
        &h,
        &format!("/v2/Services/{service}/VerificationCheck"),
        "Code=123456&To=%2B15558675310",
    )
    .await;
    // `200` is the lowest success status the check declares.
    assert_eq!(status, StatusCode::OK, "{check}");
    assert_eq!(check["to"], TO, "{check}");
    assert_eq!(check["service_sid"], service.as_str(), "{check}");
    assert!(sid_of(&check["sid"], "VE"), "{check}");
    assert_eq!(check["status"], "approved", "{check}");
    let sid = check["sid"].as_str().unwrap();
    let stored = h.stored("VerificationCheck", sid).await.unwrap();
    assert_eq!(stored["to"], TO, "{stored}");
    assert!(
        h.records().iter().any(
            |r| r.operation_id.as_deref() == Some("CreateVerificationCheck")
                && r.source.as_deref() == Some("state")
        ),
        "the executor answers the check"
    );
}

#[tokio::test]
async fn a_create_of_a_type_a_route_reads_still_keeps_what_was_sent() {
    let fill = json!({"status": "pending"});
    let h = support::described(CONTRACT, world(), vec![fill]);
    let service = service_sid();
    let (status, made) = post_form(
        &h,
        &format!("/v2/Services/{service}/Verifications"),
        "Channel=sms&To=%2B15558675310",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["to"], TO, "{made}");
    assert_eq!(made["channel"], "sms", "{made}");
    assert_eq!(made["service_sid"], service.as_str(), "{made}");
    assert!(sid_of(&made["sid"], "VE"), "{made}");
}
