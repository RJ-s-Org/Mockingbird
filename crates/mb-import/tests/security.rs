#![allow(clippy::unwrap_used)]

use mb_import::compile;
use mb_mcm::{ApiKeyLocation, Bundle, SecurityRequirement, SecurityScheme};

fn fixture(rel: &str) -> Vec<u8> {
    let path = format!("{}/../../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(path).unwrap()
}

fn req(schemes: Vec<SecurityScheme>) -> SecurityRequirement {
    SecurityRequirement { schemes }
}

fn security<'a>(b: &'a Bundle, op: &str) -> &'a [SecurityRequirement] {
    &b.operation(op).unwrap().security
}

#[test]
fn oas3_security_compiled() {
    let b = compile(&fixture("openapi/secured-3.0.yaml")).unwrap();
    // Document-level bearer applies where the operation declares nothing.
    assert_eq!(
        security(&b, "listWidgets"),
        [req(vec![SecurityScheme::Bearer])]
    );
    // Operation-level security overrides the document-level one.
    assert_eq!(
        security(&b, "searchWidgets"),
        [req(vec![SecurityScheme::ApiKey {
            location: ApiKeyLocation::Query,
            name: "apikey".into()
        }])]
    );
    // `security: []` means none.
    assert!(security(&b, "getHealth").is_empty());
    // `{}` keeps an empty requirement: authentication is optional.
    assert_eq!(
        security(&b, "getWidget"),
        [req(vec![]), req(vec![SecurityScheme::Bearer])]
    );
    assert_eq!(
        security(&b, "createWidget"),
        [req(vec![SecurityScheme::Basic])]
    );
    assert_eq!(
        security(&b, "deleteWidget"),
        [req(vec![SecurityScheme::OAuth2 {
            scopes: vec!["widgets:write".into()]
        }])]
    );
    assert_eq!(
        security(&b, "getMe"),
        [req(vec![SecurityScheme::OpenIdConnect {
            scopes: vec!["openid".into(), "profile".into()]
        }])]
    );
    // Two schemes in one requirement must both be met (AND).
    assert_eq!(
        security(&b, "updateWidget"),
        [req(vec![
            SecurityScheme::ApiKey {
                location: ApiKeyLocation::Cookie,
                name: "session_id".into()
            },
            SecurityScheme::ApiKey {
                location: ApiKeyLocation::Header,
                name: "X-Partner-Key".into()
            },
        ])]
    );
    // Unknown schemes are skipped; a requirement of only unknown schemes is dropped.
    assert_eq!(security(&b, "getAdmin"), [req(vec![SecurityScheme::Basic])]);
}

#[test]
fn swagger2_security_compiled() {
    let b = compile(&fixture("swagger/secured-2.0.json")).unwrap();
    assert_eq!(
        security(&b, "listItems"),
        [req(vec![SecurityScheme::ApiKey {
            location: ApiKeyLocation::Header,
            name: "X-API-Key".into()
        }])]
    );
    assert_eq!(
        security(&b, "createItem"),
        [req(vec![SecurityScheme::Basic])]
    );
    assert_eq!(
        security(&b, "getItem"),
        [req(vec![SecurityScheme::OAuth2 {
            scopes: vec!["items:read".into()]
        }])]
    );
    assert!(security(&b, "deleteItem").is_empty());
}

/// A contract without security compiles exactly as before: the stored
/// `openapi3` snapshot of the orders fixture still matches.
#[test]
fn orders_snapshot_unchanged() {
    let b = compile(&fixture("openapi/orders-3.0.yaml")).unwrap();
    assert!(b.operations.iter().all(|o| o.security.is_empty()));
    insta::with_settings!({prepend_module_to_snapshot => false}, {
        insta::assert_yaml_snapshot!("openapi3__orders_30_snapshot", b, {".source_digest" => "[digest]"});
    });
}
