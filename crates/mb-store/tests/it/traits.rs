#![cfg(test)]
//! Common traits on the public types: `Debug` without secrets, and
//! `Display`/`FromStr` matching the wire names.

use std::str::FromStr;

use mb_store::{
    ContractStatus, ImportJobStatus, PgStore, RequestKind, ResourceSource, SqliteControlStore,
    StoreError,
};

#[tokio::test]
async fn store_debug_never_shows_the_connection_url() {
    let pg = PgStore::connect_lazy("postgres://mb:s3ntinel-db-pass@db.example:5432/mb")
        .expect("lazy connect");
    let shown = format!("{pg:?}");
    assert!(!shown.contains("s3ntinel"), "password leaked: {shown}");
    assert!(!shown.contains("db.example"), "url leaked: {shown}");
    assert!(shown.starts_with("PgStore"), "{shown}");

    let sqlite = SqliteControlStore::connect("sqlite::memory:")
        .await
        .expect("connect");
    let shown = format!("{sqlite:?}");
    assert!(shown.starts_with("SqliteControlStore"), "{shown}");
    assert!(shown.contains("in_memory: true"), "{shown}");
}

/// `Display` is `as_str`, and `FromStr` reads it back.
fn round_trips<T>(values: &[T])
where
    T: Copy + std::fmt::Display + FromStr<Err = StoreError> + PartialEq + std::fmt::Debug,
{
    for value in values {
        let name = value.to_string();
        assert_eq!(name.parse::<T>().ok(), Some(*value), "{name}");
    }
    assert!(matches!(
        "no-such-name".parse::<T>(),
        Err(StoreError::Invalid(message)) if message.contains("\"no-such-name\"")
    ));
}

#[test]
fn wire_names_display_and_parse() {
    round_trips(&[ResourceSource::File, ResourceSource::Api]);
    round_trips(&[
        ContractStatus::Draft,
        ContractStatus::Approved,
        ContractStatus::Active,
        ContractStatus::Archived,
    ]);
    round_trips(&[
        ImportJobStatus::Queued,
        ImportJobStatus::Running,
        ImportJobStatus::Done,
        ImportJobStatus::Failed,
        ImportJobStatus::Canceled,
    ]);
    round_trips(&[
        RequestKind::Http,
        RequestKind::Graphql,
        RequestKind::Grpc,
        RequestKind::Soap,
        RequestKind::Event,
    ]);
    assert_eq!(ContractStatus::Active.to_string(), "active");
    assert_eq!(
        RequestKind::Graphql.to_string(),
        RequestKind::Graphql.as_str()
    );
}
