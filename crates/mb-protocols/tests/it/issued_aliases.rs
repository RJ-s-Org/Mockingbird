//! A member a create answers beside the record's identity, which the record itself
//! does not declare, named in `identity.alternates`: a token the server issues for the
//! record (Plaid's `access_token` for an Item). The create mints it and keeps it with
//! the record; a later request naming the record by it reads that record from state;
//! one naming a token the server never issued answers the declared not-found status.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::partial_creates::CONTRACT;
use crate::support;

fn world() -> Value {
    json!({
        "types": {"item": {"identity": {"member": "item_id", "alternates": ["access_token"]}}},
        "operations": {
            "exchange": {"kind": "create", "type": "item", "answer": {"record": ""}},
            "itemGet": {"kind": "read", "type": "item", "target": {"member": "access_token"},
                        "answer": {"record": "/item"}, "not_found": 400}
        }
    })
}

async fn exchanged(h: &support::DescribedHarness) -> Value {
    let (status, headers, body) = h
        .send(
            "POST",
            "/item/public_token/exchange",
            Some(json!({"public_token": "public-sandbox-1"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-mockingbird-source"], "state", "{body}");
    body
}

#[tokio::test]
async fn an_issued_token_names_its_record_on_later_reads() {
    let fill = json!({"institution_id": "ins_1", "products": ["auth"]});
    let h = support::described(CONTRACT, world(), vec![fill]);
    let ex = exchanged(&h).await;
    let token = ex["access_token"].as_str().unwrap_or_default().to_owned();
    assert!(!token.is_empty(), "{ex}");
    let (status, headers, body) = h
        .send(
            "POST",
            "/item/get",
            Some(json!({"access_token": token})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-mockingbird-source"], "state", "{body}");
    assert_eq!(body["item"]["item_id"], ex["item_id"], "{body}");
    assert_eq!(body["item"]["institution_id"], json!("ins_1"), "{body}");
    // The token is the server's, not a member of the record it reads.
    assert!(body["item"].get("access_token").is_none(), "{body}");
}

#[tokio::test]
async fn a_token_the_server_never_issued_is_not_found() {
    let fill = json!({"institution_id": "ins_1", "products": ["auth"]});
    let h = support::described(CONTRACT, world(), vec![fill]);
    exchanged(&h).await;
    let (status, _, body) = h
        .send(
            "POST",
            "/item/get",
            Some(json!({"access_token": "access-sandbox-never-issued"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
