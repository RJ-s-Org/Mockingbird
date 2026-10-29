#![cfg(test)]
use mb_import::compile;
use mb_mcm::{ApiKeyLocation, Bundle, SecurityRequirement, SecurityScheme};

use crate::support::fixture;

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

/// A contract without security compiles every operation unsecured, and
/// security only adds requirements: the same contract with a document-level
/// bearer requirement compiles to the same bundle plus that requirement on
/// every operation. (`openapi3::orders_30_snapshot` owns the full snapshot
/// of this fixture.)
#[test]
fn security_adds_requirements_only() {
    let source = String::from_utf8(fixture("openapi/orders-3.0.yaml")).unwrap();
    let plain = compile(source.as_bytes()).unwrap();
    assert!(!plain.operations.is_empty());
    assert!(plain.operations.iter().all(|o| o.security.is_empty()));

    let secured = source.replacen(
        "\ncomponents:\n",
        "\ncomponents:\n  securitySchemes:\n    bearer: {type: http, scheme: bearer}\n",
        1,
    ) + "security:\n  - bearer: []\n";
    assert_ne!(secured, source, "the fixture has a components section");
    let mut b = compile(secured.as_bytes()).unwrap();
    for o in &mut b.operations {
        assert_eq!(o.security, [req(vec![SecurityScheme::Bearer])], "{}", o.id);
        o.security.clear();
    }
    b.source_digest.clone_from(&plain.source_digest);
    assert_eq!(b, plain);
}
