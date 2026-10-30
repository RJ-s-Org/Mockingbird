//! Standard traits of the public types: `Display` and `Error` on the error
//! types, `Debug` without secrets, and the OpenAPI schemas of the
//! `mb-auth`/`mb-store` enums naming the same values serde writes.
#![cfg(test)]

use std::error::Error;

use axum::http::StatusCode;
use mb_admin::Problem;
use mb_admin::channels::ChannelControlError;
use serde_json::{Value, json};

#[test]
fn problem_displays_status_title_and_detail_and_is_an_error() {
    let p = Problem::not_found("no mock \"orders\"");
    assert_eq!(p.to_string(), "404 Not Found: no mock \"orders\"");
    let titled = Problem::invalid("Invalid webhooks", "webhooks[0].url: not absolute");
    assert_eq!(
        titled.to_string(),
        "422 Invalid webhooks: webhooks[0].url: not absolute"
    );
    let boxed: Box<dyn Error> = Box::new(Problem::new(StatusCode::BAD_GATEWAY, "upstream"));
    assert!(boxed.source().is_none());
}

#[test]
fn channel_control_error_displays_its_detail_and_chains_partial() {
    let failed = ChannelControlError::Failed("broker refused".into());
    assert_eq!(failed.to_string(), "broker refused");
    assert!(failed.source().is_none());

    let partial = ChannelControlError::Partial {
        published: Vec::new(),
        error: Box::new(ChannelControlError::NotRunning("stopped".into())),
    };
    assert_eq!(partial.to_string(), "stopped");
    let source = partial.source().map(ToString::to_string);
    assert_eq!(source.as_deref(), Some("stopped"));
}

/// The `enum` values of component schema `name`.
fn schema_enum(doc: &Value, name: &str) -> Value {
    doc["components"]["schemas"][name]["enum"].clone()
}

/// The serde names of `values`, as a JSON array.
fn wire<T: serde::Serialize>(values: &[T]) -> Value {
    json!(values)
}

#[test]
fn mirrored_enum_schemas_name_the_serde_values() {
    use mb_auth::{KeyScope, PrincipalKind, Role};
    use mb_store::{DataPlaneAuthMode, WebhookSigning};

    let doc = serde_json::to_value(mb_admin::openapi_document()).unwrap();
    let cases = [
        (
            "Role",
            wire(&[Role::Viewer, Role::Editor, Role::Admin, Role::Owner]),
        ),
        (
            "PrincipalKind",
            wire(&[
                PrincipalKind::User,
                PrincipalKind::ApiKey,
                PrincipalKind::Local,
            ]),
        ),
        ("KeyScope", wire(&[KeyScope::Admin, KeyScope::DataPlane])),
        (
            "WebhookSigning",
            wire(&[
                WebhookSigning::Stripe,
                WebhookSigning::HmacSha256,
                WebhookSigning::None,
            ]),
        ),
        (
            "DataPlaneAuthMode",
            wire(&[
                DataPlaneAuthMode::None,
                DataPlaneAuthMode::ApiKey,
                DataPlaneAuthMode::PassthroughValidate,
            ]),
        ),
    ];
    for (name, serde) in cases {
        assert_eq!(schema_enum(&doc, name), serde, "schema {name}");
    }
}
