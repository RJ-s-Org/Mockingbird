#![cfg(test)]
//! The library's error types, `Debug` output and name conversions.

use std::error::Error as _;
use std::str::FromStr as _;

use anyhow::anyhow;
use axum::http::StatusCode;
use mockingbird::client::ClientError;
use mockingbird::file_config::{Role, Unmapped};
use mockingbird::server::ServeError;
use mockingbird::snapshot::{Snapshot, SnapshotMode};

#[test]
fn client_error_displays_and_chains() {
    let api = ClientError::Api {
        status: StatusCode::NOT_FOUND,
        body: r#"{"title":"Not Found"}"#.into(),
    };
    assert_eq!(
        api.to_string(),
        r#"the Admin API answered 404 Not Found: {"title":"Not Found"}"#
    );
    assert!(api.source().is_none());
    assert_eq!(
        ClientError::Usage("bad flag".into()).to_string(),
        "bad flag"
    );
    let incomplete = ClientError::Incomplete {
        output: serde_json::json!({}),
        message: "2 of 3 imported".into(),
    };
    assert_eq!(incomplete.to_string(), "2 of 3 imported");

    // `Other` is transparent: its message, then its causes as `source()`,
    // so a chain printer shows each cause once.
    let other = ClientError::Other(anyhow!("connection refused").context("GET /mocks"));
    assert_eq!(other.to_string(), "GET /mocks");
    assert_eq!(
        other.source().map(ToString::to_string).as_deref(),
        Some("connection refused")
    );
    let wrapped = anyhow::Error::new(other);
    assert_eq!(format!("{wrapped:#}"), "GET /mocks: connection refused");
}

#[test]
fn serve_error_chains_its_cause_once() {
    let usage = ServeError::Usage("--watch needs a config file".into());
    assert_eq!(usage.to_string(), "--watch needs a config file");
    assert!(usage.source().is_none());

    let other = ServeError::Other(anyhow!("address in use").context("binding 127.0.0.1:8080"));
    assert_eq!(other.to_string(), "binding 127.0.0.1:8080");
    assert_eq!(
        other.source().map(ToString::to_string).as_deref(),
        Some("address in use")
    );
    let wrapped = anyhow::Error::new(other);
    assert_eq!(
        format!("{wrapped:#}"),
        "binding 127.0.0.1:8080: address in use"
    );
}

#[test]
fn role_and_unmapped_display_and_parse_their_names() {
    for role in [Role::All, Role::Control, Role::Data] {
        assert_eq!(role.to_string(), role.as_str());
        assert_eq!(Role::from_str(role.as_str()), Ok(role));
    }
    for unmapped in [Unmapped::Pass, Unmapped::Block] {
        assert_eq!(unmapped.to_string(), unmapped.as_str());
        assert_eq!(unmapped.as_str().parse::<Unmapped>(), Ok(unmapped));
    }
    assert_eq!("BLOCK".parse::<Unmapped>(), Ok(Unmapped::Block));
    let err = "drop".parse::<Unmapped>().unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"unknown proxy.unmapped value "drop"; expected pass or block"#
    );
    let _: &dyn std::error::Error = &err;
}

#[test]
fn snapshot_open_with_names_the_mode() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Snapshot::open_with(dir.path(), SnapshotMode::RECORD).unwrap();
    assert!(!rec.replays());
    assert!(rec.records());
    let play = Snapshot::open_with(dir.path(), SnapshotMode::REPLAY).unwrap();
    assert!(play.replays());
    assert!(!play.records());
    assert!(format!("{play:?}").contains("Snapshot"));
}
