//! Scenarios created through the Admin API, applied on the data plane.
#![cfg(test)]

use crate::support::{fixture, serve};
use mockingbird::config::{App, ServeArgs, build_app};
use serde_json::{Value, json};

struct Running {
    data: String,
    admin: String,
    _app: App,
}

async fn start() -> Running {
    let args = ServeArgs {
        offline: true,
        ..ServeArgs::default()
    };
    let app = build_app(&args, None).await.unwrap();
    Running {
        data: serve(app.data.clone()).await,
        admin: serve(app.admin.clone()).await,
        _app: app,
    }
}

/// The UX review's seeding: an `orders` mock with header overrides, the
/// orders contract, and an `out-of-stock` scenario forcing 409 on
/// `createOrder`.
async fn seed(app: &Running, contract: &[u8]) {
    let http = reqwest::Client::new();
    let w = format!("{}/api/v1/workspaces/default", app.admin);
    let r = http
        .post(format!("{w}/mocks"))
        .json(&json!({"name": "orders", "header_overrides": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let r = http
        .post(format!("{w}/mocks/orders/contracts?activate=true"))
        .header("content-type", "application/yaml")
        .body(contract.to_vec())
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let r = http
        .put(format!("{w}/mocks/orders/scenarios/out-of-stock"))
        .json(&json!({
            "status": 409,
            "operations": ["createOrder"],
            "set": {"code": "OUT_OF_STOCK", "message": "Item is out of stock"},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

async fn create_order(app: &Running, scenario: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{}/orders/orders", app.data))
        .json(&json!({"customerId": "c9", "total": 1}));
    if let Some(s) = scenario {
        req = req.header("X-Mockingbird-Scenario", s);
    }
    req.send().await.unwrap()
}

/// orders-3.0 with a 409 response declared on `createOrder`.
fn orders_with_conflict() -> Vec<u8> {
    let src = std::fs::read_to_string(fixture("openapi/orders-3.0.yaml")).unwrap();
    let conflict = r#"        "409":
          description: Conflict
          content:
            application/json:
              schema:
                type: object
                required: [code, message]
                properties:
                  code: {type: string}
                  message: {type: string}
        "400":"#;
    let out = src.replacen(r#"        "400":"#, conflict, 1);
    assert_ne!(out, src);
    out.into_bytes()
}

#[tokio::test]
async fn scenario_status_applies_where_declared() {
    let app = start().await;
    seed(&app, &orders_with_conflict()).await;

    // The data plane reloads the mock asynchronously after the PUT.
    let mut tries = 0;
    let (status, body) = loop {
        let r = create_order(&app, Some("out-of-stock")).await;
        let status = r.status();
        let body: Value = r.json().await.unwrap();
        tries += 1;
        if status != 400 || tries == 50 {
            break (status, body);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "OUT_OF_STOCK", "{body}");
    assert_eq!(body["message"], "Item is out of stock", "{body}");

    let r = create_order(&app, None).await;
    assert_eq!(r.status(), 201);
}

#[tokio::test]
async fn undeclared_server_error_scenario_is_answered_natively() {
    let app = start().await;
    seed(&app, &orders_with_conflict()).await;
    let r = reqwest::Client::new()
        .put(format!(
            "{}/api/v1/workspaces/default/mocks/orders/scenarios/outage",
            app.admin
        ))
        .json(&json!({"status": 503, "operations": ["createOrder"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());

    let mut tries = 0;
    let r = loop {
        let r = create_order(&app, Some("outage")).await;
        tries += 1;
        if r.status() != 400 || tries == 50 {
            break r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()["content-type"], "application/problem+json");
}

/// SNAPSHOT-OPTIN: `snapshot export --include scenarios,chaos,limits`
/// against a real Admin API, then `snapshot import` into an empty server,
/// restores the scenario, the default scenario, chaos and limits; a plain
/// export of the same mock carries none of them.
#[tokio::test]
async fn an_opt_in_snapshot_restores_scenarios_chaos_and_limits() {
    use mockingbird::snapshot::{
        self, SnapshotArgs, SnapshotCommand, SnapshotExportArgs, SnapshotPart,
    };
    let source = start().await;
    seed(&source, &orders_with_conflict()).await;
    let http = reqwest::Client::new();
    let w = format!("{}/api/v1/workspaces/default", source.admin);
    let behaviour = json!({
        "chaos": {"error_rate": 0.0, "drop_stream_rate": 0.5},
        "limits": {"max_request_bytes": 4096},
        "default_scenario": "out-of-stock",
    });
    let r = http
        .patch(format!("{w}/mocks/orders"))
        .json(&behaviour)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());

    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    snapshot::run(
        &source.admin,
        None,
        "default",
        SnapshotCommand::Export(SnapshotArgs { dir: plain.clone() }),
    )
    .await
    .unwrap();
    let mock: Value =
        serde_json::from_slice(&std::fs::read(plain.join("mocks/orders.json")).unwrap()).unwrap();
    for key in ["chaos", "limits", "default_scenario"] {
        assert!(mock.get(key).is_none(), "{key} in a default export: {mock}");
    }
    assert!(!plain.join("scenarios").exists());

    let full = tmp.path().join("full");
    let args = SnapshotExportArgs::new(
        &full,
        [
            SnapshotPart::Scenarios,
            SnapshotPart::Chaos,
            SnapshotPart::Limits,
        ],
    );
    snapshot::export_with(&source.admin, None, "default", &args)
        .await
        .unwrap();

    let target = start().await;
    let out = snapshot::run(
        &target.admin,
        None,
        "default",
        SnapshotCommand::Import(SnapshotArgs { dir: full.clone() }),
    )
    .await
    .unwrap();
    assert_eq!(out["created_mocks"], json!(["orders"]), "{out}");
    assert_eq!(out["restored_scenarios"], 1, "{out}");
    let t = format!("{}/api/v1/workspaces/default", target.admin);
    let get = |url: String| {
        let http = http.clone();
        async move {
            let r = http.get(url).send().await.unwrap();
            assert_eq!(r.status(), 200);
            r.json::<Value>().await.unwrap()
        }
    };
    let restored = get(format!("{t}/mocks/orders")).await;
    let original = get(format!("{w}/mocks/orders")).await;
    for key in ["chaos", "limits", "default_scenario"] {
        assert_eq!(restored[key], original[key], "{key}: {restored}");
    }
    assert_eq!(restored["default_scenario"], "out-of-stock");
    let mut scenarios = get(format!("{t}/mocks/orders/scenarios")).await;
    let mut expected = get(format!("{w}/mocks/orders/scenarios")).await;
    assert_eq!(scenarios.as_array().map(Vec::len), Some(1), "{scenarios}");
    scenarios[0].as_object_mut().unwrap().remove("source");
    expected[0].as_object_mut().unwrap().remove("source");
    assert_eq!(scenarios, expected);
}
