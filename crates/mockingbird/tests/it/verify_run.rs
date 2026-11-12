//! `mockingbird verify --run "<cmd>"`: runs the app's command against the
//! mocks as `mockingbird run` does and reports, from the requests it made,
//! what points at the mock.
#![cfg(test)]

use crate::support::{fixture, free_port, logged_admin, on_free_ports, port_was_busy};
use serde_json::Value;
use std::path::Path;

/// A config serving `orders` for `api.orders.test` on free ports (unmapped
/// hosts blocked); returns the data-plane and admin ports.
fn write_config(dir: &Path) -> (u16, u16) {
    let (data, admin) = (free_port(), free_port());
    std::fs::write(
        dir.join("mockingbird.yaml"),
        format!(
            "storage:\n  url: \"sqlite::memory:\"\nserver:\n  port: {data}\n  admin_port: {admin}\n  grpc_port: {}\nproxy:\n  port: {}\n  unmapped: block\nmocks:\n  orders:\n    contract: {}\n    route_prefix: /\n    hosts: [api.orders.test]\n",
            free_port(),
            free_port(),
            fixture("openapi/orders-3.0.yaml").display()
        ),
    )
    .unwrap();
    (data, admin)
}

fn mockingbird(dir: &Path) -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::cargo_bin("mockingbird").unwrap();
    cmd.current_dir(dir)
        .env("MB_CA_DIR", dir.join("ca"))
        .env_remove("MB_ADMIN_URL")
        .env_remove("MB_STATE_DIR")
        .env_remove("HTTPS_PROXY")
        .env_remove("HTTP_PROXY")
        .env_remove("https_proxy")
        .env_remove("http_proxy");
    cmd
}

/// `verify --run <command> --format json` (plus `extra`), starting the
/// server unless one answers.
fn verify_run(dir: &Path, command: &str, extra: &[&str]) -> std::process::Output {
    mockingbird(dir)
        .args(["verify", "--offline", "--format", "json", "--state-dir"])
        .arg(dir.join("state"))
        .args(extra)
        .args(["--run", command])
        .timeout(std::time::Duration::from_secs(120))
        .output()
        .unwrap()
}

/// [`verify_run`] on a fresh config, again on other ports while one was
/// taken before the server bound it. Returns the exit code and the JSON.
fn fresh(dir: &Path, command: &str, extra: &[&str]) -> (Option<i32>, Value) {
    on_free_ports(|| {
        let (_, admin) = write_config(dir);
        let out = verify_run(dir, command, extra);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if port_was_busy(&stderr) {
            return Err(stderr.into_owned());
        }
        logged_admin(&dir.join("state/server.log"), admin)?;
        let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}\nstdout: {}\nstderr: {stderr}",
                String::from_utf8_lossy(&out.stdout)
            )
        });
        Ok((out.status.code(), json))
    })
}

fn kinds(json: &Value) -> Vec<&str> {
    json["failures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap())
        .collect()
}

#[test]
fn verify_run_reports_an_unmatched_route_and_a_4xx_with_next_steps() {
    let dir = tempfile::tempdir().unwrap();
    let (code, json) = fresh(
        dir.path(),
        "curl -s -o /dev/null https://api.orders.test/nope && curl -s -o /dev/null 'https://api.orders.test/orders?status=bogus'",
        &[],
    );
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(json["ok"], false);
    assert_eq!(json["command"]["exit_code"], 0, "{json:#}");
    assert_eq!(json["window"]["requests"], 2, "{json:#}");
    assert_eq!(json["window"]["server_started"], true);
    assert_eq!(
        kinds(&json),
        ["unmatched_route", "error_status"],
        "{json:#}"
    );
    let unmatched = &json["failures"][0];
    assert_eq!(unmatched["mock"], "orders");
    assert!(
        unmatched["detail"]
            .as_str()
            .unwrap()
            .contains("GET /nope matched no operation"),
        "{unmatched}"
    );
    assert!(
        unmatched["next_step"]
            .as_str()
            .unwrap()
            .contains("edit_contract"),
        "{unmatched}"
    );
    let error = &json["failures"][1];
    assert_eq!(error["operation"], "listOrders");
    assert_eq!(error["status"], 400);
    // The problem's detail, read from the recorded response.
    assert!(
        error["detail"].as_str().unwrap().contains("query/status"),
        "{error}"
    );
    let step = error["next_step"].as_str().unwrap();
    assert!(
        step.contains("listOrders") && step.contains("get_world") && step.contains("get_request"),
        "{step}"
    );
}

#[test]
fn verify_run_passes_a_clean_command() {
    let dir = tempfile::tempdir().unwrap();
    let (code, json) = fresh(
        dir.path(),
        "curl -sSf -o /dev/null https://api.orders.test/orders",
        &[],
    );
    assert_eq!(code, Some(0), "{json:#}");
    assert_eq!(json["ok"], true);
    assert_eq!(json["command"]["exit_code"], 0);
    assert_eq!(json["window"]["requests"], 1);
    assert_eq!(json["failures"], serde_json::json!([]));
}

#[test]
fn verify_run_reports_a_failing_command_with_its_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let (code, json) = fresh(
        dir.path(),
        "curl -sSf -o /dev/null https://api.orders.test/orders; echo 'FAIL orders spec'; exit 3",
        &[],
    );
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(json["ok"], false);
    assert_eq!(json["command"]["exit_code"], 3);
    assert_eq!(json["failures"], serde_json::json!([]));
    assert!(
        json["command"]["output_tail"]
            .as_str()
            .unwrap()
            .contains("FAIL orders spec"),
        "{json:#}"
    );
}

#[test]
fn verify_run_stops_a_command_at_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let (code, json) = fresh(dir.path(), "sleep 60", &["--timeout", "1s"]);
    assert_eq!(code, Some(1), "{json:#}");
    assert_eq!(json["command"]["timed_out"], true);
    assert_eq!(json["command"]["exit_code"], Value::Null);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(45),
        "{:?}",
        started.elapsed()
    );
}

/// Requests logged before the command started do not count: against a
/// running server, only the command's own requests are examined.
#[test]
fn verify_run_only_counts_requests_inside_its_window() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let data = on_free_ports(|| {
        let (data, admin) = write_config(dir.path());
        let out = mockingbird(dir.path())
            .args([
                "server",
                "start",
                "-c",
                "mockingbird.yaml",
                "--offline",
                "--proxy",
                "--no-watch",
                "--state-dir",
            ])
            .arg(&state)
            .timeout(std::time::Duration::from_secs(90))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            let _ = mockingbird(dir.path())
                .args(["server", "stop", "--state-dir"])
                .arg(&state)
                .output();
            return Err(stderr.into_owned());
        }
        logged_admin(&state.join("server.log"), admin)?;
        Ok(data)
    });
    // An unmatched route, before the window.
    let before = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-H", "Host: api.orders.test"])
        .arg(format!("http://127.0.0.1:{data}/nope"))
        .status()
        .unwrap();
    let out = verify_run(
        dir.path(),
        "curl -sSf -o /dev/null https://api.orders.test/orders",
        &[],
    );
    let _ = mockingbird(dir.path())
        .args(["server", "stop", "--state-dir"])
        .arg(&state)
        .output();
    assert!(before.success());
    let json: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("{e}: {}", String::from_utf8_lossy(&out.stderr));
    });
    assert_eq!(out.status.code(), Some(0), "{json:#}");
    assert_eq!(json["window"]["requests"], 1, "{json:#}");
    assert_eq!(json["window"]["server_started"], false);
    assert_eq!(json["failures"], serde_json::json!([]));
}

#[test]
fn verify_run_text_names_each_failure_with_its_next_step() {
    let dir = tempfile::tempdir().unwrap();
    let out = on_free_ports(|| {
        let (_, admin) = write_config(dir.path());
        let out = mockingbird(dir.path())
            .args(["verify", "--offline", "--state-dir"])
            .arg(dir.path().join("state"))
            .args(["--run", "curl -s -o /dev/null https://api.orders.test/nope"])
            .timeout(std::time::Duration::from_secs(120))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if port_was_busy(&stderr) {
            return Err(stderr.into_owned());
        }
        logged_admin(&dir.path().join("state/server.log"), admin)?;
        Ok(out)
    });
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with(
            "verify --run failed: `curl -s -o /dev/null https://api.orders.test/nope` exited 0"
        ),
        "{text}"
    );
    assert!(
        text.contains("  unmatched_route: orders: GET /nope"),
        "{text}"
    );
    assert!(
        text.contains("    Next: Mock `orders` has no operation"),
        "{text}"
    );
}
